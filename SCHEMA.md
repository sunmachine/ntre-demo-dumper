# Database schema

One SQLite database holds any number of demos. Every table except `demos`
carries a `demo_id` column referencing `demos.id`. Filter on it to query a
single demo, or join across it for multi-demo analysis. The authoritative DDL
lives in `src/output/sqlite.rs` (`SCHEMA`); this file explains the semantics
the SQL can't. A unit test asserts every table and column is mentioned here,
so if you add a column, document it.

## Shared conventions

- **Each demo once.** A demo already in the database is skipped, even under
  another name.
- **Ticks and time.** All `tick` columns are server ticks. Convert to seconds
  with `tick / demos.tickrate` (NT;RE runs at ~66.67 ticks/s). Tick 0 is the
  start of the recording, not the start of the match. A POV recording's
  frames carry the player's client clock, which drifts from the server's,
  so the dumper times every frame from the server tick its snapshot carries
  instead. One entity therefore has at most one sample per tick in both
  recording types. `recorder_inputs` can still hold several commands on
  one tick, as the recording stores them.
- **Three player identities.** A `steamid` names a person and is the same
  in every demo, so it is the key to join event tables on, within a demo
  or across demos. A `userid` names one connection to one server. The
  server counts connections upward from its start (upstream at `bea96408`,
  `src/public/cdll_int.h:95`), so a player keeps their userid across map
  changes, gets a new one on rejoining, and has an unrelated one on another
  server or night. An `entity_id` is the player's slot in the entity list,
  used by `player_samples` and `chat.client_entity`. The `players` table
  holds all three and is the join hub:
  `deaths.victim_steamid = players.steamid`,
  `player_samples.entity_id = players.entity_id`.
- **A Steam ID on every player row.** Every table that names a player
  carries their `steamid`, so no query needs the hub just to name one.
  A slot passes to the next player who joins after its holder leaves, so a
  table that names players by `entity_id` stores the Steam ID of the
  connection holding the slot at the row's tick: the latest roster player
  on that slot whose `first_seen_tick` is at or before it. It is NULL for
  a slot no roster player held by then.
- **When `userid` is still needed.** Every bot's `steamid` is the literal
  `BOT`, so only `userid` tells two bots apart. The `userid` fields inside
  `game_events` are the wire values and join `players.userid`. A player
  who rejoins can hold a different entity slot per connection, so reaching
  `entity_id` from an event row also goes through `userid`. Event tables
  keep their `userid` columns for these uses.
- **Coordinates.** Source engine world units (16 units ≈ 1 foot), map-specific
  origin: `x`/`y` horizontal, `z` up. Angles are degrees: `yaw` 0–360
  counter-clockwise around `z`, `pitch` negative looking up / positive looking
  down, per engine convention.
- **Booleans** are stored as SQLite `INTEGER` 0/1.
- **Teams.** Teams are numbered as the game numbers them: 0 none, 1
  spectator, 2 Jinrai, 3 NSF. Round outcomes use `jinrai`, `nsf` or `tie`.
- **Tags.** Tables that depend on the demo type carry a tag line:
  - **POV only**: recorded only in first-person demos; empty for SourceTV
    (HLTV) recordings. SourceTV relays only a short list of standard game
    events, and NT;RE's own events are not on it.
  - **SourceTV only**: the server sends this data only to SourceTV; empty
    for POV recordings.
  - **PVS-limited in POV**: entity-stream data; in POV demos it covers only
    what the recorder's client was sent, while SourceTV demos cover
    everything.

  Untagged event-derived tables work in both demo types but can be empty
  for demos recorded on NT;RE versions predating the event.
- **Demos cut at a round.** `--last-round N` keeps rounds 1 to N of each
  demo. Every row from the start of the next round on is left out, so
  round N keeps its end and the time after it. Rows from before round 1,
  such as warmup, stay, because the tick series carry their state forward
  into the kept rounds. `demos.last_round` records the N. A demo parsed
  this way counts as parsed, so to get the whole demo later, parse it into
  another database file.
