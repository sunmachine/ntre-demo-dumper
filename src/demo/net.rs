//! Net-message framing for packet payloads (network protocol 24, the
//! SDK2013-MP / TF2 engine branch).
//!
//! Walks the bit-packed message stream inside a signon/packet frame: every
//! message type's wire format is known well enough to either skip it or hand
//! it to the caller. Game events are returned as raw bit chunks and parsed
//! generically against the demo's own event definitions
//! ([`parse_game_event`]), never against a hardcoded schema; NT;RE's
//! definitions differ from other games on this engine branch.
//!
//! Wire formats ported from demostf/parser (MIT), which documents this
//! engine branch precisely.

use anyhow::{bail, Result};
use std::collections::HashMap;

use super::bits::{BitChunk, BitReader};

const NETMSG_TYPE_BITS: u32 = 6;

/// One game-event field definition: name + wire type.
#[derive(Debug, Clone)]
pub struct EventEntry {
    pub name: String,
    pub kind: u8, // GameEventValueType: 1 string, 2 float, 3 long, 4 short, 5 byte, 6 bool, 7 local
}

#[derive(Debug, Clone)]
pub struct EventDef {
    pub name: String,
    pub entries: Vec<EventEntry>,
}

pub type EventDefs = HashMap<u16, EventDef>;

/// A parsed game-event field value.
#[derive(Debug, Clone)]
pub enum EventValue {
    Str(String),
    Float(f32),
    Int(i32),
    Bool(bool),
}

/// svc_ServerInfo, sent once at signon. Fields this crate has no use for
/// (protocol, server count, class CRC, sky name, replay flag) are read past.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub is_hltv: bool,
    pub is_dedicated: bool,
    pub max_classes: u16,
    /// MD5 of the map file on protocols above 17, a CRC32 in the first four
    /// bytes below that.
    pub map_hash: [u8; 16],
    /// Client slot of the recording client; its entity id is slot + 1.
    pub player_slot: u8,
    pub max_clients: u8,
    pub tick_interval: f32,
    /// `l` Linux, `w` Windows, as sent.
    pub os: char,
    pub host_name: String,
}

/// Messages surfaced to the caller; everything else is validated and skipped.
pub enum NetMessage {
    ServerInfo(ServerInfo),
    /// net_SetConVar: replicated convar name/value pairs, in wire order.
    SetConVar(Vec<(String, String)>),
    GameEventList(EventDefs),
    /// Raw event payload (starts with the 9-bit event type id).
    GameEvent(BitChunk),
    UserMessage { kind: u8, data: BitChunk },
}

