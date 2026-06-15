//! Module 5: Disk I/O & file assembly.
//!
//! A torrent is a single logical byte stream split into pieces, but on disk it
//! maps to one (single-file) or many (multi-file) files laid end to end. Piece
//! `index` lives at global offset `index * piece_length`, and a single piece
//! may **straddle** a file boundary — ending one file and beginning the next.
//!
//! [`map_region`] is the pure heart of this module: it turns a `(global_offset,
//! length)` range into the list of per-file writes that cover it. [`Storage`]
//! wraps that with async file creation and positioned writes.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use crate::download::Geometry;
use crate::error::{Error, Result};
use crate::metainfo::Metainfo;

/// One file in the torrent, placed at a global offset in the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedFile {
    /// Resolved on-disk path.
    pub path: PathBuf,
    /// File length in bytes.
    pub length: u64,
    /// Where this file begins in the concatenated torrent stream.
    pub offset: u64,
}

/// A single positioned write: copy `src[src_start..src_start+len]` to file
/// `file_index` starting at `file_offset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WriteOp {
    file_index: usize,
    file_offset: u64,
    src_start: usize,
    len: usize,
}

/// Build the file map (paths + cumulative offsets) for a torrent under
/// `base_dir`, handling both single- and multi-file layouts.
fn build_file_map(meta: &Metainfo, base_dir: impl AsRef<Path>) -> Result<Vec<MappedFile>> {
    let mut offset = 0u64;
    let mut files = Vec::new();
    for (path, length) in meta.file_paths(base_dir)? {
        files.push(MappedFile {
            path,
            length,
            offset,
        });
        // Lengths come from an untrusted `.torrent`; a hostile (or absurd) file
        // table must not silently wrap the running offset.
        offset = offset
            .checked_add(length)
            .ok_or_else(|| Error::Storage("total torrent size overflows u64".into()))?;
    }
    Ok(files)
}

/// Compute the per-file writes covering `[global_offset, global_offset + len)`.
///
/// Pure and allocation-light: walks the files once, emitting a [`WriteOp`] for
/// each that overlaps the range (zero-length and non-overlapping files are
/// skipped). A piece straddling N files yields N ops.
fn map_region(files: &[MappedFile], global_offset: u64, len: usize) -> Vec<WriteOp> {
    let region_end = global_offset + len as u64;
    let mut ops = Vec::new();
    for (file_index, file) in files.iter().enumerate() {
        let file_end = file.offset + file.length;
        // Intersect [global_offset, region_end) with [file.offset, file_end).
        let start = global_offset.max(file.offset);
        let end = region_end.min(file_end);
        if start >= end {
            continue;
        }
        ops.push(WriteOp {
            file_index,
            file_offset: start - file.offset,
            src_start: (start - global_offset) as usize,
            len: (end - start) as usize,
        });
    }
    ops
}

/// Owns the open file handles and writes verified pieces to their correct
/// locations on disk.
pub struct Storage {
    geometry: Geometry,
    files: Vec<MappedFile>,
    handles: Vec<File>,
}

impl Storage {
    /// Create (and preallocate) all of the torrent's files under `base_dir`,
    /// opening a writable handle for each.
    pub async fn create(meta: &Metainfo, base_dir: impl AsRef<Path>) -> Result<Self> {
        let geometry = Geometry::from_info(&meta.info)?;
        let files = build_file_map(meta, base_dir)?;

        let mut handles = Vec::with_capacity(files.len());
        for file in &files {
            if let Some(parent) = file.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let handle = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&file.path)
                .await?;
            // Preallocate so positioned writes always land within the file.
            handle.set_len(file.length).await?;
            handles.push(handle);
        }

        Ok(Storage {
            geometry,
            files,
            handles,
        })
    }

    /// The resolved file map (paths, lengths, offsets).
    pub fn files(&self) -> &[MappedFile] {
        &self.files
    }

    /// Write one verified piece's bytes to the correct file region(s).
    pub async fn write_piece(&mut self, index: u32, data: &[u8]) -> Result<()> {
        let global_offset = self.geometry.piece_offset(index);
        let ops = map_region(&self.files, global_offset, data.len());

        let mut covered = 0usize;
        for op in &ops {
            let handle = &mut self.handles[op.file_index];
            handle.seek(SeekFrom::Start(op.file_offset)).await?;
            handle
                .write_all(&data[op.src_start..op.src_start + op.len])
                .await?;
            covered += op.len;
        }

        // Every byte of the piece must land somewhere; otherwise the torrent
        // geometry and file map disagree.
        if covered != data.len() {
            return Err(Error::Storage(format!(
                "piece {index} mapped {covered} of {} bytes to files",
                data.len()
            )));
        }
        Ok(())
    }

    /// Flush all file handles, ensuring buffered writes reach the OS.
    pub async fn finish(&mut self) -> Result<()> {
        for handle in &mut self.handles {
            handle.flush().await?;
        }
        Ok(())
    }
}