- **Schema version.** A database records the schema version that wrote it,
  and the dumper refuses to add demos to a database written by another
  version, so parse into a new file after upgrading.

## Table index

Five groups: **reference** (who and what), **tick series** (state over
time), **event log** (discrete happenings), **derived** (deterministic
reconstructions from recovered text), and **inferred** (probabilistic
guesses). Inferred tables are named `inferred_*` and every row carries a
`confidence` from 0 to 1 and a `method` naming the rule that produced it;
rows are recomputed on every parse and may change when a rule improves.

| table | group | tags | player join |
|---|---|---|---|
| `demos` | reference | | `id` = every table's `demo_id` |
| `players` | reference | | the join hub |
| `server_cvars` | reference | | none |
| `player_samples` | tick series | PVS-limited in POV | `steamid` |
| `ghost_samples` | tick series | PVS-limited in POV | none (ghost entity) |
| `player_resource` | tick series | | `steamid` |
| `alive_counts` | tick series | | none (per team) |
| `pov_samples` | tick series | | none (the recorder) |
| `recorder_inputs` | tick series | POV only | none (the recorder) |
| `deaths` | event log | | `steamid` |
| `attacker_hits` | event log | SourceTV only, pre-July-2026 builds | `steamid` |
| `player_pings` | event log | POV only | `steamid` |
| `ghost_callouts` | event log | POV only | `steamid`, `userid` (bots only) |
| `ghost_holds` | event log | | `steamid` |
| `team_scores` | event log | POV only | none (per team) |
| `team_changes` | event log | | `steamid` |
| `rank_changes` | event log | POV only | `steamid` |
| `round_starts` | event log | | none |
| `round_results` | event log | | none |
| `chat` | event log | | `steamid` |
| `console_cmds` | event log | POV only | none (the recorder) |
| `game_events` | event log | | varies (JSON fields) |
| `announcements` | derived | | none |
| `rounds` | derived | | `steamid` |
| `inferred_hits` | inferred | | `steamid` |

## Reference tables

### `demos`

One row per parsed demo file; every other table hangs off `id`.

| column | meaning |
|---|---|
| `id` | the first 44 bits of `sha256`, so the same file gets the same id in every database |
| `sha256` | the file's SHA-256, as `sha256sum` prints it |
| `path` | demo file path as given on the command line |
| `parsed_at` | UTC timestamp of the parse run |
| `demo_protocol`, `network_protocol` | from the header; NT;RE is 3 / 24 |
| `server` | server name or IP the demo was recorded on |
| `client` | name of the recording player |
| `map`, `game_directory` | e.g. `nt_saitama_ctg`, `neo` |
| `playback_seconds`, `playback_ticks`, `playback_frames` | recording length |
| `tickrate` | `playback_ticks / playback_seconds`; use for tick-to-time conversion |
| `sourcetv` | 1 for a SourceTV recording, 0 for a recording made by a player |
| `dedicated` | 1 when recorded on a dedicated server |
| `server_os` | `linux` or `windows` |
| `host_name` | the name the server sent at connect. For SourceTV this is SourceTV's own name, and `server` holds the hostname |
| `max_clients` | player slots on the server |
| `tick_interval` | seconds per server tick, e.g. 0.015 |
| `map_md5` | MD5 of the map file, which tells versions of one map apart |
| `recorder_entity_id` | the recording client's entity; joins `players.entity_id` |
| `recorder_steamid` | the recording player's Steam ID at the start of the recording; joins `players.steamid`. NULL for a SourceTV recording, whose recorder is the SourceTV client, which the roster leaves out |
| `parser_version` | the dumper version that wrote this demo's rows |
| `last_round` | the `--last-round` this demo was parsed with, so its rows end where the next round began; NULL when the whole demo was parsed |

### `players`

Roster: one row per connection seen in the demo. Players present when
recording began come from the `userinfo` string-table dump. Late joiners
come from later updates to that table, or from a `player_connect_client`
event in a POV recording. A player who leaves and rejoins gets a new
`userid` and so a second row with the same `steamid`. Renames keep the
latest name.

