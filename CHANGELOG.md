# Changelog

All notable changes to Torrentina are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/), and the project aims to follow
[Semantic Versioning](https://semver.org/).

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
