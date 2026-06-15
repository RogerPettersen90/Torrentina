//! Live download statistics, shared between the engine and any frontend.
//!
//! [`SwarmStats`] is updated by the peer tasks as they connect, get
//! choked/unchoked, learn peer bitfields, and receive blocks. A
//! [`StatsSnapshot`] is a serializable point-in-time view the CLI or GUI can
//! render; a frontend derives download *rate* from the delta between snapshots.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde::Serialize;

/// Per-peer live state.
#[derive(Debug, Clone)]
struct PeerEntry {
    /// Whether this peer is currently choking us.
    choked: bool,
    /// How many pieces the peer advertises having.
    have_pieces: u32,
    /// Bytes received from this peer so far.
    downloaded: u64,
}

impl Default for PeerEntry {
    fn default() -> Self {
        // Peers start out choking us until they send `Unchoke`.
        PeerEntry {
            choked: true,
            have_pieces: 0,
            downloaded: 0,
        }
    }
}

/// Shared, concurrently-updated swarm statistics.
#[derive(Debug, Default)]
pub struct SwarmStats {
    /// Total wire bytes received across all peers (drives the rate display).
    total_downloaded: AtomicU64,
    /// Per-peer state, keyed by remote address.
    peers: Mutex<HashMap<SocketAddr, PeerEntry>>,
}

impl SwarmStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly connected peer.
    pub fn peer_connected(&self, addr: SocketAddr) {
        self.peers.lock().unwrap().insert(addr, PeerEntry::default());
    }

    /// Remove a peer that has disconnected.
    pub fn peer_disconnected(&self, addr: SocketAddr) {
        self.peers.lock().unwrap().remove(&addr);
    }

    /// Update whether a peer is choking us.
    pub fn set_choked(&self, addr: SocketAddr, choked: bool) {
        if let Some(entry) = self.peers.lock().unwrap().get_mut(&addr) {
            entry.choked = choked;
        }
    }

    /// Record how many pieces a peer advertises.
    pub fn set_have_count(&self, addr: SocketAddr, have_pieces: u32) {
        if let Some(entry) = self.peers.lock().unwrap().get_mut(&addr) {
            entry.have_pieces = have_pieces;
        }
    }

    /// Record `len` bytes received from a peer.
    pub fn record_block(&self, addr: SocketAddr, len: u64) {
        self.total_downloaded.fetch_add(len, Ordering::Relaxed);
        if let Some(entry) = self.peers.lock().unwrap().get_mut(&addr) {
            entry.downloaded += len;
        }
    }

    /// Number of currently connected peers.
    pub fn connected_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }

    /// Total wire bytes received so far.
    pub fn total_downloaded(&self) -> u64 {
        self.total_downloaded.load(Ordering::Relaxed)
    }

    /// A sorted snapshot of per-peer state for display.
    pub fn peer_snapshots(&self) -> Vec<PeerStat> {
        let mut peers: Vec<PeerStat> = self
            .peers
            .lock()
            .unwrap()
            .iter()
            .map(|(addr, e)| PeerStat {
                addr: addr.to_string(),
                choked: e.choked,
                have_pieces: e.have_pieces,
                downloaded_bytes: e.downloaded,
            })
            .collect();
        peers.sort_by_key(|p| std::cmp::Reverse(p.downloaded_bytes));
        peers
    }
}

/// A single peer's state in a snapshot.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PeerStat {
    pub addr: String,
    pub choked: bool,
    pub have_pieces: u32,
    pub downloaded_bytes: u64,
}

/// A serializable, point-in-time view of overall download progress.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Torrent name.
    pub name: String,
    /// Total number of pieces.
    pub total_pieces: u32,
    /// Pieces downloaded and verified.
    pub pieces_done: u32,
    /// Total content length in bytes.
    pub total_bytes: u64,
    /// Bytes accounted for by verified pieces.
    pub bytes_done: u64,
    /// Total wire bytes received (>= `bytes_done`; includes any re-downloads).
    pub wire_bytes: u64,
    /// Currently connected peers.
    pub connected_peers: usize,
    /// Whether the whole torrent is complete.
    pub complete: bool,
    /// Whether the download is currently paused.
    pub paused: bool,
    /// Per-peer detail.
    pub peers: Vec<PeerStat>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(p: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], p))
    }

    #[test]
    fn tracks_peer_lifecycle_and_bytes() {
        let s = SwarmStats::new();
        s.peer_connected(addr(1));
        s.peer_connected(addr(2));
        assert_eq!(s.connected_count(), 2);

        s.set_choked(addr(1), false);
        s.set_have_count(addr(1), 5);
        s.record_block(addr(1), 16384);
        s.record_block(addr(2), 1024);

        assert_eq!(s.total_downloaded(), 16384 + 1024);

        let snaps = s.peer_snapshots();
        // Sorted by bytes downloaded, descending.
        assert_eq!(snaps[0].addr, addr(1).to_string());
        assert!(!snaps[0].choked);
        assert_eq!(snaps[0].have_pieces, 5);
        assert_eq!(snaps[0].downloaded_bytes, 16384);

        s.peer_disconnected(addr(1));
        assert_eq!(s.connected_count(), 1);
    }
}
