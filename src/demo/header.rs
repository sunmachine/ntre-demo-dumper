use anyhow::{bail, Result};

pub const HEADER_SIZE: usize = 1072;
const MAGIC: &[u8; 8] = b"HL2DEMO\0";

/// Fixed-size header at the start of every Source engine demo.
#[derive(Debug)]
pub struct DemoHeader {
    pub demo_protocol: i32,
    pub network_protocol: i32,
    pub server_name: String,
    pub client_name: String,
    pub map_name: String,
    pub game_directory: String,
    pub playback_seconds: f32,
    pub playback_ticks: i32,
    pub playback_frames: i32,
    #[allow(dead_code)] // part of the on-disk format; not stored yet
    pub signon_length: i32,
}

fn cstr(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn i32_at(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

impl DemoHeader {
    pub fn parse(data: &[u8]) -> Result<Self> {
        if data.len() < HEADER_SIZE {
            bail!("file too small to be a demo ({} bytes)", data.len());
        }
        if &data[..8] != MAGIC {
            bail!("not a HL2DEMO file (bad magic)");
        }
        let header = Self {
            demo_protocol: i32_at(data, 8),
            network_protocol: i32_at(data, 12),
            server_name: cstr(&data[16..276]),
            client_name: cstr(&data[276..536]),
            map_name: cstr(&data[536..796]),
            game_directory: cstr(&data[796..1056]),
            playback_seconds: f32::from_le_bytes(data[1056..1060].try_into().unwrap()),
            playback_ticks: i32_at(data, 1060),
            playback_frames: i32_at(data, 1064),
            signon_length: i32_at(data, 1068),
        };
        if header.demo_protocol != 3 {
            bail!(
                "unsupported demo protocol {} (expected 3 / Source SDK 2013)",
                header.demo_protocol
            );
        }
        Ok(header)
    }

    pub fn tickrate(&self) -> f64 {
        if self.playback_seconds > 0.0 {
            self.playback_ticks as f64 / self.playback_seconds as f64
        } else {
            66.6667 // SDK 2013 multiplayer default
        }
    }
}
