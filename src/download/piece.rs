//! Piece/block geometry and single-piece assembly.
//!
//! A torrent's content is split into fixed-size *pieces* (the final one may be
//! shorter). Each piece is downloaded as a sequence of fixed-size *blocks*
//! (16 KiB; the final block of a piece may be shorter). [`Geometry`] does the
//! index arithmetic; [`PieceAssembler`] collects a single piece's blocks and
//! exposes the assembled bytes for hashing.

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::metainfo::Info;
use crate::peer::BlockInfo;

/// Standard block size for requests: 16 KiB.
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Immutable size arithmetic for a torrent: how big each piece and block is.
///
/// `Copy` so peer tasks can each hold their own cheap copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    total_length: u64,
    piece_length: u32,
    num_pieces: u32,
    last_piece_length: u32,
}

impl Geometry {
    /// Derive geometry from a parsed [`Info`].
    pub fn from_info(info: &Info) -> Result<Self> {
        let total_length = info.total_length()?;
        let piece_length: u32 = info
            .piece_length
            .try_into()
            .map_err(|_| Error::PeerProtocol("piece length exceeds u32".into()))?;
        if piece_length == 0 {
            return Err(Error::PeerProtocol("piece length is zero".into()));
        }
        let num_pieces = info.num_pieces() as u32;
        if num_pieces == 0 {
            return Err(Error::PeerProtocol("torrent has zero pieces".into()));
        }

        // The last piece holds whatever is left over.
        let full = (num_pieces as u64 - 1) * piece_length as u64;
        let last = total_length
            .checked_sub(full)
            .ok_or_else(|| Error::PeerProtocol("piece count exceeds total length".into()))?;
        if last == 0 || last > piece_length as u64 {
            return Err(Error::PeerProtocol(
                "inconsistent total length, piece length, and piece count".into(),
            ));
        }

        Ok(Geometry {
            total_length,
            piece_length,
            num_pieces,
            last_piece_length: last as u32,
        })
    }

    /// Total content length across all files.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Number of pieces.
    pub fn num_pieces(&self) -> u32 {
        self.num_pieces
    }

    /// The global byte offset where piece `index` begins in the concatenated
    /// torrent stream. Uses the nominal (full) piece length, not the index's
    /// possibly-shorter length.
    pub fn piece_offset(&self, index: u32) -> u64 {
        index as u64 * self.piece_length as u64
    }

    /// Length in bytes of the piece at `index` (the last piece is shorter).
    pub fn piece_length(&self, index: u32) -> u32 {
        if index + 1 == self.num_pieces {
            self.last_piece_length
        } else {
            self.piece_length
        }
    }

    /// Number of blocks the piece at `index` is divided into.
    pub fn num_blocks(&self, index: u32) -> u32 {
        self.piece_length(index).div_ceil(BLOCK_SIZE)
    }

    /// The [`BlockInfo`] for block `block_idx` of piece `index`.
    pub fn block_info(&self, index: u32, block_idx: u32) -> BlockInfo {
        let piece_len = self.piece_length(index);
        let begin = block_idx * BLOCK_SIZE;
        let length = BLOCK_SIZE.min(piece_len - begin);
        BlockInfo {
            index,
            begin,
            length,
        }
    }
}

/// Accumulates the blocks of a single piece until it is whole.
#[derive(Debug)]
pub struct PieceAssembler {
    index: u32,
    length: u32,
    buf: Vec<u8>,
    received: Vec<bool>,
    remaining: u32,
}

impl PieceAssembler {
    /// Start assembling piece `index`, which is `length` bytes long.
    pub fn new(index: u32, length: u32) -> Self {
        let num_blocks = length.div_ceil(BLOCK_SIZE);
        PieceAssembler {
            index,
            length,
            buf: vec![0u8; length as usize],
            received: vec![false; num_blocks as usize],
            remaining: num_blocks,
        }
    }

    /// The piece index being assembled.
    pub fn index(&self) -> u32 {
        self.index
    }

