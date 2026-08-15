//! Net-level extraction: walks the net-message stream inside signon/packet
//! frames and produces the kill feed, chat, player roster, and a generic
//! game-events table (every event, fields as JSON). Game events are parsed
//! against the demo's own definitions, so NT;RE-specific events (ghost
//! captures, rank changes…) decode without any hardcoded schema.

use anyhow::Result;
use std::collections::HashMap;

use super::{DemoContext, FrameExtractor, Summary};
use crate::demo::frames::{Frame, FrameKind};
use crate::demo::net::{parse_game_event, walk_packet, EventDefs, EventValue, NetMessage};
use crate::demo::stringtables::parse_userinfo;
use crate::output::sqlite::Db;

pub struct Kill {
    pub tick: i32,
    pub victim_userid: u32,
    pub attacker_userid: u32,
    pub assists: u32,
    pub weapon: String,
    pub headshot: bool,
    pub suicide: bool,
    pub explosive: bool,
    pub ghoster: bool,
}

pub struct ChatLine {
    pub tick: i32,
    pub client_entity: u32,
    pub from: String,
    pub text: String,
    pub team_chat: bool,
}

pub struct Player {
    pub entity_id: u32,
    pub user_id: u32,
    pub name: String,
    pub steam_id: String,
    pub is_bot: bool,
    pub first_seen_tick: i32,
}

#[derive(Default)]
pub struct NetPass {
    defs: EventDefs,
    pub events: Vec<(i32, String, String)>, // tick, name, fields as JSON
    pub kills: Vec<Kill>,
    pub chat: Vec<ChatLine>,
    pub players: HashMap<u32, Player>, // by userid
    warnings: usize,
}

const SAY_TEXT2: u8 = 4; // NT;RE user message ids (hl2_usermessages.cpp)

