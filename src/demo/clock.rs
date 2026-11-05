//! The server's clock for every demo frame.
//!
//! A frame header carries the recording client's tick. In a SourceTV
//! recording that is the server's tick less a constant. A player's client
//! keeps adjusting its clock to the server's, so in a POV recording the
//! header ticks drift from the server's by a tick or two, skipping some
//! ticks and repeating others. Each packet's net_Tick message carries the
//! server's own tick for its snapshot. [`Clock`] puts every frame on that
//! clock, shifted so most snapshots keep their header tick, and both the
//! frame walk and the entity pass read their ticks from it.
//!
//! A packet can carry reliable messages ahead of its snapshot, so the
//! net_Tick is looked for anywhere in it. A packet that carries only
//! reliable messages between snapshots, such as sounds, user messages or
//! game events, has none. It keeps the correction of the packet before it,
//! as do frames other than packets.

use std::collections::HashMap;

use crate::demo::frames::{Frame, FrameIter, FrameKind};
use crate::demo::net::{walk_packet, NetMessage};

pub struct Clock {
    /// The corrected tick of each signon and packet frame, in file order.
    packets: Vec<i32>,
}

impl Clock {
    /// Read every packet's server tick. Signon frames keep their header
    /// tick, because the signon stage carries the server's uptime tick
    /// rather than a recording-relative one.
    ///
    /// The shift is the most common gap between a snapshot's server tick
    /// and its header tick. The first snapshots after connecting arrive
    /// before the client has settled its clock, so the gap of any single
    /// packet could be off by several ticks.
    pub fn new(data: &[u8], start: usize, protocol: i32) -> Self {
        // Each signon or packet frame's header tick, and its server tick if
        // it is a snapshot.
        let mut frames: Vec<(FrameKind, i64, Option<i64>)> = Vec::new();
        for frame in FrameIter::new(data, start).map_while(Result::ok) {
            if matches!(frame.kind, FrameKind::Signon | FrameKind::Packet) {
                let server = (frame.kind == FrameKind::Packet)
                    .then(|| server_tick(frame.payload_in(data), protocol))
                    .flatten()
                    .map(i64::from);
                frames.push((frame.kind, i64::from(frame.tick), server));
            }
        }
        let mut gaps: HashMap<i64, usize> = HashMap::new();
        for &(_, header, server) in &frames {
            if let Some(server) = server {
                *gaps.entry(server - header).or_default() += 1;
            }
        }
        let base = gaps.into_iter().max_by_key(|&(gap, n)| (n, std::cmp::Reverse(gap))).map(|(gap, _)| gap);

        let mut correction: i64 = 0;
        let packets = frames
            .into_iter()
            .map(|(kind, header, server)| {
                if kind == FrameKind::Packet {
                    if let (Some(server), Some(base)) = (server, base) {
                        correction = server - base - header;
                    }
                    // A snapshot from before the client settled its clock
                    // can land before tick 0, which is the recording's start.
                    (header + correction).max(0)
                } else {
                    header
                }
            })
            .map(|tick| i32::try_from(tick).unwrap_or(i32::MAX))
            .collect();
        Clock { packets }
    }

    /// The corrected tick of the `ordinal`th signon or packet frame,
    /// counting from 0, or None past the last one read.
    pub fn packet_tick(&self, ordinal: usize) -> Option<i32> {
        self.packets.get(ordinal).copied()
    }

    /// The corrected tick of every signon and packet frame, in file order.
    pub fn packet_ticks(&self) -> &[i32] {
        &self.packets
    }

    /// The demo's frames on this clock. A frame that is not a signon or
    /// packet frame takes the correction of the packet before it.
    pub fn frames<'a>(&'a self, data: &'a [u8], start: usize) -> ClockedFrames<'a> {
        ClockedFrames { clock: self, inner: FrameIter::new(data, start), ordinal: 0, correction: 0 }
    }
}

/// The server tick of the snapshot in a packet, or None when it carries
/// none. A payload that fails to decode keeps any tick read before the
/// failure.
fn server_tick(payload: &[u8], protocol: i32) -> Option<u32> {
    let mut tick = None;
    let _ = walk_packet(payload, protocol, &mut |msg| {
        if let NetMessage::Tick(t) = msg {
            tick.get_or_insert(t);
        }
    });
    tick
}

pub struct ClockedFrames<'a> {
    clock: &'a Clock,
    inner: FrameIter<'a>,
    ordinal: usize,
    correction: i32,
}

