# Architecture

Data flows one direction through three layers, orchestrated by `pipeline.rs`:

```
.dem file
   │
   ▼
demo/       reads the on-disk format; knows nothing about gameplay
   │           header.rs        fixed 1072-byte HL2DEMO header (map, server, ticks…)
   │           frames.rs        frame iterator: command, tick, payload, recorder POV
   │           bits.rs          LSB-first bit reader (Source bf_read semantics)
   │           usercmd.rs       dem_usercmd decode (stock SDK2013 wire format)
   │           net.rs           net-message framing + generic game-event parsing
   │           stringtables.rs  dem_stringtables dump (player roster / userinfo)
   ▼
extract/    turns frames into gameplay facts; does no I/O during extraction
   │           mod.rs            FrameExtractor trait + DemoContext
   │           skim.rs           bit-shift ASCII recovery from bit-packed payloads
   │           announcements.rs  center-text messages (writes announcements + rounds)
   │           rounds.rs         pure derivation of rounds from announcements
   │           pov.rs            recorder position/angles per packet frame
   │           console.rs        recorder console commands
   ▼
output/     persists facts; all SQL lives here
               sqlite.rs   schema + insert_* methods, one demo per transaction
```

`main.rs` is CLI definition and wiring only. `pipeline.rs` is the only module
that touches all three layers: read file → walk frames → run extractors →
persist. Neither contains parsing or SQL logic.

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
3. Add the extractor to the registration list in `pipeline::parse_one` — the
   dispatch loop and summary printing handle the rest.

Frames carry byte ranges rather than slices, so extractor structs stay
lifetime-free; slice a payload with `frame.payload_in(ctx.data)`.

## File format notes

NT;RE demos are HL2DEMO, demo protocol 3, network protocol 24 — the same
engine branch as TF2. Frame headers are byte-aligned and self-describing
(command byte, int32 tick, length-prefixed payload; `dem_stringtables = 8`
exists in SDK 2013). Packet payloads are bit-packed net messages; rather than
decoding the net protocol, `skim.rs` scans each payload at all 8 bit
alignments for printable ASCII. Demos stopped mid-write can end a few bytes
short of a full frame — treated as clean EOF.

## Net-message layer

`demo/net.rs` frames the bit-packed message stream inside signon/packet
frames: every message type on this engine branch is either surfaced or
skipped by its known wire size (formats ported from
[demostf/parser](https://github.com/demostf/parser), MIT). Game events are
parsed **generically against the demo's own event definitions** — never a
hardcoded schema. This matters: NT;RE's `player_death` has different fields
than TF2's, so any parser with TF2-typed events silently misreads NT;RE
demos. The `extract/net.rs` extractor builds the kill feed, roster, chat, and
the generic `game_events` table from these messages.

## Entity layer

`extract/entities.rs` decodes svc_PacketEntities through tf-demo-parser's
sendtable machinery — a whole-file pass separate from the frame loop, since
the library owns its own demo walk. The crate is vendored at
`vendor/tf-demo-parser/` (via `[patch.crates-io]`) with one behavioral
change: upstream rejects sendtable array elements carrying the ChangesOften
flag, but the engine permits that combination and NT;RE uses it
(`DT_NEO_Player.m_rfAttackersAccumlator`), so the check is relaxed to only
reject genuinely malformed double element props. Its analyser subscribes to
`MessageType::PacketEntities` only, so the library length-skips game events
and never runs its unsafe typed-event reader. Player classes are found
dynamically (server class names ending in "Player"), props are matched by
name (`m_vecOrigin`, `m_angEyeAngles[…]`, `m_hActiveWeapon`, …) after
resolving identifiers from the demo's data tables, and the active weapon
handle resolves to a class name via per-entity class tracking. A mid-file
decode error degrades to a warning and keeps all samples decoded so far —
frame-level extraction is never affected.