/// Walk every net message in a packet payload, invoking `sink` for the
/// message types we surface. `protocol` is the header's network protocol.
pub fn walk_packet(
    payload: &[u8],
    protocol: i32,
    sink: &mut dyn FnMut(NetMessage),
) -> Result<()> {
    let mut r = BitReader::new(payload);
    while r.bits_left() >= NETMSG_TYPE_BITS as usize {
        let msg_type = r.read_bits(NETMSG_TYPE_BITS)? as u8;
        match msg_type {
            0 => {} // net_NOP (also zero-padding at the end of a payload)
            1 => {
                r.read_string()?; // net_Disconnect: reason
            }
            2 => {
                // net_File: transfer id, name, requested flag
                r.read_bits(32)?;
                r.read_string()?;
                r.read_bit()?;
            }
            3 => {
                // net_Tick: tick + host frametime/stddev
                r.skip_bits(32 + 16 + 16)?;
            }
            4 => {
                r.read_string()?; // net_StringCmd
            }
            5 => {
                // net_SetConVar: count * (name, value)
                let count = r.read_bits(8)?;
                let mut pairs = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    pairs.push((r.read_string()?, r.read_string()?));
                }
                sink(NetMessage::SetConVar(pairs));
            }
            6 => {
                r.skip_bits(8 + 32)?; // net_SignonState
            }
            7 => {
                r.read_string()?; // svc_Print
            }
            8 => sink(NetMessage::ServerInfo(read_server_info(&mut r, protocol)?)),
            10 => {
                // svc_ClassInfo
                let count = r.read_bits(16)?;
                let create_on_client = r.read_bit()?;
                if !create_on_client {
                    let bits = log_base2(count as u16) + 1;
                    for _ in 0..count {
                        r.read_bits(bits)?;
                        r.read_string()?;
                        r.read_string()?;
                    }
                }
            }
            11 => {
                r.read_bit()?; // svc_SetPause
            }
            12 => {
                // svc_CreateStringTable: header + skippable data
                r.read_string()?; // table name
                let max_entries = r.read_bits(16)? as u16;
                r.read_bits(log_base2(max_entries) + 1)?; // entry count
                let length = if protocol > 23 { r.read_var_int()? } else { r.read_bits(20)? };
                if r.read_bit()? {
                    r.skip_bits(12 + 4)?; // fixed userdata size + bits
                }
                r.read_bit()?; // compressed
                r.skip_bits(length as usize)?;
            }
            13 => {
                // svc_UpdateStringTable
                r.read_bits(5)?; // table id
                if r.read_bit()? {
                    r.read_bits(16)?; // changed entry count
                }
                let length = r.read_bits(20)?;
                r.skip_bits(length as usize)?;
            }
            14 => {
                // svc_VoiceInit
                let codec = r.read_string()?;
                let quality = r.read_bits(8)?;
                let _ = codec;
                if quality == 255 {
                    r.read_bits(16)?; // sampling rate
                }
            }
            15 => {
                // svc_VoiceData
                r.skip_bits(8 + 8)?;
                let length = r.read_bits(16)?;
                r.skip_bits(length as usize)?;
            }
            17 => {
                // svc_Sounds
                let reliable = r.read_bit()?;
                let length = if reliable {
                    r.read_bits(8)?
                } else {
                    r.read_bits(8)?; // sound count
                    r.read_bits(16)?
                };
                r.skip_bits(length as usize)?;
            }
            18 => {
                r.skip_bits(11)?; // svc_SetView
            }
            19 => {
                r.skip_bits(1 + 48)?; // svc_FixAngle
            }
            21 => {
                // svc_BSPDecal
                let has_x = r.read_bit()?;
                let has_y = r.read_bit()?;
                let has_z = r.read_bit()?;
                if has_x {
                    r.read_bit_coord()?;
                }
                if has_y {
                    r.read_bit_coord()?;
                }
                if has_z {
                    r.read_bit_coord()?;
                }
                r.read_bits(9)?; // decal texture index
                if r.read_bit()? {
                    r.skip_bits(11 + 13)?; // entity + model index
                }
                r.read_bit()?; // low priority
            }
            23 => {
                // svc_UserMessage
                let kind = r.read_bits(8)? as u8;
                let length = r.read_bits(11)? as usize;
                let data = r.read_chunk(length)?;
                sink(NetMessage::UserMessage { kind, data });
            }
            24 => {
                // svc_EntityMessage
                r.skip_bits(11 + 9)?;
                let length = r.read_bits(11)?;
                r.skip_bits(length as usize)?;
            }
            25 => {
                // svc_GameEvent
                let length = r.read_bits(11)? as usize;
                let data = r.read_chunk(length)?;
                sink(NetMessage::GameEvent(data));
            }
            26 => {
                // svc_PacketEntities
                r.read_bits(11)?; // max entries
                if r.read_bit()? {
                    r.read_bits(32)?; // delta-from tick
                }
                r.read_bit()?; // baseline
                r.read_bits(11)?; // updated entries
                let length = r.read_bits(20)?;
                r.read_bit()?; // update baseline
                r.skip_bits(length as usize)?;
            }
            27 => {
                // svc_TempEntities
                r.read_bits(8)?; // entry count
                let length = if protocol > 23 { r.read_var_int()? } else { r.read_bits(17)? };
                r.skip_bits(length as usize)?;
            }
            28 => {
                // svc_Prefetch
                r.read_bits(if protocol > 22 { 14 } else { 13 })?;
            }
            29 => {
                // svc_Menu
                r.read_bits(16)?;
                let length = r.read_bits(16)?;
                r.skip_bits(length as usize * 8)?;
            }
            30 => {
                // svc_GameEventList
                let count = r.read_bits(9)?;
                let length = r.read_bits(20)? as usize;
                let chunk = r.read_chunk(length)?;
                sink(NetMessage::GameEventList(parse_event_list(&chunk, count)?));
            }
            31 => {
                // svc_GetCvarValue
                r.read_bits(32)?;
                r.read_string()?;
            }
            32 => {
                // svc_CmdKeyValues
                let length = r.read_bits(32)?;
                r.skip_bits(length as usize * 8)?;
            }
            other => bail!("unknown net message type {other}"),
        }
    }
    Ok(())
}

