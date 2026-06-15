//! Module 4: Torrent manager & piece download logic.
//!
//! [`Coordinator`] drives the whole download. It holds the shared
//! [`PieceTracker`] and, for a given peer list, spawns one async task per peer.
//! Each task runs a small state machine over a [`PeerConnection`]:
//!
//! * announce `Interested` and wait to be unchoked,
//! * `reserve` a piece to work on (rarest-first, exclusive per peer),
//! * pipeline up to [`MAX_PIPELINED_REQUESTS`] block requests,
//! * assemble the piece, verify its SHA-1, and emit a [`VerifiedPiece`],
//! * release the piece on bad hash or disconnect so another peer can retry.
//!
//! Verified pieces are streamed out over an `mpsc` channel for Module 5 to
//! write to disk.

pub mod bitfield;
pub mod piece;
pub mod state;
pub mod stats;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinSet;

use crate::error::Result;
use crate::metainfo::{Info, InfoHash};
use crate::peer::{Block, Message, PeerConnection, PeerId};

pub use bitfield::Bitfield;
pub use piece::{Geometry, PieceAssembler, BLOCK_SIZE};
pub use state::PieceTracker;
pub use stats::{PeerStat, StatsSnapshot, SwarmStats};

/// How many block requests we keep in flight per peer at once. Pipelining hides
/// the round-trip latency between request and delivery.
pub const MAX_PIPELINED_REQUESTS: u32 = 5;

/// How long a peer may hold an in-flight piece without delivering any block
/// before we give up on it and release the piece for someone else to fetch.
/// This is the primary defence against a peer that reserves a (near-final)
/// piece and then goes silent — the classic "stuck at 99.9%" cause.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Runtime control state for a download, broadcast to every peer task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlState {
    /// Actively downloading.
    Running,
    /// Temporarily suspended: peers stop requesting and idle until resumed.
    Paused,
    /// Aborted: peer tasks shut down and [`Coordinator::run`] returns.
    Cancelled,
}

/// A fully downloaded and SHA-1-verified piece, ready to be written to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPiece {
    /// Zero-based piece index.
    pub index: u32,
    /// The verified piece bytes.
    pub data: Bytes,
}

/// Drives a torrent download across a set of peers.
pub struct Coordinator {
    name: String,
    geometry: Geometry,
    tracker: Arc<Mutex<PieceTracker>>,
    stats: Arc<SwarmStats>,
    control: watch::Sender<ControlState>,
    /// Flipped to `true` by whichever peer task completes the final piece, so
    /// every *other* peer task wakes from its `recv()` and exits promptly
    /// (otherwise idle peers block forever and `run` never returns). Shared in
    /// an `Arc` so any task can signal it.
    done: Arc<watch::Sender<bool>>,
    info_hash: InfoHash,
    peer_id: PeerId,
    request_timeout: Duration,
}

impl Coordinator {
    /// Build a coordinator for a torrent.
    pub fn new(info: &Info, info_hash: InfoHash, peer_id: PeerId) -> Result<Self> {
        let (control, _) = watch::channel(ControlState::Running);
        let (done, _) = watch::channel(false);
        Ok(Coordinator {
            name: info.name.clone(),
            geometry: Geometry::from_info(info)?,
            tracker: Arc::new(Mutex::new(PieceTracker::from_info(info)?)),
            stats: Arc::new(SwarmStats::new()),
            control,
            done: Arc::new(done),
            info_hash,
            peer_id,
            request_timeout: REQUEST_TIMEOUT,
        })
    }

