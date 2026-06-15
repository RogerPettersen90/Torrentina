//! Module 1: Bencode parsing & `.torrent` metainfo type definitions.
//!
//! This module owns the typed representation of a `.torrent` file and the
//! logic to parse one. A `.torrent` is a bencoded dictionary; the most
//! important nested value is the `info` dictionary, whose SHA-1 hash (the
//! "info hash") uniquely identifies the torrent on the wire and to trackers.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};

use crate::error::{Error, Result};

/// The number of bytes in a SHA-1 digest. Each piece hash, and the info hash
/// itself, is exactly this wide.
pub const SHA1_LEN: usize = 20;

/// A 20-byte SHA-1 info hash, wrapped in a newtype so it cannot be confused
/// with arbitrary byte buffers elsewhere (e.g. peer IDs, piece hashes) when we
/// build tracker requests and peer handshakes in later modules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InfoHash(pub [u8; SHA1_LEN]);

impl InfoHash {
    /// Borrow the raw 20 bytes (e.g. for the tracker `info_hash` query param).
    pub fn as_bytes(&self) -> &[u8; SHA1_LEN] {
        &self.0
    }
}

impl std::fmt::Display for InfoHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// One entry in a multi-file torrent's `files` list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Length of this file in bytes.
    pub length: u64,
    /// Path components, relative to the torrent's root directory. Bencode
    /// stores the path as a list so we never have to parse separators.
    pub path: Vec<String>,
    /// Optional legacy MD5 checksum; almost never present in modern torrents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub md5sum: Option<String>,
}

/// The `info` dictionary: describes the content being shared.
///
/// This is a *typed view* of the info dict, not the source of truth for the
/// info hash. A `.torrent` may carry info-dict keys we don't model here (a
/// top-level `md5sum`, a tracker `source` tag, BitTorrent v2 fields, …).
/// Re-serializing this struct would silently drop those keys and produce a
/// hash no tracker recognizes — so the real info hash is taken from the
/// original bytes at parse time (see [`Metainfo::from_bytes`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Info {
    /// Suggested file name (single-file) or root directory name (multi-file).
    pub name: String,

    /// Number of bytes in each piece (the final piece may be shorter).
    #[serde(rename = "piece length")]
    pub piece_length: u64,

    /// Concatenated 20-byte SHA-1 digests, one per piece. Stored as raw bytes
    /// via `serde_bytes` so bencode treats it as a byte string, not a list.
    #[serde(with = "serde_bytes")]
    pub pieces: Vec<u8>,

    /// Present only in single-file mode: the total length of the lone file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,

    /// Present only in multi-file mode: the list of files under `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<FileEntry>>,

    /// `1` if this is a private torrent (tracker-only, no DHT/PEX).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<u8>,
}

/// A normalized view of the torrent's file layout, hiding the single-vs-multi
/// distinction from the rest of the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileLayout {
    /// One file of the given length, named `Info::name`.
    Single { length: u64 },
    /// Multiple files rooted at a directory named `Info::name`.
    Multi { files: Vec<FileEntry> },
}

impl Info {
    /// Compute an info hash by **re-serializing** this typed struct to bencode.
    ///
    /// ⚠️ This is correct *only* when the struct captures every key the
    /// original info dict held. It does not when the torrent carries keys we
    /// don't model (the common case). Prefer [`Metainfo::info_hash`], which
    /// hashes the original bytes. This method exists for `Info` values that
    /// were constructed in-memory (tests, the loopback example) and therefore
    /// have no original bytes to point back to.
    pub fn info_hash(&self) -> Result<InfoHash> {
        let encoded = serde_bencode::to_bytes(self)?;
        let digest = Sha1::digest(&encoded);
        let mut out = [0u8; SHA1_LEN];
        out.copy_from_slice(&digest);
        Ok(InfoHash(out))
    }

    /// Total number of pieces in the torrent.
    pub fn num_pieces(&self) -> usize {
        self.pieces.len() / SHA1_LEN
    }

