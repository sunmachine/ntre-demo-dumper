# Architecture

Data flows one direction through three layers, orchestrated by `pipeline.rs`:

```
.dem file
   │
   ▼
demo/       reads the on-disk format; knows nothing about gameplay
   │           header.rs        fixed 1072-byte HL2DEMO header (map, server, ticks)
   │           frames.rs        frame iterator: command, tick, payload, recorder POV
   │           bits.rs          LSB-first bit reader (Source bf_read semantics)
   │           usercmd.rs       dem_usercmd decode (stock SDK2013 wire format)
   │           net.rs           net-message framing + generic game-event parsing
   │           stringtables.rs  dem_stringtables dump (player roster / userinfo)
   ▼
extract/    turns frames into gameplay facts; does no I/O during extraction
   │           mod.rs            FrameExtractor trait + DemoContext
   │           skim.rs           bit-shift ASCII recovery from bit-packed payloads
   │           announcements.rs  center-text messages (writes announcements)
   │           rounds.rs         rounds from start markers, round results and entity state
   │           pov.rs            recorder position/angles per packet frame
   │           console.rs        recorder console commands
   ▼
output/     persists facts; all SQL lives here
               sqlite.rs   schema + insert_* methods, one demo per transaction
```

`main.rs` is CLI definition and wiring only. `pipeline.rs` is the only module
that touches all three layers: read file → walk frames → run extractors →
persist. Neither contains parsing or SQL logic. Rounds are derived in the
pipeline after the entity pass, because the ghost capturer comes from
entity state, and are written in the same transaction as the rest.

## Layer rules

- `demo` may not depend on `extract` or `output`.
- `extract` may depend on `demo` types, never on `output` or the filesystem.
- `output` receives plain structs; it never computes gameplay facts.

## Adding an extractor

Extractors implement the `FrameExtractor` trait (`extract/mod.rs`): the
pipeline streams every frame through `on_frame`, then calls `persist` once so
the extractor writes its tables and reports `(label, count)` summary lines.

1. Create `src/extract/<fact>.rs` with a struct implementing
   `FrameExtractor`; register the module in `extract/mod.rs`.
2. Add a table to `SCHEMA` and an `insert_<fact>` method in
   `src/output/sqlite.rs` (tag rows with `demo_id`).
3. Add the extractor to the registration list in `pipeline::parse_one`; the
   dispatch loop and summary printing handle the rest.

Frames carry byte ranges rather than slices, so extractor structs stay
lifetime-free; slice a payload with `frame.payload_in(ctx.data)`.

## File format notes

NT;RE demos are HL2DEMO, demo protocol 3, network protocol 24, the same
engine branch as TF2. Frame headers are byte-aligned and self-describing
(command byte, int32 tick, length-prefixed payload; `dem_stringtables = 8`
exists in SDK 2013). Packet payloads are bit-packed net messages; alongside
decoding them, `skim.rs` scans each payload at all 8 bit alignments for
printable ASCII. Demos stopped mid-write can end a few bytes short of a full
frame; this is treated as clean EOF.

## Net-message layer

`demo/net.rs` frames the bit-packed message stream inside signon/packet
frames: every message type on this engine branch is either surfaced or
skipped by its known wire size (formats ported from
[demostf/parser](https://github.com/demostf/parser), MIT). Game events are
parsed **generically against the demo's own event definitions**, never a
hardcoded schema. NT;RE's `player_death` has different fields than TF2's, so
a parser with TF2-typed events silently misreads NT;RE demos. The
`extract/net.rs` extractor builds the kill feed, roster, chat, and the
generic `game_events` table from these messages.

SourceTV relays only the game events that `CHLTVDirector::GetModEvents`
names, in upstream `src/game/server/hltvdirector.cpp:238-257`. They are
`hltv_status`, `hltv_chat`, `player_connect`, `player_disconnect`,
`player_team`, `player_info`, `server_cvar`, `player_death`,
`player_chat`, `round_start` and `round_end`. NT;RE's own events, such as
`ghost_capture`, are not in the list, so SourceTV recordings never carry
them, and the ghost capturer has to come from entity state instead. On the
2026-09-13 and 2026-09-25 SourceTV recordings, `SELECT DISTINCT name FROM
game_events` returns only events from the list.

When a round ends the match or leads into sudden death, `SetWinningTeam`
substitutes "Team X wins the match!", "The match is tied!" or "Next round:
Sudden death!" for the round's own reason text, but the `RoundResult`
message's team field still names that round's winner, or `tie` (upstream
`src/game/shared/neo/neo_gamerules.cpp:3813-3866`, `:3906`). `rounds`
recovers the round's real reason from that team field and the game state at
the round's end tick instead.

## Entity layer

`extract/entities.rs` decodes svc_PacketEntities through tf-demo-parser's
sendtable machinery. This is a whole-file pass separate from the frame loop,
because the library owns its own demo walk. The crate is vendored at
`vendor/tf-demo-parser/` (via `[patch.crates-io]`) with one behavioral
change: upstream rejects sendtable array elements carrying the ChangesOften
flag, but the engine permits that combination and NT;RE uses it
(`DT_NEO_Player.m_rfAttackersAccumlator`), so the check is relaxed to only
reject genuinely malformed double element props. NT;RE builds from
2026-07-25 onward no longer network that array, but the patch stays for
demos recorded on earlier builds. Its analyser subscribes to
`MessageType::PacketEntities` only, so the library length-skips game events
and never runs its unsafe typed-event reader. Player classes are found
dynamically (server class names ending in "Player"), props are matched by
name (`m_vecOrigin`, `m_angEyeAngles[0]`, `m_hActiveWeapon`, and others)
after resolving identifiers from the demo's data tables, and the active
weapon handle resolves to a class name via per-entity class tracking, then
to the weapon's entity name via a static class table built from upstream's
`LINK_ENTITY_TO_CLASS(weapon_...)` lines (`entities.rs`). The
game rules proxy and the ghost capture zones are found the same way, by
class names ending in "GameRulesProxy" and "GhostCapturePoint". The proxy's
`m_iGhosterPlayer` names the ghost carrier, and each zone's `m_bIsActive`
says whether a capture into it counts. `rounds` reads both at each round
end. The server sends both entities to every client whatever their
position (upstream `src/game/shared/gamerules.cpp:102-106` and
`src/game/shared/neo/neo_ghost_cap_point.cpp:102-105`), so POV recordings
carry them too. A mid-file decode error degrades to a warning and keeps all
samples decoded so far; frame-level extraction is never affected.
