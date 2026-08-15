//! Turning raw demo frames into gameplay facts.
//!
//! Extractors implement [`FrameExtractor`]: the pipeline streams every frame
//! through `on_frame`, then calls `persist` once so the extractor writes its
//! tables and reports summary counts. Extractors own no I/O during
//! extraction and copy whatever they keep (frames hand out byte ranges into
//! the demo, available via [`DemoContext::data`]).
//!
//! To add an extractor: create a module here with a struct implementing
//! [`FrameExtractor`], give it a table + `insert_*` method in
//! `crate::output::sqlite`, and register it in `crate::pipeline::parse_one`.

pub mod announcements;
pub mod console;
pub mod inputs;
pub mod net;
pub mod pov;
pub mod rounds;
pub mod skim;

use anyhow::Result;

use crate::demo::frames::Frame;
use crate::demo::header::DemoHeader;
use crate::output::sqlite::Db;

/// Everything an extractor can see besides the frame itself.
pub struct DemoContext<'a> {
    /// The entire demo file; slice frame payload ranges into this.
    pub data: &'a [u8],
    pub header: &'a DemoHeader,
    /// Worker threads available for parallel work inside an extractor.
    pub threads: usize,
}

/// `(label, row count)` lines for the end-of-parse summary.
pub type Summary = Vec<(String, usize)>;

pub trait FrameExtractor {
    /// Called once per frame, in file order.
    fn on_frame(&mut self, ctx: &DemoContext, frame: &Frame);

    /// Called once after the frame walk: write tables, report counts.
    fn persist(&mut self, db: &Db, demo_id: i64, ctx: &DemoContext) -> Result<Summary>;
}
