//! Per-demo orchestration: read the file, stream its frames through the
//! registered extractors, and persist everything in one transaction. This is
//! the only module that sees all three layers (`demo`, `extract`, `output`);
//! it holds no parsing or SQL logic of its own.

use anyhow::{Context, Result};
use regex::Regex;
use std::path::Path;
use std::time::Instant;

use crate::demo::frames::FrameIter;
use crate::demo::header::{DemoHeader, HEADER_SIZE};
use crate::extract::{announcements, console, inputs, pov, DemoContext, FrameExtractor};
use crate::output::sqlite::Db;

pub struct Options {
    pub patterns: Vec<Regex>,
    pub all_strings: bool,
    pub pov_sample: u32,
    pub threads: usize,
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
    ];

    for frame in FrameIter::new(&data, HEADER_SIZE) {
        let frame = frame?;
        for extractor in &mut extractors {
            extractor.on_frame(&ctx, &frame);
        }
    }

    db.begin()?;
    let demo_id = db.insert_demo(&path.display().to_string(), &header)?;
    let mut summary = Vec::new();
    for extractor in &mut extractors {
        summary.extend(extractor.persist(db, demo_id, &ctx)?);
    }
    db.commit()?;

    println!();
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
