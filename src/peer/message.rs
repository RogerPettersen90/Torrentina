//! Peer wire messages and their length-prefixed framing.
//!
//! After the handshake, every message is:
//! ```text
//! <length prefix: u32 big-endian> <message id: u8> <payload...>
//! ```
//! except the keep-alive, which is just a length prefix of `0` and no id.
//!
//! [`PeerCodec`] implements tokio's [`Decoder`]/[`Encoder`] so the stream can
//! be driven by a `Framed`, while the codec itself is a pure bytes-in/bytes-out
//! transform that we can unit-test without any sockets.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use crate::error::{Error, Result};

/// Message type identifiers (the byte following the length prefix).
mod id {
    pub const CHOKE: u8 = 0;
    pub const UNCHOKE: u8 = 1;
    pub const INTERESTED: u8 = 2;
    pub const NOT_INTERESTED: u8 = 3;
    pub const HAVE: u8 = 4;
    pub const BITFIELD: u8 = 5;
    pub const REQUEST: u8 = 6;
    pub const PIECE: u8 = 7;
    pub const CANCEL: u8 = 8;
    pub const PORT: u8 = 9;
}

/// Upper bound on a single message's declared length. Guards against a hostile
/// peer sending a huge prefix that would force a massive buffer allocation.
/// Comfortably above any legitimate block (typically 16 KiB).
const MAX_MESSAGE_LEN: usize = 1 << 20; // 1 MiB

/// Identifies a block within the torrent: used by `Request` and `Cancel`, and
/// to describe what a `Piece` answers. A block is a contiguous range
/// `[begin, begin + length)` inside piece `index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockInfo {
    /// Zero-based piece index.
    pub index: u32,
    /// Byte offset of the block within the piece.
    pub begin: u32,
    /// Length of the block in bytes.
    pub length: u32,
}

/// A delivered block of piece data (the `Piece` message payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Piece this block belongs to.
    pub index: u32,
    /// Offset of this block within the piece.
    pub begin: u32,
    /// The block's bytes.
    pub data: Bytes,
}

/// A single peer wire protocol message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// Periodic no-op to keep the connection alive (length prefix `0`).
    KeepAlive,
    /// Sender will not serve requests until it unchokes us.
    Choke,
    /// Sender will now serve our requests.
    Unchoke,
    /// Sender wants data we have.
    Interested,
    /// Sender no longer wants our data.
    NotInterested,
    /// Sender has acquired the given piece.
    Have(u32),
    /// Bitfield of pieces the sender has (1 bit per piece, MSB-first).
    Bitfield(Bytes),
    /// Request for a block.
    Request(BlockInfo),
    /// A delivered block.
    Piece(Block),
    /// Cancel a previously requested block.
    Cancel(BlockInfo),
    /// DHT port announcement (BEP 5).
    Port(u16),
}

/// Encoder/decoder for the length-prefixed message stream.
#[derive(Debug, Default, Clone, Copy)]
pub struct PeerCodec;

impl PeerCodec {
    pub fn new() -> Self {
        PeerCodec
    }
}

