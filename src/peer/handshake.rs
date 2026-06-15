//! The BitTorrent peer handshake (the fixed 68-byte opening exchange).
//!
//! Layout:
//! ```text
//! | 1 byte | 19 bytes              | 8 bytes  | 20 bytes  | 20 bytes |
//! | pstrlen| "BitTorrent protocol" | reserved | info_hash | peer_id  |
//! ```

use crate::error::{Error, Result};
use crate::metainfo::{InfoHash, SHA1_LEN};
use crate::peer::{PeerId, PEER_ID_LEN};

/// Protocol identifier string for BitTorrent v1.
pub const PSTR: &[u8] = b"BitTorrent protocol";

/// Total handshake length: `1 + 19 + 8 + 20 + 20`.
pub const HANDSHAKE_LEN: usize = 1 + 19 + 8 + SHA1_LEN + PEER_ID_LEN;

/// A parsed (or to-be-sent) handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    /// 8 reserved bytes used for extension negotiation (DHT, fast, extended
    /// protocol). We send zeros and don't yet act on a peer's flags.
    pub reserved: [u8; 8],
    /// The torrent both sides claim to be sharing.
    pub info_hash: InfoHash,
    /// The sender's peer ID.
    pub peer_id: PeerId,
}

impl Handshake {
    /// Build a handshake for the given torrent and our peer ID, with all
    /// reserved bits cleared.
    pub fn new(info_hash: InfoHash, peer_id: PeerId) -> Self {
        Handshake {
            reserved: [0u8; 8],
            info_hash,
            peer_id,
        }
    }

    /// Serialize to the 68-byte wire form.
    pub fn to_bytes(&self) -> [u8; HANDSHAKE_LEN] {
        let mut buf = [0u8; HANDSHAKE_LEN];
        buf[0] = PSTR.len() as u8; // 19
        buf[1..20].copy_from_slice(PSTR);
        buf[20..28].copy_from_slice(&self.reserved);
        buf[28..48].copy_from_slice(self.info_hash.as_bytes());
        buf[48..68].copy_from_slice(self.peer_id.as_bytes());
        buf
    }

    /// Parse and validate a 68-byte handshake from the wire.
    pub fn from_bytes(buf: &[u8]) -> Result<Self> {
        if buf.len() != HANDSHAKE_LEN {
            return Err(Error::PeerProtocol(format!(
                "handshake must be {HANDSHAKE_LEN} bytes, got {}",
                buf.len()
            )));
        }
        if buf[0] as usize != PSTR.len() {
            return Err(Error::PeerProtocol(format!(
                "unexpected pstrlen {}",
                buf[0]
            )));
        }
        if &buf[1..20] != PSTR {
            return Err(Error::PeerProtocol("unexpected protocol string".into()));
        }

        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&buf[20..28]);

        let mut info_hash = [0u8; SHA1_LEN];
        info_hash.copy_from_slice(&buf[28..48]);

        let mut peer_id = [0u8; PEER_ID_LEN];
        peer_id.copy_from_slice(&buf[48..68]);

        Ok(Handshake {
            reserved,
            info_hash: InfoHash(info_hash),
            peer_id: PeerId(peer_id),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_bytes() {
        let hs = Handshake::new(InfoHash([0xAB; 20]), PeerId(*b"-TN0001-ABCDEFGHIJKL"));
        let bytes = hs.to_bytes();
        assert_eq!(bytes.len(), 68);
        assert_eq!(bytes[0], 19);
        assert_eq!(&bytes[1..20], PSTR);

        let parsed = Handshake::from_bytes(&bytes).unwrap();
        assert_eq!(parsed, hs);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(matches!(
            Handshake::from_bytes(&[0u8; 10]),
            Err(Error::PeerProtocol(_))
        ));
    }

    #[test]
    fn rejects_bad_protocol_string() {
        let mut bytes = Handshake::new(InfoHash([0; 20]), PeerId([0; 20])).to_bytes();
        bytes[1] = b'X'; // corrupt the pstr
        assert!(matches!(
            Handshake::from_bytes(&bytes),
            Err(Error::PeerProtocol(_))
        ));
    }

    #[test]
    fn rejects_bad_pstrlen() {
        let mut bytes = Handshake::new(InfoHash([0; 20]), PeerId([0; 20])).to_bytes();
        bytes[0] = 18; // wrong pstrlen
        assert!(matches!(
            Handshake::from_bytes(&bytes),
            Err(Error::PeerProtocol(_))
        ));
    }
}
