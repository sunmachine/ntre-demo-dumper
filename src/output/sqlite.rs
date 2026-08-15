//! SQLite persistence: schema and every insert statement.
//!
//! One database can hold many demos; every child row is tagged with the
//! `demos.id` returned by `insert_demo`. When adding a table for a new
//! extractor, add it to `SCHEMA` and give it an `insert_*` method here.

use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

use crate::demo::frames::ViewInfo;
use crate::demo::header::DemoHeader;
use crate::extract::announcements::Announcement;
use crate::extract::rounds::Round;

pub struct Db {
    conn: Connection,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS demos (
    id INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    parsed_at TEXT NOT NULL DEFAULT (datetime('now')),
    demo_protocol INTEGER NOT NULL,
    network_protocol INTEGER NOT NULL,
    server TEXT NOT NULL,
    client TEXT NOT NULL,
    map TEXT NOT NULL,
    game_directory TEXT NOT NULL,
    playback_seconds REAL NOT NULL,
    playback_ticks INTEGER NOT NULL,
    playback_frames INTEGER NOT NULL,
    tickrate REAL NOT NULL
);

-- Center-text / game announcements recovered from packet payloads.
CREATE TABLE IF NOT EXISTS announcements (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    seconds REAL NOT NULL,
    text TEXT NOT NULL
);

-- Rounds derived from start/win announcements.
CREATE TABLE IF NOT EXISTS rounds (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    round_number INTEGER,
    start_tick INTEGER,
    end_tick INTEGER,
    winner TEXT,
    win_reason TEXT
);

-- Recorder point-of-view, per packet frame: position and view angles.
CREATE TABLE IF NOT EXISTS pov_samples (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    x REAL NOT NULL, y REAL NOT NULL, z REAL NOT NULL,
    pitch REAL NOT NULL, yaw REAL NOT NULL, roll REAL NOT NULL
);

-- Recorder raw input per tick, from dem_usercmd frames. `buttons` is the
-- raw 32-bit field; common bits are exposed as generated columns
-- (attack = fired, aim = aim-down-sights, lean_*, thermoptic, vision).
CREATE TABLE IF NOT EXISTS recorder_inputs (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    buttons INTEGER NOT NULL,
    impulse INTEGER NOT NULL,
    weaponselect INTEGER,
    pitch REAL NOT NULL, yaw REAL NOT NULL, roll REAL NOT NULL,
    forwardmove REAL NOT NULL, sidemove REAL NOT NULL, upmove REAL NOT NULL,
    mousedx INTEGER NOT NULL, mousedy INTEGER NOT NULL,
    attack     INTEGER GENERATED ALWAYS AS ((buttons >> 0) & 1) VIRTUAL,
    jump       INTEGER GENERATED ALWAYS AS ((buttons >> 1) & 1) VIRTUAL,
    duck       INTEGER GENERATED ALWAYS AS ((buttons >> 2) & 1) VIRTUAL,
    attack2    INTEGER GENERATED ALWAYS AS ((buttons >> 11) & 1) VIRTUAL,
    reload     INTEGER GENERATED ALWAYS AS ((buttons >> 13) & 1) VIRTUAL,
    sprint     INTEGER GENERATED ALWAYS AS ((buttons >> 17) & 1) VIRTUAL,
    aim        INTEGER GENERATED ALWAYS AS ((buttons >> 27) & 1) VIRTUAL,
    lean_left  INTEGER GENERATED ALWAYS AS ((buttons >> 28) & 1) VIRTUAL,
    lean_right INTEGER GENERATED ALWAYS AS ((buttons >> 29) & 1) VIRTUAL,
    thermoptic INTEGER GENERATED ALWAYS AS ((buttons >> 30) & 1) VIRTUAL,
    vision     INTEGER GENERATED ALWAYS AS ((buttons >> 31) & 1) VIRTUAL
);

-- Console commands issued by the recorder during playback.
CREATE TABLE IF NOT EXISTS console_cmds (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    cmd TEXT NOT NULL
);

-- Player roster: from the string-table dump at recording start plus
-- player_connect/player_info game events for late joiners.
CREATE TABLE IF NOT EXISTS players (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    entity_id INTEGER NOT NULL,
    userid INTEGER NOT NULL,
    name TEXT NOT NULL,
    steamid TEXT NOT NULL,
    is_bot INTEGER NOT NULL,
    first_seen_tick INTEGER NOT NULL
);

-- Kill feed from the player_death game event (NT;RE definition).
CREATE TABLE IF NOT EXISTS kills (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    victim_userid INTEGER NOT NULL,
    victim_name TEXT,
    attacker_userid INTEGER NOT NULL,
    attacker_name TEXT,
    assists INTEGER NOT NULL,
    weapon TEXT NOT NULL,
    headshot INTEGER NOT NULL,
    suicide INTEGER NOT NULL,
    explosive INTEGER NOT NULL,
    ghoster INTEGER NOT NULL
);

-- Chat lines (SayText2 user messages).
CREATE TABLE IF NOT EXISTS chat (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    client_entity INTEGER NOT NULL,
    from_name TEXT NOT NULL,
    text TEXT NOT NULL,
    team_chat INTEGER NOT NULL
);

-- Every game event, fields as JSON (queryable via SQLite's json functions).
CREATE TABLE IF NOT EXISTS game_events (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    name TEXT NOT NULL,
    fields TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_game_events_demo_name ON game_events(demo_id, name);
CREATE INDEX IF NOT EXISTS idx_kills_demo_tick ON kills(demo_id, tick);
CREATE INDEX IF NOT EXISTS idx_announcements_demo_tick ON announcements(demo_id, tick);
CREATE INDEX IF NOT EXISTS idx_pov_demo_tick ON pov_samples(demo_id, tick);
CREATE INDEX IF NOT EXISTS idx_inputs_demo_tick ON recorder_inputs(demo_id, tick);
"#;

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn begin(&self) -> Result<()> {
        Ok(self.conn.execute_batch("BEGIN")?)
    }

    pub fn commit(&self) -> Result<()> {
        Ok(self.conn.execute_batch("COMMIT")?)
    }

    pub fn insert_demo(&self, path: &str, h: &DemoHeader) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO demos (path, demo_protocol, network_protocol, server, client, map,
                                game_directory, playback_seconds, playback_ticks,
                                playback_frames, tickrate)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                path,
                h.demo_protocol,
                h.network_protocol,
                h.server_name,
                h.client_name,
                h.map_name,
                h.game_directory,
                h.playback_seconds,
                h.playback_ticks,
                h.playback_frames,
                h.tickrate(),
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn insert_announcements(
        &self,
        demo_id: i64,
        announcements: &[Announcement],
        tickrate: f64,
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO announcements (demo_id, tick, seconds, text) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for a in announcements {
            ins.execute(rusqlite::params![demo_id, a.tick, a.tick as f64 / tickrate, a.text])?;
        }
        Ok(())
    }

    pub fn insert_rounds(&self, demo_id: i64, rounds: &[Round]) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO rounds (demo_id, round_number, start_tick, end_tick, winner, win_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for r in rounds {
            ins.execute(rusqlite::params![
                demo_id, r.number, r.start_tick, r.end_tick, r.winner, r.reason
            ])?;
        }
        Ok(())
    }

    pub fn insert_pov_samples(&self, demo_id: i64, samples: &[(i32, ViewInfo)]) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO pov_samples (demo_id, tick, x, y, z, pitch, yaw, roll)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for (tick, v) in samples {
            ins.execute(rusqlite::params![
                demo_id, tick, v.origin[0], v.origin[1], v.origin[2],
                v.angles[0], v.angles[1], v.angles[2],
            ])?;
        }
        Ok(())
    }

    pub fn insert_recorder_inputs(
        &self,
        demo_id: i64,
        cmds: &[(i32, crate::demo::usercmd::UserCmd)],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO recorder_inputs (demo_id, tick, buttons, impulse, weaponselect,
                                          pitch, yaw, roll, forwardmove, sidemove, upmove,
                                          mousedx, mousedy)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )?;
        for (tick, c) in cmds {
            ins.execute(rusqlite::params![
                demo_id,
                tick,
                c.buttons as i64,
                c.impulse,
                c.weaponselect,
                c.viewangles[0],
                c.viewangles[1],
                c.viewangles[2],
                c.forwardmove,
                c.sidemove,
                c.upmove,
                c.mousedx,
                c.mousedy,
            ])?;
        }
        Ok(())
    }