    /// Override the per-request stall timeout (mainly for tests; production uses
    /// the [`REQUEST_TIMEOUT`] default).
    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.request_timeout = timeout;
    }

    /// Suspend the download: peers stop requesting new blocks and idle.
    pub fn pause(&self) {
        let _ = self.control.send(ControlState::Paused);
    }

    /// Resume a paused download.
    pub fn resume(&self) {
        let _ = self.control.send(ControlState::Running);
    }

    /// Abort the download: every peer task shuts down and `run` returns.
    pub fn cancel(&self) {
        let _ = self.control.send(ControlState::Cancelled);
    }

    /// The current control state.
    pub fn control_state(&self) -> ControlState {
        *self.control.borrow()
    }

    /// Shared tracker handle (e.g. to inspect progress).
    pub fn tracker(&self) -> Arc<Mutex<PieceTracker>> {
        Arc::clone(&self.tracker)
    }

    /// Shared live-stats handle.
    pub fn stats(&self) -> Arc<SwarmStats> {
        Arc::clone(&self.stats)
    }

    /// A point-in-time snapshot of download progress for display.
    pub async fn snapshot(&self) -> StatsSnapshot {
        let tracker = self.tracker.lock().await;
        let total_pieces = tracker.num_pieces() as u32;
        let have = tracker.have_bitfield();
        let mut pieces_done = 0u32;
        let mut bytes_done = 0u64;
        for i in 0..total_pieces {
            if have.has(i as usize) {
                pieces_done += 1;
                bytes_done += self.geometry.piece_length(i) as u64;
            }
        }
        StatsSnapshot {
            name: self.name.clone(),
            total_pieces,
            pieces_done,
            total_bytes: self.geometry.total_length(),
            bytes_done,
            wire_bytes: self.stats.total_downloaded(),
            connected_peers: self.stats.connected_count(),
            complete: tracker.is_complete(),
            paused: self.control_state() == ControlState::Paused,
            peers: self.stats.peer_snapshots(),
        }
    }

    /// Connect to every peer and download until the torrent is complete (or all
    /// peers are exhausted). Verified pieces are sent on `completed`; when this
    /// future resolves, every peer task has finished and `completed`'s matching
    /// receiver will observe the channel closing once `completed` is dropped.
    pub async fn run(&self, peers: Vec<SocketAddr>, completed: mpsc::Sender<VerifiedPiece>) {
        let mut tasks = JoinSet::new();
        for addr in peers {
            let geometry = self.geometry;
            let tracker = Arc::clone(&self.tracker);
            let stats = Arc::clone(&self.stats);
            let control = self.control.subscribe();
            let info_hash = self.info_hash;
            let peer_id = self.peer_id;
            let request_timeout = self.request_timeout;
            let done_tx = Arc::clone(&self.done);
            let done_rx = self.done.subscribe();
            let completed = completed.clone();
            tasks.spawn(async move {
                // A failure against one peer must not abort the whole download.
                let _ = peer_task(
                    addr, info_hash, peer_id, geometry, tracker, stats, control, completed,
                    request_timeout, done_tx, done_rx,
                )
                .await;
            });
        }
        // Drop our own sender so the receiver closes once all tasks are done.
        drop(completed);
        while tasks.join_next().await.is_some() {}
    }
}

/// Connect to one peer, then run its download session, keeping the swarm stats
/// registry in sync for the lifetime of the connection.
#[allow(clippy::too_many_arguments)]
async fn peer_task(
    addr: SocketAddr,
    info_hash: InfoHash,
    peer_id: PeerId,
    geometry: Geometry,
    tracker: Arc<Mutex<PieceTracker>>,
    stats: Arc<SwarmStats>,
    control: watch::Receiver<ControlState>,
    completed: mpsc::Sender<VerifiedPiece>,
    request_timeout: Duration,
    done_tx: Arc<watch::Sender<bool>>,
    done_rx: watch::Receiver<bool>,
) -> Result<()> {
    let conn = PeerConnection::connect(addr, info_hash, peer_id).await?;
    stats.peer_connected(addr);
    let result = run_session(
        conn, geometry, tracker, &stats, addr, control, completed, request_timeout, done_tx,
        done_rx,
    )
    .await;
    stats.peer_disconnected(addr);
    result
}

