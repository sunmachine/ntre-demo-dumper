//! SQLite persistence: schema and every insert statement.
//!
//! One database can hold many demos; every child row is tagged with the
//! `demos.id` returned by `insert_demo`. When adding a table for a new
//! extractor, add it to `SCHEMA`, give it an `insert_*` method here, and
//! document it in SCHEMA.md (a test enforces the last part).
//!
//! Comments inside a CREATE TABLE statement are preserved by SQLite and shown
//! by `.schema`, so they double as end-user documentation; comments between
//! statements do not.

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
---------------------------------------------------------------- reference

CREATE TABLE IF NOT EXISTS demos (
    id INTEGER PRIMARY KEY,        -- referenced by every table's demo_id
    path TEXT NOT NULL,
    parsed_at TEXT NOT NULL DEFAULT (datetime('now')),
    demo_protocol INTEGER NOT NULL,
    network_protocol INTEGER NOT NULL,
    server TEXT NOT NULL,
    client TEXT NOT NULL,          -- name of the recording player
    map TEXT NOT NULL,
    game_directory TEXT NOT NULL,
    playback_seconds REAL NOT NULL,
    playback_ticks INTEGER NOT NULL,
    playback_frames INTEGER NOT NULL,
    tickrate REAL NOT NULL         -- seconds = tick / tickrate, in all tables
);

-- Player roster: from the string-table dump at recording start plus
-- player_connect/player_info game events for late joiners.
CREATE TABLE IF NOT EXISTS players (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    entity_id INTEGER NOT NULL,    -- joins player_samples.entity_id, chat.client_entity
    userid INTEGER NOT NULL,       -- joins kills.*_userid and game-event userid fields
    name TEXT NOT NULL,            -- latest name if the player renamed
    steamid TEXT NOT NULL,
    is_bot INTEGER NOT NULL,
    first_seen_tick INTEGER NOT NULL
);

--------------------------------------------------------------- tick series

-- All-player entity samples (on-change): position, eye angles, weapon,
-- health, team (2=Jinrai, 3=NSF), life state. POV demos only see entities
-- in the recorder's PVS; in_pvs=0 marks a player leaving it.
CREATE TABLE IF NOT EXISTS player_samples (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    entity_id INTEGER NOT NULL,   -- joins players.entity_id
    x REAL NOT NULL, y REAL NOT NULL, z REAL NOT NULL,  -- player origin (feet)
    eye_pitch REAL NOT NULL, eye_yaw REAL NOT NULL,     -- degrees
    vx REAL NOT NULL, vy REAL NOT NULL, vz REAL NOT NULL,  -- velocity, units/s
    weapon TEXT NOT NULL,         -- active weapon class; '' until first seen
    health INTEGER NOT NULL,
    team INTEGER NOT NULL,        -- 0 none, 1 spectator, 2 Jinrai, 3 NSF
    class INTEGER NOT NULL,       -- m_iNeoClass: 0 recon, 1 assault, 2 support, 3 VIP
    camo INTEGER NOT NULL,        -- 1 while thermoptic camo is active
    alive INTEGER NOT NULL,
    in_pvs INTEGER NOT NULL       -- 0 = player just left the recorder's PVS
);

-- Ghost entity position (on-change). Reliable while the ghost is dropped;
-- while carried, use the carrier's player_samples rows (weapon = 'ghost').
CREATE TABLE IF NOT EXISTS ghost_samples (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    entity_id INTEGER NOT NULL,   -- the ghost entity, not a player
    x REAL NOT NULL, y REAL NOT NULL, z REAL NOT NULL
);

-- Per-player scoreboard state from the player resource entity (on-change).
CREATE TABLE IF NOT EXISTS player_resource (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    entity_id INTEGER NOT NULL,   -- player slot; joins players.entity_id
    xp INTEGER NOT NULL,          -- NT;RE XP (the scoreboard rank number)
    score INTEGER NOT NULL,
    deaths INTEGER NOT NULL,
    ping INTEGER NOT NULL
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
-- (attack = fired, zoom = held aim-down-sights, aim = ADS-toggle keypress).
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
    zoom       INTEGER GENERATED ALWAYS AS ((buttons >> 19) & 1) VIRTUAL, -- held ADS
    aim        INTEGER GENERATED ALWAYS AS ((buttons >> 27) & 1) VIRTUAL, -- ADS-toggle keypress

    lean_left  INTEGER GENERATED ALWAYS AS ((buttons >> 28) & 1) VIRTUAL,
    lean_right INTEGER GENERATED ALWAYS AS ((buttons >> 29) & 1) VIRTUAL,
    thermoptic INTEGER GENERATED ALWAYS AS ((buttons >> 30) & 1) VIRTUAL,
    vision     INTEGER GENERATED ALWAYS AS ((buttons >> 31) & 1) VIRTUAL
);