A player has two names. `name` is the Steam name from the userinfo string
table. `neo_name` is the in-game name the player sets with the `neo_name`
setting, which other players see instead of the Steam name unless they turn
that off. It comes from the player entity's `m_szNeoName` property, so in a
POV recording it is NULL for a player who never came within the recorder's
view.

| column | meaning |
|---|---|
| `entity_id` | entity slot; joins `player_samples.entity_id` and `chat.client_entity` |
| `userid` | connection id, see Shared conventions; unique within a demo, which an index enforces; joins the `userid` columns of event tables and the `userid` fields in `game_events` |
| `name` | Steam name, the latest if they renamed |
| `neo_name` | in-game name, the last one set while recorded; NULL if the player set none |
| `steamid` | e.g. `[U:1:12345678]`, or `BOT` for every bot; joins the `steamid` columns of event tables |
| `is_bot` | 1 for server bots |
| `first_seen_tick` | when the player first appeared: 0 for players present when the recording began, the tick the server announced them for late joiners |

### `server_cvars`

The server settings that differ from the game's defaults. A setting that
is absent ran at its default. Match rules such as `sv_neo_ctg_round_limit`
and `sv_neo_comp_name` are here. The game's build version is not recorded
in demos.

| column | meaning |
|---|---|
| `tick` | 0 for settings in force when recording began, otherwise when one changed |
| `name` | setting name |
| `value` | setting value, as text |

## Tick series

State over time. Rows are written on change unless noted, so carry values
forward between rows when resampling.

### `player_samples`

**Tags: PVS-limited in POV**

All-player state over time, decoded from delta-compressed entity updates;
this is the heatmap table. Rows are written **on change** (~66/s while a
player is moving), so carry values forward between rows when resampling.
In POV demos, players well out of the recorder's sight produce no rows.

| column | meaning |
|---|---|
| `tick` | sample time |
| `entity_id` | joins `players.entity_id` |
| `steamid` | the player holding the slot at `tick`; joins `players.steamid` |
| `x`, `y`, `z` | world position (player origin, at the feet) |
| `eye_pitch`, `eye_yaw` | aim direction, degrees |
| `vx`, `vy`, `vz` | velocity, world units/s |
| `weapon` | weapon's entity name without `weapon_`; see Weapon reference below. NULL until a weapon is seen, or while the active class isn't a weapon |
| `health` | current HP; NT;RE class maxima are 100 Recon/VIP, 120 Assault, 225 Support (`neo_player_shared.h`). Negative while dead = overkill damage; spectator entities sit at 1 |
| `team` | 0 unassigned, 1 spectator, 2 Jinrai, 3 NSF |
| `class` | NT;RE class: 0 recon, 1 assault, 2 support, 3 VIP |
| `camo` | 1 while thermoptic camo is active |
| `alive` | 1 while alive (engine life state 0) |
| `in_pvs` | 0 marks the player leaving the recorder's PVS; the row holds their last known state |

### `ghost_samples`

**Tags: PVS-limited in POV**

The ghost entity's own position over time (on-change). Reliable while the
ghost is dropped or in the world; while a player carries it, track the
carrier instead: `ghost_holds` names who held it, and their
`player_samples` rows give the position. The carrier can holster the
ghost, so `weapon = 'ghost'` misses part of a hold. The `entity_id` is the
ghost entity, not a player. Columns: `tick`, `entity_id`,
`x`, `y`, `z`.

### `player_resource`

Per-player scoreboard state from the player resource entity, on change.
Slower-moving than `player_samples` and available for every player at all
times, PVS included.

| column | meaning |
|---|---|
| `tick` | sample time |
| `entity_id` | player slot; joins `players.entity_id` |
| `steamid` | the player holding the slot at `tick`; joins `players.steamid` |
| `team` | `m_iTeam`, coded as in `player_samples.team` |
| `alive` | `m_bAlive`: 1 if the player was alive at the last resource refresh |
| `connected` | `m_bConnected`: 1 while a player holds the slot. A disconnect clears this, the account id and the userid but not `team` or `alive`, so a slot keeps its last `team` and `alive` after its player leaves (upstream at `bea96408`, `src/game/server/player_resource.cpp:250-255`) |
| `xp` | NT;RE XP, the number the scoreboard ranks by |
| `score` | engine score field |
| `deaths` | death count |
| `ping` | latency in ms; 0 for bots |

