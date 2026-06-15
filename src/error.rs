//! Crate-wide error type.
//!
//! We use `thiserror` to derive a single, structured error enum. Lower-level
//! errors (I/O, bencode decoding) are wrapped via `#[from]` so that the `?`
//! operator works ergonomically throughout the crate, while domain-specific
//! failures get their own descriptive variants.

use thiserror::Error;

/// The result type used across the entire crate.
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    /// Failure reading the `.torrent` file from disk.
    #[error("failed to read torrent file: {0}")]
    Io(#[from] std::io::Error),

    /// The bytes could not be decoded as valid bencode / metainfo.
    #[error("failed to decode bencode: {0}")]
    BencodeDecode(#[from] serde_bencode::Error),

    /// The `pieces` field must be a flat concatenation of 20-byte SHA-1
    /// digests, so its length must be an exact multiple of 20.
    #[error("`pieces` field has invalid length {0}: not a multiple of 20")]
    InvalidPiecesLength(usize),

    /// The info dictionary contained neither `length` (single-file) nor
    /// `files` (multi-file), or contained both, which is malformed.
    #[error("info dictionary is neither valid single-file nor multi-file layout")]
    InvalidFileLayout,

    /// The raw bencode could not be walked to locate the `info` dictionary's
    /// exact byte span (needed to compute a faithful info hash).
    #[error("malformed torrent: {0}")]
    MalformedTorrent(String),

    // ---- Module 2: tracker communication ----
    /// Underlying HTTP transport error (connection, TLS, status, body).
    #[error("tracker HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    /// The announce URL could not be parsed.
    #[error("invalid tracker URL: {0}")]
    UrlParse(#[from] url::ParseError),

    /// The announce URL used a scheme we don't speak (only http/https/udp).
    #[error("unsupported tracker scheme: {0}")]
    UnsupportedScheme(String),

    /// The tracker responded with an explicit failure reason.
    #[error("tracker returned failure: {0}")]
    TrackerFailure(String),

    /// A compact peers blob was not a clean multiple of the per-peer width.
    #[error("malformed compact peers data: {0} bytes is not a multiple of {1}")]
    InvalidPeersData(usize, usize),

    /// A UDP tracker reply was malformed or violated the protocol.
    #[error("UDP tracker protocol error: {0}")]
    UdpProtocol(String),

    /// A network operation (UDP send/recv) did not complete in time.
    #[error("tracker request timed out")]
    Timeout,

    // ---- Module 3: peer wire protocol ----
    /// A peer violated the wire protocol: bad handshake, malformed or
    /// oversized message, or an info-hash mismatch.
    #[error("peer protocol error: {0}")]
    PeerProtocol(String),

    // ---- Module 5: disk I/O & file assembly ----
    /// A logical inconsistency while writing to disk (e.g. a piece's bytes did
    /// not map onto any file region).
    #[error("storage error: {0}")]
    Storage(String),
}