---------------------------------------------------------------- event log

-- Kill feed from the player_death game event (NT;RE definition).
CREATE TABLE IF NOT EXISTS kills (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    victim_userid INTEGER NOT NULL,   -- joins players.userid
    victim_name TEXT,                 -- resolved at parse time; NULL if unknown
    attacker_userid INTEGER NOT NULL, -- 0 = world / environment
    attacker_name TEXT,
    assists INTEGER NOT NULL,
    weapon TEXT NOT NULL,
    headshot INTEGER NOT NULL,
    suicide INTEGER NOT NULL,
    explosive INTEGER NOT NULL,
    ghoster INTEGER NOT NULL          -- victim was carrying the ghost
);

-- Hit log from the per-attacker damage accumulator
-- (m_rfAttackersAccumlator). Each row = the attacker landed damage on the
-- victim at this tick. accumulator is the fractional damage carry (< 1),
-- not an amount; join the victim's health drop in player_samples at the
-- same tick for the amount. SourceTV demos only.
CREATE TABLE IF NOT EXISTS attacker_hits (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    victim_entity_id INTEGER NOT NULL,   -- joins players.entity_id
    attacker_entity_id INTEGER NOT NULL, -- joins players.entity_id
    accumulator REAL NOT NULL            -- fractional carry; 0 = respawn reset
);

-- In-game location pings (player_ping game events).
CREATE TABLE IF NOT EXISTS player_pings (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    userid INTEGER NOT NULL,      -- pinging player; joins players.userid
    team INTEGER NOT NULL,        -- pinging player's team
    x INTEGER NOT NULL, y INTEGER NOT NULL, z INTEGER NOT NULL,  -- pinged position
    ghoster_ping INTEGER NOT NULL -- the event's ghosterping flag
);

-- Automatic enemy-position callouts while the ghost is held
-- (ghost_enemy_callout game events).
CREATE TABLE IF NOT EXISTS ghost_callouts (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    userid INTEGER NOT NULL,        -- ghost carrier; joins players.userid
    team INTEGER NOT NULL,          -- carrier's team
    target_userid INTEGER NOT NULL, -- spotted enemy; joins players.userid
    x INTEGER NOT NULL, y INTEGER NOT NULL, z INTEGER NOT NULL  -- spotted position
);

-- Cumulative team score updates (team_score game events).
CREATE TABLE IF NOT EXISTS team_scores (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    team INTEGER NOT NULL,   -- 2 Jinrai, 3 NSF
    score INTEGER NOT NULL   -- team's score as of this tick
);

-- Team joins and switches (player_team game events).
CREATE TABLE IF NOT EXISTS team_changes (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    userid INTEGER NOT NULL,     -- joins players.userid
    team INTEGER NOT NULL,       -- new team
    old_team INTEGER NOT NULL,
    disconnect INTEGER NOT NULL  -- 1 when the change is a disconnect
);

-- Rank progression (player_rankchange game events).
CREATE TABLE IF NOT EXISTS rank_changes (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    userid INTEGER NOT NULL,  -- joins players.userid
    old_rank INTEGER NOT NULL,
    new_rank INTEGER NOT NULL
);

