//! Recorder point-of-view sampling: position + view angles per packet frame,
//! taken from the byte-aligned democmdinfo the demo layer already decoded.

use anyhow::Result;

use super::{DemoContext, FrameExtractor, Summary};
use crate::demo::frames::{Frame, FrameKind, ViewInfo};
use crate::output::sqlite::Db;

pub struct PovSampler {
    every: u32,
    counter: u32,
    samples: Vec<(i32, ViewInfo)>,
}

impl PovSampler {
    /// Keep every `every`-th packet frame's view (1 = all).
    pub fn new(every: u32) -> Self {
        Self { every: every.max(1), counter: 0, samples: Vec::new() }
    }
}

impl FrameExtractor for PovSampler {
    fn on_frame(&mut self, _ctx: &DemoContext, frame: &Frame) {
        if frame.kind != FrameKind::Packet {
            return;
        }
        if let Some(view) = frame.view {
            if self.counter.is_multiple_of(self.every) {
                self.samples.push((frame.tick, view));
            }
            self.counter += 1;
        }
    }

    fn persist(&mut self, db: &Db, demo_id: i64, _ctx: &DemoContext) -> Result<Summary> {
        db.insert_pov_samples(demo_id, &self.samples)?;
        Ok(vec![("POV samples".into(), self.samples.len())])
    }
}
