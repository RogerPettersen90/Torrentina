//! Torrentina desktop GUI (Tauri v2).
//!
//! The Rust side manages a registry of torrents keyed by their info-hash hex
//! ("id"). Each torrent runs the engine's [`Coordinator`] on a background task
//! and streams per-torrent events to the webview:
//!
//! * `stats`    — a [`StatsSnapshot`] (tagged with `id`) roughly twice a second,
//! * `log`      — human-readable progress/diagnostic lines (tagged with `id`),
//! * `finished` — `{ id, status }` once a download ends.
//!
//! The registry is persisted to `torrents.json` in the app data dir so the list
//! survives restarts (restored entries come back *stopped*, not auto-resumed).
//! All download logic lives in the `torrentina` library crate; this file only
//! orchestrates, persists, and forwards progress to the UI.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use torrentina::download::VerifiedPiece;
use torrentina::tracker::{self, AnnounceParams, Event};
use torrentina::{ControlState, Coordinator, Metainfo, PeerId, Storage};

/// A persisted, serializable description of one torrent in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    /// Info-hash hex; the stable unique id used everywhere.
    id: String,
    name: String,
    /// Path to the `.torrent` file (re-parsed on start / restart).
    torrent_path: String,
    /// Output directory the content is downloaded into.
    output: String,
    total_bytes: u64,
    /// Resolved absolute paths of every file in the torrent (for data deletion).
    files: Vec<String>,
    /// For multi-file torrents, the `output/<name>` directory to remove wholesale.
    root: Option<String>,
    /// One of: downloading | paused | complete | stopped | error.
    status: String,
}

/// Live handle for one torrent: its persisted record plus the running
/// coordinator (absent when stopped/restored).
struct Handle {
    coordinator: Option<Arc<Coordinator>>,
    record: Record,
}

/// The whole app's torrent registry, keyed by id.
#[derive(Default)]
struct AppState {
    torrents: Mutex<HashMap<String, Handle>>,
}

/// A log line forwarded to the UI, tagged with the owning torrent.
#[derive(Clone, Serialize)]
struct LogEvent {
    id: String,
    level: String,
    message: String,
}

/// A stats snapshot tagged with the owning torrent's id.
#[derive(Clone, Serialize)]
struct TorrentStats {
    id: String,
    #[serde(flatten)]
    snapshot: torrentina::StatsSnapshot,
}

/// A terminal-status event for one torrent.
#[derive(Clone, Serialize)]
struct FinishedEvent {
    id: String,
    status: String,
}

/// Emit a log line for torrent `id` to the frontend.
fn log(app: &AppHandle, id: &str, level: &str, message: impl Into<String>) {
    let _ = app.emit(
        "log",
        LogEvent {
            id: id.to_string(),
            level: level.to_string(),
            message: message.into(),
        },
    );
}

/// Path to the persisted registry file, creating the data dir if needed.
fn store_path(app: &AppHandle) -> anyhow::Result<PathBuf> {
    let dir = app.path().app_data_dir().context("resolving app data dir")?;
    std::fs::create_dir_all(&dir).context("creating app data dir")?;
    Ok(dir.join("torrents.json"))
}