### `alive_counts`

How many players each team has alive, written when a team's count
changes. The latest row for a team at or before a tick is its count at
that tick. The first rows of a demo give both teams' counts at the first
resource update.

The counts come from `player_resource.team`, `alive` and `connected`, so
they cover every player in both recording types. A slot counts while it
is both connected and alive, because a player who leaves keeps their
slot's last `team` and `alive`.

The resource trails the game by up to a refresh, so changes are timed
from the events behind them. A death in `deaths` takes its victim out on
the death's own tick. A player coming alive moves back to the start tick
of their round when it falls within 10 ticks before the resource shows
them. Any other change stays on the resource tick, a few ticks late.
That covers a player leaving or switching team while alive, a late
joiner's first spawn, and every respawn in a mode that respawns players
during a round, such as JGR.

Measured on 33 recordings dated 2026-03-19 to 2026-10-24, 32 of them
SourceTV and one POV:

| measure | result |
|---|---|
| deaths the resource shows 0 to 8 ticks after the death | 2900 of 2901 |
| spawns outside JGR the resource shows 0 to 8 ticks after a round start | 2414 of 2469 |
| SourceTV round starts where the count matches the entities' life state | 738 of 738 |
| SourceTV death ticks where the count matches the entities' life state | 2781 of 2782 |

The death the resource never showed came 4 ticks before a round started,
and the victim was alive again by the next refresh. The mismatch is a
player who had left the server, whose last entity sample is still marked
alive.

| column | meaning |
|---|---|
| `tick` | when the count changed |
| `round_number` | the round the tick falls in, as in `deaths.round_number`; NULL before the first round |
| `round_index` | that round's `rounds.round_index`; join on it with `demo_id`. NULL before the first round |
| `team` | 2 Jinrai, 3 NSF |
| `alive` | players on the team alive from this tick on |

Who was left alive on the victim's team after each death:

```sql
SELECT d.tick, d.victim_team,
       (SELECT a.alive FROM alive_counts a
        WHERE a.demo_id = d.demo_id AND a.team = d.victim_team AND a.tick <= d.tick
        ORDER BY a.tick DESC LIMIT 1) AS left_alive
FROM deaths d
WHERE d.demo_id = (SELECT id FROM demos WHERE map = 'nt_saitama_ctg');
```

### `pov_samples`

The recorder's own view, one row per packet frame (~66/s, unconditionally).
Denser and simpler than `player_samples` for the recording player. Thin with
`--pov-sample N`. In SourceTV demos these rows exist but are the
auto-director camera, not a player.

| column | meaning |
|---|---|
| `tick` | sample time |
| `x`, `y`, `z` | view position (eye level, unlike `player_samples`) |
| `pitch`, `yaw`, `roll` | view angles, degrees |

### `recorder_inputs`

**Tags: POV only**

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

## Event log

Discrete happenings, one row each, mostly decoded from the demo's own game
event definitions.

### `deaths`

One row per death, from NT;RE's own `player_death` game event definition.
The table is named for the victim, because every row has one while some
rows have no killer, and `ghoster` describes the victim too.