fn json_escape(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn fields_to_json(fields: &[(String, EventValue)]) -> String {
    let mut out = String::from("{");
    for (i, (name, value)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        json_escape(name, &mut out);
        out.push(':');
        match value {
            EventValue::Str(s) => json_escape(s, &mut out),
            EventValue::Float(f) => out.push_str(&format!("{f}")),
            EventValue::Int(n) => out.push_str(&format!("{n}")),
            EventValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        }
    }
    out.push('}');
    out
}

/// Strip Source chat control codes: \x01-\x06 mode markers, \x07 + RRGGBB,
/// \x08 + RRGGBBAA.
fn strip_chat_codes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c as u32 {
            1..=6 => {}
            7 => {
                for _ in 0..6 {
                    chars.next();
                }
            }
            8 => {
                for _ in 0..8 {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

fn field<'a>(fields: &'a [(String, EventValue)], name: &str) -> Option<&'a EventValue> {
    fields.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

fn int_field(fields: &[(String, EventValue)], name: &str) -> u32 {
    match field(fields, name) {
        Some(EventValue::Int(n)) => *n,
        _ => 0,
    }
}

fn bool_field(fields: &[(String, EventValue)], name: &str) -> bool {
    matches!(field(fields, name), Some(EventValue::Bool(true)))
}

fn str_field(fields: &[(String, EventValue)], name: &str) -> String {
    match field(fields, name) {
        Some(EventValue::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

impl NetPass {
    fn on_game_event(&mut self, tick: i32, name: &str, fields: &[(String, EventValue)]) {
        match name {
            "player_death" => self.kills.push(Kill {
                tick,
                victim_userid: int_field(fields, "userid"),
                attacker_userid: int_field(fields, "attacker"),
                assists: int_field(fields, "assists"),
                weapon: str_field(fields, "weapon"),
                headshot: bool_field(fields, "headshot"),
                suicide: bool_field(fields, "suicide"),
                explosive: bool_field(fields, "explosive"),
                ghoster: bool_field(fields, "ghoster"),
            }),
            "player_connect" | "player_info" => {
                let user_id = int_field(fields, "userid");
                self.players.entry(user_id).or_insert_with(|| Player {
                    entity_id: int_field(fields, "index") + 1,
                    user_id,
                    name: str_field(fields, "name"),
                    steam_id: str_field(fields, "networkid"),
                    is_bot: bool_field(fields, "bot"),
                    first_seen_tick: tick,
                });
            }
            "player_changename" | "player_changeneoname" => {
                let user_id = int_field(fields, "userid");
                if let Some(p) = self.players.get_mut(&user_id) {
                    let newname = str_field(fields, "newname");
                    if !newname.is_empty() {
                        p.name = newname;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_user_message(&mut self, tick: i32, kind: u8, data: &crate::demo::bits::BitChunk) {
        if kind != SAY_TEXT2 {
            return;
        }
        // SayText2: client entity u8, raw-text flag u8, then either a bare
        // string or (kind string, from string, message string).
        let parse = || -> Result<ChatLine> {
            let mut r2 = data.reader();
            let client = r2.read_bits(8)?;
            let _raw = r2.read_bits(8)?;
            let first = r2.read_string()?;
            if first.starts_with('#') || first.contains("Chat") {
                let from = r2.read_string()?;
                let text = r2.read_string()?;
                let team_chat = first.contains("Team");
                Ok(ChatLine { tick, client_entity: client, from, text, team_chat })
            } else {
                Ok(ChatLine {
                    tick,
                    client_entity: client,
                    from: String::new(),
                    text: first,
                    team_chat: false,
                })
            }
        };
        if let Ok(mut line) = parse() {
            line.text = strip_chat_codes(&line.text).trim().to_string();
            line.from = strip_chat_codes(&line.from).trim().to_string();
            if !line.text.is_empty() {
                self.chat.push(line);
            }
        }
    }
}

impl FrameExtractor for NetPass {
    fn on_frame(&mut self, ctx: &DemoContext, frame: &Frame) {
        match frame.kind {
            FrameKind::Signon | FrameKind::Packet => {
                let tick = frame.tick;
                let payload = frame.payload_in(ctx.data);
                let protocol = ctx.header.network_protocol;
                // Collect first, then mutate self: the sink closure can't
                // borrow self mutably while we also parse against self.defs.
                let mut collected: Vec<NetMessage> = Vec::new();
                let result = walk_packet(payload, protocol, &mut |msg| collected.push(msg));
                if result.is_err() {
                    self.warnings += 1;
                }
                for msg in collected {
                    match msg {
                        NetMessage::GameEventList(defs) => self.defs = defs,
                        NetMessage::GameEvent(chunk) => {
                            match parse_game_event(&chunk, &self.defs) {
                                Ok((name, fields)) => {
                                    self.on_game_event(tick, &name, &fields);
                                    self.events.push((tick, name, fields_to_json(&fields)));
                                }
                                Err(_) => self.warnings += 1,
                            }
                        }
                        NetMessage::UserMessage { kind, data } => {
                            self.on_user_message(tick, kind, &data);
                        }
                    }
                }
            }
            FrameKind::StringTables => {
                if let Ok(players) = parse_userinfo(frame.payload_in(ctx.data)) {
                    for p in players {
                        if p.is_hltv {
                            continue;
                        }
                        self.players.entry(p.user_id).or_insert_with(|| Player {
                            entity_id: p.entity_id,
                            user_id: p.user_id,
                            name: p.name,
                            steam_id: p.steam_id,
                            is_bot: p.is_fake_player,
                            first_seen_tick: frame.tick,
                        });
                    }
                }
            }
            _ => {}
        }
    }

    fn persist(&mut self, db: &Db, demo_id: i64, _ctx: &DemoContext) -> Result<Summary> {
        let mut players: Vec<&Player> = self.players.values().collect();
        players.sort_by_key(|p| p.entity_id);
        db.insert_players(demo_id, &players)?;
        let name_of = |userid: u32| -> Option<String> {
            self.players.get(&userid).map(|p| p.name.clone())
        };
        db.insert_kills(demo_id, &self.kills, &name_of)?;
        db.insert_chat(demo_id, &self.chat)?;
        db.insert_game_events(demo_id, &self.events)?;
        let mut summary: Summary = vec![
            ("players".into(), self.players.len()),
            ("kills".into(), self.kills.len()),
            ("chat lines".into(), self.chat.len()),
            ("game events".into(), self.events.len()),
        ];
        if self.warnings > 0 {
            summary.push(("net decode warnings".into(), self.warnings));
        }
        Ok(summary)
    }
}