impl Decoder for PeerCodec {
    type Item = Message;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Message>> {
        // Need the 4-byte length prefix before we can do anything.
        if src.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([src[0], src[1], src[2], src[3]]) as usize;

        // Length of 0 is a keep-alive: just consume the prefix.
        if len == 0 {
            src.advance(4);
            return Ok(Some(Message::KeepAlive));
        }
        if len > MAX_MESSAGE_LEN {
            return Err(Error::PeerProtocol(format!(
                "declared message length {len} exceeds maximum {MAX_MESSAGE_LEN}"
            )));
        }

        // Wait until the whole frame (prefix + body) has arrived.
        if src.len() < 4 + len {
            // Hint the buffer to reserve room for the rest of this frame.
            src.reserve(4 + len - src.len());
            return Ok(None);
        }

        // Consume the prefix, then split off exactly the message body.
        src.advance(4);
        let mut body = src.split_to(len); // body = <id><payload...>
        let msg_id = body[0];
        body.advance(1); // body now holds just the payload

        let message = match msg_id {
            id::CHOKE => Self::expect_empty(body, Message::Choke)?,
            id::UNCHOKE => Self::expect_empty(body, Message::Unchoke)?,
            id::INTERESTED => Self::expect_empty(body, Message::Interested)?,
            id::NOT_INTERESTED => Self::expect_empty(body, Message::NotInterested)?,
            id::HAVE => {
                Self::expect_len(&body, 4, "have")?;
                Message::Have(body.get_u32())
            }
            id::BITFIELD => Message::Bitfield(body.freeze()),
            id::REQUEST => {
                Self::expect_len(&body, 12, "request")?;
                Message::Request(read_block_info(&mut body))
            }
            id::PIECE => {
                Self::expect_min_len(&body, 8, "piece")?;
                let index = body.get_u32();
                let begin = body.get_u32();
                Message::Piece(Block {
                    index,
                    begin,
                    data: body.freeze(),
                })
            }
            id::CANCEL => {
                Self::expect_len(&body, 12, "cancel")?;
                Message::Cancel(read_block_info(&mut body))
            }
            id::PORT => {
                Self::expect_len(&body, 2, "port")?;
                Message::Port(body.get_u16())
            }
            other => {
                return Err(Error::PeerProtocol(format!(
                    "unknown message id {other}"
                )))
            }
        };
        Ok(Some(message))
    }
}

impl PeerCodec {
    /// A fixed-length, payload-free message: the payload must be empty.
    fn expect_empty(body: BytesMut, msg: Message) -> Result<Message> {
        if body.is_empty() {
            Ok(msg)
        } else {
            Err(Error::PeerProtocol(format!(
                "{msg:?} message must have no payload, got {} bytes",
                body.len()
            )))
        }
    }

    fn expect_len(body: &BytesMut, expected: usize, name: &str) -> Result<()> {
        if body.len() == expected {
            Ok(())
        } else {
            Err(Error::PeerProtocol(format!(
                "{name} message payload must be {expected} bytes, got {}",
                body.len()
            )))
        }
    }

    fn expect_min_len(body: &BytesMut, min: usize, name: &str) -> Result<()> {
        if body.len() >= min {
            Ok(())
        } else {
            Err(Error::PeerProtocol(format!(
                "{name} message payload must be at least {min} bytes, got {}",
                body.len()
            )))
        }
    }
}

/// Read a 12-byte `index/begin/length` triple as a [`BlockInfo`].
fn read_block_info(body: &mut BytesMut) -> BlockInfo {
    BlockInfo {
        index: body.get_u32(),
        begin: body.get_u32(),
        length: body.get_u32(),
    }
}

impl Encoder<Message> for PeerCodec {
    type Error = Error;

    fn encode(&mut self, item: Message, dst: &mut BytesMut) -> Result<()> {
        match item {
            Message::KeepAlive => dst.put_u32(0),
            Message::Choke => put_simple(dst, id::CHOKE),
            Message::Unchoke => put_simple(dst, id::UNCHOKE),
            Message::Interested => put_simple(dst, id::INTERESTED),
            Message::NotInterested => put_simple(dst, id::NOT_INTERESTED),
            Message::Have(index) => {
                dst.put_u32(5); // 1 (id) + 4 (index)
                dst.put_u8(id::HAVE);
                dst.put_u32(index);
            }
            Message::Bitfield(bits) => {
                dst.put_u32(1 + bits.len() as u32);
                dst.put_u8(id::BITFIELD);
                dst.put_slice(&bits);
            }
            Message::Request(b) => put_block_info(dst, id::REQUEST, b),
            Message::Cancel(b) => put_block_info(dst, id::CANCEL, b),
            Message::Piece(block) => {
                dst.put_u32(9 + block.data.len() as u32); // 1 + 4 + 4 + data
                dst.put_u8(id::PIECE);
                dst.put_u32(block.index);
                dst.put_u32(block.begin);
                dst.put_slice(&block.data);
            }
            Message::Port(port) => {
                dst.put_u32(3); // 1 (id) + 2 (port)
                dst.put_u8(id::PORT);
                dst.put_u16(port);
            }
        }
        Ok(())
    }
}