| column | meaning |
|---|---|
| `tick` | when the player died |
| `round_number` | the number of the round the death fell in, as in `rounds.round_number`. NULL before the first round began, as in warmup |
| `round_index` | that round's `rounds.round_index`; join on it with `demo_id`, because `round_number` repeats. NULL when `round_number` is |
| `post_round` | 1 when the death fell after its round ended and before the next round began, while the result is on screen |
| `victim_steamid`, `attacker_steamid`, `assister_steamid` | the players' Steam IDs, looked up in the roster at parse time; join `players.steamid`. NULL for the attacker of a world death, for the assister when nobody assisted, and for a player missing from the roster |
| `victim_userid`, `attacker_userid` | connection ids from the event's `userid` and `attacker` fields; join `players.userid`; attacker 0 = world/environment |
| `victim_name`, `attacker_name` | resolved at parse time; NULL if unknown |
| `victim_team`, `attacker_team`, `assister_team` | each player's team at the death, coded as in `player_samples.team`, from the latest `player_resource.team` of their slot at or before `tick`. NULL for the attacker of a world death, for the assister when nobody assisted, for a player missing from the roster, and when the entity pass failed |
| `assists` | userid of the assisting player, or 0 for none; joins `players.userid` (NT;RE defines the event field as "user ID who assists") |
| `weapon` | weapon's entity name without `weapon_`, e.g. `srm`; see Weapon reference below. A grenade or detpack kill's inflictor is mapped to the weapon that made it |
| `headshot`, `suicide`, `explosive` | kill flags |
| `ghoster` | 1 if the victim was carrying the ghost. After a round ends the game stops updating its carrier, so a post-round death can carry a stale 1; see `ghost_holds` |

A world death is a death no player caused. Its `attacker_userid` is 0 and
its `attacker_name` is NULL. The game then puts the class name of whatever
dealt the damage in `weapon` (upstream at `bea96408`,
`src/game/shared/neo/neo_gamerules.cpp:4533-4536`). `trigger_hurt` is a damage
zone the map author placed, such as a pit or an out-of-bounds area.
`worldspawn` is the map itself, as in a fatal fall. `suicide` is 1 on
every world death, because the game sets it whenever the killer is not a
player (`:4581`). `headshot` reports the last body part hit before the
death (`:4580`), so on a world death it describes an earlier hit and not
the death.

The teams come from the player resource rather than the player entity,
because the server sends the resource to every client (upstream at
`bea96408`, `src/game/server/player_resource.cpp:126`). A POV recording
therefore has the team of a player the recorder never saw. The server
refreshes the resource every 0.1 seconds (`:138`, `:220`), so a team change
reaches it up to 7 ticks late.

A round runs from its start tick through its end tick. The death that ends
an elimination round lands on the end tick or just before it, so it
counts in the round with `post_round` 0. Across 33 recordings dated
2026-03-19 to 2026-10-24:

| elimination rounds | count |
|---|---|
| with a death on the end tick | 233 |
| with the last death 1 tick before the end tick | 1 |
| with a death 1 to 3 ticks after the end tick | 0 |

The query behind the table lists each death near an elimination round's
end with its offset:

```sql
SELECT r.round_number, d.tick - r.end_tick
FROM rounds r JOIN deaths d ON d.demo_id = r.demo_id
WHERE r.win_reason = 'elimination'
  AND d.tick BETWEEN r.end_tick - 3 AND r.end_tick + 3;
```

A round with no recorded start, because the recording began during it,
takes every death before its end. A round aborted by an admin pause or a
match restart runs until the next start, and the round that follows can
carry the same `round_number`, so `round_index` tells them apart.

Apart from world deaths and deaths nobody assisted, every userid in a row
should have a roster row. The dumper warns at parse time when a death names
a userid that the roster never saw. That row's names and Steam IDs for the
missing player are NULL, and the warning marks a roster gap worth
reporting with the demo.

### `attacker_hits`

**Tags: SourceTV only, pre-July-2026 builds**

Hit log from NT;RE's per-attacker damage accumulator
(`m_rfAttackersAccumlator`), written on change: each row means the attacker
landed damage on the victim at that tick. The accumulator value itself is
only the fractional carry of damage (always below 1), so join the victim's
health drop in `player_samples` at the same tick to get the amount.

Empty for demos recorded on NT;RE builds from 2026-07-25 onward. Upstream
commit `cf2f30e0` ("Damage reporting fixup") stopped networking the
accumulator arrays; the server now sends per-death damage totals to the
dying player alone, via the `KillerDamageInfo` user message, which SourceTV
never receives. Demos from earlier builds still populate this table.

| column | meaning |
|---|---|
| `tick` | when the hit landed |
| `victim_entity_id` | joins `players.entity_id` |
| `victim_steamid` | joins `players.steamid` |
| `attacker_entity_id` | joins `players.entity_id` |
| `attacker_steamid` | joins `players.steamid` |
| `accumulator` | fractional damage carry; a `0` row is the victim's respawn reset |