-- Round starts as wire facts (round_start game events); the rounds table
-- is derived from announcements instead.
CREATE TABLE IF NOT EXISTS round_starts (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    objective TEXT NOT NULL,     -- unreliable: DEATHMATCH even in CTG games
    timelimit INTEGER NOT NULL,
    fraglimit INTEGER NOT NULL
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

-- Console commands issued by the recorder during playback.
CREATE TABLE IF NOT EXISTS console_cmds (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    cmd TEXT NOT NULL
);

-- Every game event, fields as JSON (queryable via SQLite's json functions).
CREATE TABLE IF NOT EXISTS game_events (
    id INTEGER PRIMARY KEY,
    demo_id INTEGER NOT NULL REFERENCES demos(id),
    tick INTEGER NOT NULL,
    name TEXT NOT NULL,
    fields TEXT NOT NULL
);

------------------------------------------------------------------ derived

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

CREATE INDEX IF NOT EXISTS idx_player_samples_demo_tick ON player_samples(demo_id, tick);
CREATE INDEX IF NOT EXISTS idx_player_samples_demo_entity ON player_samples(demo_id, entity_id, tick);
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

    /// Abandon the current transaction (fails harmlessly if none is open).
    pub fn rollback(&self) -> Result<()> {
        Ok(self.conn.execute_batch("ROLLBACK")?)
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

    pub fn insert_player_pings(
        &self,
        demo_id: i64,
        pings: &[crate::extract::net::Ping],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO player_pings (demo_id, tick, userid, team, x, y, z, ghoster_ping)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for p in pings {
            ins.execute(rusqlite::params![
                demo_id, p.tick, p.userid, p.team, p.x, p.y, p.z, p.ghoster_ping,
            ])?;
        }
        Ok(())
    }

    pub fn insert_ghost_callouts(
        &self,
        demo_id: i64,
        callouts: &[crate::extract::net::GhostCallout],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO ghost_callouts (demo_id, tick, userid, team, target_userid, x, y, z)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for c in callouts {
            ins.execute(rusqlite::params![
                demo_id, c.tick, c.userid, c.team, c.target_userid, c.x, c.y, c.z,
            ])?;
        }
        Ok(())
    }

    pub fn insert_team_scores(
        &self,
        demo_id: i64,
        scores: &[crate::extract::net::TeamScore],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO team_scores (demo_id, tick, team, score) VALUES (?1, ?2, ?3, ?4)",
        )?;
        for s in scores {
            ins.execute(rusqlite::params![demo_id, s.tick, s.team, s.score])?;
        }
        Ok(())
    }

    pub fn insert_team_changes(
        &self,
        demo_id: i64,
        changes: &[crate::extract::net::TeamChange],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO team_changes (demo_id, tick, userid, team, old_team, disconnect)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for c in changes {
            ins.execute(rusqlite::params![
                demo_id, c.tick, c.userid, c.team, c.old_team, c.disconnect,
            ])?;
        }
        Ok(())
    }

    pub fn insert_rank_changes(
        &self,
        demo_id: i64,
        changes: &[crate::extract::net::RankChange],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO rank_changes (demo_id, tick, userid, old_rank, new_rank)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for c in changes {
            ins.execute(rusqlite::params![demo_id, c.tick, c.userid, c.old_rank, c.new_rank])?;
        }
        Ok(())
    }

    pub fn insert_round_starts(
        &self,
        demo_id: i64,
        starts: &[crate::extract::net::RoundStart],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO round_starts (demo_id, tick, objective, timelimit, fraglimit)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for s in starts {
            ins.execute(rusqlite::params![demo_id, s.tick, s.objective, s.timelimit, s.fraglimit])?;
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

    pub fn insert_player_samples(
        &self,
        demo_id: i64,
        samples: &[crate::extract::entities::PlayerSample],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO player_samples (demo_id, tick, entity_id, x, y, z, eye_pitch, eye_yaw,
                                         vx, vy, vz, weapon, health, team, class, camo,
                                         alive, in_pvs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18)",
        )?;
        for s in samples {
            ins.execute(rusqlite::params![
                demo_id, s.tick, s.entity_id, s.x, s.y, s.z, s.eye_pitch, s.eye_yaw,
                s.vx, s.vy, s.vz, s.weapon, s.health, s.team, s.class_num, s.camo,
                s.alive, s.in_pvs,
            ])?;
        }
        Ok(())
    }

    pub fn insert_ghost_samples(
        &self,
        demo_id: i64,
        samples: &[crate::extract::entities::GhostSample],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO ghost_samples (demo_id, tick, entity_id, x, y, z)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for s in samples {
            ins.execute(rusqlite::params![demo_id, s.tick, s.entity_id, s.x, s.y, s.z])?;
        }
        Ok(())
    }

    pub fn insert_player_resource(
        &self,
        demo_id: i64,
        samples: &[crate::extract::entities::ResourceSample],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO player_resource (demo_id, tick, entity_id, xp, score, deaths, ping)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for s in samples {
            ins.execute(rusqlite::params![
                demo_id, s.tick, s.entity_id, s.xp, s.score, s.deaths, s.ping,
            ])?;
        }
        Ok(())
    }

    pub fn insert_attacker_hits(
        &self,
        demo_id: i64,
        samples: &[crate::extract::entities::DamageSample],
    ) -> Result<()> {
        let mut ins = self.conn.prepare(
            "INSERT INTO attacker_hits (demo_id, tick, victim_entity_id, attacker_entity_id,
                                        accumulator)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for s in samples {
            ins.execute(rusqlite::params![
                demo_id, s.tick, s.victim_entity_id, s.attacker_entity_id, s.damage,
            ])?;
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

#[cfg(test)]
mod tests {
    use super::SCHEMA;
    use rusqlite::Connection;

    /// SCHEMA.md must mention (in backticks) every table and every column,
    /// generated columns included, so schema changes can't silently outrun
    /// the documentation.
    #[test]
    fn schema_md_documents_every_table_and_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let doc = include_str!("../../SCHEMA.md");

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(!tables.is_empty());

        for table in &tables {
            assert!(
                doc.contains(&format!("`{table}`")),
                "SCHEMA.md does not document table `{table}`"
            );
            let columns: Vec<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_xinfo('{table}')"))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            for column in &columns {
                assert!(
                    doc.contains(&format!("`{column}`")),
                    "SCHEMA.md does not document column `{column}` of table `{table}`"
                );
            }
        }
    }
}
