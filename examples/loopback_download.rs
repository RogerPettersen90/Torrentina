//! A self-contained, end-to-end demonstration of the Torrentina engine.
//!
//! It builds a synthetic torrent in memory, starts a *seeder* on a real TCP
//! socket (speaking the actual peer wire protocol), then runs the real
//! [`Coordinator`] to download every piece, verify each SHA-1, and write the
//! result to disk via [`Storage`] — printing a live progress bar throughout.
//!
//! Run with:  `cargo run --example loopback_download`

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use sha1::{Digest, Sha1};
use tokio::net::TcpListener;
use tokio_util::codec::Framed;

use torrentina::download::{Bitfield, Coordinator, VerifiedPiece};
use torrentina::metainfo::{Info, Metainfo};
use torrentina::peer::connection::handshake;
use torrentina::peer::message::PeerCodec;
use torrentina::peer::PeerId;
use torrentina::{Block, InfoHash, Message, Storage};

const PIECE_LEN: usize = 256 * 1024; // 256 KiB pieces (16 blocks each)
const TOTAL: usize = 4 * 1024 * 1024 + 12345; // ~4 MiB, deliberately not piece-aligned

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 1. Build synthetic content + a matching single-file torrent.
    let data: Vec<u8> = (0..TOTAL).map(|i| (i.wrapping_mul(31) % 251) as u8).collect();
    let mut pieces = Vec::new();
    for chunk in data.chunks(PIECE_LEN) {
        pieces.extend_from_slice(&Sha1::digest(chunk));
    }
    let info = Info {
        name: "demo.bin".into(),
        piece_length: PIECE_LEN as u64,
        pieces,
        length: Some(TOTAL as u64),
        files: None,
        private: None,
    };
    let meta = Metainfo {
        announce: None,
        announce_list: None,
        comment: None,
        created_by: None,
        creation_date: None,
        encoding: None,
        raw_info_hash: None,
        info,
    };
    let info_hash = meta.info_hash()?;
    let num_pieces = meta.info.num_pieces();

    println!("Torrentina — end-to-end loopback demo");
    println!("  content : {} bytes ({:.2} MiB)", TOTAL, TOTAL as f64 / 1048576.0);
    println!("  pieces  : {num_pieces} × {PIECE_LEN} bytes (last piece shorter)");
    println!("  infohash: {info_hash}\n");

    // 2. Start a seeder on a real TCP socket.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let seed_addr = listener.local_addr()?;
    let seeder = tokio::spawn(run_seeder(listener, info_hash, data.clone(), num_pieces));
    println!("Seeder listening on {seed_addr}\n");

    // 3. Prepare disk output (a temp dir) and the coordinator.
    let out_dir = tempfile::tempdir()?;
    let storage = Storage::create(&meta, out_dir.path()).await?;
    let coordinator = Arc::new(Coordinator::new(&meta.info, info_hash, PeerId::generate())?);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<VerifiedPiece>(64);

    // 4. Disk-writer task: persist each verified piece.
    let consumer = tokio::spawn(async move {
        let mut storage = storage;
        while let Some(piece) = rx.recv().await {
            storage.write_piece(piece.index, &piece.data).await?;
        }
        storage.finish().await?;
        anyhow::Ok(())
    });

    // 5. Progress printer: snapshot the coordinator and render a bar.
    let start = Instant::now();
    let printer_coord = Arc::clone(&coordinator);
    let printer = tokio::spawn(async move {
        loop {
            let s = printer_coord.snapshot().await;
            print_bar(&s, start);
            if s.complete {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });

    // 6. Run the real download against the seeder.
    coordinator.run(vec![seed_addr], tx).await;
    consumer.await??;
    let _ = printer.await;
    print_bar(&coordinator.snapshot().await, start);
    println!("\n");

    // 7. Verify what landed on disk matches the original, byte for byte.
    let written = std::fs::read(out_dir.path().join("demo.bin"))?;
    let ok = written == data;
    println!(
        "Disk file : {} bytes, sha1 {}",
        written.len(),
        hex(&Sha1::digest(&written))
    );
    println!("Original  : {} bytes, sha1 {}", data.len(), hex(&Sha1::digest(&data)));
    println!(
        "\nResult    : {}",
        if ok {
            "✅ SUCCESS — downloaded file matches the original exactly"
        } else {
            "❌ MISMATCH"
        }
    );

    seeder.abort();
    if !ok {
        anyhow::bail!("downloaded data did not match");
    }
    Ok(())
}

/// A minimal seeder: handshake, advertise all pieces, unchoke, then serve every
/// block request with the correct slice (with a tiny delay so the progress bar
/// is visible).
async fn run_seeder(listener: TcpListener, info_hash: InfoHash, data: Vec<u8>, num_pieces: usize) {
    let (mut stream, _) = listener.accept().await.unwrap();
    handshake(&mut stream, info_hash, PeerId(*b"-TN0001-SEEDSEEDSEED"))
        .await
        .unwrap();

    let mut full = Bitfield::new(num_pieces);
    for i in 0..num_pieces {
        full.set(i);
    }
    let mut framed = Framed::new(stream, PeerCodec::new());
    framed.send(Message::Bitfield(full.to_message_bytes())).await.unwrap();
    framed.send(Message::Unchoke).await.unwrap();

    while let Some(Ok(msg)) = framed.next().await {
        if let Message::Request(bi) = msg {
            let start = bi.index as usize * PIECE_LEN + bi.begin as usize;
            let end = start + bi.length as usize;
            tokio::time::sleep(Duration::from_millis(1)).await; // simulate latency
            framed
                .send(Message::Piece(Block {
                    index: bi.index,
                    begin: bi.begin,
                    data: Bytes::copy_from_slice(&data[start..end]),
                }))
                .await
                .unwrap();
        }
    }
}

/// Render an in-place progress bar from a stats snapshot.
fn print_bar(s: &torrentina::StatsSnapshot, start: Instant) {
    let pct = if s.total_bytes > 0 {
        s.bytes_done as f64 / s.total_bytes as f64 * 100.0
    } else {
        100.0
    };
    let width = 32;
    let filled = (pct / 100.0 * width as f64).round() as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(width - filled);
    let secs = start.elapsed().as_secs_f64().max(0.001);
    let rate = s.wire_bytes as f64 / secs / 1024.0;
    print!(
        "\r[{bar}] {pct:5.1}%  {}/{} pieces  {} peer(s)  {:.0} KiB/s",
        s.pieces_done, s.total_pieces, s.connected_peers, rate
    );
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
