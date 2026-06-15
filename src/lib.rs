//! Torrentina — a lightweight, efficient local BitTorrent client.
//!
//! The crate is built one module per protocol layer:
//!
//! 1. [`metainfo`] — bencode parsing & `.torrent` type definitions
//! 2. [`tracker`]  — tracker communication (HTTP/UDP)
//! 3. [`peer`]     — peer wire protocol (handshake & messaging)
//! 4. [`download`] — torrent manager & piece download logic
//! 5. [`disk`]     — disk I/O & file assembly

pub mod disk;
pub mod download;
pub mod error;
pub mod metainfo;
pub mod peer;
pub mod tracker;

pub use disk::{assemble, MappedFile, Storage};
pub use download::{
    ControlState, Coordinator, Geometry, PeerStat, PieceTracker, StatsSnapshot, SwarmStats,
    VerifiedPiece, BLOCK_SIZE,
};
pub use error::{Error, Result};
pub use metainfo::{FileEntry, FileLayout, Info, InfoHash, Metainfo};
pub use peer::{Block, BlockInfo, Handshake, Message, PeerConnection, PeerId};
pub use tracker::{announce, AnnounceParams, AnnounceResponse, Event};
