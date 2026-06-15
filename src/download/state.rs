//! Shared, global download state: the coordination point all peer tasks share.
//!
//! [`PieceTracker`] owns the authoritative view of progress — which pieces we
//! have, how rare each piece is, and which pieces are currently *claimed* by a
//! peer so two peers don't redundantly download the same one. It also performs
//! SHA-1 verification of completed pieces against the metainfo's hashes.

use sha1::{Digest, Sha1};

use crate::download::bitfield::Bitfield;
use crate::error::Result;
use crate::metainfo::{Info, SHA1_LEN};

/// The shared piece-level state, intended to live behind a `Mutex`.
#[derive(Debug)]
pub struct PieceTracker {
    /// Expected SHA-1 of each piece, in order.
    piece_hashes: Vec<[u8; SHA1_LEN]>,
    /// Pieces we have fully downloaded and verified.
    have: Bitfield,
    /// How many connected peers advertise each piece (rarest-first input).
    availability: Vec<u32>,
    /// Pieces currently being downloaded by some peer.
    claimed: Vec<bool>,
}

impl PieceTracker {
    /// Build a tracker from a parsed [`Info`].
    pub fn from_info(info: &Info) -> Result<Self> {
        let piece_hashes: Vec<[u8; SHA1_LEN]> = info.piece_hashes()?.copied().collect();
        let n = piece_hashes.len();
        Ok(PieceTracker {
            piece_hashes,
            have: Bitfield::new(n),
            availability: vec![0; n],
            claimed: vec![false; n],
        })
    }

    /// Total number of pieces.
    pub fn num_pieces(&self) -> usize {
        self.piece_hashes.len()
    }

    /// Whether all pieces have been downloaded and verified.
    pub fn is_complete(&self) -> bool {
        self.have.is_complete()
    }

    /// Our current `have` bitfield (e.g. to advertise to peers).
    pub fn have_bitfield(&self) -> &Bitfield {
        &self.have
    }

    /// Whether we have already downloaded and verified piece `index`.
    pub fn has(&self, index: u32) -> bool {
        self.have.has(index as usize)
    }

    /// Record that a peer advertised a single piece (a `have` message).
    pub fn add_availability(&mut self, index: usize) {
        if index < self.availability.len() {
            self.availability[index] += 1;
        }
    }

    /// Record an entire peer bitfield's worth of availability at once.
    pub fn add_bitfield_availability(&mut self, bitfield: &Bitfield) {
        for i in 0..self.availability.len() {
            if bitfield.has(i) {
                self.availability[i] += 1;
            }
        }
    }

    /// Undo a peer's availability contribution when it disconnects (or replaces
    /// its bitfield). Without this, pieces advertised by long-departed peers
    /// stay counted forever, so a piece many transient peers once had looks
    /// permanently common and rarest-first stops prioritizing it. `saturating`
    /// guards against any accounting drift from a misbehaving peer.
    pub fn remove_bitfield_availability(&mut self, bitfield: &Bitfield) {
        for i in 0..self.availability.len() {
            if bitfield.has(i) {
                self.availability[i] = self.availability[i].saturating_sub(1);
            }
        }
    }

    /// Pick a piece for a peer to download, claiming it exclusively.
    ///
    /// Among pieces we still need, that the peer has, and that no other peer is
    /// already downloading, choose the **rarest** (lowest availability). Returns
    /// the chosen index and marks it claimed, or `None` if there is nothing to
    /// do for this peer right now.
    pub fn reserve_piece(&mut self, peer_has: &Bitfield) -> Option<u32> {
        let index = self.rarest_needed(peer_has, true)?;
        self.claimed[index as usize] = true;
        Some(index)
    }

    /// Endgame reservation: pick a still-needed piece the peer has **even if
    /// another peer already claimed it**. Used when [`reserve_piece`] finds
    /// nothing left to claim exclusively but the torrent isn't complete, so a
    /// piece stranded behind a slow/silent peer can be fetched in parallel
    /// rather than freezing the whole download. Does not change `claimed`
    /// (the piece may legitimately be in flight on several peers at once).
    pub fn reserve_piece_endgame(&self, peer_has: &Bitfield) -> Option<u32> {
        self.rarest_needed(peer_has, false)
    }

    /// The rarest piece we still need and the peer has. When `skip_claimed` is
    /// true, pieces another peer is already downloading are excluded.
    fn rarest_needed(&self, peer_has: &Bitfield, skip_claimed: bool) -> Option<u32> {
        let mut best: Option<(u32, u32)> = None; // (availability, index)
        for i in 0..self.num_pieces() {
            if self.have.has(i) || !peer_has.has(i) || (skip_claimed && self.claimed[i]) {
                continue;
            }
            let avail = self.availability[i];
            if best.is_none_or(|(b, _)| avail < b) {
                best = Some((avail, i as u32));
            }
        }
        best.map(|(_, index)| index)
    }

    /// Release a previously reserved piece so another peer can take it (used on
    /// failed verification or peer disconnect mid-piece).
    pub fn release_piece(&mut self, index: u32) {
        if let Some(slot) = self.claimed.get_mut(index as usize) {
            *slot = false;
        }
    }

