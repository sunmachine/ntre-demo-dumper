//! Net-level extraction: walks the net-message stream inside signon/packet
//! frames and produces the deaths, chat, player roster, and a generic
//! game-events table (every event, fields as JSON). Game events are parsed
//! against the demo's own definitions, so NT;RE-specific events (ghost
//! captures, rank changes) decode without any hardcoded schema.

use anyhow::Result;
use std::collections::HashMap;

use super::{DemoContext, FrameExtractor, Summary};
use crate::demo::frames::{Frame, FrameKind};
use crate::demo::net::{
    parse_game_event, walk_packet, EventDefs, EventValue, NetMessage, ServerInfo,
};
use crate::demo::stringtables::{parse_userinfo, parse_userinfo_update, PlayerInfo};
use crate::extract::entities::{NeoNameChange, ResourceSample};
use crate::extract::rounds::{place, Round};
use crate::output::sqlite::Db;

pub struct Death {
    pub tick: i32,
    pub victim_userid: u32,
    pub attacker_userid: u32,
    pub assists: u32,
    pub weapon: String,
    pub headshot: bool,
    pub suicide: bool,
    pub explosive: bool,
    pub ghoster: bool,
    /// The round the death fell in, set by `NetPass::place_deaths` once the
    /// rounds are derived; None before the first round began.
    pub round_number: Option<i32>,
    /// The death fell after its round ended and before the next one began.
    pub post_round: bool,
    /// The teams of the victim, attacker and assister at the death, set by
    /// `NetPass::place_teams`; None for the world, for nobody, or when unknown.
    pub victim_team: Option<i64>,
    pub attacker_team: Option<i64>,
    pub assister_team: Option<i64>,
}

pub struct ChatLine {
    pub tick: i32,
    pub client_entity: u32,
    pub from: String,
    pub text: String,
    pub team_chat: bool,
}

pub struct Ping {
    pub tick: i32,
    pub userid: u32,
    pub team: i64,
    pub x: i64,
    pub y: i64,
    pub z: i64,
    pub ghoster_ping: bool,
}

pub struct GhostCallout {
    pub tick: i32,
    pub userid: u32,
    pub team: i64,
    pub target_entity_id: u32,
    pub x: i64,
    pub y: i64,
    pub z: i64,
}

pub struct TeamScore {
    pub tick: i32,
    pub team: i64,
    pub score: i64,
}

pub struct TeamChange {
    pub tick: i32,
    pub userid: u32,
    pub team: i64,
    pub old_team: i64,
    pub disconnect: bool,
}

pub struct RankChange {
    pub tick: i32,
    pub userid: u32,
    pub old_rank: i64,
    pub new_rank: i64,
}

pub struct RoundStart {
    pub tick: i32,
    pub objective: String,
    pub timelimit: i64,
    pub fraglimit: i64,
}

/// One round end as announced by NT;RE's RoundResult user message.
#[derive(Clone)]
pub struct RoundResult {
    pub tick: i32,
    /// `jinrai`, `nsf`, or `tie`, as sent.
    pub team: String,
    /// The victory text shown on screen, e.g. "Team NSF wins by capturing
    /// the ghost!" or "TIE"; empty when the server sent none.
    pub message: String,
}

/// One replicated convar value (net_SetConVar). The signon snapshot is at
/// tick 0; later rows are changes made while recording.
pub struct ServerCvar {
    pub tick: i32,
    pub name: String,
    pub value: String,
}

pub struct Player {
    pub entity_id: u32,
    pub user_id: u32,
    pub name: String,
    pub steam_id: String,
    pub is_bot: bool,
    pub first_seen_tick: i32,
    /// The last non-empty `neo_name` the player's entity carried; None
    /// when the player set none, or never came into a POV recorder's view.
    pub neo_name: Option<String>,
}

/// The userid of the connection holding entity slot `entity_id` at `tick`.
/// A slot passes to a later joiner when its player leaves, so the latest
/// arrival at or before `tick` holds it.
pub fn slot_holder(players: &HashMap<u32, Player>, entity_id: u32, tick: i32) -> Option<u32> {
    players
        .values()
        .filter(|p| p.entity_id == entity_id && p.first_seen_tick <= tick)
        .max_by_key(|p| (p.first_seen_tick, p.user_id))
        .map(|p| p.user_id)
}

