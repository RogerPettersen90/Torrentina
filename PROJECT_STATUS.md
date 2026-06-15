# Torrentina — Project Status

_Last updated: 2026-06-05_

> **2026-06-05 — stall fix + multi-torrent GUI.** Cured downloads hanging at
> ~99.9%: pieces stranded behind a choked/silent/slow peer are now freed by
> (a) releasing a reserved piece when the peer chokes us, (b) a per-request
> stall timeout (`Coordinator::set_request_timeout`, default 30s), and
> (c) an endgame fallback (`PieceTracker::reserve_piece_endgame`) that lets an
> idle peer fetch a claimed-but-needed piece in parallel. The GUI now runs
> **many torrents at once** (registry keyed by info-hash in `gui/src/main.rs`,
> per-torrent `stats`/`log`/`finished` events), **persists** the list to
> `torrents.json` in the app data dir (restored *stopped* on launch), and can
> **remove** a torrent three ways: list only / list + data / data only.
>
> **2026-06-05 — info-hash fix.** Trackers were returning "torrent not found"
> because the info hash was computed by re-serializing the typed `Info` struct,
> which silently dropped any info-dict key we don't model (e.g. the top-level
> `md5sum` in the LibreOffice torrent). Now we hash the **original** `info`-dict
> bytes via a raw bencode walk in `metainfo.rs`. The other three audited tracker
> pitfalls (binary percent-encoding, 6-byte compact peer strides, UDP BEP 15
> support) were already correct.

A lightweight, from-scratch BitTorrent client in Rust: an engine **library** + a
**CLI** + a **Tauri v2 desktop GUI**. Built incrementally, module by module,
with strict typing, `thiserror` error handling, `tokio` async, and tests at
every layer.

## ▶ Resume here tomorrow

Everything below compiles and is green: **61 tests pass, clippy clean across the
workspace.** Pick up from **"Next steps"** at the bottom. Quick sanity check
after reopening:

```bash
cargo test                       # 61 pass
cargo run --example loopback_download   # live end-to-end demo (downloads to disk, verifies SHA-1)
cd gui && cargo tauri dev        # launch the desktop GUI
```

## Status by module (all 5 + extras DONE)

