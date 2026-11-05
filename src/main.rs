//                    ████
//       ████████    ██████████     ▓▓▓▓
//     ████████████  ███████████   ▓▓▓▓▓▓
//    █████░░░█████░ █████░░░░░░░  ▓▓▓▓▓▓░
//    █████░  █████░ █████░         ▓▓▓▓░░
//    █████░  █████░ █████░          ░░░░
//    █████░  █████░ ██████████      █████
//    █████░  █████░ ██████████░     █████░
//     ░░░░░  █████░  ░░░░░░░░░░    ████░░░
//            █████░               ███░░░
//             ░░░░░                ░░░
//
//      N E O T O K Y O ; R E B U I L D

//! CLI entry point: argument parsing and wiring only. The work happens in
//! `pipeline` (per-demo orchestration), backed by `demo` (file format),
//! `extract` (gameplay facts), and `output` (SQLite). See ARCHITECTURE.md.

mod demo;
mod extract;
mod output;
mod pipeline;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::thread;

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

    /// Keep rounds 1 to N only; drop every row from round N+1's start on
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(i32).range(1..))]
    last_round: Option<i32>,

    /// Leave these tables empty, comma-separated; repeatable. Every table but demos can be omitted
    #[arg(long, value_name = "TABLES", value_delimiter = ',')]
    omit: Vec<String>,

    /// Leave every table named *_samples empty: player_samples, pov_samples and ghost_samples
    #[arg(long)]
    omit_samples: bool,
}

const BANNER: &str = r#"
                   ████
      ████████    ██████████     ▓▓▓▓
    ████████████  ███████████   ▓▓▓▓▓▓
   █████░░░█████░ █████░░░░░░░  ▓▓▓▓▓▓░
   █████░  █████░ █████░         ▓▓▓▓░░
   █████░  █████░ █████░          ░░░░
   █████░  █████░ ██████████      █████
   █████░  █████░ ██████████░     █████░
    ░░░░░  █████░  ░░░░░░░░░░    ████░░░
           █████░               ███░░░
            ░░░░░                ░░░

     N E O T O K Y O ; R E B U I L D
       :: D E M O ; D U M P E R ::
"#;

fn main() -> Result<()> {
    let args = Args::parse();
    println!("{BANNER}");
    let opts = pipeline::Options {
        patterns: extract::announcements::compile_patterns(&args.r#match)?,
        all_strings: args.all_strings,
        pov_sample: args.pov_sample,
        threads: args
            .threads
            .unwrap_or_else(|| thread::available_parallelism().map(|n| n.get()).unwrap_or(1)),
        last_round: args.last_round,
    };
    let mut db = output::sqlite::Db::open(&args.output)?;
    let mut omit = args.omit;
    if args.omit_samples {
        omit.extend(db.table_names()?.into_iter().filter(|t| t.ends_with("_samples")));
    }
    db.omit(omit)?;
    if args.pov_sample != 1 && db.omitted().any(|t| t == "pov_samples") {
        pipeline::print_logs(&[(
            pipeline::LogLevel::Warning,
            "--pov-sample has no effect while pov_samples is omitted".into(),
        )]);
    }
    let mut failures = 0usize;
    let mut skipped = 0usize;
    for demo_path in &args.demos {
        match pipeline::parse_one(demo_path, &db, &opts) {
            Ok(pipeline::Outcome::Parsed) => {}
            Ok(pipeline::Outcome::Skipped) => skipped += 1,
            Err(e) => {
                let _ = db.rollback(); // parse may have died inside its transaction
                pipeline::print_logs(&[(
                    pipeline::LogLevel::Error,
                    format!("{}: {e:#}", demo_path.display()),
                )]);
                failures += 1;
            }
        }
    }
    if skipped > 0 {
        println!("  {skipped} demo(s) skipped, already in the database");
    }
    if failures > 0 {
        anyhow::bail!("{failures} demo(s) failed to parse");
    }
    Ok(())
}