fn read_server_info(r: &mut BitReader, protocol: i32) -> Result<ServerInfo> {
    r.skip_bits(16 + 32)?; // protocol, server count
    let is_hltv = r.read_bit()?;
    let is_dedicated = r.read_bit()?;
    r.skip_bits(32)?; // client.dll CRC
    let max_classes = r.read_bits(16)? as u16;
    let mut map_hash = [0u8; 16];
    let hash_len = if protocol > 17 { 16 } else { 4 };
    for byte in map_hash.iter_mut().take(hash_len) {
        *byte = r.read_u8()?;
    }
    let player_slot = r.read_u8()?;
    let max_clients = r.read_u8()?;
    let tick_interval = r.read_f32()?;
    let os = r.read_u8()? as char;
    r.read_string()?; // game dir, also in the header
    r.read_string()?; // map, also in the header
    r.read_string()?; // sky name
    let host_name = r.read_string()?;
    if protocol > 15 {
        r.read_bit()?; // replay flag
    }
    Ok(ServerInfo {
        is_hltv,
        is_dedicated,
        max_classes,
        map_hash,
        player_slot,
        max_clients,
        tick_interval,
        os,
        host_name,
    })
}

fn log_base2(mut n: u16) -> u32 {
    let mut result = 0;
    while n > 1 {
        n >>= 1;
        result += 1;
    }
    result
}

