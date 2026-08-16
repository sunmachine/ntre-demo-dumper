//! Recorder input stream: buttons (fire, aim, lean, thermoptic), movement,
//! and weapon switches, decoded from `dem_usercmd` frames. This is the
//! "did the recorder shoot this tick" signal.

use anyhow::Result;

use super::{DemoContext, FrameExtractor, Summary};
use crate::demo::frames::{Frame, FrameKind};
use crate::demo::usercmd::{read_usercmd, UserCmd};
use crate::output::sqlite::Db;

#[derive(Default)]
pub struct RecorderInputs {
    pub cmds: Vec<(i32, UserCmd)>,
    decode_errors: usize,
}

impl FrameExtractor for RecorderInputs {
    fn on_frame(&mut self, ctx: &DemoContext, frame: &Frame) {
        if frame.kind != FrameKind::UserCmd {
            return;
        }
        match read_usercmd(frame.payload_in(ctx.data)) {
            Ok(cmd) => self.cmds.push((frame.tick, cmd)),
            Err(_) => self.decode_errors += 1,
        }
    }

    fn persist(&mut self, db: &Db, demo_id: i64, _ctx: &DemoContext) -> Result<Summary> {
        db.insert_recorder_inputs(demo_id, &self.cmds)?;
        let mut summary: Summary = vec![("recorder inputs".into(), self.cmds.len())];
        if self.decode_errors > 0 {
            summary.push(("input decode errors".into(), self.decode_errors));
        }
        Ok(summary)
    }
}
