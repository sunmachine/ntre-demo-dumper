//! Console commands the recorder issued during the demo
//! (`dem_consolecmd` frames; plain null-terminated strings).

use anyhow::Result;

use super::{DemoContext, FrameExtractor, Summary};
use crate::demo::frames::{Frame, FrameKind};
use crate::output::sqlite::Db;

#[derive(Default)]
pub struct ConsoleCmds {
    cmds: Vec<(i32, String)>,
}

impl FrameExtractor for ConsoleCmds {
    fn on_frame(&mut self, ctx: &DemoContext, frame: &Frame) {
        if frame.kind != FrameKind::ConsoleCmd {
            return;
        }
        let payload = frame.payload_in(ctx.data);
        let end = payload.iter().position(|&b| b == 0).unwrap_or(payload.len());
        let cmd = String::from_utf8_lossy(&payload[..end]).trim().to_string();
        if !cmd.is_empty() {
            self.cmds.push((frame.tick, cmd));
        }
    }

    fn persist(&mut self, db: &Db, demo_id: i64, _ctx: &DemoContext) -> Result<Summary> {
        db.insert_console_cmds(demo_id, &self.cmds)?;
        Ok(vec![("console cmds".into(), self.cmds.len())])
    }
}
