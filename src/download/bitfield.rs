//! A fixed-size set of pieces, stored as a wire-compatible bitfield.
//!
//! Bits are packed MSB-first: piece `i` lives in byte `i / 8` at bit mask
//! `0x80 >> (i % 8)`. This matches the on-wire `bitfield` message exactly, so
//! converting to/from [`crate::peer::Message::Bitfield`] is a plain byte copy.

use bytes::Bytes;

use crate::error::{Error, Result};

/// A set of pieces by index, backed by a packed bitfield.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bitfield {
    bytes: Vec<u8>,
    num_pieces: usize,
}

impl Bitfield {
    /// Number of bytes needed to hold `num_pieces` bits.
    fn byte_len(num_pieces: usize) -> usize {
        num_pieces.div_ceil(8)
    }

    /// Create an empty bitfield sized for `num_pieces`.
    pub fn new(num_pieces: usize) -> Self {
        Bitfield {
            bytes: vec![0u8; Self::byte_len(num_pieces)],
            num_pieces,
        }
    }

    /// Wrap raw wire bytes as a bitfield for `num_pieces` pieces.
    ///
    /// Validates the length: a peer must send exactly `ceil(num_pieces / 8)`
    /// bytes. (We do not reject nonzero spare/padding bits — lenient by design.)
    pub fn from_bytes(bytes: Vec<u8>, num_pieces: usize) -> Result<Self> {
        let expected = Self::byte_len(num_pieces);
        if bytes.len() != expected {
            return Err(Error::PeerProtocol(format!(
                "bitfield is {} bytes but {num_pieces} pieces need {expected}",
                bytes.len()
            )));
        }
        Ok(Bitfield { bytes, num_pieces })
    }

    /// Total number of pieces this bitfield can describe.
    pub fn num_pieces(&self) -> usize {
        self.num_pieces
    }

    /// Whether piece `index` is present. Out-of-range indices are `false`.
    pub fn has(&self, index: usize) -> bool {
        if index >= self.num_pieces {
            return false;
        }
        let mask = 0x80u8 >> (index % 8);
        self.bytes[index / 8] & mask != 0
    }

    /// Mark piece `index` as present. Out-of-range indices are ignored.
    pub fn set(&mut self, index: usize) {
        if index >= self.num_pieces {
            return;
        }
        let mask = 0x80u8 >> (index % 8);
        self.bytes[index / 8] |= mask;
    }

    /// Count of pieces present.
    pub fn count(&self) -> usize {
        (0..self.num_pieces).filter(|&i| self.has(i)).count()
    }

    /// Whether every piece is present.
    pub fn is_complete(&self) -> bool {
        self.count() == self.num_pieces
    }

    /// Borrow the packed wire bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Copy the bitfield into a wire-ready `Bytes` for a `Bitfield` message.
    pub fn to_message_bytes(&self) -> Bytes {
        Bytes::copy_from_slice(&self.bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_byte_buffer_by_ceil() {
        assert_eq!(Bitfield::new(0).as_bytes().len(), 0);
        assert_eq!(Bitfield::new(1).as_bytes().len(), 1);
        assert_eq!(Bitfield::new(8).as_bytes().len(), 1);
        assert_eq!(Bitfield::new(9).as_bytes().len(), 2);
    }

    #[test]
    fn set_and_has_are_msb_first() {
        let mut bf = Bitfield::new(10);
        assert!(!bf.has(0));
        bf.set(0); // most-significant bit of byte 0
        bf.set(7); // least-significant bit of byte 0
        bf.set(8); // most-significant bit of byte 1
        assert_eq!(bf.as_bytes(), &[0b1000_0001, 0b1000_0000]);
        assert!(bf.has(0) && bf.has(7) && bf.has(8));
        assert!(!bf.has(1) && !bf.has(9));
        assert_eq!(bf.count(), 3);
    }

    #[test]
    fn out_of_range_access_is_safe() {
        let mut bf = Bitfield::new(3);
        bf.set(99); // ignored
        assert!(!bf.has(99));
        assert!(!bf.has(3));
    }

    #[test]
    fn completeness_tracks_all_pieces() {
        let mut bf = Bitfield::new(3);
        assert!(!bf.is_complete());
        bf.set(0);
        bf.set(1);
        bf.set(2);
        assert!(bf.is_complete());
    }

    #[test]
    fn from_bytes_validates_length() {
        assert!(Bitfield::from_bytes(vec![0u8; 2], 9).is_ok());
        assert!(matches!(
            Bitfield::from_bytes(vec![0u8; 1], 9),
            Err(Error::PeerProtocol(_))
        ));
    }
}