    /// Iterate over each piece's 20-byte SHA-1 hash.
    ///
    /// Returns an error if `pieces` is not a clean multiple of 20 bytes.
    pub fn piece_hashes(&self) -> Result<impl Iterator<Item = &[u8; SHA1_LEN]>> {
        if !self.pieces.len().is_multiple_of(SHA1_LEN) {
            return Err(Error::InvalidPiecesLength(self.pieces.len()));
        }
        Ok(self
            .pieces
            .chunks_exact(SHA1_LEN)
            .map(|chunk| chunk.try_into().expect("chunk is exactly SHA1_LEN")))
    }

    /// Normalize the single-vs-multi file representation.
    pub fn layout(&self) -> Result<FileLayout> {
        match (self.length, &self.files) {
            (Some(length), None) => Ok(FileLayout::Single { length }),
            (None, Some(files)) => Ok(FileLayout::Multi {
                files: files.clone(),
            }),
            // Both present or neither present is malformed.
            _ => Err(Error::InvalidFileLayout),
        }
    }

    /// Total content length in bytes, across all files.
    pub fn total_length(&self) -> Result<u64> {
        match self.layout()? {
            FileLayout::Single { length } => Ok(length),
            FileLayout::Multi { files } => Ok(files.iter().map(|f| f.length).sum()),
        }
    }
}

/// The top-level `.torrent` metainfo dictionary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metainfo {
    /// Primary tracker URL. Optional because DHT-only / announce-list-only
    /// torrents may omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announce: Option<String>,

    /// Tiered list of backup trackers (BEP 12). Outer list = tiers, inner =
    /// trackers within a tier.
    #[serde(rename = "announce-list", default, skip_serializing_if = "Option::is_none")]
    pub announce_list: Option<Vec<Vec<String>>>,

    /// The info dictionary describing the content.
    pub info: Info,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    #[serde(rename = "created by", default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    #[serde(rename = "creation date", default, skip_serializing_if = "Option::is_none")]
    pub creation_date: Option<i64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,

    /// SHA-1 of the *original* `info`-dict bytes, captured during
    /// [`Metainfo::from_bytes`]. This — not a re-serialization of [`Info`] —
    /// is the authoritative info hash. `None` when the `Metainfo` was built by
    /// hand rather than parsed from bytes.
    #[serde(skip)]
    pub raw_info_hash: Option<InfoHash>,
}

/// Locate the exact byte span of the top-level `info` value in a bencoded
/// metainfo buffer and hash it. Hashing the original bytes (rather than a
/// re-encode of our typed struct) is the only way to get an info hash that
/// matches what every other client and tracker computes.
fn raw_info_hash(buf: &[u8]) -> Result<InfoHash> {
    let info = extract_raw_info(buf)?;
    let mut out = [0u8; SHA1_LEN];
    out.copy_from_slice(&Sha1::digest(info));
    Ok(InfoHash(out))
}

/// Return the raw bytes of the top-level `info` value, by a minimal bencode
/// walk that never copies or re-encodes.
fn extract_raw_info(buf: &[u8]) -> Result<&[u8]> {
    if buf.first() != Some(&b'd') {
        return Err(Error::MalformedTorrent(
            "metainfo is not a bencode dictionary".into(),
        ));
    }
    let mut i = 1;
    while *buf
        .get(i)
        .ok_or_else(|| Error::MalformedTorrent("unterminated top-level dictionary".into()))?
        != b'e'
    {
        let (key, after_key) = read_byte_string(buf, i)?;
        let val_end = skip_value(buf, after_key)?;
        if key == b"info" {
            return Ok(&buf[after_key..val_end]);
        }
        i = val_end;
    }
    Err(Error::MalformedTorrent("metainfo has no `info` dictionary".into()))
}

