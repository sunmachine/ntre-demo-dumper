```text
                   ████
      ████████    ██████████     ▓▓▓▓
    ████████████  ███████████   ▓▓▓▓▓▓
   █████░░░█████░ █████░░░░░░░  ▓▓▓▓▓▓░
   █████░  █████░ █████░         ▓▓▓▓░░
   █████░  █████░ █████░          ░░░░
   █████░  █████░ ██████████      █████
   █████░  █████░ ██████████░     █████░
    ░░░░░  █████░  ░░░░░░░░░░    ████░░░
           █████░               ███░░░
            ░░░░░                ░░░

     N E O T O K Y O ; R E B U I L D
```

# NT;RE Demo Dumper Tool

Offline gameplay-data extractor for [NEOTOKYO;REBUILD](https://github.com/NeotokyoRebuild/neo)
(NT;RE) demo files. Reads `.dem` recordings and writes gameplay data to SQLite.
The crate/binary is named `ntre-demo-dumper`.

## What it extracts

- **Demo metadata:** map, server, recorder, duration, tick rate, whether
  it is a SourceTV or POV recording, and the map file's MD5.
- **Server settings:** the rules the server changed from the defaults,
  such as round limit and competition name.
- **Announcements:** round starts, round winners, ghost captures, with tick and
  wall-clock timestamps.
- **Rounds:** start and end tick, winner (including ties), win reason,
  and who captured the ghost.
- **Recorder POV:** position and view angles every packet frame (~66/s), ready
  for heatmaps of the recording player.
- **Recorder inputs:** per-tick buttons (fire, jump, duck, reload, sprint, and
  NEO's aim/lean/thermoptic/vision), movement axes, mouse deltas, and weapon
  switches, decoded from `dem_usercmd` frames. Convenience boolean columns are
  generated from the raw buttons field.
- **Console commands** issued by the recorder.
- **Inferred hits:** who dealt each point of damage, reconstructed from
  health drops and enemy aim with a confidence per row, for demos where the
  game no longer networks damage attribution.
- **Kill feed:** NT;RE's `player_death` game event: victim, attacker, assists,
  weapon, headshot/suicide/explosive/ghoster flags, with names resolved via the
  roster.
- **Player roster:** name, userid, SteamID, bot flag, from the string-table
  dump at recording start plus later updates to it for late joiners.
- **Chat:** SayText2 user messages.
- **All game events:** every event the recording carries, decoded against
  the demo's own event definitions and stored with fields as JSON (query
  with SQLite's `json_extract`).
- **All-player entity samples:** position, eye angles, active weapon, health,
  team, and life state for every player, on change (~66/s while moving),
  decoded from delta-compressed entity updates via the demo's own sendtables.
  Note: a POV demo only contains entities within the recorder's PVS; SourceTV
  demos contain everyone, always.

## How it works

See [ARCHITECTURE.md](ARCHITECTURE.md) for the layer layout and how to add a
new extractor.

The demo is walked frame by frame; each frame header carries its command,
tick, and payload length, so the walk is deterministic and byte-aligned.
Three kinds of extraction run over it:

- **Net-message parsing:** packet payloads are decoded as bit-packed net
  messages, producing the kill feed, chat, roster, and game events.
- **Entity decoding:** a separate whole-file pass decodes delta-compressed
  entity updates via the demo's own sendtables, producing `player_samples`.
- **ASCII skim:** payloads are also scanned at all 8 bit alignments for
  printable text, which yields the announcements. Skimming is parallelized
  across all available cores.

## Building (atomic hosts)

Two container options are provided.

**Dev Container standard** (`.devcontainer/devcontainer.json`) works with VS Code,
the `devcontainer` CLI, or plain docker/podman:

```sh
docker run --rm -v "$PWD":/workspace -w /workspace \
  mcr.microsoft.com/devcontainers/rust:1 cargo build --release
```

**distrobox** (`distrobox.ini`):

```sh
distrobox assemble create --file distrobox.ini
distrobox enter ntre-dev -- cargo build --release
distrobox enter ntre-dev -- cargo test
```

On a mutable host, plain `cargo build --release` works; the only system
dependency is a C compiler (SQLite is bundled).

## Usage

```sh
ntre-demo-dumper my_demo.dem                 # writes ntre_demos.sqlite
ntre-demo-dumper -o out.sqlite *.dem         # multiple demos, one database
ntre-demo-dumper --pov-sample 10 my.dem      # thin POV samples to every 10th frame
ntre-demo-dumper --match 'REGEX' my.dem      # capture extra announcement patterns
ntre-demo-dumper --all-strings my.dem        # exploratory: keep every recovered string
```

A demo already in the database is skipped, even under another name.

The tables cover metadata, announcements, rounds, scores, roster, kills,
pings, chat, game events, and per-tick samples. Some fill only for SourceTV
recordings or only for POV recordings. See [SCHEMA.md](SCHEMA.md) for the
table list, which tables depend on the recording type, column semantics,
join keys, and example queries; `.schema` in the sqlite3 shell shows the
commented DDL.

After upgrading the dumper, parse into a new database file: it refuses to
add demos to one written by another version.

## License

MIT; see [LICENSE.md](LICENSE.md). The vendored
[tf-demo-parser](vendor/tf-demo-parser) is used under the MIT option of its
`MIT OR Apache-2.0` license; see its
[LICENSE.md](vendor/tf-demo-parser/LICENSE.md).
