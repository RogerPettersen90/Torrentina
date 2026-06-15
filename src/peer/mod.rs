//! Module 3: Peer wire protocol — identity, handshake, messaging, connection.
//!
//! Submodules:
//! * [`handshake`]  — the fixed 68-byte opening exchange.
//! * [`message`]    — the [`Message`] enum and the `Framed` codec for the
//!   length-prefixed message stream that follows the handshake.
//! * [`connection`] — [`PeerConnection`], which ties TCP + handshake + codec
//!   together into a `send`/`recv` interface.

pub mod connection;
pub mod handshake;
pub mod message;

pub use connection::PeerConnection;
pub use handshake::Handshake;
pub use message::{Block, BlockInfo, Message, PeerCodec};

use rand::Rng;

/// Length of a BitTorrent peer ID, in bytes. Always 20.
pub const PEER_ID_LEN: usize = 20;

/// Prefix identifying this client, Azureus-style: `-XX####-`, where `XX` is a
/// two-letter client tag and `####` is a version. `TN` = "Torrentina".
const CLIENT_PREFIX: &[u8; 8] = b"-TN0001-";

/// A 20-byte peer ID. Newtyped so it can't be mixed up with an
/// [`crate::metainfo::InfoHash`] or raw buffers on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerId(pub [u8; PEER_ID_LEN]);

impl PeerId {
    /// Generate a fresh peer ID: the client prefix followed by random bytes.
    pub fn generate() -> Self {
        let mut id = [0u8; PEER_ID_LEN];
        id[..CLIENT_PREFIX.len()].copy_from_slice(CLIENT_PREFIX);
        rand::thread_rng().fill(&mut id[CLIENT_PREFIX.len()..]);
        PeerId(id)
    }

    /// Borrow the raw 20 bytes (for tracker params and the handshake).
    pub fn as_bytes(&self) -> &[u8; PEER_ID_LEN] {
        &self.0
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Peer IDs are usually mostly-ASCII; show printable bytes verbatim and
        // escape the rest, which keeps logs readable.
        for &b in &self.0 {
            if b.is_ascii_graphic() || b == b' ' {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_peer_id_has_client_prefix_and_is_random() {
        let a = PeerId::generate();
        let b = PeerId::generate();
        assert_eq!(&a.0[..CLIENT_PREFIX.len()], CLIENT_PREFIX);
        assert_eq!(&b.0[..CLIENT_PREFIX.len()], CLIENT_PREFIX);
        // Overwhelmingly likely to differ in the random suffix.
        assert_ne!(a, b);
    }
}