/// Parse a bencode byte string `<len>:<bytes>` at `i`; return the bytes and the
/// index just past them.
fn read_byte_string(buf: &[u8], i: usize) -> Result<(&[u8], usize)> {
    let colon = buf[i..]
        .iter()
        .position(|&b| b == b':')
        .map(|off| i + off)
        .ok_or_else(|| Error::MalformedTorrent("byte string missing ':' delimiter".into()))?;
    let len: usize = std::str::from_utf8(&buf[i..colon])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| Error::MalformedTorrent("invalid byte-string length".into()))?;
    let start = colon + 1;
    let end = start
        .checked_add(len)
        .filter(|&e| e <= buf.len())
        .ok_or_else(|| Error::MalformedTorrent("byte string runs past end of buffer".into()))?;
    Ok((&buf[start..end], end))
}

/// Return the index just past the bencode value beginning at `i`.
fn skip_value(buf: &[u8], i: usize) -> Result<usize> {
    let kind = *buf
        .get(i)
        .ok_or_else(|| Error::MalformedTorrent("unexpected end of buffer".into()))?;
    match kind {
        // Integer: i<digits>e
        b'i' => buf[i..]
            .iter()
            .position(|&b| b == b'e')
            .map(|off| i + off + 1)
            .ok_or_else(|| Error::MalformedTorrent("unterminated integer".into())),
        // List or dictionary: skip contained values until the matching 'e'.
        b'l' | b'd' => {
            let mut j = i + 1;
            while *buf
                .get(j)
                .ok_or_else(|| Error::MalformedTorrent("unterminated container".into()))?
                != b'e'
            {
                j = skip_value(buf, j)?;
            }
            Ok(j + 1)
        }
        // Byte string.
        b'0'..=b'9' => Ok(read_byte_string(buf, i)?.1),
        other => Err(Error::MalformedTorrent(format!(
            "unexpected bencode byte 0x{other:02x} at offset {i}"
        ))),
    }
}

impl Metainfo {
    /// Parse metainfo from in-memory bencoded bytes.
    ///
    /// Besides the typed fields, this captures the SHA-1 of the original
    /// `info`-dict bytes so [`info_hash`](Self::info_hash) is faithful even for
    /// torrents carrying keys we don't model.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let mut meta: Metainfo = serde_bencode::from_bytes(bytes)?;
        meta.raw_info_hash = Some(raw_info_hash(bytes)?);
        Ok(meta)
    }

    /// Read and parse a `.torrent` file from disk.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    /// The torrent's info hash.
    ///
    /// Returns the hash captured from the original `info`-dict bytes when this
    /// `Metainfo` was parsed via [`from_bytes`](Self::from_bytes). For a
    /// hand-constructed `Metainfo` (no original bytes), it falls back to
    /// re-serializing the typed [`Info`].
    pub fn info_hash(&self) -> Result<InfoHash> {
        match self.raw_info_hash {
            Some(hash) => Ok(hash),
            None => self.info.info_hash(),
        }
    }

    /// All announce URLs (primary + announce-list), de-duplicated by first
    /// appearance and flattened across tiers. Useful for Module 2.
    pub fn all_trackers(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut push = |url: &String| {
            if seen.insert(url.clone()) {
                out.push(url.clone());
            }
        };
        if let Some(announce) = &self.announce {
            push(announce);
        }
        if let Some(tiers) = &self.announce_list {
            for tier in tiers {
                for url in tier {
                    push(url);
                }
            }
        }
        out
    }

    /// Resolve on-disk paths for each file, rooted at `base_dir`.
    ///
    /// For a single-file torrent this is `base_dir/<name>`. For multi-file it
    /// is `base_dir/<name>/<path components...>`. Returned alongside each
    /// file's length, which Module 5 will use to lay out the files.
    ///
    /// Every name component (the torrent `name` and each `path` element) is
    /// validated to be a single, plain segment — a malicious `.torrent` cannot
    /// use `..`, an absolute path, or embedded separators to escape `base_dir`
    /// (see [`validate_path_component`]).
    pub fn file_paths(&self, base_dir: impl AsRef<Path>) -> Result<Vec<(PathBuf, u64)>> {
        let base = base_dir.as_ref();
        validate_path_component(&self.info.name)?;
        match self.info.layout()? {
            FileLayout::Single { length } => {
                Ok(vec![(base.join(&self.info.name), length)])
            }
            FileLayout::Multi { files } => {
                let root = base.join(&self.info.name);
                files
                    .iter()
                    .map(|f| {
                        let mut p = root.clone();
                        for component in &f.path {
                            validate_path_component(component)?;
                            p.push(component);
                        }
                        Ok((p, f.length))
                    })
                    .collect()
            }
        }
    }
}

