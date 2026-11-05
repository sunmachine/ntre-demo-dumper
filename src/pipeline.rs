//! Per-demo orchestration: read the file, skip it if the database already
//! holds it, stream its frames through the registered extractors, and persist
//! everything in one transaction. This is the only module that sees all three
//! layers (`demo`, `extract`, `output`); it holds no parsing or SQL logic of
//! its own.

use anyhow::{Context, Result};
use regex::Regex;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Instant;

use crate::demo::clock::Clock;
use crate::demo::header::{DemoHeader, HEADER_SIZE};
use crate::demo::identity::DemoIdentity;
use crate::extract::{
    alive, announcements, console, entities, ghost_holds, inferred_hits, inputs, net, pov, rounds, DemoContext,
    FrameExtractor,
};
use crate::output::sqlite::Db;

pub struct Options {
    pub patterns: Vec<Regex>,
    pub all_strings: bool,
    pub pov_sample: u32,
    pub threads: usize,
    /// Keep rounds up to this one and cut the demo's rows at the next
    /// round's start.
    pub last_round: Option<i32>,
}

pub enum LogLevel {
    Warning,
    Error,
}

/// Print collected log lines as a `logs:` section, with the level colored
/// when stdout is a terminal (yellow warning, red error; NO_COLOR disables).
/// Goes to stdout so it can't interleave out of order with the surrounding
/// report. No-op when there is nothing to report.
pub fn print_logs(logs: &[(LogLevel, String)]) {
    if logs.is_empty() {
        return;
    }
    let color = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    println!("  logs:");
    for (level, message) in logs {
        let label = match (level, color) {
            (LogLevel::Warning, true) => "\x1b[33mwarning\x1b[0m",
            (LogLevel::Warning, false) => "warning",
            (LogLevel::Error, true) => "\x1b[31merror\x1b[0m",
            (LogLevel::Error, false) => "error",
        };
        println!("    {label}: {message}");
    }
    println!();
}

/// What `parse_one` did with a demo. A skip is not a failure, because the
/// demo's rows are already in the database.
pub enum Outcome {
    Parsed,
    Skipped,
}