/// The Steam ID of the connection holding each entity slot at a tick, by
/// the same rule as `slot_holder`. Tables that name a player by slot store
/// it beside the slot, and `player_samples` asks for every row, so the
/// holders are sorted once per slot instead of scanned per row.
pub struct SlotOwners {
    /// Per slot, `(first_seen_tick, user_id, steam_id)` in ascending order.
    by_slot: HashMap<u32, Vec<(i32, u32, String)>>,
}

impl SlotOwners {
    pub fn new(players: &HashMap<u32, Player>) -> Self {
        let mut by_slot: HashMap<u32, Vec<(i32, u32, String)>> = HashMap::new();
        for p in players.values() {
            by_slot.entry(p.entity_id).or_default().push((
                p.first_seen_tick,
                p.user_id,
                p.steam_id.clone(),
            ));
        }
        for holders in by_slot.values_mut() {
            holders.sort_unstable();
        }
        SlotOwners { by_slot }
    }

    /// The Steam ID of whoever held `entity_id` at `tick`, or None for a
    /// slot no roster player held by then.
    pub fn steamid_at(&self, entity_id: u32, tick: i64) -> Option<&str> {
        let holders = self.by_slot.get(&entity_id)?;
        let after = holders.partition_point(|&(first_seen, _, _)| i64::from(first_seen) <= tick);
        Some(holders[after.checked_sub(1)?].2.as_str())
    }
}

#[derive(Default)]
pub struct NetPass {
    defs: EventDefs,
    pub events: Vec<(i32, String, String)>, // tick, name, fields as JSON
    pub deaths: Vec<Death>,
    pub pings: Vec<Ping>,
    pub callouts: Vec<GhostCallout>,
    pub team_scores: Vec<TeamScore>,
    pub team_changes: Vec<TeamChange>,
    pub rank_changes: Vec<RankChange>,
    pub round_starts: Vec<RoundStart>,
    pub round_results: Vec<RoundResult>,
    pub chat: Vec<ChatLine>,
    pub players: HashMap<u32, Player>, // by userid
    /// The first svc_ServerInfo; `None` if the signon never carried one.
    pub server_info: Option<ServerInfo>,
    pub cvars: Vec<ServerCvar>,
    /// How many svc_CreateStringTable messages have been seen, which is the
    /// number the next table gets.
    tables_created: u8,
    /// The userinfo table's number and entry limit, once it is created.
    userinfo_table: Option<(u8, u16)>,
    warnings: usize,
}

// NT;RE user message ids (hl2_usermessages.cpp; RoundResult is NEO-specific).
const SAY_TEXT2: u8 = 4;
const ROUND_RESULT: u8 = 28;

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
/// SayText2: client entity u8, chat flag u8, a format string, then up to
/// four parameter strings. A format string is either a localization token
/// (`#HL2MP_Chat_All`) or, in NT;RE, a literal template such as
/// `%s1: %s2`, `[Jinrai] %s1: %s2` (team chat), or `*DEAD* %s1: %s2`; in
/// both cases `%s1` is the sender's name and `%s2` the message. Anything
/// else is a bare server line with no sender.
fn parse_say_text2(tick: i32, data: &crate::demo::bits::BitChunk) -> Result<ChatLine> {
    let mut r = data.reader();
    let client = r.read_bits(8)?;
    let _chat = r.read_bits(8)?;
    let format = r.read_string()?;
    let is_template = format.starts_with('#') || format.contains("Chat") || format.contains("%s1");
    if is_template {
        let from = r.read_string()?;
        let text = r.read_string()?;
        let team_chat = format.contains("Team") || format.contains('[');
        Ok(ChatLine { tick, client_entity: client, from, text, team_chat })
    } else {
        Ok(ChatLine {
            tick,
            client_entity: client,
            from: String::new(),
            text: format,
            team_chat: false,
        })
    }
}