/// Reject a path component taken from a `.torrent` that could escape the
/// download directory.
///
/// A hostile torrent can place `..`, an absolute path, or embedded separators
/// in its `name` or a file's `path` list; pushing those straight onto a
/// `PathBuf` would let it write anywhere the process can (classic path
/// traversal). We require each component to be a single, plain, non-relative
/// segment with no separators or NUL bytes.
fn validate_path_component(component: &str) -> Result<()> {
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('/')
        || component.contains('\\')
        || component.contains('\0')
    {
        return Err(Error::MalformedTorrent(format!(
            "unsafe path component {component:?} in torrent (possible path traversal)"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bencoded `info` dictionary used in the tests below, hand-written
    /// with keys already in canonical (sorted) order so we can hash it
    /// independently of our serde round-trip:
    ///   length < name < piece length < pieces
    const INFO_BYTES: &[u8] =
        b"d6:lengthi12e4:name4:test12:piece lengthi32768e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";

    /// A complete single-file torrent wrapping `INFO_BYTES`. Top-level keys are
    /// sorted: announce < info.
    fn single_file_torrent() -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"d8:announce15:http://tracker/4:info");
        buf.extend_from_slice(INFO_BYTES);
        buf.extend_from_slice(b"e");
        buf
    }

    #[test]
    fn parses_single_file_torrent() {
        let bytes = single_file_torrent();
        let meta = Metainfo::from_bytes(&bytes).expect("should parse");

        assert_eq!(meta.announce.as_deref(), Some("http://tracker/"));
        assert_eq!(meta.info.name, "test");
        assert_eq!(meta.info.piece_length, 32768);
        assert_eq!(meta.info.length, Some(12));
        assert_eq!(meta.info.files, None);
        assert_eq!(meta.info.num_pieces(), 1);
        assert_eq!(meta.info.total_length().unwrap(), 12);
        assert_eq!(
            meta.info.layout().unwrap(),
            FileLayout::Single { length: 12 }
        );
    }

    #[test]
    fn info_hash_matches_independent_sha1_of_info_dict() {
        // Independently hash the known raw info-dict bytes...
        let expected = {
            let mut out = [0u8; SHA1_LEN];
            out.copy_from_slice(&Sha1::digest(INFO_BYTES));
            InfoHash(out)
        };

        // ...and confirm our parse + re-serialize path reproduces it exactly.
        let meta = Metainfo::from_bytes(&single_file_torrent()).unwrap();
        let actual = meta.info_hash().unwrap();

        assert_eq!(actual, expected, "info hash must match raw info-dict SHA-1");
    }

    #[test]
    fn info_hash_uses_raw_bytes_and_preserves_unmodeled_keys() {
        // An info dict carrying a top-level `md5sum` key (sorted before `name`)
        // that our `Info` struct does NOT model. A re-serialization would drop
        // it and yield the wrong hash; hashing the raw bytes must not.
        //   length < md5sum < name < piece length < pieces
        let info: &[u8] = b"d6:lengthi12e6:md5sum4:beef4:name4:test12:piece lengthi32768e6:pieces20:AAAAAAAAAAAAAAAAAAAAe";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"d8:announce15:http://tracker/4:info");
        bytes.extend_from_slice(info);
        bytes.extend_from_slice(b"e");

        let expected = {
            let mut out = [0u8; SHA1_LEN];
            out.copy_from_slice(&Sha1::digest(info));
            InfoHash(out)
        };

        let meta = Metainfo::from_bytes(&bytes).unwrap();
        // The authoritative hash matches the raw info bytes...
        assert_eq!(meta.info_hash().unwrap(), expected);
        // ...and is demonstrably different from the lossy re-serialization,
        // proving the bug this guards against is real.
        assert_ne!(meta.info.info_hash().unwrap(), expected);
    }

    #[test]
    fn piece_hashes_iterates_20_byte_chunks() {
        let meta = Metainfo::from_bytes(&single_file_torrent()).unwrap();
        let hashes: Vec<_> = meta.info.piece_hashes().unwrap().collect();
        assert_eq!(hashes.len(), 1);
        assert_eq!(hashes[0], b"AAAAAAAAAAAAAAAAAAAA");
    }

    #[test]
    fn rejects_pieces_not_multiple_of_20() {
        let info = Info {
            name: "x".into(),
            piece_length: 16,
            pieces: vec![0u8; 25], // not a multiple of 20
            length: Some(16),
            files: None,
            private: None,
        };
        assert!(matches!(
            info.piece_hashes().err(),
            Some(Error::InvalidPiecesLength(25))
        ));
    }

    #[test]
    fn parses_multi_file_torrent_and_resolves_paths() {
        // info dict with a `files` list instead of `length`.
        // Keys sorted: files < name < piece length < pieces.
        let info = b"d5:filesld6:lengthi10e4:pathl3:dir1:aeed6:lengthi20e4:pathl1:beee\
4:name3:dir12:piece lengthi16e6:pieces20:BBBBBBBBBBBBBBBBBBBBe";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"d8:announce15:http://tracker/4:info");
        bytes.extend_from_slice(info);
        bytes.extend_from_slice(b"e");

        let meta = Metainfo::from_bytes(&bytes).expect("should parse multi-file");
        assert_eq!(meta.info.length, None);
        assert_eq!(meta.info.total_length().unwrap(), 30);

        let files = meta.info.files.as_ref().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, vec!["dir".to_string(), "a".to_string()]);

        let paths = meta.file_paths("/downloads").unwrap();
        assert_eq!(
            paths,
            vec![
                (PathBuf::from("/downloads/dir/dir/a"), 10),
                (PathBuf::from("/downloads/dir/b"), 20),
            ]
        );
    }

    #[test]
    fn rejects_path_traversal_in_file_paths() {
        // A multi-file torrent whose path tries to climb out of the download
        // dir via `..` must be rejected, not resolved to `/etc/...`.
        let info = b"d5:filesld6:lengthi10e4:pathl2:..6:passwdeee\
4:name3:dir12:piece lengthi16e6:pieces20:CCCCCCCCCCCCCCCCCCCCe";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"d8:announce15:http://tracker/4:info");
        bytes.extend_from_slice(info);
        bytes.extend_from_slice(b"e");

        let meta = Metainfo::from_bytes(&bytes).expect("parses; rejection is at resolve time");
        assert!(matches!(
            meta.file_paths("/downloads"),
            Err(Error::MalformedTorrent(_))
        ));
    }

    #[test]
    fn rejects_path_traversal_in_torrent_name() {
        // A single-file torrent whose `name` is `..` would escape the base dir.
        let meta = Metainfo {
            announce: None,
            announce_list: None,
            info: Info {
                name: "..".into(),
                piece_length: 16,
                pieces: vec![0u8; 20],
                length: Some(16),
                files: None,
                private: None,
            },
            comment: None,
            created_by: None,
            creation_date: None,
            encoding: None,
            raw_info_hash: None,
        };
        assert!(matches!(
            meta.file_paths("/downloads"),
            Err(Error::MalformedTorrent(_))
        ));
    }

    #[test]
    fn collects_all_trackers_deduplicated() {
        let meta = Metainfo {
            announce: Some("http://primary/".into()),
            announce_list: Some(vec![
                vec!["http://primary/".into(), "http://t1/".into()],
                vec!["http://t2/".into()],
            ]),
            info: Info {
                name: "x".into(),
                piece_length: 16,
                pieces: vec![0u8; 20],
                length: Some(16),
                files: None,
                private: None,
            },
            comment: None,
            created_by: None,
            creation_date: None,
            encoding: None,
            raw_info_hash: None,
        };
        assert_eq!(
            meta.all_trackers(),
            vec!["http://primary/", "http://t1/", "http://t2/"]
        );
    }
}
