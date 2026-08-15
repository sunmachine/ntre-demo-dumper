use anyhow::{bail, Result};
use std::ops::Range;

/// Demo protocol 3 (Source SDK 2013) frame commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    Signon,
    Packet,
    SyncTick,
    ConsoleCmd,
    UserCmd,
    DataTables,
    Stop,
    StringTables,
}

/// The recorder's point of view, from the 76-byte democmdinfo_t that
/// prefixes every signon/packet frame. Byte-aligned and free to read.
#[derive(Debug, Clone, Copy)]
pub struct ViewInfo {
    pub origin: [f32; 3],
    pub angles: [f32; 3], // pitch, yaw, roll
}

/// One demo frame. `payload` is a byte range into the demo file, so frames
/// are lifetime-free and cheap to store; slice it with [`Frame::payload_in`].
#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: FrameKind,
    pub tick: i32,
    pub view: Option<ViewInfo>,
    pub payload: Range<usize>,
}

impl Frame {
    pub fn payload_in<'a>(&self, data: &'a [u8]) -> &'a [u8] {
        &data[self.payload.clone()]
    }
}

const CMDINFO_SIZE: usize = 76; // flags + 6 vectors (view origin/angles/local angles, x2)

fn f32_at(buf: &[u8], off: usize) -> f32 {
    f32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// Iterator over demo frames. Reads each frame header and length prefix,
/// never decoding payload contents.
pub struct FrameIter<'a> {
    data: &'a [u8],
    pos: usize,
    done: bool,
}

impl<'a> FrameIter<'a> {
    /// `data` is the whole file; iteration starts after the fixed header.
    pub fn new(data: &'a [u8], start: usize) -> Self {
        Self { data, pos: start, done: false }
    }

    fn take(&mut self, n: usize) -> Result<Range<usize>> {
        if self.pos + n > self.data.len() {
            bail!("truncated demo: wanted {} bytes at offset {}", n, self.pos);
        }
        let r = self.pos..self.pos + n;
        self.pos += n;
        Ok(r)
    }

    fn take_i32(&mut self) -> Result<i32> {
        let r = self.take(4)?;
        Ok(i32::from_le_bytes(self.data[r].try_into().unwrap()))
    }

    fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.done {
            return Ok(None);
        }
        // A demo stopped mid-write can end a few bytes short; treat a
        // missing final frame header as a clean end of stream.
        if self.pos + 5 > self.data.len() {
            self.done = true;
            return Ok(None);
        }
        let cmd = self.data[self.take(1)?.start];
        let tick = self.take_i32()?;
        let frame = match cmd {
            1 | 2 => {
                let info = &self.data[self.take(CMDINFO_SIZE)?];
                let view = ViewInfo {
                    origin: [f32_at(info, 4), f32_at(info, 8), f32_at(info, 12)],
                    angles: [f32_at(info, 16), f32_at(info, 20), f32_at(info, 24)],
                };
                self.take(8)?; // in/out sequence numbers
                let len = self.take_i32()?;
                let payload = self.take(len.max(0) as usize)?;
                Frame {
                    kind: if cmd == 1 { FrameKind::Signon } else { FrameKind::Packet },
                    tick,
                    view: Some(view),
                    payload,
                }
            }
            3 => Frame { kind: FrameKind::SyncTick, tick, view: None, payload: 0..0 },
            4 | 5 | 6 | 8 => {
                if cmd == 5 {
                    self.take(4)?; // outgoing usercmd sequence number
                }
                let len = self.take_i32()?;
                let payload = self.take(len.max(0) as usize)?;
                let kind = match cmd {
                    4 => FrameKind::ConsoleCmd,
                    5 => FrameKind::UserCmd,
                    6 => FrameKind::DataTables,
                    _ => FrameKind::StringTables,
                };
                Frame { kind, tick, view: None, payload }
            }
            7 => {
                self.done = true;
                Frame { kind: FrameKind::Stop, tick, view: None, payload: 0..0 }
            }
            other => bail!("unknown frame command {} at offset {}", other, self.pos - 5),
        };
        Ok(Some(frame))
    }
}

impl<'a> Iterator for FrameIter<'a> {
    type Item = Result<Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_frame() {
            Ok(Some(f)) => Some(Ok(f)),
            Ok(None) => None,
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
