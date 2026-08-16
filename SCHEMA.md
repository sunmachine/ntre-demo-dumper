# Database schema

One SQLite database holds any number of demos. Every table except `demos`
carries a `demo_id` column referencing `demos.id`. Filter on it to query a
single demo, or join across it for multi-demo analysis. The authoritative DDL
lives in `src/output/sqlite.rs` (`SCHEMA`); this file explains the semantics
the SQL can't. A unit test asserts every table and column is mentioned here,
so if you add a column, document it.

## Shared conventions

- **Ticks and time.** All `tick` columns are server ticks. Convert to seconds
  with `tick / demos.tickrate` (NT;RE runs at ~66.67 ticks/s). Tick 0 is the
  start of the recording, not the start of the match.
- **Two player identities.** A `userid` is a server session id, used by
  `kills` and most game events. An `entity_id` is the player's slot in the
  entity list, used by `player_samples` and `chat.client_entity`. The
  `players` table holds both and is the join hub:
  `kills.victim_userid = players.userid`,
  `player_samples.entity_id = players.entity_id`.
- **Coordinates.** Source engine world units (16 units ≈ 1 foot), map-specific
  origin: `x`/`y` horizontal, `z` up. Angles are degrees: `yaw` 0–360
  counter-clockwise around `z`, `pitch` negative looking up / positive looking
  down, per engine convention.
- **Booleans** are stored as SQLite `INTEGER` 0/1.

## Tables

### `demos`

One row per parsed demo file; every other table hangs off `id`.

| column | meaning |
|---|---|
| `id` | primary key, referenced by every `demo_id` |
| `path` | demo file path as given on the command line |
| `parsed_at` | UTC timestamp of the parse run |
| `demo_protocol`, `network_protocol` | from the header; NT;RE is 3 / 24 |
| `server` | server name or IP the demo was recorded on |
| `client` | name of the recording player |
| `map`, `game_directory` | e.g. `nt_saitama_ctg`, `neo` |
| `playback_seconds`, `playback_ticks`, `playback_frames` | recording length |
| `tickrate` | `playback_ticks / playback_seconds`; use for tick-to-time conversion |

### `players`

Roster: one row per player seen in the demo, from the string-table dump at
recording start plus `player_connect` events for late joiners. Renames keep
the latest name.

| column | meaning |
|---|---|
| `entity_id` | entity slot; joins `player_samples.entity_id` and `chat.client_entity` |
| `userid` | server session id; joins `kills.*_userid` and `userid` fields in `game_events` |
| `name` | player name (latest, if they renamed) |
| `steamid` | e.g. `[U:1:12345678]`, or `BOT` |
| `is_bot` | 1 for server bots |
| `first_seen_tick` | when the player first appeared |

### `kills`

Kill feed from NT;RE's own `player_death` game event definition.

| column | meaning |
|---|---|
| `tick` | when the kill happened |
| `victim_userid`, `attacker_userid` | join `players.userid`; attacker 0 = world/environment |
| `victim_name`, `attacker_name` | resolved at parse time; NULL if unknown |
| `assists` | assist count reported by the mod |
| `weapon` | weapon string from the event, e.g. `weapon_srm` |
| `headshot`, `suicide`, `explosive` | kill flags |
| `ghoster` | 1 if the victim was carrying the ghost |

### `rounds`

Derived heuristically from start/win announcements. `round_starts` holds the
wire-fact start events; this table adds winners and end ticks.

| column | meaning |
|---|---|
| `round_number` | 1-based; NULL if the first start marker was missed |
| `start_tick`, `end_tick` | NULL when the demo started mid-round or ended before the round did |
| `winner` | e.g. `Jinrai`, `NSF`; NULL for an unfinished final round |
| `win_reason` | text from the winning announcement |

### `player_samples`

All-player state over time, decoded from delta-compressed entity updates;
this is the heatmap table. Rows are written **on change** (~66/s while a
player is moving), so carry values forward between rows when resampling.

**PVS caveat:** a POV demo only contains entities the recorder's client was
sent, so players well out of sight produce no rows. SourceTV demos contain
everyone at all times.