/// Drain a stream of verified pieces from the coordinator straight to disk,
/// then flush. This is the glue between Module 4 and the filesystem.
pub async fn assemble(
    mut storage: Storage,
    mut pieces: tokio::sync::mpsc::Receiver<crate::download::VerifiedPiece>,
) -> Result<()> {
    while let Some(piece) = pieces.recv().await {
        storage.write_piece(piece.index, &piece.data).await?;
    }
    storage.finish().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metainfo::{FileEntry, Info};

    fn files(spec: &[(u64, u64)]) -> Vec<MappedFile> {
        spec.iter()
            .enumerate()
            .map(|(i, &(offset, length))| MappedFile {
                path: PathBuf::from(format!("f{i}")),
                length,
                offset,
            })
            .collect()
    }

    #[test]
    fn maps_region_within_a_single_file() {
        let files = files(&[(0, 100)]);
        let ops = map_region(&files, 10, 20);
        assert_eq!(
            ops,
            vec![WriteOp { file_index: 0, file_offset: 10, src_start: 0, len: 20 }]
        );
    }

    #[test]
    fn maps_region_straddling_two_files() {
        // file 0: [0, 40000), file 1: [40000, 70000)
        let files = files(&[(0, 40000), (40000, 30000)]);
        // A piece at global 32768, length 32768 -> [32768, 65536).
        let ops = map_region(&files, 32768, 32768);
        assert_eq!(
            ops,
            vec![
                // tail of file 0: [32768, 40000) -> 7232 bytes at file offset 32768
                WriteOp { file_index: 0, file_offset: 32768, src_start: 0, len: 7232 },
                // head of file 1: [40000, 65536) -> 25536 bytes at file offset 0
                WriteOp { file_index: 1, file_offset: 0, src_start: 7232, len: 25536 },
            ]
        );
    }

    #[test]
    fn maps_region_at_exact_file_boundary() {
        let files = files(&[(0, 100), (100, 100)]);
        // Range [100, 150) lies entirely in file 1.
        let ops = map_region(&files, 100, 50);
        assert_eq!(
            ops,
            vec![WriteOp { file_index: 1, file_offset: 0, src_start: 0, len: 50 }]
        );
    }

    #[test]
    fn map_region_skips_zero_length_and_nonoverlapping_files() {
        // file 1 is zero-length and must be skipped cleanly.
        let files = files(&[(0, 50), (50, 0), (50, 50)]);
        let ops = map_region(&files, 0, 100);
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].file_index, 0);
        assert_eq!(ops[1].file_index, 2);
    }

    /// End-to-end multi-file write: a piece straddles two files (one nested in
    /// a subdir), pieces are written out of order, and we read everything back.
    #[tokio::test]
    async fn writes_multi_file_torrent_to_disk() {
        let piece_len = 32768usize;
        let (len_a, len_b) = (40000usize, 30000usize);
        let total = len_a + len_b; // 70000 -> 3 pieces (last is short)
        let num_pieces = total.div_ceil(piece_len);

        let data: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

        let meta = Metainfo {
            announce: None,
            announce_list: None,
            comment: None,
            created_by: None,
            creation_date: None,
            encoding: None,
            raw_info_hash: None,
            info: Info {
                name: "torrent".into(),
                piece_length: piece_len as u64,
                pieces: vec![0u8; num_pieces * 20],
                length: None,
                files: Some(vec![
                    FileEntry { length: len_a as u64, path: vec!["a.bin".into()], md5sum: None },
                    FileEntry {
                        length: len_b as u64,
                        path: vec!["sub".into(), "b.bin".into()],
                        md5sum: None,
                    },
                ]),
                private: None,
            },
        };

        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::create(&meta, dir.path()).await.unwrap();

        // Write pieces in reverse order to prove offsets are positioned, not
        // sequential/append.
        let geometry = Geometry::from_info(&meta.info).unwrap();
        for index in (0..num_pieces as u32).rev() {
            let start = geometry.piece_offset(index) as usize;
            let plen = geometry.piece_length(index) as usize;
            storage
                .write_piece(index, &data[start..start + plen])
                .await
                .unwrap();
        }
        storage.finish().await.unwrap();

        // Read the files back and verify they match the expected slices.
        let path_a = dir.path().join("torrent").join("a.bin");
        let path_b = dir.path().join("torrent").join("sub").join("b.bin");
        let got_a = std::fs::read(&path_a).unwrap();
        let got_b = std::fs::read(&path_b).unwrap();

        assert_eq!(got_a, &data[..len_a], "file A contents");
        assert_eq!(got_b, &data[len_a..], "file B contents");
        assert_eq!(got_a.len(), len_a);
        assert_eq!(got_b.len(), len_b);
    }

    #[tokio::test]
    async fn writes_single_file_torrent_to_disk() {
        let data: Vec<u8> = (0..5000u32).map(|i| i as u8).collect();
        let meta = Metainfo {
            announce: None,
            announce_list: None,
            comment: None,
            created_by: None,
            creation_date: None,
            encoding: None,
            raw_info_hash: None,
            info: Info {
                name: "single.bin".into(),
                piece_length: 16384,
                pieces: vec![0u8; 20],
                length: Some(5000),
                files: None,
                private: None,
            },
        };

        let dir = tempfile::tempdir().unwrap();
        let mut storage = Storage::create(&meta, dir.path()).await.unwrap();
        storage.write_piece(0, &data).await.unwrap();
        storage.finish().await.unwrap();

        let got = std::fs::read(dir.path().join("single.bin")).unwrap();
        assert_eq!(got, data);
    }
}