/// Encode a zero-payload message: length `1`, just the id byte.
fn put_simple(dst: &mut BytesMut, msg_id: u8) {
    dst.put_u32(1);
    dst.put_u8(msg_id);
}

/// Encode a `request`/`cancel`: length `13`, id, then the 12-byte triple.
fn put_block_info(dst: &mut BytesMut, msg_id: u8, b: BlockInfo) {
    dst.put_u32(13); // 1 + 4 + 4 + 4
    dst.put_u8(msg_id);
    dst.put_u32(b.index);
    dst.put_u32(b.begin);
    dst.put_u32(b.length);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode a message, then decode it back, asserting we recover the input
    /// and that the buffer is fully consumed.
    fn round_trip(msg: Message) {
        let mut codec = PeerCodec::new();
        let mut buf = BytesMut::new();
        codec.encode(msg.clone(), &mut buf).unwrap();
        let decoded = codec.decode(&mut buf).unwrap().expect("a full message");
        assert_eq!(decoded, msg);
        assert!(buf.is_empty(), "decoder should consume the whole frame");
    }

    #[test]
    fn round_trips_all_message_types() {
        round_trip(Message::KeepAlive);
        round_trip(Message::Choke);
        round_trip(Message::Unchoke);
        round_trip(Message::Interested);
        round_trip(Message::NotInterested);
        round_trip(Message::Have(42));
        round_trip(Message::Bitfield(Bytes::from_static(&[0b1010_1010, 0xff])));
        round_trip(Message::Request(BlockInfo {
            index: 1,
            begin: 16384,
            length: 16384,
        }));
        round_trip(Message::Piece(Block {
            index: 7,
            begin: 32768,
            data: Bytes::from_static(b"some block data"),
        }));
        round_trip(Message::Cancel(BlockInfo {
            index: 2,
            begin: 0,
            length: 16384,
        }));
        round_trip(Message::Port(6881));
    }

    #[test]
    fn decode_waits_for_a_complete_frame() {
        let mut codec = PeerCodec::new();
        let mut buf = BytesMut::new();
        codec.encode(Message::Have(5), &mut buf).unwrap();

        // Feed the bytes one at a time; only the final byte completes the frame.
        let full = buf.split();
        let mut partial = BytesMut::new();
        for (i, byte) in full.iter().enumerate() {
            partial.put_u8(*byte);
            let result = codec.decode(&mut partial).unwrap();
            if i + 1 < full.len() {
                assert!(result.is_none(), "incomplete frame must yield None");
            } else {
                assert_eq!(result, Some(Message::Have(5)));
            }
        }
    }

    #[test]
    fn decodes_two_back_to_back_messages() {
        let mut codec = PeerCodec::new();
        let mut buf = BytesMut::new();
        codec.encode(Message::Unchoke, &mut buf).unwrap();
        codec.encode(Message::Have(9), &mut buf).unwrap();

        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Unchoke));
        assert_eq!(codec.decode(&mut buf).unwrap(), Some(Message::Have(9)));
        assert_eq!(codec.decode(&mut buf).unwrap(), None);
    }

    #[test]
    fn rejects_oversized_length_prefix() {
        let mut codec = PeerCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32((MAX_MESSAGE_LEN + 1) as u32);
        assert!(matches!(
            codec.decode(&mut buf),
            Err(Error::PeerProtocol(_))
        ));
    }

    #[test]
    fn rejects_wrong_payload_length() {
        // A "have" frame claiming 4 payload bytes but the id says have with a
        // 3-byte payload: declared len = 1 (id) + 3 = 4.
        let mut codec = PeerCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(4);
        buf.put_u8(id::HAVE);
        buf.put_slice(&[0, 0, 0]); // only 3 bytes instead of 4
        assert!(matches!(
            codec.decode(&mut buf),
            Err(Error::PeerProtocol(_))
        ));
    }

    #[test]
    fn rejects_unknown_message_id() {
        let mut codec = PeerCodec::new();
        let mut buf = BytesMut::new();
        buf.put_u32(1);
        buf.put_u8(99); // not a valid id
        assert!(matches!(
            codec.decode(&mut buf),
            Err(Error::PeerProtocol(_))
        ));
    }
}