| column | meaning |
|---|---|
| `tick` | sample time |
| `entity_id` | joins `players.entity_id` |
| `x`, `y`, `z` | world position (player origin, at the feet) |
| `eye_pitch`, `eye_yaw` | aim direction, degrees |
| `vx`, `vy`, `vz` | velocity, world units/s |
| `weapon` | active weapon class, prefix-stripped (empty until a weapon is seen) |
| `health` | current HP; NT;RE class maxima are 100 Recon/VIP, 120 Assault, 225 Support (`neo_player_shared.h`). Negative while dead = overkill damage; spectator entities sit at 1 |
| `team` | 0 unassigned, 1 spectator, 2 Jinrai, 3 NSF |
| `class` | NT;RE class: 0 recon, 1 assault, 2 support, 3 VIP |
| `camo` | 1 while thermoptic camo is active |
| `alive` | 1 while alive (engine life state 0) |
| `in_pvs` | 0 marks the player leaving the recorder's PVS; the row holds their last known state |

### `pov_samples`

The recorder's own view, one row per packet frame (~66/s, unconditionally).
Denser and simpler than `player_samples` for the recording player. Thin with
`--pov-sample N`.

| column | meaning |
|---|---|
| `tick` | sample time |
| `x`, `y`, `z` | view position (eye level, unlike `player_samples`) |
| `pitch`, `yaw`, `roll` | view angles, degrees |

### `recorder_inputs`

The recorder's raw input per tick, from `dem_usercmd` frames. This table
records what the recorder pressed; `pov_samples` records where they were.

| column | meaning |
|---|---|
| `tick` | input time |
| `buttons` | raw 32-bit button field |
| `impulse` | impulse command (0 = none) |
| `weaponselect` | entity index of a requested weapon switch; NULL = no switch |
| `pitch`, `yaw`, `roll` | view angles submitted with the command |
| `forwardmove`, `sidemove`, `upmove` | movement analog axes (units/s) |
| `mousedx`, `mousedy` | raw mouse deltas |

Generated convenience columns decode `buttons` (query them like real
columns): `attack` (fired, bit 0), `jump` (1), `duck` (2), `attack2` (alt
fire, 11), `reload` (13), `sprint` (17), `zoom` (19), and NT;RE's `aim` (27),
`lean_left` (28), `lean_right` (29), `thermoptic` (30), `vision` (31).

Aiming caveat: use `zoom` for "was the player aiming"; it is the held
aim-down-sights state. `aim` is NT;RE's ADS-toggle keybind, set only on the
tick the key is tapped, and stays 0 for players on the default `+zoom` bind.

### `player_pings`

In-game location pings, from `player_ping` game events. All players' pings
are present, not just the recorder's.

| column | meaning |
|---|---|
| `tick` | when the ping was placed |
| `userid` | pinging player; joins `players.userid` |
| `team` | pinging player's team, coded as in `player_samples.team` |
| `x`, `y`, `z` | pinged world position |
| `ghoster_ping` | the event's `ghosterping` flag |

### `ghost_callouts`

Automatic enemy-position callouts generated while a player carries the
ghost, from `ghost_enemy_callout` game events. A log of the enemy intel the
carrier's team received.

| column | meaning |
|---|---|
| `tick` | when the callout fired |
| `userid` | ghost carrier; joins `players.userid` |
| `team` | carrier's team, coded as in `player_samples.team` |
| `target_userid` | spotted enemy; joins `players.userid` |
| `x`, `y`, `z` | spotted enemy's world position |

### `team_scores`

Cumulative team score updates, from `team_score` game events. One row per
update; the latest row at or before a tick is the score at that tick.
Columns: `tick`, `team` (2 Jinrai, 3 NSF), `score`.

### `team_changes`

Team joins and switches, from `player_team` game events.

| column | meaning |
|---|---|
| `tick` | when the change happened |
| `userid` | joins `players.userid` |
| `team`, `old_team` | new and previous team, coded as in `player_samples.team` |
| `disconnect` | 1 when the change is a player disconnecting |

### `rank_changes`

Rank progression, from `player_rankchange` game events. Columns: `tick`,
`userid` (joins `players.userid`), `old_rank`, `new_rank` (rank index,
increasing with XP).

### `round_starts`

Round starts as wire facts, from `round_start` game events. Unlike `rounds`,
these carry no winner. Columns: `tick`, `objective`, `timelimit`,
`fraglimit`. Caveat: observed demos report `objective` as `DEATHMATCH` even
in capture-the-ghost games, so treat it with suspicion.

