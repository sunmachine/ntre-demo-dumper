//! Per-demo orchestration: read the file, stream its frames through the
//! registered extractors, and persist everything in one transaction. This is
//! the only module that sees all three layers (`demo`, `extract`, `output`);
//! it holds no parsing or SQL logic of its own.

use anyhow::{Context, Result};
use regex::Regex;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Instant;

use crate::demo::frames::FrameIter;
use crate::demo::header::{DemoHeader, HEADER_SIZE};
use crate::extract::{announcements, console, entities, inputs, net, pov, DemoContext, FrameExtractor};
use crate::output::sqlite::Db;

pub struct Options {
    pub patterns: Vec<Regex>,
    pub all_strings: bool,
    pub pov_sample: u32,
    pub threads: usize,
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

pub fn parse_one(path: &Path, db: &Db, opts: &Options) -> Result<()> {
    let started = Instant::now();
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
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

    // Register extractors here (see extract/mod.rs for the recipe).
    let mut extractors: Vec<Box<dyn FrameExtractor>> = vec![
        Box::new(announcements::Announcements::new(
            opts.patterns.clone(),
            opts.all_strings,
        )),
        Box::new(pov::PovSampler::new(opts.pov_sample)),
        Box::new(console::ConsoleCmds::default()),
        Box::new(inputs::RecorderInputs::default()),
        Box::new(net::NetPass::default()),
    ];

    let mut logs: Vec<(LogLevel, String)> = Vec::new();

    // A demo stopped mid-write can end partway through a frame; keep
    // everything extracted before the break and warn instead of failing.
    for frame in FrameIter::new(&data, HEADER_SIZE) {
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
    let entity_output = match entities::run(&data) {
        Ok(output) => Some(output),
        Err(e) => {
            logs.push((LogLevel::Warning, format!("entity pass failed: {e}")));
            None
        }
    };

    db.begin()?;
    let demo_id = db.insert_demo(&path.display().to_string(), &header)?;
    let mut summary = Vec::new();
    for extractor in &mut extractors {
        summary.extend(extractor.persist(db, demo_id, &ctx)?);
    }
    if let Some(output) = &entity_output {
        db.insert_player_samples(demo_id, &output.samples)?;
        summary.push(("player samples".into(), output.samples.len()));
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
    Ok(())
}
