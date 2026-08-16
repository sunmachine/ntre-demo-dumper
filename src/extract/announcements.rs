//! Center-text announcement recovery, plus the rounds derived from it.
//!
//! Collects every packet payload during the frame walk, then (at persist
//! time) runs the ASCII skim over them in parallel, keeps substrings that
//! match known announcement patterns, de-duplicates reliable-message
//! resends, and derives per-round records via `super::rounds`.

use anyhow::{Context, Result};
use regex::Regex;
use std::ops::Range;
use std::thread;

use super::{rounds, skim, DemoContext, FrameExtractor, Summary};
use crate::demo::frames::{Frame, FrameKind};
use crate::output::sqlite::Db;

/// A line of game text, anchored to the tick of the packet that carried it.
pub struct Announcement {
    pub tick: i32,
    pub text: String,
}

/// Announcement texts extracted by default. Each pattern's *match* is what
/// gets stored; the bit-shifted extraction leaves garbage bytes around the
/// real message, so patterns must cover the full text they want kept.
pub const DEFAULT_PATTERNS: &[&str] = &[
    r"- [A-Z]+ ROUND \d+ STARTED -",
    r"Team \w+ wins[ a-z]*!",
    r"[A-Za-z ]*captured the ghost[^!]*!",
    r"OVERTIME|SUDDEN DEATH|MATCH POINT",
];

/// Compile the default patterns plus any user-supplied extras.
pub fn compile_patterns(extra: &[String]) -> Result<Vec<Regex>> {
    let mut patterns: Vec<Regex> = DEFAULT_PATTERNS
        .iter()
        .map(|p| Regex::new(p).unwrap())
        .collect();
    for p in extra {
        patterns.push(Regex::new(p).with_context(|| format!("bad --match pattern: {p}"))?);
    }
    Ok(patterns)
}

pub struct Announcements {
    patterns: Vec<Regex>,
    all_strings: bool,
    packets: Vec<(i32, Range<usize>)>,
}

impl Announcements {
    pub fn new(patterns: Vec<Regex>, all_strings: bool) -> Self {
        Self { patterns, all_strings, packets: Vec::new() }
    }

    /// Skim the collected packets across `ctx.threads` workers; results are
    /// sorted by tick and de-duplicated.
    fn skim_all(&self, ctx: &DemoContext) -> Vec<Announcement> {
        let chunk_size = self.packets.len().div_ceil(ctx.threads.max(1));
        let mut found: Vec<Announcement> = thread::scope(|s| {
            let handles: Vec<_> = self
                .packets
                .chunks(chunk_size.max(1))
                .map(|chunk| {
                    s.spawn(move || {
                        let mut out = Vec::new();
                        let mut strings = Vec::new();
                        for (tick, range) in chunk {
                            strings.clear();
                            skim::skim(&ctx.data[range.clone()], &mut strings);
                            for raw in &strings {
                                if self.all_strings {
                                    out.push(Announcement { tick: *tick, text: raw.clone() });
                                } else {
                                    for re in &self.patterns {
                                        if let Some(m) = re.find(raw) {
                                            out.push(Announcement {
                                                tick: *tick,
                                                text: m.as_str().to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                        out
                    })
                })
                .collect();
            handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
        });
        found.sort_by_key(|a| a.tick);
        dedup(found, ctx.header.tickrate())
    }
}

impl FrameExtractor for Announcements {
    fn on_frame(&mut self, _ctx: &DemoContext, frame: &Frame) {
        if frame.kind == FrameKind::Packet {
            self.packets.push((frame.tick, frame.payload.clone()));
        }
    }

    fn persist(&mut self, db: &Db, demo_id: i64, ctx: &DemoContext) -> Result<Summary> {
        let announcements = self.skim_all(ctx);
        let rounds = rounds::derive(&announcements);
        db.insert_announcements(demo_id, &announcements, ctx.header.tickrate())?;
        db.insert_rounds(demo_id, &rounds)?;
        Ok(vec![
            ("announcements".into(), announcements.len()),
            ("rounds".into(), rounds.len()),
        ])
    }
}

/// The engine may resend a reliable message; drop repeats of the same text
/// arriving within a few seconds of the original.
fn dedup(announcements: Vec<Announcement>, tickrate: f64) -> Vec<Announcement> {
    let window = (tickrate * 3.0) as i32;
    let mut deduped: Vec<Announcement> = Vec::new();
    for a in announcements {
        if deduped
            .last()
            .is_some_and(|p| p.text == a.text && a.tick - p.tick < window)
        {
            continue;
        }
        deduped.push(a);
    }
    deduped
}