    pub fn insert_players(&self, demo_id: i64, players: &[&crate::extract::net::Player]) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO players (demo_id, entity_id, userid, name, steamid, is_bot, first_seen_tick)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for p in players {
            ins.execute(rusqlite::params![
                demo_id, p.entity_id, p.user_id, p.name, p.steam_id, p.is_bot, p.first_seen_tick,
            ])?;
        }
        Ok(())
    }

    pub fn insert_kills(
        &self,
        demo_id: i64,
        kills: &[crate::extract::net::Kill],
        name_of: &dyn Fn(u32) -> Option<String>,
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO kills (demo_id, tick, victim_userid, victim_name, attacker_userid,
                                attacker_name, assists, weapon, headshot, suicide, explosive, ghoster)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        )?;
        for k in kills {
            ins.execute(rusqlite::params![
                demo_id,
                k.tick,
                k.victim_userid,
                name_of(k.victim_userid),
                k.attacker_userid,
                name_of(k.attacker_userid),
                k.assists,
                k.weapon,
                k.headshot,
                k.suicide,
                k.explosive,
                k.ghoster,
            ])?;
        }
        Ok(())
    }

    pub fn insert_chat(&self, demo_id: i64, chat: &[crate::extract::net::ChatLine]) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO chat (demo_id, tick, client_entity, from_name, text, team_chat)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for c in chat {
            ins.execute(rusqlite::params![
                demo_id, c.tick, c.client_entity, c.from, c.text, c.team_chat,
            ])?;
        }
        Ok(())
    }

    pub fn insert_game_events(&self, demo_id: i64, events: &[(i32, String, String)]) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO game_events (demo_id, tick, name, fields) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for (tick, name, fields) in events {
            ins.execute(rusqlite::params![demo_id, tick, name, fields])?;
        }
        Ok(())
    }

    pub fn insert_console_cmds(&self, demo_id: i64, cmds: &[(i32, String)]) -> Result<()> {
        let mut ins = self
            .conn
            .prepare("INSERT INTO console_cmds (demo_id, tick, cmd) VALUES (?1, ?2, ?3)")?;
        for (tick, cmd) in cmds {
            ins.execute(rusqlite::params![demo_id, tick, cmd])?;
        }
        Ok(())
    }
}
