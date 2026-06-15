# Changelog

All notable changes to Torrentina are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Security

- **Path-traversal guard.** Every name component from a `.torrent` (the `info`
  `name` and each multi-file `path` element) is now validated before it is
  joined onto the download directory. A hostile torrent can no longer use `..`,
  an absolute path, or embedded separators to write files outside the chosen
  output directory (`Metainfo::file_paths`).

### Hardened

- **Peer connect/handshake timeouts.** `PeerConnection::connect` now wraps both
  the TCP dial and the handshake exchange in a 10s `tokio::time::timeout`, so a
  dead or unresponsive peer no longer pins a task for the OS-level SYN timeout.
- **Overflow-checked file layout.** The cumulative file-offset accumulation in
  `disk.rs` uses `checked_add`, rejecting a torrent whose declared sizes would
  wrap `u64` instead of silently writing to wrong offsets.

### Fixed

- **Availability decremented on disconnect.** A peer's contribution to the
  rarest-first availability counts is now undone when it disconnects (or
  replaces its bitfield), so pieces advertised by long-departed peers no longer
  look permanently common and distort piece selection.

### Changed

- `Bitfield::count`/`is_complete` are now O(1) (count maintained incrementally)
  instead of rescanning every piece on each `have`/`bitfield` message and
  progress snapshot.
- `Metainfo::all_trackers` de-duplicates via a `HashSet` (was O(n²)).
- The GUI now surfaces a log line when deleting a torrent's on-disk data fails,
  instead of silently swallowing the error.

## [0.1.0] — 2026-06-05

First complete, personal-use release: a from-scratch BitTorrent engine plus a
CLI and a multi-torrent Tauri desktop GUI.

### Added

- **Engine (modules 1–5).** Bencode/`.torrent` parsing; HTTP(S) + UDP (BEP 15)
  trackers with raw percent-encoding and compact/dict peer parsing; full peer
  wire protocol over async TCP; a `Coordinator` with rarest-first piece
  selection, 16 KiB pipelined requests, and per-piece SHA-1 verification;
  single- and multi-file disk output for straddling pieces.
- **CLI** (`torrentina <TORRENT> [-o DIR] [-p PORT] [--numwant N]`) with a
  progress bar.
- **Loopback example** — a self-contained real-TCP seeder→download→disk→verify
  demo.
- **Desktop GUI (Tauri v2)** — multiple concurrent torrents, per-torrent
  progress, **estimated time remaining**, collapsible peer table, pause / resume
  / cancel, a **3-way remove** (list only · list + data · data only), and a
  **persisted torrent list** restored on launch.

### Fixed

- **Info hash** is now computed from the *original* `info`-dict bytes via a raw
  bencode walk, instead of re-serializing the typed struct (which dropped
  unmodeled keys such as a top-level `md5sum`, `source`, or BitTorrent v2
  fields). This fixed trackers replying "torrent not found" / returning 0 peers.
- **Downloads stalling at ~99.9%.** Pieces stranded behind a choked, silent, or
  slow peer are now freed by releasing a reserved piece on choke, a per-request
  stall timeout (`Coordinator::set_request_timeout`, default 30s), and an
  endgame fallback (`PieceTracker::reserve_piece_endgame`) that lets an idle
  peer fetch a claimed-but-needed piece in parallel.
- **Download never reported complete** with idle peers connected. Completion is
  now broadcast so every peer task wakes and exits, allowing `run` to return,
  the disk flush to happen, and the final status to persist.
- **GUI**: a newly added torrent now appears in the list immediately (instead of
  only after a restart); the status badge reflects the snapshot directly, so it
  shows **finished**/**paused** reliably.