pub fn parse_one(path: &Path, db: &Db, opts: &Options) -> Result<Outcome> {
    let started = Instant::now();
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let identity = DemoIdentity::of(&data);
    if let Some((demo_id, first_path)) = db.find_demo(&identity.sha256)? {
        println!("  demo file    {}", path.display());
        println!(
            "  skipped      already in the database as demo #{demo_id}, first parsed from {first_path}"
        );
        println!();
        return Ok(Outcome::Skipped);
    }
    let header = DemoHeader::parse(&data)?;
    println!("  demo file    {}", path.display());
    println!("  map          {}", header.map_name);
    println!("  server       {}", header.server_name);
    println!("  recorded by  {}", header.client_name);
    println!(
        "  length       {:.1}s / {} ticks ({:.1} ticks/s)",
        header.playback_seconds,
        header.playback_ticks,
        header.tickrate()
    );

    let ctx = DemoContext { data: &data, header: &header, threads: opts.threads };

    // Register extractors here (see extract/mod.rs for the recipe). The
    // announcements and net passes are named because rounds need both, and
    // the inferred-hits pass reads the net pass's deaths and roster.
    let mut announcements = announcements::Announcements::new(opts.patterns.clone(), opts.all_strings);
    let mut net = net::NetPass::default();
    let mut pov = pov::PovSampler::new(opts.pov_sample);
    let mut console = console::ConsoleCmds::default();
    let mut inputs = inputs::RecorderInputs::default();
    let mut extractors: Vec<&mut dyn FrameExtractor> =
        vec![&mut announcements, &mut pov, &mut console, &mut inputs, &mut net];

    let mut logs: Vec<(LogLevel, String)> = Vec::new();

    // Every pass reads its ticks from the server's clock, because a POV
    // recording's frame headers carry the client's skewed clock.
    let clock = Clock::new(&data, HEADER_SIZE, header.network_protocol);

    // A demo stopped mid-write can end partway through a frame; keep
    // everything extracted before the break and warn instead of failing.
    for frame in clock.frames(&data, HEADER_SIZE) {
        match frame {
            Ok(frame) => {
                for extractor in &mut extractors {
                    extractor.on_frame(&ctx, &frame);
                }
            }
            Err(e) => {
                logs.push((LogLevel::Warning, format!("frame walk stopped early: {e}")));
                break;
            }
        }
    }

    // Whole-file entity pass (all-player positions/aim/weapons); a failure
    // here degrades to a warning and never blocks the frame-level data.
    let entity_output = match entities::run(&data, &clock) {
        Ok(output) => Some(output),
        Err(e) => {
            logs.push((LogLevel::Warning, format!("entity pass failed: {e}")));
            None
        }
    };

    drop(extractors);
    if let Some(output) = &entity_output {
        net.apply_neo_names(&output.neo_name_changes);
        net.place_teams(&output.resource_samples);
    }
    // Without the entity pass the teams are unknown, so only world deaths
    // and suicides get a teamkill flag.
    net.mark_teamkills(entity_output.as_ref().map(|o| &o.game_type_changes[..]).unwrap_or_default());
    // The roster should hold every connection, so a death naming a userid
    // outside it points at a roster gap rather than a world death.
    let missing = net.deaths_missing_from_roster();
    if missing > 0 {
        logs.push((
            LogLevel::Warning,
            format!("{missing} death(s) name a player missing from the roster; their names and Steam IDs are NULL"),
        ));
    }
    let server_info = net.server_info.clone();
    let hits = entity_output
        .as_ref()
        .map(|output| inferred_hits::infer(&output.samples, &net.deaths, &net.players))
        .unwrap_or_default();
    // Rounds take start markers from the announcements, ends from the net
    // pass, and the capturer from entity state, so they wait for all three.
    let evidence = rounds::Evidence::new(entity_output.as_ref(), &net.players, &net.deaths);
    let mut rounds = rounds::derive(announcements.found(&ctx), &net.round_results, &evidence);
    if let Some(output) = &entity_output {
        rounds::place_ghost_spawns(&mut rounds, &output.ghost_samples);
    }
    let cut = match opts.last_round {
        Some(last) => rounds::cut_after(&mut rounds, last)?,
        None => None,
    };
    net.place_deaths(&rounds);
    let alive_counts = entity_output
        .as_ref()
        .map(|output| alive::counts(&output.resource_samples, &net.deaths, &net.players, &rounds))
        .unwrap_or_default();
    let holds = entity_output
        .as_ref()
        .map(|output| ghost_holds::holds(&output.carrier_changes, &net.players, &rounds))
        .unwrap_or_default();
    // Tables that name a player by entity slot also store the Steam ID of
    // whoever held the slot at the row's tick.
    let owners = net::SlotOwners::new(&net.players);
    let recorder_steamid = server_info
        .as_ref()
        .and_then(|s| owners.steamid_at(u32::from(s.player_slot) + 1, 0))
        .map(str::to_string);
    let mut extractors: Vec<&mut dyn FrameExtractor> =
        vec![&mut announcements, &mut pov, &mut console, &mut inputs, &mut net];

    db.begin()?;
    let demo_id = identity.id;
    db.insert_demo(
        &identity,
        &path.display().to_string(),
        &header,
        server_info.as_ref(),
        recorder_steamid.as_deref(),
        opts.last_round,
    )?;
    let mut summary = Vec::new();
    for extractor in &mut extractors {
        summary.extend(extractor.persist(db, demo_id, &ctx)?);
    }
    db.insert_rounds(demo_id, &rounds, &|userid| net.steamid_of(userid))?;
    summary.push(("rounds".into(), rounds.len()));
    if let Some(output) = &entity_output {
        db.insert_player_samples(demo_id, &output.samples, &owners)?;
        db.insert_ghost_samples(demo_id, &output.ghost_samples)?;
        db.insert_player_resource(demo_id, &output.resource_samples, &owners)?;
        db.insert_attacker_hits(demo_id, &output.damage_samples, &owners)?;
        db.insert_inferred_hits(demo_id, &hits, &owners)?;
        db.insert_alive_counts(demo_id, &alive_counts)?;
        db.insert_ghost_holds(demo_id, &holds, &owners)?;
        summary.push(("player samples".into(), output.samples.len()));
        summary.push(("ghost samples".into(), output.ghost_samples.len()));
        summary.push(("resource samples".into(), output.resource_samples.len()));
        summary.push(("attacker hits".into(), output.damage_samples.len()));
        summary.push(("inferred hits".into(), hits.len()));
        summary.push(("alive counts".into(), alive_counts.len()));
        summary.push(("ghost holds".into(), holds.len()));
        if output.player_classes.is_empty() {
            logs.push((
                LogLevel::Warning,
                "no player classes found in sendtables (entity samples empty?)".into(),
            ));
        }
        if let Some(warning) = &output.warning {
            logs.push((LogLevel::Warning, warning.clone()));
        }
    }
    // Every pass ran over the whole file, so the cut is made on the rows
    // they wrote, inside the same transaction.
    if let (Some(tick), Some(last)) = (cut, opts.last_round) {
        summary.push((format!("rows cut after round {last}"), db.cut_at(demo_id, tick)?));
    }
    db.commit()?;

    println!();
    print_logs(&logs);
    println!("  demo #{demo_id} extracted");
    let width = summary.iter().map(|(l, _)| l.len()).max().unwrap_or(0).max("parsed in".len());
    for (label, count) in &summary {
        println!("    {label:<width$}  {count}");
    }
    println!(
        "    {:<width$}  {:.2}s ({} thread{})",
        "parsed in",
        started.elapsed().as_secs_f64(),
        opts.threads,
        if opts.threads == 1 { "" } else { "s" }
    );
    println!();
    Ok(Outcome::Parsed)
}
