# 🌀 Torrentina

A lightweight, efficient BitTorrent client built from scratch in Rust — as a
reusable **engine library**, a **CLI**, and a **Tauri v2 desktop GUI**.

> Status: complete and stable for personal use. All five protocol modules are
> implemented and tested (**64 tests, clippy-clean**), with a multi-torrent
> desktop UI. See [`PROJECT_STATUS.md`](./PROJECT_STATUS.md) for the detailed
> module breakdown and roadmap.

## Features

- **Bencode / `.torrent` parsing** with a correct SHA-1 info hash computed from
  the *original* info-dict bytes (handles unmodeled keys like `md5sum`,
  `source`, and BitTorrent v2 fields).
- **HTTP(S) and UDP (BEP 15) trackers**, with spec-correct raw percent-encoding
  and compact + dictionary peer parsing.
- **Full peer wire protocol** (handshake + all messages) over async TCP.
- **Robust multi-peer downloading**: rarest-first piece selection, 16 KiB
  pipelined requests, per-piece SHA-1 verification, and resilience against
  stalled peers via release-on-choke, a per-request timeout, and an endgame
  fallback (no more "stuck at 99.9%").
- **Single- and multi-file output**, including pieces that straddle file
  boundaries.
- **Desktop GUI**: multiple concurrent torrents, per-torrent progress / ETA /
  peer table, pause / resume / cancel, a 3-way remove (list only · list + data ·
  data only), and a torrent list that persists across restarts.
- Three frontends: the **GUI**, a **CLI**, and a self-contained **demo**.

## Install (Linux)

A prebuilt Debian package is produced by the build (see below):

```bash
sudo dpkg -i target/release/bundle/deb/Torrentina_0.1.1_amd64.deb
```

This registers an app icon and menu entry — launch **Torrentina** from your app
launcher. To uninstall: `sudo apt remove torrentina-gui`.

Or run the standalone binary without installing:

```bash
./target/release/torrentina-gui
```

## Build from source

Requirements: a recent **Rust** toolchain, and for the GUI the Tauri Linux
system deps (`webkit2gtk-4.1`, `libsoup3`, `gtk3`) plus the Tauri CLI
(`cargo install tauri-cli`).

```bash
# Engine library + CLI
cargo build --release

# Desktop app (.deb bundle + standalone binary), run from the gui/ crate
cd gui && cargo tauri build --bundles deb
```

Artifacts land in the workspace-root `target/release/` (this is a Cargo
workspace, so not `gui/target/`).

## Usage

### Desktop GUI

```bash
cd gui && cargo tauri dev      # development window
```

Add a `.torrent` (path or **Browse…**), pick an output directory, and **Add
torrent**. Each torrent gets its own row with live progress, an estimated time
remaining, a collapsible peer table, and Pause/Resume + Remove controls. The
list and its statuses are saved to the app data dir and restored on next launch
(restored torrents come back *stopped*; press Start to resume).

### CLI

```bash
cargo run --release -- <TORRENT> [OPTIONS]
```

| Flag | Default | Description |
|------|---------|-------------|
| `<TORRENT>` | — | Path to the `.torrent` file |
| `-o, --output <DIR>` | `.` | Directory to download into |
| `-p, --port <PORT>` | `6881` | TCP port advertised to the tracker |
| `--numwant <N>` | `50` | Max peers to request from each tracker |

Example: `cargo run --release -- ubuntu.torrent -o ~/Downloads`

### Live demo (no network needed)

```bash
cargo run --example loopback_download
```

Spins up a local seeder over real TCP, downloads from it, writes to disk, and
verifies the result byte-for-byte — a self-contained proof of the full pipeline.

## Architecture

The engine is a library; the CLI and GUI are thin drivers over it. Pure logic
(parsing, piece geometry, file mapping, stats) is **synchronous and
unit-tested**; async code is a thin layer on top. Library errors are typed
(`thiserror`); the binaries use `anyhow`.

| # | Module | Path | Responsibility |
|---|--------|------|----------------|
| 1 | Metainfo / Bencode | `src/metainfo.rs` | Parse `.torrent`; typed `Metainfo`/`Info`; faithful `InfoHash` |
| 2 | Tracker | `src/tracker/` | HTTP(S) + UDP announce; peer-list parsing |
| 3 | Peer wire | `src/peer/` | Handshake, message codec, connection |
| 4 | Download manager | `src/download/` | `Coordinator`, rarest-first + endgame, SHA-1 verify, stats |
| 5 | Disk | `src/disk.rs` | Write verified pieces to the right file offsets |

Data flow: parse `.torrent` → announce to trackers → connect peers →
`Coordinator` downloads + verifies pieces → stream `VerifiedPiece`s → `Storage`
writes them to disk.

## Development

```bash
cargo test                              # 64 tests
cargo clippy --workspace --all-targets  # clean
```

```
src/            engine library (modules 1–5) + CLI (src/main.rs)
examples/       loopback_download.rs — live demo
gui/            Tauri v2 desktop app (torrentina-gui)
PROJECT_STATUS.md   detailed status, run instructions, next steps
CHANGELOG.md        notable changes
```

## Packaging for other platforms

This is a Tauri app, so Windows (`.msi`/`.exe`) and macOS (`.dmg`) bundles come
from `cargo tauri build` run **on those OSes** (Tauri does not cross-compile);
a GitHub Actions matrix with `tauri-apps/tauri-action` is the usual way to build
all three from one release. See `PROJECT_STATUS.md` for the full checklist.

## License

[MIT](./LICENSE) © 2026 Roger Pettersen.