### `player_pings`

**Tags: POV only**

In-game location pings, from `player_ping` game events. All players' pings
are present, not just the recorder's.

| column | meaning |
|---|---|
| `tick` | when the ping was placed |
| `steamid` | pinging player; joins `players.steamid`. NULL if missing from the roster |
| `userid` | pinging player's connection id, from the event; joins `players.userid` |
| `team` | pinging player's team, coded as in `player_samples.team` |
| `x`, `y`, `z` | pinged world position |
| `ghoster_ping` | 1 when the pinging player carried the ghost or was the VIP |

### `ghost_holds`

Who held the ghost, one row per stretch a player held it. The game rules
proxy's `m_iGhosterPlayer` names the ghost weapon's owner, whether or not
the owner has it out, so this table covers the time a carrier spends on
another weapon. The game sets the property from the owner on every think
while the round is live (upstream at `bea96408`,
`src/game/shared/neo/neo_gamerules.cpp:1414-1428`). The server sends the
proxy to every client, so both recording types carry it.

Once a round is over, the think returns before that update (`:1362-1404`)
and the property keeps its last value until the next round resets it
(`:3516-3522`). A hold still open at its round's end therefore ends at the
round's end tick, and a pickup after the round ended has no row.

A hold covers `tick <= t AND t <= end_tick`. When the ghost passes straight
from one player to another, the next hold begins on the previous hold's
`end_tick`.

| column | meaning |
|---|---|
| `tick` | when the player took the ghost |
| `end_tick` | the last tick of the hold, inclusive: the tick the property moved off the player, or their round's end tick if that came first. A carrier's death lands on this tick, and the game's `deaths.ghoster` still names them there. NULL when the recording cut off the round |
| `round_number` | the round the hold began in, as in `deaths.round_number` |
| `round_index` | that round's `rounds.round_index`; join on it with `demo_id` |
| `entity_id` | the holder's slot; joins `players.entity_id` |
| `steamid` | the holder; joins `players.steamid` |
| `userid` | the holder's connection id; joins `players.userid` and tells bots apart |

Measured on 33 recordings dated 2026-03-19 to 2026-10-24:

| check | result |
|---|---|
| live `player_samples` rows with `weapon = 'ghost'` inside a round that fall in a hold | 512538 of 512544 |
| capturers in `rounds` holding the ghost at the round's end tick | 37 of 37 |
| deaths before their round ended with `ghoster` 1 that fall in the victim's hold | 82 of 82 |
| live `player_samples` rows inside holds with another weapon out | 29593 of 585404 |

The six samples outside a hold are in one POV recording, each on a round's
start tick, where the respawned player's entity still shows the weapon of
the round before.

### `ghost_callouts`

**Tags: POV only**

Enemy positions called out by a bot carrying the ghost, from
`ghost_enemy_callout` game events. The carrier's team sees them on the
compass. Players never send callouts, so a game without bots has no rows.

| column | meaning |
|---|---|
| `tick` | when the callout fired |
| `steamid` | ghost carrier's Steam ID, which is `BOT` because only bots call out |
| `userid` | ghost carrier, from the event; joins `players.userid` and is the column that identifies the bot |
| `team` | carrier's team, coded as in `player_samples.team` |
| `target_entity_id` | spotted enemy; joins `players.entity_id` |
| `target_steamid` | spotted enemy's Steam ID; joins `players.steamid` |
| `x`, `y`, `z` | spotted enemy's world position |

### `team_scores`

**Tags: POV only**

Cumulative team score updates, from `team_score` game events. One row per
update; the latest row at or before a tick is the score at that tick.
Columns: `tick`, `team` (2 Jinrai, 3 NSF), `score`.

### `team_changes`

Team joins and switches, from `player_team` game events.

| column | meaning |
|---|---|
| `tick` | when the change happened |
| `steamid` | the player; joins `players.steamid`. NULL if missing from the roster |
| `userid` | connection id, from the event; joins `players.userid` |
| `team`, `old_team` | new and previous team, coded as in `player_samples.team` |
| `disconnect` | 1 when the change is a player disconnecting |

