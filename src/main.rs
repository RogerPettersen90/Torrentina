//! Torrentina CLI — wires the five library modules into a working downloader:
//!
//! parse `.torrent` → announce to trackers → collect peers → run the
//! coordinator → stream verified pieces to disk, printing progress.

use std::collections::HashSet;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::Parser;
use tokio::sync::mpsc;

use torrentina::download::VerifiedPiece;
use torrentina::tracker::{self, AnnounceParams, Event};
use torrentina::{Coordinator, Metainfo, PeerId, Storage};

/// A lightweight BitTorrent client.
#[derive(Parser, Debug)]
#[command(name = "torrentina", version, about)]
struct Args {
    /// Path to the `.torrent` file.
    torrent: PathBuf,

    /// Directory to download into.
    #[arg(short, long, default_value = ".")]
    output: PathBuf,

    /// TCP port to advertise to the tracker.
    #[arg(short, long, default_value_t = 6881)]
    port: u16,

    /// Maximum number of peers to request from each tracker.
    #[arg(long, default_value_t = 50)]
    numwant: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // ---- Module 1: parse the .torrent ----
    let meta = Metainfo::from_file(&args.torrent)
        .with_context(|| format!("reading torrent file {}", args.torrent.display()))?;
    let info_hash = meta.info_hash()?;
    let total_length = meta.info.total_length()?;
    let num_pieces = meta.info.num_pieces();
    let peer_id = PeerId::generate();

    println!("Torrent : {}", meta.info.name);
    println!("Info hash: {info_hash}");
    println!(
        "Size    : {} bytes across {} piece(s)",
        total_length, num_pieces
    );

    // ---- Module 2: announce to trackers, collecting peers ----
    let params = AnnounceParams {
        info_hash,
        peer_id,
        port: args.port,
        uploaded: 0,
        downloaded: 0,
        left: total_length,
        event: Event::Started,
        compact: true,
        numwant: Some(args.numwant),
    };

    let peers = collect_peers(&meta, &params).await?;
    println!("Peers   : {} unique address(es)", peers.len());

    // ---- Module 5: prepare on-disk files ----
    let storage = Storage::create(&meta, &args.output)
        .await
        .context("creating output files")?;

    // ---- Module 4: run the download, streaming pieces to disk ----
    let coordinator = Coordinator::new(&meta.info, info_hash, peer_id)?;
    let (tx, rx) = mpsc::channel::<VerifiedPiece>(64);

    // Consumer task: write each verified piece to disk and report progress.
    let consumer = tokio::spawn(consume_to_disk(storage, rx, num_pieces, total_length));

    // Drive the swarm. Returns once every peer task has finished; dropping the
    // last `tx` (held inside `run`) then closes the consumer's channel.
    coordinator.run(peers, tx).await;

    consumer.await.context("disk writer task panicked")??;

    if coordinator.tracker().lock().await.is_complete() {
        println!("\nDownload complete: {}", meta.info.name);
        Ok(())
    } else {
        bail!("\ndownload stalled: peers exhausted before all pieces were obtained");
    }
}

/// Try each tracker in turn, returning the de-duplicated union of peers from
/// the first tracker(s) that respond. Stops early once we have some peers.
async fn collect_peers(meta: &Metainfo, params: &AnnounceParams) -> anyhow::Result<Vec<SocketAddr>> {
    let mut seen = HashSet::new();
    let mut peers = Vec::new();

    for url in meta.all_trackers() {
        match tracker::announce(&url, params).await {
            Ok(resp) => {
                println!(
                    "  tracker {url}: {} peer(s), reannounce in {}s",
                    resp.peers.len(),
                    resp.interval
                );
                for addr in resp.peers {
                    if seen.insert(addr) {
                        peers.push(addr);
                    }
                }
                if !peers.is_empty() {
                    break;
                }
            }
            Err(e) => eprintln!("  tracker {url} failed: {e}"),
        }
    }

    if peers.is_empty() {
        bail!("no peers returned by any tracker");
    }
    Ok(peers)
}

/// Receive verified pieces, write them to disk, and render a progress bar.
async fn consume_to_disk(
    mut storage: Storage,
    mut rx: mpsc::Receiver<VerifiedPiece>,
    num_pieces: usize,
    total_length: u64,
) -> anyhow::Result<()> {
    let mut done = 0usize;
    let mut bytes = 0u64;

    while let Some(piece) = rx.recv().await {
        bytes += piece.data.len() as u64;
        storage
            .write_piece(piece.index, &piece.data)
            .await
            .with_context(|| format!("writing piece {}", piece.index))?;
        done += 1;
        print_progress(done, num_pieces, bytes, total_length);
    }

    storage.finish().await.context("flushing files")?;
    Ok(())
}

/// Render an in-place progress line to stdout.
fn print_progress(done: usize, total: usize, bytes: u64, total_bytes: u64) {
    let pct = if total_bytes > 0 {
        bytes as f64 / total_bytes as f64 * 100.0
    } else {
        100.0
    };
    let width = 30;
    let filled = (pct / 100.0 * width as f64).round() as usize;
    let bar: String = "█".repeat(filled) + &"░".repeat(width - filled);
    print!("\r[{bar}] {pct:5.1}%  {done}/{total} pieces");
    let _ = std::io::stdout().flush();
}