fn parse_event_list(chunk: &BitChunk, count: u32) -> Result<EventDefs> {
    let mut r = chunk.reader();
    let mut defs = EventDefs::with_capacity(count as usize);
    for _ in 0..count {
        let id = r.read_bits(9)? as u16;
        let name = r.read_string()?;
        let mut entries = Vec::new();
        loop {
            let kind = r.read_bits(3)? as u8;
            if kind == 0 {
                break;
            }
            entries.push(EventEntry { name: r.read_string()?, kind });
        }
        defs.insert(id, EventDef { name, entries });
    }
    Ok(defs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::bits::testutil::BitWriter;

    /// Build a payload: GameEventList defining a NEO-style player_death,
    /// then a GameEvent using it, then a SayText2 user message, then padding.
    #[test]
    fn walks_event_list_event_and_user_message() {
        let mut w = BitWriter::default();

        // svc_GameEventList (30): count 9 bits, length 20 bits, defs
        let mut defs = BitWriter::default();
        defs.write_bits(42, 9); // event id
        defs.write_string("player_death");
        for (name, kind) in [
            ("userid", 4u32),   // short
            ("attacker", 4),
            ("assists", 4),
            ("weapon", 1),      // string
            ("headshot", 6),    // bool
            ("suicide", 6),
            ("deathIcon", 1),
            ("explosive", 6),
            ("ghoster", 6),
        ] {
            defs.write_bits(kind, 3);
            defs.write_string(name);
        }
        defs.write_bits(0, 3); // end of entries
        let defs_bits = defs.bytes.len() * 8; // whole bytes; fine for the test
        w.write_bits(30, 6);
        w.write_bits(1, 9);
        w.write_bits(defs_bits as u32, 20);
        for b in &defs.bytes {
            w.write_bits(*b as u32, 8);
        }

        // svc_GameEvent (25): length 11 bits, then id + values per definition
        let mut ev = BitWriter::default();
        ev.write_bits(42, 9);
        ev.write_bits(7, 16); // userid
        ev.write_bits(3, 16); // attacker
        ev.write_bits(1, 16); // assists
        ev.write_string("wep_mpn"); // weapon
        ev.write_bit(true); // headshot
        ev.write_bit(false); // suicide
        ev.write_string("mpn"); // deathIcon
        ev.write_bit(false); // explosive
        ev.write_bit(true); // ghoster
        let ev_bits = ev.bytes.len() * 8;
        w.write_bits(25, 6);
        w.write_bits(ev_bits as u32, 11);
        for b in &ev.bytes {
            w.write_bits(*b as u32, 8);
        }

        // svc_UserMessage (23): kind 8 bits, length 11 bits, data
        let mut um = BitWriter::default();
        um.write_bits(2, 8); // client entity
        um.write_bits(1, 8); // raw flag
        um.write_string("NEO_Chat_All");
        um.write_string("sun");
        um.write_string("gg");
        let um_bits = um.bytes.len() * 8;
        w.write_bits(23, 6);
        w.write_bits(4, 8); // SayText2
        w.write_bits(um_bits as u32, 11);
        for b in &um.bytes {
            w.write_bits(*b as u32, 8);
        }

        let mut got_defs = None;
        let mut got_event = None;
        let mut got_um = None;
        walk_packet(&w.bytes, 24, &mut |msg| match msg {
            NetMessage::GameEventList(d) => got_defs = Some(d),
            NetMessage::GameEvent(c) => got_event = Some(c),
            NetMessage::UserMessage { kind, data } => got_um = Some((kind, data)),
            _ => {}
        })
        .unwrap();

        let defs = got_defs.expect("event list");
        assert_eq!(defs.get(&42).unwrap().name, "player_death");
        assert_eq!(defs.get(&42).unwrap().entries.len(), 9);

        let (name, fields) = parse_game_event(&got_event.expect("event"), &defs).unwrap();
        assert_eq!(name, "player_death");
        let get = |n: &str| fields.iter().find(|(f, _)| f == n).map(|(_, v)| v.clone());
        assert!(matches!(get("userid"), Some(EventValue::Int(7))));
        assert!(matches!(get("attacker"), Some(EventValue::Int(3))));
        assert!(matches!(get("headshot"), Some(EventValue::Bool(true))));
        assert!(matches!(get("ghoster"), Some(EventValue::Bool(true))));
        match get("weapon") {
            Some(EventValue::Str(s)) => assert_eq!(s, "wep_mpn"),
            other => panic!("bad weapon field: {other:?}"),
        }

        let (kind, _data) = got_um.expect("user message");
        assert_eq!(kind, 4);
    }

    /// Short and long fields must sign-extend; byte must not.
    #[test]
    fn event_ints_are_signed_except_byte() {
        let mut defs_w = BitWriter::default();
        defs_w.write_bits(1, 9);
        defs_w.write_string("ping");
        for (name, kind) in [("x", 4u32), ("big", 3), ("small", 5)] {
            defs_w.write_bits(kind, 3);
            defs_w.write_string(name);
        }
        defs_w.write_bits(0, 3);
        let defs = parse_event_list(
            &BitChunk { bytes: defs_w.bytes.clone(), bit_len: defs_w.bytes.len() * 8 },
            1,
        )
        .unwrap();

        let mut ev = BitWriter::default();
        ev.write_bits(1, 9);
        ev.write_bits(-880i16 as u16 as u32, 16); // short
        ev.write_bits(-70000i32 as u32, 32); // long
        ev.write_bits(200, 8); // byte: unsigned
        let (_, fields) =
            parse_game_event(&BitChunk { bytes: ev.bytes, bit_len: 0 }, &defs).unwrap();
        let get = |n: &str| fields.iter().find(|(f, _)| f == n).map(|(_, v)| v.clone());
        assert!(matches!(get("x"), Some(EventValue::Int(-880))));
        assert!(matches!(get("big"), Some(EventValue::Int(-70000))));
        assert!(matches!(get("small"), Some(EventValue::Int(200))));
    }

    /// svc_ServerInfo decodes field by field and leaves the reader aligned
    /// on the next message; net_SetConVar keeps its pairs in order.
    #[test]
    fn surfaces_server_info_and_convars() {
        let mut w = BitWriter::default();
        w.write_bits(8, 6);
        w.write_bits(24, 16); // protocol
        w.write_bits(27, 32); // server count
        w.write_bit(true); // hltv
        w.write_bit(true); // dedicated
        w.write_bits(u32::MAX, 32); // client CRC
        w.write_bits(244, 16); // max classes
        for b in 0..16u32 {
            w.write_bits(b, 8); // map md5
        }
        w.write_bits(0, 8); // player slot
        w.write_bits(17, 8); // max clients
        w.write_f32(0.015);
        w.write_bits(b'l' as u32, 8);
        for s in ["neo", "ntre_ghost_ctg", "sky_day01_09", "SourceTV"] {
            w.write_string(s);
        }
        w.write_bit(false); // replay
        w.write_bits(5, 6);
        w.write_bits(2, 8);
        for s in ["sv_neo_comp_name", "ads26", "tv_transmitall", "1"] {
            w.write_string(s);
        }
        w.write_bits(18, 6); // svc_SetView follows, to prove alignment
        w.write_bits(1, 11);

        let mut info = None;
        let mut cvars = None;
        walk_packet(&w.bytes, 24, &mut |msg| match msg {
            NetMessage::ServerInfo(i) => info = Some(i),
            NetMessage::SetConVar(c) => cvars = Some(c),
            _ => {}
        })
        .unwrap();
        let info = info.expect("server info");
        assert!(info.is_hltv && info.is_dedicated);
        assert_eq!((info.max_classes, info.player_slot, info.max_clients), (244, 0, 17));
        assert_eq!(info.map_hash[15], 15);
        assert_eq!(info.tick_interval, 0.015);
        assert_eq!((info.os, info.host_name.as_str()), ('l', "SourceTV"));
        let cvars = cvars.expect("convars");
        assert_eq!(cvars[0], ("sv_neo_comp_name".into(), "ads26".into()));
        assert_eq!(cvars.len(), 2);
    }

    /// Skippable messages must consume exactly their wire size.
    #[test]
    fn skips_messages_without_desync() {
        let mut w = BitWriter::default();
        // net_Tick
        w.write_bits(3, 6);
        w.write_bits(1234, 32);
        w.write_bits(10, 16);
        w.write_bits(2, 16);
        // svc_SetView
        w.write_bits(18, 6);
        w.write_bits(1, 11);
        // svc_GameEvent with empty defs is still framed correctly
        w.write_bits(25, 6);
        w.write_bits(9, 11);
        w.write_bits(500, 9);
        let mut events = 0;
        walk_packet(&w.bytes, 24, &mut |msg| {
            if matches!(msg, NetMessage::GameEvent(_)) {
                events += 1;
            }
        })
        .unwrap();
        assert_eq!(events, 1);
    }
}

/// Parse a raw game event against the demo's definitions. Returns the event
/// name and its fields in definition order.
pub fn parse_game_event(
    chunk: &BitChunk,
    defs: &EventDefs,
) -> Result<(String, Vec<(String, EventValue)>)> {
    let mut r = chunk.reader();
    let id = r.read_bits(9)? as u16;
    let Some(def) = defs.get(&id) else {
        bail!("game event with unknown type id {id}");
    };
    let mut fields = Vec::with_capacity(def.entries.len());
    for entry in &def.entries {
        let value = match entry.kind {
            1 => EventValue::Str(r.read_string()?),
            2 => EventValue::Float(r.read_f32()?),
            // Short and long are signed, byte is unsigned, matching the
            // engine's WriteShort/WriteLong/WriteByte.
            3 => EventValue::Int(r.read_bits(32)? as i32),
            4 => EventValue::Int(r.read_bits(16)? as u16 as i16 as i32),
            5 => EventValue::Int(r.read_bits(8)? as i32),
            6 => EventValue::Bool(r.read_bit()?),
            7 => continue, // local: not transmitted
            other => bail!("unknown game event value type {other}"),
        };
        fields.push((entry.name.clone(), value));
    }
    Ok((def.name.clone(), fields))
}