### `rank_changes`

**Tags: POV only**

Rank progression, from `player_rankchange` game events. Columns: `tick`,
`steamid` (joins `players.steamid`), `userid` (connection id, from the
event; joins `players.userid`), `old_rank`, `new_rank` (rank index,
increasing with XP).

### `round_starts`

Round starts as wire facts, from `round_start` game events. Unlike `rounds`,
these carry no winner. Columns: `tick`, `objective`, `timelimit`,
`fraglimit`. Caveat: NT;RE hardcodes `objective` to `DEATHMATCH` on every
round start, whatever the game mode, so ignore it.

### `round_results`

Round ends as wire facts, from NT;RE's `RoundResult` user message, which
the server sends to every client (SourceTV included) when a round ends.
Stalemates are announced here and nowhere else on screen, so this is the
only complete record of round outcomes.

| column | meaning |
|---|---|
| `tick` | when the round ended |
| `team` | `jinrai`, `nsf`, or `tie`, as sent |
| `message` | the victory text, e.g. `Team NSF wins by capturing the ghost!` or `TIE`; empty for map-scripted wins. For a round that also ends the match or leads into sudden death, this holds the match-end text, e.g. `Team NSF wins the match!`, instead of the round's own reason |

A round aborted by an admin pause and restarted sends no result.

### `chat`

SayText2 user messages, control codes stripped.

| column | meaning |
|---|---|
| `tick` | when the line was sent |
| `client_entity` | sender's entity slot; joins `players.entity_id` |
| `steamid` | sender's Steam ID; joins `players.steamid`. NULL for a server message |
| `from_name` | sender name as transmitted (may be empty for server messages) |
| `text` | the message |
| `team_chat` | 1 for team-only chat |

### `console_cmds`

**Tags: POV only**

Console commands issued by the recorder during the recording. Columns:
`tick`, `cmd`.

### `game_events`

Every game event the recording carries, decoded against the demo's own
event definitions. SourceTV recordings carry only a few types, such as
`player_death` and `round_start`. Events such as `ghost_capture` appear
only in POV recordings; for captures in any recording, use
`rounds.capturer_steamid`.

| column | meaning |
|---|---|
| `tick` | event time |
| `name` | event name, e.g. `player_death`, `round_start` |
| `fields` | event fields as a JSON object |

Query fields with SQLite's JSON functions:

```sql
SELECT tick, json_extract(fields, '$.userid') AS victim_userid
FROM game_events
WHERE demo_id = (SELECT id FROM demos WHERE map = 'nt_saitama_ctg')
  AND name = 'player_death';

SELECT DISTINCT name FROM game_events  -- what's in this demo?
WHERE demo_id = (SELECT id FROM demos WHERE map = 'nt_saitama_ctg');
```

## Derived tables

Heuristics over text recovered by the ASCII skim, not wire facts.

### `announcements`

Center-screen text recovered by the ASCII skim (round starts, winners, ghost
captures). `rounds` takes its start markers from these. `seconds` is precomputed
`tick / tickrate`. Columns: `tick`, `seconds`, `text`.

### `rounds`

One row per round: the `ROUND N STARTED` announcement supplies the number
and start tick, and `round_results` supplies the end tick and outcome. A
demo without `RoundResult` messages falls back to `Team X wins ...`
announcements for its ends, which cannot see ties; there, a match-end line
leaves the round's winner and reason NULL, because it names the match's
winner rather than the round's. `round_starts` holds the wire-fact start
events. The capturer comes from the capture zones and the ghost carrier in
the entity data.

When a round ends the match or leads into sudden death, the game shows
that message instead of the round's reason. `win_reason` then comes from
the round result's team field and the game state at the round's last
tick: `tie` when no team won, `objective` when the capture zones switched
off or the VIP was killed, `elimination` when the losing team had no one
alive. Otherwise it is NULL.

