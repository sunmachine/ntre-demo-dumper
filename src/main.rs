mod db;
mod frames;
mod header;
mod skim;

use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use std::path::PathBuf;
use std::thread;

use frames::{Frame, FrameIter, FrameKind};
use header::{DemoHeader, HEADER_SIZE};

/// Extract gameplay data from NEOTOKYO;REBUILD (NT;RE) demo files into SQLite.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Input .dem file(s)
    #[arg(required = true)]
    demos: Vec<PathBuf>,

    /// Output SQLite database
    #[arg(short, long, default_value = "ntre_demos.sqlite")]
    output: PathBuf,

    /// Additional regex pattern(s) to capture as announcements
    #[arg(short, long)]
    r#match: Vec<String>,

    /// Capture every recovered ASCII string, unfiltered (exploratory; noisy)
    #[arg(long)]
    all_strings: bool,

    /// Store every Nth POV sample (1 = every packet frame)
    #[arg(long, default_value_t = 1)]
    pov_sample: u32,

    /// Worker threads for payload skimming (default: all cores)
    #[arg(short = 'j', long)]
    threads: Option<usize>,
}

/// Announcement texts we extract by default. Each needs a capture-free full
/// match; the matched substring is what gets stored (bit-shifted extraction
/// leaves garbage bytes around the real message).
const DEFAULT_PATTERNS: &[&str] = &[
    r"- [A-Z]+ ROUND \d+ STARTED -",
    r"Team \w+ wins[ a-z]*!",
    r"[A-Za-z ]*captured the ghost[^!]*!",
    r"OVERTIME|SUDDEN DEATH|MATCH POINT",
];

struct Announcement {
    tick: i32,
    text: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut patterns: Vec<Regex> = DEFAULT_PATTERNS
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect();
    for p in &args.r#match {
        patterns.push(Regex::new(p).with_context(|| format!("bad --match pattern: {p}"))?);
    }

    let threads = args
        .threads
        .unwrap_or_else(|| thread::available_parallelism().map(|n| n.get()).unwrap_or(1));

    let db = db::Db::open(&args.output)?;

    for demo_path in &args.demos {
        parse_one(demo_path, &db, &patterns, args.all_strings, args.pov_sample, threads)?;
    }
    Ok(())
}

