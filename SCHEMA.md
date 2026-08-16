# Database schema

One SQLite database holds any number of demos. Every table except `demos`
carries a `demo_id` column referencing `demos.id` — filter on it to query a
single demo, or join across it for multi-demo analysis. The authoritative DDL
lives in `src/output/sqlite.rs` (`SCHEMA`); this file explains the semantics
the SQL can't. A unit test asserts every table and column is mentioned here,
so if you add a column, document it.

## Shared conventions

- **Ticks and time.** All `tick` columns are server ticks. Convert to seconds
  with `tick / demos.tickrate` (NT;RE runs at ~66.67 ticks/s). Tick 0 is the
  start of the recording, not the start of the match.
- **Two player identities.** A player has a `userid` (a server session id —
  used by `kills` and most game events) and an `entity_id` (their slot in the
  entity list — used by `player_samples` and `chat.client_entity`). The
  `players` table holds both, so it's the join hub:
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
| `tickrate` | `playback_ticks / playback_seconds`; use for tick→time conversion |

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
| `victim_name`, `attacker_name` | resolved at parse time for convenience (NULL if unknown) |
| `assists` | assist count reported by the mod |
| `weapon` | weapon string from the event, e.g. `weapon_srm` |
| `headshot`, `suicide`, `explosive` | kill flags |
| `ghoster` | 1 if the victim was carrying the ghost |

### `rounds`

Derived from start/win announcements (not a wire-format fact — heuristic).

| column | meaning |
|---|---|
| `round_number` | 1-based; NULL if the first start marker was missed |
| `start_tick`, `end_tick` | NULL when the demo started mid-round or ended before the round did |
| `winner` | e.g. `Jinrai`, `NSF`; NULL for an unfinished final round |
| `win_reason` | text from the winning announcement |

### `player_samples`

All-player state over time, decoded from delta-compressed entity updates —
the heatmap table. Rows are written **on change** (~66/s while a player is
moving), so carry values forward between rows when resampling.

**PVS caveat:** a POV demo only contains entities the recorder's client was
sent — players well out of sight produce no rows. SourceTV demos contain
everyone at all times.

| column | meaning |
|---|---|
| `tick` | sample time |
| `entity_id` | joins `players.entity_id` |
| `x`, `y`, `z` | world position (player origin, at the feet) |
| `eye_pitch`, `eye_yaw` | aim direction, degrees |
| `weapon` | active weapon class, prefix-stripped (empty until a weapon is seen) |
| `health` | current HP |
| `team` | 0 unassigned, 1 spectator, 2 Jinrai, 3 NSF |
| `alive` | 1 while alive (engine life state 0) |
| `in_pvs` | 0 marks the player leaving the recorder's PVS — last known state, position stale after it |

### `pov_samples`

The recorder's own view, one row per packet frame (~66/s, unconditionally —
denser and simpler than `player_samples` for the recording player). Thin with
`--pov-sample N`.

| column | meaning |
|---|---|
| `tick` | sample time |
| `x`, `y`, `z` | view position (eye level, unlike `player_samples`) |
| `pitch`, `yaw`, `roll` | view angles, degrees |

### `recorder_inputs`

The recorder's raw input per tick from `dem_usercmd` frames — what they
*pressed*, vs. `pov_samples` which is where they *were*.

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
fire, 11), `reload` (13), `sprint` (17), and NT;RE's `aim` (aim-down-sights,
27), `lean_left` (28), `lean_right` (29), `thermoptic` (30), `vision` (31).

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
event definitions — NT;RE-specific events included (`ghost_capture`,
`player_rankchange`, `vip_death`, …). This is the escape hatch: anything not
promoted to its own table is queryable here.

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

`player_samples.weapon` is the server weapon class with its `CWeapon`/`C`
prefix stripped (e.g. `CWeaponSRM` → `SRM`); `kills.weapon` is the string
NT;RE transmits in the `player_death` event — conventionally the entity name
without the `weapon_` prefix, but the exact spelling is unvalidated against a
real demo yet. The authoritative lists live in the NT;RE repo:
[weapon classes](https://github.com/NeotokyoRebuild/neo/tree/master/src/game/shared/neo/weapons)
and [weapon scripts](https://github.com/NeotokyoRebuild/neo/tree/master/game/neo/scripts)
(entity names, HUD names, damage stats).

Weapons as of Aug 2026 (entity name → gameplay role):

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

Values are mod data, not wire format — new NT;RE releases can add weapons
without breaking this parser; unknown names simply appear as-is. Roles for
the NT;RE-only additions come from script stats, not official descriptions.

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