| column | meaning |
|---|---|
| `round_number` | as announced by the game; NULL if the first start marker was missed. A round restarted after an admin pause repeats its number, and a match restart starts the count again, so one number can name several rounds of a demo |
| `round_index` | the round's position in the demo, counting from 1 in tick order; unique within a demo, so other tables join on it |
| `start_tick`, `end_tick` | NULL when the demo started mid-round, or the round never ended (cut off, or aborted by a pause) |
| `winner` | `jinrai`, `nsf` or `tie`, as in `round_results.team`; NULL for a round that never ended or whose winner is unknown |
| `win_reason` | how the round was won, below; NULL when unknown. The full text is in `round_results.message` |
| `capturer_steamid` | the player who carried the ghost into the capture zone; joins `players.steamid`. NULL unless the round was won by a capture |
| `capturer_userid` | the capturer's connection id; joins `players.userid` and tells bots apart |

| `win_reason` | meaning |
|---|---|
| `objective` | the ghost was captured, or the VIP escaped or was killed |
| `elimination` | every player on the losing team was killed |
| `score` | a points win in TDM, JGR or deathmatch |
| `forfeit` | the VIP left the game |
| `tie` | no team won |

## Inferred tables

Probabilistic reconstructions. Query with the `confidence` column in mind,
and filter on `method` if a rule changes underneath you.

### `inferred_hits`

Who dealt each point of damage. NT;RE builds from 2026-07-25 onward no
longer network the accumulator behind `attacker_hits`, so this table
rebuilds attribution from what every demo still carries: one row per health
drop in `player_samples`, with the attacker taken from the kill event for
the fatal hit and, for every other hit, the living enemy whose aim points
closest at the victim on that tick.

| column | meaning |
|---|---|
| `tick` | when the victim's health dropped |
| `victim_entity_id` | joins `players.entity_id` |
| `victim_steamid` | joins `players.steamid` |
| `attacker_entity_id` | joins `players.entity_id`; NULL when no enemy's aim was within 15 degrees |
| `attacker_steamid` | joins `players.steamid`; NULL when `attacker_entity_id` is |
| `damage` | the health drop, so overkill on the fatal hit |
| `aim_error` | degrees between the attacker's view direction and the victim; NULL for kill-event rows |
| `confidence` | 1 for kill-event rows; otherwise falls from 1 at zero aim error to 0 at 20 degrees |
| `method` | `kill-event`, `aim-v1`, or `none` |

Measured against the March 2026 demos, where the accumulator gives ground
truth, the `aim-v1` rule names the right attacker on 92% of hits. Grenade
and fall damage produce rows too; they are usually unattributed or low
confidence, because no one's aim explains them. Health drops whose damage
is a whole number leave no accumulator trace, so this table also holds
hits that `attacker_hits` never listed.

## Weapon reference

`deaths.weapon` and `player_samples.weapon` both hold the weapon's entity
name without its `weapon_` prefix, as listed below. NULL means the weapon
is unknown.
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
| `weapon_remotedet`, `weapon_smac` | detpack, SMAC (NT;RE) |
| `weapon_ghost` | the ghost (objective) |

The set is open: names added by future NT;RE releases appear as-is.

## Example: kills per player across demos

Steam IDs hold across demos, so no join is needed. Bots share one Steam ID
and are left out.

```sql
SELECT attacker_steamid, count(*) AS kills
FROM deaths
WHERE attacker_steamid IS NOT NULL AND attacker_steamid <> 'BOT'
GROUP BY attacker_steamid
ORDER BY kills DESC;
```

## Example: heatmap query

Death positions on one demo, by joining each death to the victim's last
known position:

```sql
SELECT d.tick, ps.x, ps.y
FROM deaths d
JOIN players v   ON v.demo_id = d.demo_id AND v.userid = d.victim_userid
JOIN player_samples ps
  ON ps.demo_id = d.demo_id AND ps.entity_id = v.entity_id
  AND ps.tick = (SELECT MAX(tick) FROM player_samples
                 WHERE demo_id = d.demo_id AND entity_id = v.entity_id
                   AND tick <= d.tick)
WHERE d.demo_id = (SELECT id FROM demos WHERE map = 'nt_saitama_ctg');
```