fn parse_one(
    path: &PathBuf,
    db: &db::Db,
    patterns: &[Regex],
    all_strings: bool,
    pov_sample: u32,
    threads: usize,
) -> Result<()> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let header = DemoHeader::parse(&data)?;
    println!(
        "{}: map {}, server {}, recorded by {}, {:.1}s / {} ticks ({:.1}/s)",
        path.display(),
        header.map_name,
        header.server_name,
        header.client_name,
        header.playback_seconds,
        header.playback_ticks,
        header.tickrate()
    );

    // Sequential frame walk: collect POV samples, console commands, and the
    // (tick, payload) list for the parallel skim. Payload slices borrow from
    // `data`, so this stays allocation-light.
    let mut packets: Vec<(i32, &[u8])> = Vec::new();
    let mut pov: Vec<(i32, frames::ViewInfo)> = Vec::new();
    let mut console_cmds: Vec<(i32, String)> = Vec::new();
    let mut pov_counter = 0u32;

    for frame in FrameIter::new(&data, HEADER_SIZE) {
        let frame: Frame = frame?;
        match frame.kind {
            FrameKind::Packet => {
                if let Some(view) = frame.view {
                    if pov_counter % pov_sample.max(1) == 0 {
                        pov.push((frame.tick, view));
                    }
                    pov_counter += 1;
                }
                packets.push((frame.tick, frame.payload));
            }
            FrameKind::ConsoleCmd => {
                let end = frame.payload.iter().position(|&b| b == 0).unwrap_or(frame.payload.len());
                let cmd = String::from_utf8_lossy(&frame.payload[..end]).trim().to_string();
                if !cmd.is_empty() {
                    console_cmds.push((frame.tick, cmd));
                }
            }
            _ => {}
        }
    }

    // Parallel skim over packet payloads.
    let chunk_size = packets.len().div_ceil(threads.max(1));
    let mut announcements: Vec<Announcement> = thread::scope(|s| {
        let handles: Vec<_> = packets
            .chunks(chunk_size.max(1))
            .map(|chunk| {
                s.spawn(move || {
                    let mut found = Vec::new();
                    let mut strings = Vec::new();
                    for &(tick, payload) in chunk {
                        strings.clear();
                        skim::skim(payload, &mut strings);
                        for raw in &strings {
                            if all_strings {
                                found.push(Announcement { tick, text: raw.clone() });
                            } else {
                                for re in patterns {
                                    if let Some(m) = re.find(raw) {
                                        found.push(Announcement {
                                            tick,
                                            text: m.as_str().to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                    found
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    announcements.sort_by_key(|a| a.tick);

    // The engine may resend a reliable message; drop repeats of the same text
    // within a short window.
    let tickrate = header.tickrate();
    let dedup_window = (tickrate * 3.0) as i32;
    let mut deduped: Vec<Announcement> = Vec::new();
    for a in announcements {
        if deduped
            .last()
            .is_some_and(|p| p.text == a.text && a.tick - p.tick < dedup_window)
        {
            continue;
        }
        deduped.push(a);
    }

    // Store everything in one transaction.
    db.conn.execute_batch("BEGIN")?;
    let demo_id = db.insert_demo(&path.display().to_string(), &header)?;
    {
        let mut ins = db.conn.prepare(
            "INSERT INTO announcements (demo_id, tick, seconds, text) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for a in &deduped {
            ins.execute(rusqlite::params![demo_id, a.tick, a.tick as f64 / tickrate, a.text])?;
        }
        let mut ins = db.conn.prepare(
            "INSERT INTO pov_samples (demo_id, tick, x, y, z, pitch, yaw, roll)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for (tick, v) in &pov {
            ins.execute(rusqlite::params![
                demo_id, tick, v.origin[0], v.origin[1], v.origin[2],
                v.angles[0], v.angles[1], v.angles[2],
            ])?;
        }
        let mut ins = db
            .conn
            .prepare("INSERT INTO console_cmds (demo_id, tick, cmd) VALUES (?1, ?2, ?3)")?;
        for (tick, cmd) in &console_cmds {
            ins.execute(rusqlite::params![demo_id, tick, cmd])?;
        }
    }
    let rounds = derive_rounds(&deduped);
    {
        let mut ins = db.conn.prepare(
            "INSERT INTO rounds (demo_id, round_number, start_tick, end_tick, winner, win_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for r in &rounds {
            ins.execute(rusqlite::params![
                demo_id, r.number, r.start_tick, r.end_tick, r.winner, r.reason
            ])?;
        }
    }
    db.conn.execute_batch("COMMIT")?;

    println!(
        "  -> demo #{demo_id}: {} announcements, {} rounds, {} POV samples, {} console cmds",
        deduped.len(),
        rounds.len(),
        pov.len(),
        console_cmds.len()
    );
    Ok(())
}

struct Round {
    number: Option<i32>,
    start_tick: Option<i32>,
    end_tick: Option<i32>,
    winner: Option<String>,
    reason: Option<String>,
}

/// Pair up "ROUND N STARTED" and "Team X wins ..." announcements. A demo that
/// starts mid-round yields a first round with no start marker.
fn derive_rounds(announcements: &[Announcement]) -> Vec<Round> {
    let start_re = Regex::new(r"ROUND (\d+) STARTED").unwrap();
    let win_re = Regex::new(r"Team (\w+) wins( [a-z ]*)?!").unwrap();
    let mut rounds: Vec<Round> = Vec::new();
    let mut open: Option<Round> = None;

    for a in announcements {
        if let Some(c) = start_re.captures(&a.text) {
            if let Some(r) = open.take() {
                rounds.push(r); // previous round never saw a win message
            }
            open = Some(Round {
                number: c[1].parse().ok(),
                start_tick: Some(a.tick),
                end_tick: None,
                winner: None,
                reason: None,
            });
        } else if let Some(c) = win_re.captures(&a.text) {
            let mut r = open.take().unwrap_or(Round {
                number: None,
                start_tick: None,
                end_tick: None,
                winner: None,
                reason: None,
            });
            r.end_tick = Some(a.tick);
            r.winner = Some(c[1].to_string());
            r.reason = c.get(2).map(|m| m.as_str().trim().to_string());
            rounds.push(r);
        }
    }
    if let Some(r) = open {
        rounds.push(r);
    }

    // A round that ended before the first start marker is the one prior to it.
    let numbers: Vec<Option<i32>> = rounds.iter().map(|r| r.number).collect();
    for (i, r) in rounds.iter_mut().enumerate() {
        if r.number.is_none() {
            r.number = numbers
                .get(i + 1)
                .copied()
                .flatten()
                .map(|next| next - 1)
                .or(Some(i as i32 + 1));
        }
    }
    rounds
}