/// RoundResult (NT;RE, `CNEORules::SetWinningTeam`): winning team string
/// (`jinrai`, `nsf`, `tie`), the server time as f32, then the victory
/// message. The message is omitted for map-scripted wins.
fn parse_round_result(tick: i32, data: &crate::demo::bits::BitChunk) -> Result<RoundResult> {
    let mut r = data.reader();
    let team = r.read_string()?;
    let _server_time = r.read_f32()?;
    let message = if r.bits_left() >= 8 { r.read_string()? } else { String::new() };
    Ok(RoundResult { tick, team, message: message.trim().to_string() })
}

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

fn int_field(fields: &[(String, EventValue)], name: &str) -> i64 {
    match field(fields, name) {
        Some(EventValue::Int(n)) => *n as i64,
        _ => 0,
    }
}

/// `int_field` for id-like fields that are never negative in practice.
fn id_field(fields: &[(String, EventValue)], name: &str) -> u32 {
    int_field(fields, name).max(0) as u32
}

fn bool_field(fields: &[(String, EventValue)], name: &str) -> bool {
    // Events encode flags as Bool or as Short/Byte (e.g. player_connect_client's
    // `bot` is a short) depending on the definition; accept both.
    match field(fields, name) {
        Some(EventValue::Bool(b)) => *b,
        Some(EventValue::Int(n)) => *n != 0,
        _ => false,
    }
}

fn str_field(fields: &[(String, EventValue)], name: &str) -> String {
    match field(fields, name) {
        Some(EventValue::Str(s)) => s.clone(),
        _ => String::new(),
    }
}

/// For a grenade or detpack kill, the event reports the inflictor rather
/// than the holder's weapon (upstream `neo_gamerules.cpp:4341-4351`), so
/// `weapon` arrives as the projectile's own entity name. Map it to the
/// weapon that threw or placed it; any other value, such as a world damage
/// cause, passes through as the event sent it.
fn death_weapon(weapon: String) -> String {
    match weapon.as_str() {
        "neo_grenade_frag" => "grenade".to_string(),
        "neo_deployed_detpack" => "remotedet".to_string(),
        "neo_grenade_smoke" => "smokegrenade".to_string(),
        _ => weapon,
    }
}

impl NetPass {
    /// The Steam ID of the player holding `userid`, or `None` for a userid
    /// the roster never saw, which includes the world's userid 0.
    pub fn steamid_of(&self, userid: u32) -> Option<String> {
        self.players.get(&userid).map(|p| p.steam_id.clone())
    }

    /// Give each roster row the latest neo name its entity slot carried
    /// while the connection held it. An empty name is skipped, so a player
    /// who clears their neo name keeps the last one they set, and a slot's
    /// next player, whose entity starts out empty, cannot blank the name of
    /// the player before them.
    pub fn apply_neo_names(&mut self, changes: &[NeoNameChange]) {
        for change in changes.iter().filter(|c| !c.name.is_empty()) {
            let tick = i32::try_from(change.tick).unwrap_or(i32::MAX);
            if let Some(user_id) = slot_holder(&self.players, change.entity_id, tick) {
                if let Some(p) = self.players.get_mut(&user_id) {
                    p.neo_name = Some(change.name.clone());
                }
            }
        }
    }

    /// Give each death the round it fell in. Rounds need the entity pass
    /// and the announcements, so they exist only after the frame walk.
    pub fn place_deaths(&mut self, rounds: &[Round]) {
        for d in &mut self.deaths {
            let round = place(rounds, d.tick);
            d.round_number = round.as_ref().and_then(|r| r.number);
            d.post_round = round.is_some_and(|r| r.post_round);
        }
    }

    /// Give each death the teams of its victim, attacker and assister, from
    /// the player resource's team array at the death's tick. The server
    /// sends the resource to every client, so POV recordings have the team
    /// of a player the recorder never saw.
    pub fn place_teams(&mut self, resource: &[ResourceSample]) {
        // Each slot's team changes, in tick order.
        let mut changes: HashMap<u32, Vec<(u32, i64)>> = HashMap::new();
        for s in resource {
            let slot = changes.entry(s.entity_id).or_default();
            if slot.last().is_none_or(|&(_, team)| team != s.team) {
                slot.push((s.tick, s.team));
            }
        }
        let players = &self.players;
        let team_of = |userid: u32, tick: i32| {
            let slot = changes.get(&players.get(&userid)?.entity_id)?;
            let after = slot.partition_point(|&(t, _)| i64::from(t) <= i64::from(tick));
            Some(slot[after.checked_sub(1)?].1)
        };
        for d in &mut self.deaths {
            d.victim_team = team_of(d.victim_userid, d.tick);
            d.attacker_team = team_of(d.attacker_userid, d.tick);
            d.assister_team = team_of(d.assists, d.tick);
        }
    }