/// The per-peer download state machine over an established connection.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    mut conn: PeerConnection,
    geometry: Geometry,
    tracker: Arc<Mutex<PieceTracker>>,
    stats: &SwarmStats,
    addr: SocketAddr,
    mut control: watch::Receiver<ControlState>,
    completed: mpsc::Sender<VerifiedPiece>,
    request_timeout: Duration,
    done_tx: Arc<watch::Sender<bool>>,
    mut done_rx: watch::Receiver<bool>,
) -> Result<()> {
    let num_pieces = geometry.num_pieces() as usize;
    let mut peer_has = Bitfield::new(num_pieces);
    let mut choked = true;

    // Express interest immediately so the peer can decide to unchoke us.
    conn.send(Message::Interested).await?;

    // The piece we're currently downloading, if any.
    let mut current: Option<PieceAssembler> = None;
    let mut next_block: u32 = 0; // next block index to request for `current`
    let mut in_flight: u32 = 0; // outstanding (requested, unanswered) blocks

    loop {
        // Honour the current control state before doing any work. Copy it out
        // so the `watch::Ref` is released before we may `.await` on `control`.
        let state = *control.borrow_and_update();
        match state {
            ControlState::Cancelled => {
                if let Some(asm) = &current {
                    tracker.lock().await.release_piece(asm.index());
                }
                break;
            }
            ControlState::Paused => {
                // Drop any in-progress piece so other state stays clean, then
                // idle until the control state changes (resume or cancel).
                if let Some(asm) = current.take() {
                    tracker.lock().await.release_piece(asm.index());
                    next_block = 0;
                    in_flight = 0;
                }
                if control.changed().await.is_err() {
                    break; // coordinator dropped
                }
                continue;
            }
            ControlState::Running => {}
        }

        // If idle and unchoked, try to claim a new piece to work on.
        if current.is_none() && !choked {
            let mut t = tracker.lock().await;
            let reserved = t.reserve_piece(&peer_has);
            let reserved = match reserved {
                Some(index) => Some(index),
                // Nothing left to claim exclusively. If the torrent is done we
                // stop; otherwise every remaining piece is already claimed by
                // (possibly stalled) peers, so enter endgame and help fetch one
                // in parallel rather than idling while the last piece hangs.
                None if t.is_complete() => {
                    drop(t);
                    let _ = done_tx.send(true); // ensure siblings wake too
                    break; // torrent done; nothing left for anyone
                }
                None => t.reserve_piece_endgame(&peer_has),
            };
            drop(t);
            if let Some(index) = reserved {
                current = Some(PieceAssembler::new(index, geometry.piece_length(index)));
                next_block = 0;
                in_flight = 0;
            }
        }

        // Refill the request pipeline for the current piece.
        if !choked {
            if let Some(asm) = &current {
                let index = asm.index();
                let total_blocks = geometry.num_blocks(index);
                while in_flight < MAX_PIPELINED_REQUESTS && next_block < total_blocks {
                    conn.send(Message::Request(geometry.block_info(index, next_block)))
                        .await?;
                    next_block += 1;
                    in_flight += 1;
                }
            }
        }

        // Await the next message, but wake promptly if the control state
        // changes (so pause/cancel take effect without waiting on the peer),
        // and give up on a piece whose blocks we requested but that never
        // arrive (a silent peer must not strand its piece forever).
        let recv_result = tokio::select! {
            _ = control.changed() => continue,
            // Another peer completed the torrent: stop waiting and exit.
            _ = done_rx.changed() => break,
            _ = tokio::time::sleep(request_timeout), if current.is_some() && in_flight > 0 => {
                if let Some(asm) = current.take() {
                    tracker.lock().await.release_piece(asm.index());
                }
                next_block = 0;
                in_flight = 0;
                continue;
            }
            res = conn.recv() => res,
        };
        let Some(message) = recv_result? else {
            // Peer closed the connection; relinquish any in-progress piece.
            if let Some(asm) = &current {
                tracker.lock().await.release_piece(asm.index());
            }
            break;
        };

        match message {
            Message::Choke => {
                choked = true;
                stats.set_choked(addr, true);
                // A choked peer can't deliver, so don't let it keep its piece
                // reserved — release it so another peer can pick it up.
                if let Some(asm) = current.take() {
                    tracker.lock().await.release_piece(asm.index());
                    next_block = 0;
                    in_flight = 0;
                }
            }
            Message::Unchoke => {
                choked = false;
                stats.set_choked(addr, false);
            }
            Message::Have(index) => {
                peer_has.set(index as usize);
                tracker.lock().await.add_availability(index as usize);
                stats.set_have_count(addr, peer_has.count() as u32);
            }
            Message::Bitfield(bytes) => {
                peer_has = Bitfield::from_bytes(bytes.to_vec(), num_pieces)?;
                tracker.lock().await.add_bitfield_availability(&peer_has);
                stats.set_have_count(addr, peer_has.count() as u32);
            }
            Message::Piece(block) => {
                stats.record_block(addr, block.data.len() as u64);
                let torrent_complete =
                    handle_block(block, &mut current, &mut in_flight, &tracker, &completed).await?;
                if torrent_complete {
                    // Wake every other peer task so they exit too and `run`
                    // can return.
                    let _ = done_tx.send(true);
                    break; // that block completed the final piece
                }
            }
            // Choke/interest from the peer about *us*, DHT port, etc.: ignore.
            _ => {}
        }
    }

    Ok(())
}

