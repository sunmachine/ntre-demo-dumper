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
    };
    let db = output::sqlite::Db::open(&args.output)?;
    for demo_path in &args.demos {
        pipeline::parse_one(demo_path, &db, &opts)?;
    }
    Ok(())
}