    /// Verify assembled piece bytes against the expected SHA-1.
    pub fn verify(&self, index: u32, data: &[u8]) -> bool {
        match self.piece_hashes.get(index as usize) {
            Some(expected) => Sha1::digest(data).as_slice() == expected,
            None => false,
        }
    }

    /// Mark a verified piece as owned (and no longer claimed).
    pub fn mark_have(&mut self, index: u32) {
        self.have.set(index as usize);
        self.release_piece(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info_with_hashes(hashes: &[[u8; 20]]) -> Info {
        let mut pieces = Vec::new();
        for h in hashes {
            pieces.extend_from_slice(h);
        }
        Info {
            name: "t".into(),
            piece_length: 16384,
            pieces,
            length: Some(16384 * hashes.len() as u64),
            files: None,
            private: None,
        }
    }

    fn full_bitfield(n: usize) -> Bitfield {
        let mut bf = Bitfield::new(n);
        for i in 0..n {
            bf.set(i);
        }
        bf
    }

    #[test]
    fn reserve_prefers_rarest_and_claims_exclusively() {
        let mut t = PieceTracker::from_info(&info_with_hashes(&[[0; 20]; 3])).unwrap();
        let all = full_bitfield(3);

        // Make piece 2 the rarest (availability 0), 0 and 1 more common.
        t.add_availability(0);
        t.add_availability(0);
        t.add_availability(1);

        // First reservation grabs the rarest, piece 2.
        assert_eq!(t.reserve_piece(&all), Some(2));
        // Next grabs piece 1 (availability 1 < piece 0's 2); 2 is now claimed.
        assert_eq!(t.reserve_piece(&all), Some(1));
        // Then piece 0.
        assert_eq!(t.reserve_piece(&all), Some(0));
        // Nothing left to claim.
        assert_eq!(t.reserve_piece(&all), None);
    }

    #[test]
    fn does_not_reserve_pieces_we_have_or_peer_lacks() {
        let mut t = PieceTracker::from_info(&info_with_hashes(&[[0; 20]; 3])).unwrap();
        t.mark_have(0);

        // Peer only has piece 1.
        let mut peer = Bitfield::new(3);
        peer.set(1);
        assert_eq!(t.reserve_piece(&peer), Some(1));
        assert_eq!(t.reserve_piece(&peer), None); // 0 we have, 2 peer lacks
    }

    #[test]
    fn release_allows_reclaiming() {
        let mut t = PieceTracker::from_info(&info_with_hashes(&[[0; 20]; 2])).unwrap();
        let all = full_bitfield(2);
        let a = t.reserve_piece(&all).unwrap();
        t.release_piece(a);
        // Released piece is available again.
        assert!(t.reserve_piece(&all).is_some());
    }

    #[test]
    fn endgame_reservation_ignores_claims_but_not_have() {
        let mut t = PieceTracker::from_info(&info_with_hashes(&[[0; 20]; 2])).unwrap();
        let all = full_bitfield(2);

        // Claim both pieces exclusively; nothing left for a normal reservation.
        assert!(t.reserve_piece(&all).is_some());
        assert!(t.reserve_piece(&all).is_some());
        assert_eq!(t.reserve_piece(&all), None);

        // Endgame still offers a claimed-but-needed piece...
        assert!(t.reserve_piece_endgame(&all).is_some());
        // ...but never one we already have.
        t.mark_have(0);
        t.mark_have(1);
        assert_eq!(t.reserve_piece_endgame(&all), None);
    }

    #[test]
    fn removing_availability_is_saturating_and_changes_rarest_order() {
        let mut t = PieceTracker::from_info(&info_with_hashes(&[[0; 20]; 2])).unwrap();
        let only_0 = {
            let mut bf = Bitfield::new(2);
            bf.set(0);
            bf
        };
        let only_1 = {
            let mut bf = Bitfield::new(2);
            bf.set(1);
            bf
        };
        let all = full_bitfield(2);

        // Availability ends at piece 0 = 2, piece 1 = 1, so piece 1 is rarest
        // and gets reserved first.
        t.add_bitfield_availability(&only_1);
        t.add_bitfield_availability(&only_0);
        t.add_bitfield_availability(&only_0);
        assert_eq!(t.reserve_piece(&all), Some(1));
        t.release_piece(1);

        // One piece-0 holder disconnects: piece 0 drops to availability 1,
        // tying piece 1, so the tie-break now hands out piece 0 first — the
        // decrement demonstrably shifted selection.
        t.remove_bitfield_availability(&only_0);
        assert_eq!(t.reserve_piece(&all), Some(0));

        // Over-removing past zero must saturate, not underflow/panic.
        t.remove_bitfield_availability(&all);
        t.remove_bitfield_availability(&all);
    }

    #[test]
    fn verify_and_complete() {
        let data = b"hello piece";
        let hash: [u8; 20] = Sha1::digest(data).into();
        let mut t = PieceTracker::from_info(&info_with_hashes(&[hash])).unwrap();

        assert!(t.verify(0, data));
        assert!(!t.verify(0, b"wrong"));

        assert!(!t.is_complete());
        t.mark_have(0);
        assert!(t.is_complete());
    }
}