    /// How many deaths name a victim, attacker or assister that the roster
    /// never saw. Those rows get NULL names and Steam IDs. Userid 0 is the
    /// world as attacker and nobody as assister, so it does not count.
    pub fn deaths_missing_from_roster(&self) -> usize {
        let missing = |userid: u32| userid != 0 && !self.players.contains_key(&userid);
        self.deaths
            .iter()
            .filter(|k| missing(k.victim_userid) || missing(k.attacker_userid) || missing(k.assists))
            .count()
    }

    fn on_game_event(&mut self, tick: i32, name: &str, fields: &[(String, EventValue)]) {
        match name {
            "player_death" => self.deaths.push(Death {
                tick,
                victim_userid: id_field(fields, "userid"),
                attacker_userid: id_field(fields, "attacker"),
                assists: id_field(fields, "assists"),
                weapon: death_weapon(str_field(fields, "weapon")),
                headshot: bool_field(fields, "headshot"),
                suicide: bool_field(fields, "suicide"),
                explosive: bool_field(fields, "explosive"),
                ghoster: bool_field(fields, "ghoster"),
                round_number: None,
                post_round: false,
                victim_team: None,
                attacker_team: None,
                assister_team: None,
            }),
            "player_ping" => self.pings.push(Ping {
                tick,
                userid: id_field(fields, "userid"),
                team: int_field(fields, "playerteam"),
                x: int_field(fields, "pingx"),
                y: int_field(fields, "pingy"),
                z: int_field(fields, "pingz"),
                ghoster_ping: bool_field(fields, "ghosterping"),
            }),
            "ghost_enemy_callout" => self.callouts.push(GhostCallout {
                tick,
                userid: id_field(fields, "userid"),
                team: int_field(fields, "team"),
                target_entity_id: id_field(fields, "targetid"),
                x: int_field(fields, "targetx"),
                y: int_field(fields, "targety"),
                z: int_field(fields, "targetz"),
            }),
            "team_score" => self.team_scores.push(TeamScore {
                tick,
                team: int_field(fields, "teamid"),
                score: int_field(fields, "score"),
            }),
            "player_team" => self.team_changes.push(TeamChange {
                tick,
                userid: id_field(fields, "userid"),
                team: int_field(fields, "team"),
                old_team: int_field(fields, "oldteam"),
                disconnect: bool_field(fields, "disconnect"),
            }),
            "player_rankchange" => self.rank_changes.push(RankChange {
                tick,
                userid: id_field(fields, "userid"),
                old_rank: int_field(fields, "oldRank"),
                new_rank: int_field(fields, "newRank"),
            }),
            "round_start" => self.round_starts.push(RoundStart {
                tick,
                objective: str_field(fields, "objective"),
                timelimit: int_field(fields, "timelimit"),
                fraglimit: int_field(fields, "fraglimit"),
            }),
            // player_connect_client is what NT;RE actually fires for joins
            // (bots included) observed mid-recording.
            "player_connect" | "player_connect_client" | "player_info" => {
                let user_id = id_field(fields, "userid");
                self.players.entry(user_id).or_insert_with(|| Player {
                    entity_id: id_field(fields, "index") + 1,
                    user_id,
                    name: str_field(fields, "name"),
                    steam_id: str_field(fields, "networkid"),
                    is_bot: bool_field(fields, "bot"),
                    first_seen_tick: tick,
                    neo_name: None,
                });
            }
            // player_changeneoname is left out: `name` keeps the Steam name,
            // and the neo name comes from the entity pass for both
            // recording types, in `apply_neo_names`.
            "player_changename" => {
                let user_id = id_field(fields, "userid");
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

    /// Add userinfo entries to the roster. A userid already in the roster
    /// keeps its row, because the table repeats a player's entry whenever
    /// any part of it changes.
    fn on_userinfo(&mut self, tick: i32, players: Vec<PlayerInfo>) {
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
                first_seen_tick: tick,
                neo_name: None,
            });
        }
    }

