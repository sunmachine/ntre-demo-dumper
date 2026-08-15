# NT;RE Demo Dumper Tool

Offline gameplay-data extractor for [NEOTOKYO;REBUILD](https://github.com/NeotokyoRebuild/neo)
(NT;RE) demo files. Reads `.dem` recordings and writes gameplay data to SQLite.
The crate/binary is named `ntre-demo-dumper`.

## What it extracts (phase 1)

- **Demo metadata** — map, server, recorder, duration, tickrate (from the fixed
  1072-byte `HL2DEMO` header; NT;RE demos are demo protocol 3 / network protocol 24,
  same engine branch as TF2).
- **Announcements** — round starts, round winners, ghost captures, with tick and
  wall-clock timestamps.
- **Rounds** — derived start/end tick, winner, and win reason per round.
- **Recorder POV** — position and view angles every packet frame (~66/s), ready
  for heatmaps of the recording player.
- **Recorder inputs** — per-tick buttons (fire, jump, duck, reload, sprint, and
  NEO's aim/lean/thermoptic/vision), movement axes, mouse deltas, and weapon
  switches, decoded from `dem_usercmd` frames. Convenience boolean columns are
  generated from the raw buttons field.
- **Console commands** issued by the recorder.

## How it works

See [ARCHITECTURE.md](ARCHITECTURE.md) for the layer layout and how to add a
new extractor.

The demo is walked frame by frame (each frame header carries its command, tick,
and payload length — fully deterministic and byte-aligned). Packet payloads are
bit-packed net messages, so instead of decoding the whole net protocol, each
payload is scanned at all 8 bit alignments for printable ASCII runs, which are
then matched against announcement patterns. Skimming is parallelized across all
available cores.

Planned next phases: real net-message parsing for the kill feed, chat, and
player roster (phase 2), and sendtable-driven entity decoding for all-player
positions/aim vectors (phase 3, likely building on
[tf-demo-parser](https://github.com/demostf/parser) since NT;RE shares TF2's
engine branch).

## Building (atomic hosts)

Two container options are provided.

**Dev Container standard** (`.devcontainer/devcontainer.json`) — works with VS Code,
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

On a mutable host, plain `cargo build --release` works — the only system
dependency is a C compiler (SQLite is bundled).

## Usage

```sh
ntre-demo-dumper my_demo.dem                 # writes ntre_demos.sqlite
ntre-demo-dumper -o out.sqlite *.dem         # multiple demos, one database
ntre-demo-dumper --pov-sample 10 my.dem      # thin POV samples to every 10th frame
ntre-demo-dumper --match 'REGEX' my.dem      # capture extra announcement patterns
ntre-demo-dumper --all-strings my.dem        # exploratory: keep every recovered string
```

Tables: `demos`, `announcements`, `rounds`, `pov_samples`, `console_cmds`.

## License

MIT; see [LICENSE.md](LICENSE.md).