    /// Copy a received block into place.
    ///
    /// `begin` must be block-aligned and within bounds, and `data` must fit.
    /// Re-delivering an already-received block is a harmless no-op (it does not
    /// double-count toward completion).
    pub fn add_block(&mut self, begin: u32, data: &[u8]) -> Result<()> {
        if !begin.is_multiple_of(BLOCK_SIZE) {
            return Err(Error::PeerProtocol(format!(
                "block offset {begin} is not block-aligned"
            )));
        }
        let end = begin
            .checked_add(data.len() as u32)
            .filter(|&e| e <= self.length)
            .ok_or_else(|| {
                Error::PeerProtocol(format!(
                    "block [{begin}, {begin}+{}) exceeds piece length {}",
                    data.len(),
                    self.length
                ))
            })?;

        let block_idx = (begin / BLOCK_SIZE) as usize;
        self.buf[begin as usize..end as usize].copy_from_slice(data);
        if !self.received[block_idx] {
            self.received[block_idx] = true;
            self.remaining -= 1;
        }
        Ok(())
    }

    /// Whether every block of the piece has been received.
    pub fn is_complete(&self) -> bool {
        self.remaining == 0
    }

    /// The assembled bytes so far (meaningful once [`is_complete`] is true).
    ///
    /// [`is_complete`]: PieceAssembler::is_complete
    pub fn data(&self) -> &[u8] {
        &self.buf
    }

    /// Consume the assembler, yielding the piece bytes.
    pub fn into_bytes(self) -> Bytes {
        Bytes::from(self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::Info;

    /// Build an `Info` for a torrent of `total` bytes with `piece_len` pieces.
    /// The `pieces` hash blob is filler; geometry ignores its contents.
    fn info(total: u64, piece_len: u64) -> Info {
        let num_pieces = total.div_ceil(piece_len) as usize;
        Info {
            name: "t".into(),
            piece_length: piece_len,
            pieces: vec![0u8; num_pieces * 20],
            length: Some(total),
            files: None,
            private: None,
        }
    }

    #[test]
    fn geometry_handles_short_final_piece_and_block() {
        // 2 full 32 KiB pieces + a 5000-byte tail piece.
        let g = Geometry::from_info(&info(2 * 32768 + 5000, 32768)).unwrap();
        assert_eq!(g.num_pieces(), 3);
        assert_eq!(g.piece_length(0), 32768);
        assert_eq!(g.piece_length(2), 5000);

        // Full piece -> two 16 KiB blocks.
        assert_eq!(g.num_blocks(0), 2);
        assert_eq!(
            g.block_info(0, 1),
            BlockInfo { index: 0, begin: 16384, length: 16384 }
        );

        // Tail piece -> a single 5000-byte block.
        assert_eq!(g.num_blocks(2), 1);
        assert_eq!(
            g.block_info(2, 0),
            BlockInfo { index: 2, begin: 0, length: 5000 }
        );
    }

    #[test]
    fn geometry_rejects_inconsistent_sizes() {
        // pieces blob says 4 pieces but total only spans ~1.x pieces.
        let mut bad = info(40000, 32768); // 2 pieces' worth
        bad.pieces = vec![0u8; 4 * 20]; // claim 4 pieces
        assert!(Geometry::from_info(&bad).is_err());
    }

    #[test]
    fn assembler_collects_blocks_then_completes() {
        let mut asm = PieceAssembler::new(0, 20000); // 16384 + 3616
        assert!(!asm.is_complete());

        asm.add_block(0, &[1u8; 16384]).unwrap();
        assert!(!asm.is_complete());
        asm.add_block(16384, &[2u8; 3616]).unwrap();
        assert!(asm.is_complete());

        let bytes = asm.into_bytes();
        assert_eq!(bytes.len(), 20000);
        assert_eq!(bytes[0], 1);
        assert_eq!(bytes[16384], 2);
    }

    #[test]
    fn assembler_ignores_duplicate_block() {
        let mut asm = PieceAssembler::new(0, 16384);
        asm.add_block(0, &[1u8; 16384]).unwrap();
        assert!(asm.is_complete());
        // Duplicate delivery must not underflow `remaining`.
        asm.add_block(0, &[1u8; 16384]).unwrap();
        assert!(asm.is_complete());
    }

    #[test]
    fn assembler_rejects_misaligned_and_oversized_blocks() {
        let mut asm = PieceAssembler::new(0, 16384);
        assert!(asm.add_block(100, &[0u8; 10]).is_err()); // not aligned
        assert!(asm.add_block(0, &[0u8; 20000]).is_err()); // overflows piece
    }
}