    /// SourceTV appends its own `tv_transmitall` to the server's list, so a
    /// name can repeat within one message; the last value wins.
    fn on_set_convar(&mut self, tick: i32, pairs: Vec<(String, String)>) {
        for (name, value) in pairs {
            if let Some(row) =
                self.cvars.iter_mut().rev().take_while(|c| c.tick == tick).find(|c| c.name == name)
            {
                row.value = value;
            } else {
                self.cvars.push(ServerCvar { tick, name, value });
            }
        }
    }

    fn on_user_message(&mut self, tick: i32, kind: u8, data: &crate::demo::bits::BitChunk) {
        if kind == ROUND_RESULT {
            match parse_round_result(tick, data) {
                Ok(result) => self.round_results.push(result),
                Err(_) => self.warnings += 1,
            }
            return;
        }
        if kind != SAY_TEXT2 {
            return;
        }
        if let Ok(mut line) = parse_say_text2(tick, data) {
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
                // Signon frames carry the server's uptime tick; the snapshot
                // they hold is the state at the start of the recording.
                let state_tick = if frame.kind == FrameKind::Signon { 0 } else { tick };
                for msg in collected {
                    match msg {
                        NetMessage::ServerInfo(info) => {
                            self.server_info.get_or_insert(info);
                        }
                        NetMessage::SetConVar(pairs) => self.on_set_convar(state_tick, pairs),
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
                        NetMessage::CreateStringTable { name, max_entries } => {
                            if name == "userinfo" {
                                self.userinfo_table = Some((self.tables_created, max_entries));
                            }
                            self.tables_created += 1;
                        }
                        NetMessage::UpdateStringTable { table_id, changed, data } => {
                            let Some((_, max_entries)) =
                                self.userinfo_table.filter(|(id, _)| *id == table_id)
                            else {
                                continue;
                            };
                            match parse_userinfo_update(&data, max_entries, changed) {
                                Ok(players) => self.on_userinfo(state_tick, players),
                                Err(_) => self.warnings += 1,
                            }
                        }
                    }
                }
            }
            FrameKind::StringTables => {
                // The signon-stage frames carry the server's uptime tick,
                // not a recording-relative one, so a roster player is
                // "first seen" at tick 0: the start of the recording.
                if let Ok(players) = parse_userinfo(frame.payload_in(ctx.data)) {
                    self.on_userinfo(0, players);
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
        let steamid_of = |userid: u32| self.steamid_of(userid);
        db.insert_deaths(demo_id, &self.deaths, &name_of, &steamid_of)?;
        db.insert_player_pings(demo_id, &self.pings, &steamid_of)?;
        let owners = SlotOwners::new(&self.players);
        db.insert_ghost_callouts(demo_id, &self.callouts, &steamid_of, &owners)?;
        db.insert_team_scores(demo_id, &self.team_scores)?;
        db.insert_team_changes(demo_id, &self.team_changes, &steamid_of)?;
        db.insert_rank_changes(demo_id, &self.rank_changes, &steamid_of)?;
        db.insert_round_starts(demo_id, &self.round_starts)?;
        db.insert_round_results(demo_id, &self.round_results)?;
        db.insert_chat(demo_id, &self.chat, &owners)?;
        db.insert_game_events(demo_id, &self.events)?;
        db.insert_server_cvars(demo_id, &self.cvars)?;
        let mut summary: Summary = vec![
            ("players".into(), self.players.len()),
            ("deaths".into(), self.deaths.len()),
            ("player pings".into(), self.pings.len()),
            ("ghost callouts".into(), self.callouts.len()),
            ("team scores".into(), self.team_scores.len()),
            ("team changes".into(), self.team_changes.len()),
            ("rank changes".into(), self.rank_changes.len()),
            ("round starts".into(), self.round_starts.len()),
            ("round results".into(), self.round_results.len()),
            ("chat lines".into(), self.chat.len()),
            ("game events".into(), self.events.len()),
            ("server cvars".into(), self.cvars.len()),
        ];
        if self.warnings > 0 {
            summary.push(("net decode warnings".into(), self.warnings));
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demo::bits::testutil::BitWriter;
    use crate::demo::bits::BitChunk;

    fn chunk(w: BitWriter) -> BitChunk {
        let bit_len = w.bytes.len() * 8;
        BitChunk { bytes: w.bytes, bit_len }
    }

    fn say_text2(format: &str, params: &[&str]) -> BitChunk {
        let mut w = BitWriter::default();
        w.write_bits(9, 8); // client entity
        w.write_bits(1, 8); // chat flag
        w.write_string(format);
        for p in params {
            w.write_string(p);
        }
        chunk(w)
    }

    #[test]
    fn ntre_literal_template_yields_sender_and_message() {
        let chunk = say_text2("%s1: %s2", &["Scylitha", "gl hf", "", ""]);
        let line = parse_say_text2(5, &chunk).unwrap();
        assert_eq!(line.client_entity, 9);
        assert_eq!(line.from, "Scylitha");
        assert_eq!(line.text, "gl hf");
        assert!(!line.team_chat);
    }

    #[test]
    fn team_and_dead_prefixes() {
        let line = parse_say_text2(1, &say_text2("[Jinrai] %s1: %s2", &["a", "rush", "", ""])).unwrap();
        assert!(line.team_chat);
        let line = parse_say_text2(1, &say_text2("*DEAD* %s1: %s2", &["a", "gg", "", ""])).unwrap();
        assert!(!line.team_chat);
        assert_eq!(line.text, "gg");
    }

    #[test]
    fn localization_token_and_bare_line() {
        let line = parse_say_text2(1, &say_text2("#HL2MP_Chat_Team", &["a", "b"])).unwrap();
        assert!(line.team_chat);
        assert_eq!((line.from.as_str(), line.text.as_str()), ("a", "b"));
        let line = parse_say_text2(1, &say_text2("Server restarting", &[])).unwrap();
        assert_eq!(line.text, "Server restarting");
        assert!(line.from.is_empty());
    }

    #[test]
    fn repeated_convar_in_one_message_keeps_last_value() {
        let mut pass = NetPass::default();
        pass.on_set_convar(0, vec![("tv_transmitall".into(), "1".into()), ("tv_transmitall".into(), "0".into())]);
        pass.on_set_convar(500, vec![("tv_transmitall".into(), "1".into())]);
        let rows: Vec<_> = pass.cvars.iter().map(|c| (c.tick, c.value.as_str())).collect();
        assert_eq!(rows, vec![(0, "0"), (500, "1")]);
    }

    fn roster_player(user_id: u32) -> Player {
        Player {
            entity_id: user_id,
            user_id,
            name: String::new(),
            steam_id: String::new(),
            is_bot: false,
            first_seen_tick: 0,
            neo_name: None,
        }
    }

    fn death(victim_userid: u32, attacker_userid: u32, assists: u32) -> Death {
        Death {
            tick: 0,
            victim_userid,
            attacker_userid,
            assists,
            weapon: String::new(),
            headshot: false,
            suicide: false,
            explosive: false,
            ghoster: false,
            round_number: None,
            post_round: false,
            victim_team: None,
            attacker_team: None,
            assister_team: None,
        }
    }

    #[test]
    fn deaths_missing_from_roster_skip_the_world_and_no_assister() {
        let pass = NetPass {
            players: HashMap::from([(1, roster_player(1)), (2, roster_player(2))]),
            deaths: vec![
                death(1, 2, 0), // both players known, nobody assisted
                death(1, 0, 0), // world death
                death(9, 2, 0), // victim unknown
                death(1, 9, 0), // attacker unknown
                death(1, 2, 9), // assister unknown
            ],
            ..NetPass::default()
        };
        assert_eq!(pass.deaths_missing_from_roster(), 3);
    }

    #[test]
    fn slot_owners_follow_the_slot_to_its_next_player() {
        // Userid 1 holds slot 5 from the start; userid 2 takes it at tick 100.
        let mut first = roster_player(1);
        first.entity_id = 5;
        first.steam_id = "[U:1:1]".into();
        let mut second = roster_player(2);
        second.entity_id = 5;
        second.first_seen_tick = 100;
        second.steam_id = "[U:1:2]".into();
        let owners = SlotOwners::new(&HashMap::from([(1, first), (2, second)]));
        assert_eq!(owners.steamid_at(5, 0), Some("[U:1:1]"));
        assert_eq!(owners.steamid_at(5, 99), Some("[U:1:1]"));
        assert_eq!(owners.steamid_at(5, 100), Some("[U:1:2]"));
        assert_eq!(owners.steamid_at(6, 100), None);
    }

    fn resource(tick: u32, entity_id: u32, team: i64) -> ResourceSample {
        ResourceSample { tick, entity_id, team, alive: true, connected: true, xp: 0, score: 0, deaths: 0, ping: 0 }
    }

    #[test]
    fn death_teams_come_from_the_resource_at_the_death_tick() {
        let at = |tick: i32, victim: u32, attacker: u32, assists: u32| Death {
            tick,
            ..death(victim, attacker, assists)
        };
        let mut pass = NetPass {
            players: HashMap::from([(1, roster_player(1)), (2, roster_player(2)), (3, roster_player(3))]),
            deaths: vec![at(50, 1, 2, 3), at(100, 1, 0, 0), at(200, 1, 2, 0), at(5, 1, 2, 0)],
            ..NetPass::default()
        };
        // Player 1 switches from Jinrai to NSF at tick 100. Player 3 has no
        // sample yet at tick 5.
        let samples = [resource(10, 1, 2), resource(10, 2, 3), resource(10, 3, 2), resource(100, 1, 3)];
        pass.place_teams(&samples);
        let teams: Vec<_> =
            pass.deaths.iter().map(|d| (d.victim_team, d.attacker_team, d.assister_team)).collect();
        assert_eq!(
            teams,
            [
                (Some(2), Some(3), Some(2)),
                (Some(3), None, None), // the world and nobody have no team
                (Some(3), Some(3), None),
                (None, None, None), // before any sample
            ]
        );
    }

    fn neo_name(tick: u32, entity_id: u32, name: &str) -> NeoNameChange {
        NeoNameChange { tick, entity_id, name: name.to_string() }
    }

    #[test]
    fn neo_names_go_to_the_connection_holding_the_slot() {
        // Userid 1 holds slot 5 from the start; userid 2 takes it at tick 100.
        let mut pass = NetPass {
            players: HashMap::from([
                (1, Player { entity_id: 5, ..roster_player(1) }),
                (2, Player { entity_id: 5, first_seen_tick: 100, ..roster_player(2) }),
                (3, Player { entity_id: 6, ..roster_player(3) }),
            ]),
            ..NetPass::default()
        };
        pass.apply_neo_names(&[
            neo_name(10, 5, "first"),
            neo_name(50, 5, "renamed"),
            neo_name(100, 5, ""), // the new player's entity starts out empty
            neo_name(101, 5, "joiner"),
        ]);
        assert_eq!(pass.players[&1].neo_name.as_deref(), Some("renamed"));
        assert_eq!(pass.players[&2].neo_name.as_deref(), Some("joiner"));
        assert_eq!(pass.players[&3].neo_name, None);
    }

    #[test]
    fn neo_name_rename_event_leaves_the_steam_name() {
        let mut pass = NetPass {
            players: HashMap::from([(1, Player { name: "steam".into(), ..roster_player(1) })]),
            ..NetPass::default()
        };
        let fields = [("userid".to_string(), EventValue::Int(1)), ("newname".to_string(), EventValue::Str("neo".into()))];
        pass.on_game_event(5, "player_changeneoname", &fields);
        assert_eq!(pass.players[&1].name, "steam");
        pass.on_game_event(6, "player_changename", &fields);
        assert_eq!(pass.players[&1].name, "neo");
    }

    #[test]
    fn round_result_reads_team_time_and_message() {
        let mut w = BitWriter::default();
        w.write_string("tie");
        w.write_f32(1822.26);
        w.write_string("TIE\n");
        let r = parse_round_result(7, &chunk(w)).unwrap();
        assert_eq!((r.tick, r.team.as_str(), r.message.as_str()), (7, "tie", "TIE"));

        let mut w = BitWriter::default();
        w.write_string("nsf");
        w.write_f32(1.0);
        let r = parse_round_result(8, &chunk(w)).unwrap();
        assert_eq!((r.team.as_str(), r.message.as_str()), ("nsf", ""));
    }
}