/// Apply a received block to the current piece. Returns `Ok(true)` if the
/// torrent is now complete and the session should stop.
async fn handle_block(
    block: Block,
    current: &mut Option<PieceAssembler>,
    in_flight: &mut u32,
    tracker: &Arc<Mutex<PieceTracker>>,
    completed: &mpsc::Sender<VerifiedPiece>,
) -> Result<bool> {
    // Ignore blocks that don't belong to the piece we're assembling.
    let Some(asm) = current.as_mut() else {
        return Ok(false);
    };
    if block.index != asm.index() {
        return Ok(false);
    }

    asm.add_block(block.begin, &block.data)?;
    *in_flight = in_flight.saturating_sub(1);

    if !asm.is_complete() {
        return Ok(false);
    }

    // Piece is whole: take it out, verify, and either keep or retry it.
    let asm = current.take().expect("current is Some");
    let index = asm.index();
    let data = asm.into_bytes();

    let mut tracker = tracker.lock().await;
    // In endgame, another peer may have already completed this same piece.
    if tracker.has(index) {
        return Ok(tracker.is_complete());
    }
    if tracker.verify(index, &data) {
        tracker.mark_have(index);
        let complete = tracker.is_complete();
        drop(tracker);
        let _ = completed.send(VerifiedPiece { index, data }).await;
        return Ok(complete);
    }

    // Bad hash: discard and let it be re-reserved.
    tracker.release_piece(index);
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::Info;
    use crate::peer::connection::handshake;
    use crate::peer::message::PeerCodec;
    use futures_util::{SinkExt, StreamExt};
    use sha1::{Digest, Sha1};
    use tokio::net::TcpListener;
    use tokio_util::codec::Framed;

    /// Build deterministic content plus a matching single-file `Info`.
    fn make_torrent(total: usize, piece_len: usize) -> (Vec<u8>, Info) {
        let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();
        let mut pieces = Vec::new();
        for chunk in data.chunks(piece_len) {
            pieces.extend_from_slice(&Sha1::digest(chunk));
        }
        let info = Info {
            name: "test".into(),
            piece_length: piece_len as u64,
            pieces,
            length: Some(total as u64),
            files: None,
            private: None,
        };
        (data, info)
    }

    /// A minimal seeder: handshake, advertise all pieces, unchoke, then answer
    /// every block request with the correct slice of `data`.
    async fn run_seeder(
        listener: TcpListener,
        info_hash: InfoHash,
        data: Vec<u8>,
        piece_len: usize,
        num_pieces: usize,
        block_delay: std::time::Duration,
    ) {
        let (mut stream, _) = listener.accept().await.unwrap();
        handshake(&mut stream, info_hash, PeerId(*b"-TN0001-SEEDSEEDSEED"))
            .await
            .unwrap();

        let mut full = Bitfield::new(num_pieces);
        for i in 0..num_pieces {
            full.set(i);
        }
        let mut framed = Framed::new(stream, PeerCodec::new());
        framed
            .send(Message::Bitfield(full.to_message_bytes()))
            .await
            .unwrap();
        framed.send(Message::Unchoke).await.unwrap();

        // Serve requests until the client disconnects.
        while let Some(Ok(msg)) = framed.next().await {
            if let Message::Request(bi) = msg {
                let start = bi.index as usize * piece_len + bi.begin as usize;
                let end = start + bi.length as usize;
                let block = Bytes::copy_from_slice(&data[start..end]);
                if !block_delay.is_zero() {
                    tokio::time::sleep(block_delay).await;
                }
                framed
                    .send(Message::Piece(Block {
                        index: bi.index,
                        begin: bi.begin,
                        data: block,
                    }))
                    .await
                    .unwrap();
            }
        }
    }

    /// A seeder that advertises only `advertise` pieces and silently *withholds*
    /// every block request for piece `withhold` (simulating a peer that reserves
    /// a piece and then stalls). Stays connected so the piece would hang forever
    /// without the endgame/timeout fallbacks.
    async fn run_partial_seeder(
        listener: TcpListener,
        info_hash: InfoHash,
        data: Vec<u8>,
        piece_len: usize,
        num_pieces: usize,
        advertise: Vec<u32>,
        withhold: Option<u32>,
    ) {
        let (mut stream, _) = listener.accept().await.unwrap();
        handshake(&mut stream, info_hash, PeerId(*b"-TN0001-PARTIALSEED1"))
            .await
            .unwrap();

        let mut bf = Bitfield::new(num_pieces);
        for i in &advertise {
            bf.set(*i as usize);
        }
        let mut framed = Framed::new(stream, PeerCodec::new());
        framed.send(Message::Bitfield(bf.to_message_bytes())).await.unwrap();
        framed.send(Message::Unchoke).await.unwrap();

        while let Some(Ok(msg)) = framed.next().await {
            if let Message::Request(bi) = msg {
                if withhold == Some(bi.index) {
                    continue; // hold this piece hostage
                }
                let start = bi.index as usize * piece_len + bi.begin as usize;
                let end = start + bi.length as usize;
                framed
                    .send(Message::Piece(Block {
                        index: bi.index,
                        begin: bi.begin,
                        data: Bytes::copy_from_slice(&data[start..end]),
                    }))
                    .await
                    .unwrap();
            }
        }
    }

    #[tokio::test]
    async fn endgame_recovers_a_piece_stranded_by_a_stalled_peer() {
        // 4 pieces. One peer advertises only the last piece and then withholds
        // it forever; a second peer has everything. Without endgame the last
        // piece would stay claimed by the stalled peer and the download would
        // hang at 3/4 (the "99.9%" bug). With it, the second peer fetches it.
        let piece_len = 16384;
        let num_pieces = 4u32;
        let total = num_pieces as usize * piece_len;
        let (data, info) = make_torrent(total, piece_len);
        let info_hash = info.info_hash().unwrap();
        let last = num_pieces - 1;

        // Stalling peer: only has the last piece, never serves it.
        let stall_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stall_addr = stall_listener.local_addr().unwrap();
        let stall = tokio::spawn(run_partial_seeder(
            stall_listener,
            info_hash,
            data.clone(),
            piece_len,
            num_pieces as usize,
            vec![last],
            Some(last),
        ));

        // Full peer: has and serves everything.
        let full_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let full_addr = full_listener.local_addr().unwrap();
        let full = tokio::spawn(run_seeder(
            full_listener,
            info_hash,
            data.clone(),
            piece_len,
            num_pieces as usize,
            std::time::Duration::from_millis(5),
        ));

        let mut coordinator = Coordinator::new(&info, info_hash, PeerId::generate()).unwrap();
        // Short timeout so the stalled peer's task winds down quickly once done.
        coordinator.set_request_timeout(std::time::Duration::from_millis(200));
        let coordinator = Arc::new(coordinator);
        let (tx, mut rx) = mpsc::channel::<VerifiedPiece>(num_pieces as usize);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let run_coord = Arc::clone(&coordinator);
        let run = tokio::spawn(async move { run_coord.run(vec![stall_addr, full_addr], tx).await });

        wait_for_pieces(&coordinator, num_pieces).await;
        assert!(coordinator.tracker().lock().await.is_complete());

        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("run should return after completion")
            .unwrap();

        stall.abort();
        full.abort();
    }

    /// A peer that advertises an *empty* bitfield (has nothing we can request),
    /// unchokes, and then stays connected and silent. Models the real-world case
    /// of an idle peer that must not keep `run` alive after the torrent finishes.
    async fn run_idle_peer(listener: TcpListener, info_hash: InfoHash, num_pieces: usize) {
        let (mut stream, _) = listener.accept().await.unwrap();
        handshake(&mut stream, info_hash, PeerId(*b"-TN0001-IDLEIDLEIDLE"))
            .await
            .unwrap();
        let mut framed = Framed::new(stream, PeerCodec::new());
        framed
            .send(Message::Bitfield(Bitfield::new(num_pieces).to_message_bytes()))
            .await
            .unwrap();
        framed.send(Message::Unchoke).await.unwrap();
        // Sit idle, consuming and ignoring anything, until the client hangs up.
        while let Some(Ok(_)) = framed.next().await {}
    }

    #[tokio::test]
    async fn run_returns_when_complete_even_with_idle_peers_connected() {
        // One seeder has everything; a second peer is connected but idle (has no
        // pieces) and never disconnects. Before completion was broadcast, that
        // idle peer blocked in `recv()` forever and `run` never returned — the
        // "stuck on DOWNLOADING at 100%" bug. `run` must now return promptly.
        let piece_len = 16384;
        let num_pieces = 4usize;
        let total = num_pieces * piece_len;
        let (data, info) = make_torrent(total, piece_len);
        let info_hash = info.info_hash().unwrap();

        let full_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let full_addr = full_listener.local_addr().unwrap();
        let full = tokio::spawn(run_seeder(
            full_listener,
            info_hash,
            data,
            piece_len,
            num_pieces,
            std::time::Duration::ZERO,
        ));

        let idle_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let idle_addr = idle_listener.local_addr().unwrap();
        let idle = tokio::spawn(run_idle_peer(idle_listener, info_hash, num_pieces));

        // Default (long) request timeout: if `run` returns quickly it's the
        // completion broadcast doing it, not a timeout firing.
        let coordinator = Arc::new(Coordinator::new(&info, info_hash, PeerId::generate()).unwrap());
        let (tx, mut rx) = mpsc::channel::<VerifiedPiece>(num_pieces);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let run_coord = Arc::clone(&coordinator);
        let run = tokio::spawn(async move { run_coord.run(vec![full_addr, idle_addr], tx).await });

        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("run must return promptly once complete, despite the idle peer")
            .unwrap();
        assert!(coordinator.tracker().lock().await.is_complete());

        full.abort();
        idle.abort();
    }

    /// Wait until `coordinator` reports at least `target` verified pieces,
    /// giving up after a generous timeout to avoid hanging a failed test.
    async fn wait_for_pieces(coordinator: &Coordinator, target: u32) {
        for _ in 0..200 {
            if coordinator.snapshot().await.pieces_done >= target {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {target} piece(s)");
    }

    #[tokio::test]
    async fn downloads_and_verifies_whole_torrent_from_a_peer() {
        // 2 full 32 KiB pieces (2 blocks each) + a short 5000-byte tail piece.
        let piece_len = 32768;
        let total = 2 * piece_len + 5000;
        let (data, info) = make_torrent(total, piece_len);
        let info_hash = info.info_hash().unwrap();
        let num_pieces = info.num_pieces();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seeder = tokio::spawn(run_seeder(
            listener,
            info_hash,
            data.clone(),
            piece_len,
            num_pieces,
            std::time::Duration::ZERO,
        ));

        let coordinator = Coordinator::new(&info, info_hash, PeerId::generate()).unwrap();
        let (tx, mut rx) = mpsc::channel::<VerifiedPiece>(num_pieces.max(1));

        // Collect verified pieces concurrently with the download running.
        let collector = tokio::spawn(async move {
            let mut assembled = vec![0u8; total];
            let mut got = 0;
            while let Some(p) = rx.recv().await {
                let start = p.index as usize * piece_len;
                assembled[start..start + p.data.len()].copy_from_slice(&p.data);
                got += 1;
            }
            (assembled, got)
        });

        coordinator.run(vec![addr], tx).await;
        let (assembled, got) = collector.await.unwrap();

        assert_eq!(got, num_pieces, "every piece should be delivered once");
        assert_eq!(assembled, data, "reconstructed content must match original");
        assert!(coordinator.tracker().lock().await.is_complete());

        seeder.abort();
    }

    #[tokio::test]
    async fn cancel_stops_the_download_before_completion() {
        // 8 pieces, served slowly so we can cancel mid-flight.
        let piece_len = 16384;
        let total = 8 * piece_len;
        let (data, info) = make_torrent(total, piece_len);
        let info_hash = info.info_hash().unwrap();
        let num_pieces = info.num_pieces();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seeder = tokio::spawn(run_seeder(
            listener,
            info_hash,
            data,
            piece_len,
            num_pieces,
            std::time::Duration::from_millis(15),
        ));

        let coordinator = Arc::new(Coordinator::new(&info, info_hash, PeerId::generate()).unwrap());
        let (tx, mut rx) = mpsc::channel::<VerifiedPiece>(num_pieces);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let run_coord = Arc::clone(&coordinator);
        let run = tokio::spawn(async move { run_coord.run(vec![addr], tx).await });

        // Let one piece land, then cancel.
        wait_for_pieces(&coordinator, 1).await;
        coordinator.cancel();

        // `run` must return promptly after cancellation.
        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("run should return after cancel")
            .unwrap();

        assert_eq!(coordinator.control_state(), ControlState::Cancelled);
        assert!(
            !coordinator.tracker().lock().await.is_complete(),
            "cancelled download must not be complete"
        );
        seeder.abort();
    }

    #[tokio::test]
    async fn pause_halts_progress_and_resume_completes() {
        let piece_len = 16384;
        let total = 6 * piece_len;
        let (data, info) = make_torrent(total, piece_len);
        let info_hash = info.info_hash().unwrap();
        let num_pieces = info.num_pieces();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seeder = tokio::spawn(run_seeder(
            listener,
            info_hash,
            data,
            piece_len,
            num_pieces,
            std::time::Duration::from_millis(15),
        ));

        let coordinator = Arc::new(Coordinator::new(&info, info_hash, PeerId::generate()).unwrap());
        let (tx, mut rx) = mpsc::channel::<VerifiedPiece>(num_pieces);
        tokio::spawn(async move { while rx.recv().await.is_some() {} });

        let run_coord = Arc::clone(&coordinator);
        let run = tokio::spawn(async move { run_coord.run(vec![addr], tx).await });

        // Pause after the first piece and confirm progress stops.
        wait_for_pieces(&coordinator, 1).await;
        coordinator.pause();
        assert!(coordinator.snapshot().await.paused);
        let after_pause = coordinator.snapshot().await.pieces_done;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let still = coordinator.snapshot().await.pieces_done;
        assert_eq!(after_pause, still, "no pieces should complete while paused");

        // Resume and let it finish.
        coordinator.resume();
        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("run should finish after resume")
            .unwrap();
        assert!(coordinator.tracker().lock().await.is_complete());
        seeder.abort();
    }
}