/// Persist the current registry to disk (best-effort).
fn persist(app: &AppHandle) {
    let state = app.state::<AppState>();
    let records: Vec<Record> = state
        .torrents
        .lock()
        .unwrap()
        .values()
        .map(|h| h.record.clone())
        .collect();
    if let Ok(path) = store_path(app) {
        if let Ok(json) = serde_json::to_vec_pretty(&records) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// Update a torrent's status in the registry and persist.
fn set_status(app: &AppHandle, id: &str, status: &str) {
    {
        let state = app.state::<AppState>();
        let mut map = state.torrents.lock().unwrap();
        if let Some(h) = map.get_mut(id) {
            h.record.status = status.to_string();
        }
    }
    persist(app);
}

/// Delete the on-disk data for a torrent: a multi-file torrent's whole
/// `output/<name>` directory, or a single-file torrent's lone file. Only ever
/// touches the torrent's own paths, never the broader output directory.
fn delete_data(record: &Record) {
    if let Some(root) = &record.root {
        let _ = std::fs::remove_dir_all(root);
    } else {
        for f in &record.files {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Tauri command: open a native file picker and return the chosen `.torrent`
/// path (or `None` if the user cancelled).
#[tauri::command]
async fn pick_torrent(app: AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .add_filter("Torrent files", &["torrent"])
        .set_title("Select a .torrent file")
        .pick_file(move |path| {
            let _ = tx.send(path.map(|p| p.to_string()));
        });
    rx.await.ok().flatten()
}

/// Tauri command: open a native folder picker and return the chosen directory.
#[tauri::command]
async fn pick_folder(app: AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Select an output directory")
        .pick_folder(move |path| {
            let _ = tx.send(path.map(|p| p.to_string()));
        });
    rx.await.ok().flatten()
}

/// Tauri command: list every torrent in the registry (for rebuilding the UI).
#[tauri::command]
fn list_torrents(state: tauri::State<AppState>) -> Vec<Record> {
    state
        .torrents
        .lock()
        .unwrap()
        .values()
        .map(|h| h.record.clone())
        .collect()
}

/// Tauri command: pause torrent `id` (no-op if not running).
#[tauri::command]
fn pause_download(app: AppHandle, state: tauri::State<AppState>, id: String) {
    if let Some(c) = state
        .torrents
        .lock()
        .unwrap()
        .get(&id)
        .and_then(|h| h.coordinator.clone())
    {
        c.pause();
    }
    set_status(&app, &id, "paused");
}

/// Tauri command: resume torrent `id`.
#[tauri::command]
fn resume_download(app: AppHandle, state: tauri::State<AppState>, id: String) {
    if let Some(c) = state
        .torrents
        .lock()
        .unwrap()
        .get(&id)
        .and_then(|h| h.coordinator.clone())
    {
        c.resume();
    }
    set_status(&app, &id, "downloading");
}

/// Tauri command: cancel torrent `id` (leaves it in the list as stopped).
#[tauri::command]
fn cancel_download(app: AppHandle, state: tauri::State<AppState>, id: String) {
    if let Some(c) = state
        .torrents
        .lock()
        .unwrap()
        .get(&id)
        .and_then(|h| h.coordinator.clone())
    {
        c.cancel();
    }
    set_status(&app, &id, "stopped");
}

/// Tauri command: remove torrent `id` from the app.
///
/// `mode` is one of:
/// * `"list"`          — forget the torrent, keep files on disk;
/// * `"list_and_data"` — forget the torrent **and** delete its files;
/// * `"data_only"`     — delete its files but keep the entry (stopped).
#[tauri::command]
fn remove_torrent(app: AppHandle, state: tauri::State<AppState>, id: String, mode: String) {
    // Stop any running download and pull out the record we may need to delete.
    let record = {
        let mut map = state.torrents.lock().unwrap();
        let Some(handle) = map.get_mut(&id) else {
            return;
        };
        if let Some(c) = &handle.coordinator {
            c.cancel();
        }
        handle.coordinator = None;
        if mode == "data_only" {
            handle.record.status = "stopped".into();
        }
        handle.record.clone()
    };

    // For the two non-"data_only" modes, drop the entry entirely.
    if mode != "data_only" {
        state.torrents.lock().unwrap().remove(&id);
    }
    if mode == "list_and_data" || mode == "data_only" {
        delete_data(&record);
    }
    persist(&app);
}

/// Tauri command: add a new torrent and start downloading immediately.
/// Returns the new torrent's record (so the UI can render its row at once), or
/// an error string.
#[tauri::command]
async fn add_torrent(
    app: AppHandle,
    torrent: String,
    output: String,
) -> Result<Record, String> {
    let record = build_record(&torrent, &output, "downloading").map_err(|e| format!("{e:#}"))?;
    let id = record.id.clone();

    {
        let state = app.state::<AppState>();
        let mut map = state.torrents.lock().unwrap();
        if map.contains_key(&id) {
            return Err("This torrent is already in the list.".into());
        }
        map.insert(
            id.clone(),
            Handle {
                coordinator: None,
                record: record.clone(),
            },
        );
    }
    persist(&app);

    spawn_pipeline(app, id, torrent, output);
    Ok(record)
}

/// Tauri command: (re)start a stopped torrent already in the registry.
#[tauri::command]
fn start_torrent(app: AppHandle, state: tauri::State<AppState>, id: String) -> Result<(), String> {
    let (torrent, output) = {
        let map = state.torrents.lock().unwrap();
        let Some(h) = map.get(&id) else {
            return Err("Unknown torrent.".into());
        };
        if h.coordinator.is_some() {
            return Err("Torrent is already running.".into());
        }
        (h.record.torrent_path.clone(), h.record.output.clone())
    };
    set_status(&app, &id, "downloading");
    spawn_pipeline(app.clone(), id, torrent, output);
    Ok(())
}

/// Parse a `.torrent` and assemble its persisted [`Record`].
fn build_record(torrent: &str, output: &str, status: &str) -> anyhow::Result<Record> {
    let meta = Metainfo::from_file(torrent).with_context(|| format!("reading {torrent}"))?;
    let id = meta.info_hash()?.to_string();
    let total_bytes = meta.info.total_length()?;

    let files = meta
        .file_paths(output)?
        .into_iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    // Multi-file torrents nest everything under `output/<name>`; single-file
    // torrents have no such directory.
    let root = if meta.info.files.is_some() {
        Some(
            PathBuf::from(output)
                .join(&meta.info.name)
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };

    Ok(Record {
        id,
        name: meta.info.name.clone(),
        torrent_path: torrent.to_string(),
        output: output.to_string(),
        total_bytes,
        files,
        root,
        status: status.to_string(),
    })
}

/// Spawn the download pipeline for torrent `id` on a background task.
fn spawn_pipeline(app: AppHandle, id: String, torrent: String, output: String) {
    tauri::async_runtime::spawn(async move {
        if let Err(e) = run_pipeline(app.clone(), id.clone(), torrent, output).await {
            log(&app, &id, "error", format!("{e:#}"));
            set_status(&app, &id, "error");
            let _ = app.emit(
                "finished",
                FinishedEvent {
                    id,
                    status: "error".into(),
                },
            );
        }
    });
}

/// The full download pipeline for one torrent, wired to UI events.
async fn run_pipeline(
    app: AppHandle,
    id: String,
    torrent: String,
    output: String,
) -> anyhow::Result<()> {
    // Module 1: parse the .torrent.
    let meta = Metainfo::from_file(&torrent).with_context(|| format!("reading {torrent}"))?;
    let info_hash = meta.info_hash()?;
    let total = meta.info.total_length()?;
    let peer_id = PeerId::generate();
    log(
        &app,
        &id,
        "info",
        format!(
            "{} — {} bytes across {} pieces",
            meta.info.name,
            total,
            meta.info.num_pieces()
        ),
    );

    // Module 2: announce to trackers and gather peers.
    let params = AnnounceParams {
        info_hash,
        peer_id,
        port: 6881,
        uploaded: 0,
        downloaded: 0,
        left: total,
        event: Event::Started,
        compact: true,
        numwant: Some(50),
    };

    let mut seen = HashSet::new();
    let mut peers: Vec<SocketAddr> = Vec::new();
    for url in meta.all_trackers() {
        match tracker::announce(&url, &params).await {
            Ok(resp) => {
                log(
                    &app,
                    &id,
                    "info",
                    format!("tracker {url}: {} peer(s)", resp.peers.len()),
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
            Err(e) => log(&app, &id, "warn", format!("tracker {url}: {e}")),
        }
    }
    anyhow::ensure!(!peers.is_empty(), "no peers returned by any tracker");
    log(
        &app,
        &id,
        "info",
        format!("connecting to {} peer(s)", peers.len()),
    );

    // Module 5: prepare output files.
    let storage = Storage::create(&meta, &output)
        .await
        .context("creating output files")?;

    // Module 4: coordinator + verified-piece channel.
    let coordinator = Arc::new(Coordinator::new(&meta.info, info_hash, peer_id)?);
    // Publish the coordinator so pause/resume/cancel reach this download. If the
    // entry was removed before we got here, abort.
    {
        let state = app.state::<AppState>();
        let mut map = state.torrents.lock().unwrap();
        match map.get_mut(&id) {
            Some(h) => h.coordinator = Some(Arc::clone(&coordinator)),
            None => return Ok(()),
        }
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel::<VerifiedPiece>(64);

    // Disk writer: drain verified pieces to disk.
    let (writer_app, writer_id) = (app.clone(), id.clone());
    let consumer = tauri::async_runtime::spawn(async move {
        let mut storage = storage;
        while let Some(piece) = rx.recv().await {
            storage
                .write_piece(piece.index, &piece.data)
                .await
                .with_context(|| format!("writing piece {}", piece.index))?;
        }
        storage.finish().await.context("flushing files")?;
        log(&writer_app, &writer_id, "info", "all pieces flushed to disk");
        anyhow::Ok(())
    });

    // Stats emitter: push a snapshot to the UI ~twice a second until finished.
    let finished = Arc::new(AtomicBool::new(false));
    let (emit_coord, emit_app, emit_flag, emit_id) =
        (coordinator.clone(), app.clone(), finished.clone(), id.clone());
    let emitter = tauri::async_runtime::spawn(async move {
        loop {
            let snapshot = emit_coord.snapshot().await;
            let complete = snapshot.complete;
            let _ = emit_app.emit(
                "stats",
                TorrentStats {
                    id: emit_id.clone(),
                    snapshot,
                },
            );
            if complete || emit_flag.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });

    // Drive the swarm to completion (or peer exhaustion).
    coordinator.run(peers, tx).await;
    finished.store(true, Ordering::Relaxed);

    consumer.await.context("disk writer panicked")??;
    let _ = emitter.await;

    // Final snapshot + outcome.
    let snapshot = coordinator.snapshot().await;
    let complete = snapshot.complete;
    let cancelled = coordinator.control_state() == ControlState::Cancelled;
    let _ = app.emit(
        "stats",
        TorrentStats {
            id: id.clone(),
            snapshot,
        },
    );
    let status = if complete {
        log(&app, &id, "info", "download complete");
        "complete"
    } else if cancelled {
        log(&app, &id, "warn", "download stopped");
        "stopped"
    } else {
        log(&app, &id, "warn", "peers exhausted before completion");
        "stalled"
    };

    // Clear the live coordinator and record the final status.
    {
        let state = app.state::<AppState>();
        let mut map = state.torrents.lock().unwrap();
        if let Some(h) = map.get_mut(&id) {
            h.coordinator = None;
            h.record.status = status.to_string();
        }
    }
    persist(&app);
    let _ = app.emit(
        "finished",
        FinishedEvent {
            id,
            status: status.to_string(),
        },
    );
    Ok(())
}

/// Load the persisted registry into the app state on startup. Restored entries
/// come back *stopped* (never auto-resumed); completed ones stay complete.
fn restore(app: &AppHandle) {
    let records: Vec<Record> = store_path(app)
        .ok()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();

    let state = app.state::<AppState>();
    let mut map = state.torrents.lock().unwrap();
    for mut record in records {
        if record.status != "complete" {
            record.status = "stopped".into();
        }
        map.insert(
            record.id.clone(),
            Handle {
                coordinator: None,
                record,
            },
        );
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .setup(|app| {
            restore(&app.handle().clone());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add_torrent,
            start_torrent,
            list_torrents,
            pick_torrent,
            pick_folder,
            pause_download,
            resume_download,
            cancel_download,
            remove_torrent
        ])
        .run(tauri::generate_context!())
        .expect("error while running Torrentina");
}