### `chat`

SayText2 user messages, control codes stripped.

| column | meaning |
|---|---|
| `tick` | when the line was sent |
| `client_entity` | sender's entity slot; joins `players.entity_id` |
| `from_name` | sender name as transmitted (may be empty for server messages) |
| `text` | the message |
| `team_chat` | 1 for team-only chat |

### `game_events`

Every game event in the demo, decoded generically against the demo's own
event definitions, so NT;RE-specific events are included (`ghost_capture`,
`player_rankchange`, `vip_death`, and others). This is the escape hatch:
anything not promoted to its own table is queryable here.

| column | meaning |
|---|---|
| `tick` | event time |
| `name` | event name, e.g. `player_death`, `ghost_capture` |
| `fields` | event fields as a JSON object |

Query fields with SQLite's JSON functions:

```sql
SELECT tick, json_extract(fields, '$.userid') AS userid
FROM game_events WHERE demo_id = 1 AND name = 'ghost_capture';

SELECT DISTINCT name FROM game_events WHERE demo_id = 1;  -- what's in this demo?
```

### `announcements`

Center-screen text recovered by the ASCII skim (round starts, winners, ghost
captures). `rounds` is derived from these. `seconds` is precomputed
`tick / tickrate`. Columns: `tick`, `seconds`, `text`.

### `console_cmds`

Console commands issued by the recorder during the recording. Columns:
`tick`, `cmd`.

## Weapon reference

`player_samples.weapon` is the server weapon class, lowercased with its
`CWeapon`/`C` prefix stripped: `CWeaponSRM` becomes `srm`, silenced and
scoped variants keep their suffix (`mpn_s`, `jittes`, `m41s`).
`kills.weapon` is the string from the `player_death` event: the weapon
entity name without its `weapon_` prefix (`srm`, `zr68c`, `jittescoped`).
Grenade kills report the projectile class instead (`neo_grenade_frag`).
The authoritative lists live in the NT;RE repo:
[weapon classes](https://github.com/NeotokyoRebuild/neo/tree/master/src/game/shared/neo/weapons)
and [weapon scripts](https://github.com/NeotokyoRebuild/neo/tree/master/game/neo/scripts)
(entity names, HUD names, damage stats).

Weapons as of August 2026, by entity name:

| entity | role |
|---|---|
| `weapon_milso`, `weapon_kyla`, `weapon_tachi` | pistols |
| `weapon_mpn`, `weapon_mpn_unsilenced` | SMG (+ unsilenced variant) |
| `weapon_jitte`, `weapon_jittescoped` | machine pistol (+ scoped) |
| `weapon_zr68c`, `weapon_zr68l`, `weapon_zr68s` | ZR68 rifle family (compact / scoped / silenced) |
| `weapon_srm`, `weapon_srm_s` | SRM rifle (+ silenced) |
| `weapon_mx`, `weapon_mx_silenced` | MX rifle (+ silenced) |
| `weapon_m41`, `weapon_m41s`, `weapon_m41l` | M41 rifle (+ silenced / scoped) |
| `weapon_balc` | BALC3 burst rifle (NT;RE addition) |
| `weapon_supa7`, `weapon_aa13` | shotguns (pump / auto) |
| `weapon_pz`, `weapon_pbk56s` | machine guns |
| `weapon_srs` | bolt sniper rifle |
| `weapon_knife` | melee |
| `weapon_grenade`, `weapon_smokegrenade` | frag / smoke grenades |
| `weapon_remotedet`, `weapon_proxmine`, `weapon_smac` | detpack, proximity mine, SMAC (NT;RE) |
| `weapon_ghost` | the ghost (objective) |

The set is open: names added by future NT;RE releases appear as-is.

## Example: heatmap query

Kill positions for one player on one demo, by joining the kill feed to the
victim's last known position:

```sql
SELECT k.tick, ps.x, ps.y
FROM kills k
JOIN players v   ON v.demo_id = k.demo_id AND v.userid = k.victim_userid
JOIN player_samples ps
  ON ps.demo_id = k.demo_id AND ps.entity_id = v.entity_id
  AND ps.tick = (SELECT MAX(tick) FROM player_samples
                 WHERE demo_id = k.demo_id AND entity_id = v.entity_id
                   AND tick <= k.tick)
WHERE k.demo_id = 1;
```