| # | Module | File(s) | What it does |
|---|--------|---------|--------------|
| 1 | Metainfo / Bencode | `src/metainfo.rs` | Parse `.torrent`; typed `Metainfo`/`Info`; `InfoHash` (SHA-1 of the **original** `info`-dict bytes, captured by a raw bencode walk at parse time — never a lossy re-serialization, so it's faithful even for unmodeled keys like a top-level `md5sum`/`source`/BT v2 fields); piece-hash iterator; single/multi-file layout; path resolution |
| 2 | Tracker | `src/tracker/{mod,http,udp}.rs` | HTTP(S) + UDP (BEP 15) announce; correct binary percent-encoding; compact/dict peer parsing; one `announce()` dispatch on URL scheme |
| 3 | Peer wire | `src/peer/{mod,handshake,message,connection}.rs` | 68-byte handshake; full `Message` enum + `PeerCodec` (tokio_util `Decoder`/`Encoder`); `PeerConnection` (TCP + handshake + `Framed`) |
| 4 | Download manager | `src/download/{mod,bitfield,piece,state,stats}.rs` | `Coordinator` spawns a task per peer; rarest-first exclusive piece reservation **+ endgame fallback for stranded pieces**; **per-request stall timeout** (`set_request_timeout`); release-on-choke; 16 KiB pipelined block requests; SHA-1 verify; `VerifiedPiece` channel; **pause/resume/cancel**; live `StatsSnapshot` |
| 5 | Disk | `src/disk.rs` | `Storage` writes verified pieces to correct file offsets (single + multi-file, straddling pieces); `assemble()` drains the channel to disk |

**Extras done:**
- `src/main.rs` — CLI (`torrentina <TORRENT> [-o DIR] [-p PORT] [--numwant N]`) with progress bar.
- `examples/loopback_download.rs` — self-contained live demo (real TCP seeder → real download → disk → byte-for-byte verify).
- `gui/` — Tauri v2 desktop app (see below).

## GUI (Tauri v2) — `gui/`

- **Multi-torrent**: `gui/src/main.rs` holds a registry
  `AppState { Mutex<HashMap<id, Handle>> }` keyed by info-hash hex. Commands:
  `add_torrent`→id, `start_torrent`, `pause_download`/`resume_download`/
  `cancel_download` (all by id), `remove_torrent(id, mode)` where mode ∈
  `list | list_and_data | data_only`, and `list_torrents`. Events are tagged
  with `id`: `stats` (`{id, ...StatsSnapshot}` ~2×/s), `log` (`{id,level,message}`),
  `finished` (`{id, status}`, status `complete|stopped|stalled|error`).
- **Persistence**: the registry is written to `torrents.json` in the app data
  dir after every change and reloaded on startup (a `setup` hook); restored
  entries come back **stopped** (not auto-resumed — engine has no disk-resume).
- `gui/dist/{index.html,styles.css,app.js}` — vanilla JS frontend (no bundler;
  uses `withGlobalTauri`). One row per torrent (progress, metrics, collapsible
  peer table), global log pane, Browse/Add controls, per-row Start/Pause-Resume,
  and a 3-option Remove modal.
- `gui/tauri.conf.json`, `gui/capabilities/default.json`, `gui/build.rs`,
  `gui/icons/` (generated). Plugin: `tauri-plugin-dialog`.
- Repo is a Cargo **workspace**: root crate `torrentina` (lib + CLI bin) +
  member `gui` (`torrentina-gui`).

## How to run

```bash
# Tests + lint
cargo test
cargo clippy --workspace --all-targets

# CLI
cargo run -- path/to/file.torrent -o ./downloads

# Live demo (no network needed; proves the full pipeline)
cargo run --example loopback_download

# Desktop GUI (needs a display)
cd gui && cargo tauri dev          # dev window
cargo tauri build                  # bundled app
```

Toolchain present on this machine: Rust 1.95, Node 22, tauri-cli 2.11,
webkit2gtk-4.1, gtk3, libsoup3.

## Known limitations / deliberate simplifications

- **No periodic re-announce** — we announce once (`Started`) to get peers; we
  don't re-announce on the tracker `interval`, send `Completed`/`Stopped`, or
  rotate `announce-list` tiers beyond the first that yields peers.
- **No resume-from-disk** — completed pieces are written but on restart we don't
  scan existing files to rebuild the `have` bitfield; a download starts over.
  (The GUI persists the torrent *list*, but a restored incomplete torrent
  re-downloads from zero when Started.)
- **Pause drops the partial in-flight piece** — completed+verified pieces are
  kept, but the one piece a peer was mid-way through restarts on resume.
- **Endgame is coarse** — an idle peer may re-fetch a stranded piece in parallel
  (cures the 99.9% stall), but we don't send `Cancel` to the loser, so the final
  piece can be fetched twice. Correct, slightly wasteful at the tail.
- **No upload/seeding** — leech only; we never serve pieces to other peers.
- **Availability not decremented on disconnect** — only affects rarest-first
  precision, not correctness.

## Next steps (suggested priority order)

1. **Handshake timeout** — `download/mod.rs` now has a per-request stall
   timeout; still worth wrapping `PeerConnection::connect`/handshake in
   `tokio::time::timeout` so a dead peer's task winds down fast.
2. **Periodic re-announce** — re-announce on `interval`, send `Completed` when
   done and `Stopped` on cancel/exit. Lives in the CLI/GUI orchestration +
   maybe a small loop in `tracker`.
3. **Resume from disk** — on `Storage::create`, read existing files, hash each
   piece, and pre-populate `PieceTracker.have`. Touches `disk.rs` +
   `download/state.rs`. Would make the GUI's persisted list truly resumable.
4. **Endgame `Cancel`** — send `Cancel` for the duplicated tail piece once one
   peer delivers it, to avoid the small double-fetch at the very end.
5. **GUI polish** — native completion notification, persist last-used paths,
   per-torrent speed limits. (Multiple concurrent torrents + list persistence
   are now done.)
6. **Seeding/upload** — accept inbound connections, serve `Piece` on request.

## Conventions in this codebase (keep consistent)

- Pure logic (parsing, bit math, piece geometry, file mapping, stats) is kept
  **synchronous and unit-tested**; async code is a thin driver on top.
- Errors: library uses typed `crate::Error` (`thiserror`, in `src/error.rs`);
  binaries (CLI, GUI) use `anyhow` at the top level.
- Every module has a `#[cfg(test)]` block; networked paths are proven with
  real-TCP loopback tests (see `peer/connection.rs`, `download/mod.rs`).
- Standard crates only: tokio, serde, serde_bencode, sha1, bytes, tokio-util,
  reqwest (rustls), thiserror/anyhow, tauri.