impl Iterator for ClockedFrames<'_> {
    type Item = anyhow::Result<Frame>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut frame = match self.inner.next()? {
            Ok(frame) => frame,
            Err(e) => return Some(Err(e)),
        };
        match frame.kind {
            FrameKind::Signon | FrameKind::Packet => {
                let tick = self.clock.packet_tick(self.ordinal).unwrap_or(frame.tick);
                self.ordinal += 1;
                if frame.kind == FrameKind::Packet {
                    self.correction = tick - frame.tick;
                }
                frame.tick = tick;
            }
            _ => frame.tick += self.correction,
        }
        Some(Ok(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::bits::testutil::BitWriter;

    /// A packet frame (command 2) with `payload`.
    fn packet(tick: i32, payload: &[u8]) -> Vec<u8> {
        let mut out = vec![2];
        out.extend(tick.to_le_bytes());
        out.extend([0u8; 76 + 8]); // democmdinfo_t and sequence numbers
        out.extend((payload.len() as i32).to_le_bytes());
        out.extend(payload);
        out
    }

    /// A packet whose snapshot opens with net_Tick for `server`.
    fn snapshot(tick: i32, server: u32) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.write_bits(3, 6);
        w.write_bits(server, 32);
        w.write_bits(0, 16);
        w.write_bits(0, 16);
        packet(tick, &w.bytes)
    }

    /// A packet carrying only a reliable message, svc_GameEvent's type.
    fn reliable(tick: i32) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.write_bits(25, 6);
        w.write_bits(0, 11);
        packet(tick, &w.bytes)
    }

    /// A usercmd frame (command 5) with an empty payload.
    fn usercmd(tick: i32) -> Vec<u8> {
        let mut out = vec![5];
        out.extend(tick.to_le_bytes());
        out.extend(0i32.to_le_bytes()); // outgoing sequence
        out.extend(0i32.to_le_bytes()); // payload length
        out
    }

    fn ticks(frames: &[Vec<u8>]) -> Vec<(FrameKind, i32)> {
        let data = frames.concat();
        let clock = Clock::new(&data, 0, 24);
        clock.frames(&data, 0).map(|f| f.unwrap()).map(|f| (f.kind, f.tick)).collect()
    }

    #[test]
    fn a_constant_offset_keeps_the_header_ticks() {
        let frames = [snapshot(10, 12418), snapshot(11, 12419), snapshot(12, 12420)];
        assert_eq!(
            ticks(&frames),
            [(FrameKind::Packet, 10), (FrameKind::Packet, 11), (FrameKind::Packet, 12)]
        );
    }

    #[test]
    fn a_skewed_client_clock_follows_the_server() {
        // The client skips header tick 218 and repeats 219, while the
        // server's ticks run on without a gap.
        let frames = [snapshot(217, 1961), snapshot(219, 1962), snapshot(219, 1963)];
        assert_eq!(
            ticks(&frames),
            [(FrameKind::Packet, 217), (FrameKind::Packet, 218), (FrameKind::Packet, 219)]
        );
    }

    #[test]
    fn frames_without_a_server_tick_keep_the_last_correction() {
        // After the third snapshot the header runs one tick ahead of the
        // server, so the reliable packet and the usercmd move back one too.
        let frames =
            [snapshot(100, 1844), snapshot(101, 1845), snapshot(103, 1846), reliable(103), usercmd(104)];
        assert_eq!(
            ticks(&frames),
            [
                (FrameKind::Packet, 100),
                (FrameKind::Packet, 101),
                (FrameKind::Packet, 102),
                (FrameKind::Packet, 102),
                (FrameKind::UserCmd, 103),
            ]
        );
    }

    #[test]
    fn a_snapshot_behind_reliable_messages_still_gives_its_tick() {
        // svc_GameEvent with an empty body, then the snapshot's net_Tick.
        let mut w = BitWriter::default();
        w.write_bits(25, 6);
        w.write_bits(0, 11);
        w.write_bits(3, 6);
        w.write_bits(1846, 32);
        w.write_bits(0, 16);
        w.write_bits(0, 16);
        let frames = [snapshot(100, 1844), snapshot(101, 1845), packet(101, &w.bytes)];
        assert_eq!(
            ticks(&frames),
            [(FrameKind::Packet, 100), (FrameKind::Packet, 101), (FrameKind::Packet, 102)]
        );
    }

    #[test]
    fn no_tick_falls_before_the_recording_starts() {
        let frames = [snapshot(2, 1730), snapshot(30, 1774), snapshot(31, 1775)];
        assert_eq!(ticks(&frames)[0], (FrameKind::Packet, 0));
    }

    #[test]
    fn an_early_snapshot_off_the_usual_gap_does_not_shift_the_rest() {
        // The first snapshot arrives before the client settled its clock.
        let frames = [snapshot(20, 1756), snapshot(30, 1774), snapshot(31, 1775), snapshot(32, 1776)];
        assert_eq!(
            ticks(&frames),
            [(FrameKind::Packet, 12), (FrameKind::Packet, 30), (FrameKind::Packet, 31), (FrameKind::Packet, 32)]
        );
    }
}
