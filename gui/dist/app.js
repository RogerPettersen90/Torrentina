// Torrentina GUI frontend. Uses the global Tauri API (withGlobalTauri = true),
// so there's no bundler/npm step: just invoke commands and listen for events.
//
// The app manages many torrents at once. Each torrent is a row keyed by its
// info-hash id; `stats`/`log`/`finished` events carry that id and are routed to
// the matching row.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (sel, root = document) => root.querySelector(sel);

// id -> { el, name, lastWireBytes, lastStamp }
const rows = new Map();
// The torrent id the remove modal is currently acting on.
let removeTarget = null;

function fmtBytes(n) {
  if (n < 1024) return `${n} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toFixed(v < 10 ? 2 : 1)} ${units[i]}`;
}

// Human-readable duration from a number of seconds (two coarsest units).
function fmtDuration(secs) {
  if (!isFinite(secs) || secs <= 0) return "—";
  secs = Math.round(secs);
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (d > 0) return `${d}d ${h}h`;
  if (h > 0) return `${h}h ${m}m`;
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

// User-facing label for an internal status key (the engine says "complete";
// the badge reads "finished").
function statusLabel(status) {
  return status === "complete" ? "finished" : status;
}

function badgeClass(status) {
  switch (status) {
    case "downloading": return "active";
    case "paused": return "paused";
    case "complete": return "done";
    case "stalled":
    case "error": return "error";
    default: return "";
  }
}

function updateEmptyNote() {
  $("#empty").style.display = rows.size === 0 ? "block" : "none";
}

// Reflect a status into a row's badge and button visibility/state.
function applyStatus(entry, status) {
  const el = entry.el;
  const badge = $(".badge", el);
  badge.textContent = statusLabel(status);
  badge.className = "badge" + (badgeClass(status) ? " " + badgeClass(status) : "");

  const start = $(".start", el);
  const pause = $(".pause", el);
  const running = status === "downloading" || status === "paused";
  // Start is offered only for re-runnable stopped states, never when complete.
  const startable = status === "stopped" || status === "stalled" || status === "error";

  start.classList.toggle("hidden", !startable);
  pause.classList.toggle("hidden", !running);
  start.disabled = !startable;
  pause.disabled = !running;
  pause.textContent = status === "paused" ? "Resume" : "Pause";
}

function createRow(record) {
  const tpl = $("#row-tpl").content.firstElementChild.cloneNode(true);
  $(".name", tpl).textContent = record.name;
  $(".size", tpl).textContent = `0 B / ${fmtBytes(record.total_bytes)}`;

  const entry = { el: tpl, name: record.name, lastWireBytes: 0, lastStamp: 0, smoothedRate: 0 };
  rows.set(record.id, entry);

  $(".start", tpl).addEventListener("click", () => withErr(() => invoke("start_torrent", { id: record.id })));
  $(".pause", tpl).addEventListener("click", () => togglePause(record.id));
  $(".remove", tpl).addEventListener("click", () => openRemoveModal(record.id, record.name));

  $("#torrents").appendChild(tpl);
  applyStatus(entry, record.status);
  updateEmptyNote();
  return entry;
}

function render(s) {
  const entry = rows.get(s.id);
  if (!entry) return;
  const el = entry.el;

  const pct = s.total_bytes > 0 ? (s.bytes_done / s.total_bytes) * 100 : 0;
  $(".fill", el).style.width = `${pct.toFixed(1)}%`;
  $(".percent", el).textContent = `${pct.toFixed(1)}%`;
  $(".pieces", el).textContent = `${s.pieces_done} / ${s.total_pieces} pieces`;
  $(".peers", el).textContent = `${s.connected_peers} peer${s.connected_peers === 1 ? "" : "s"}`;
  $(".size", el).textContent = `${fmtBytes(s.bytes_done)} / ${fmtBytes(s.total_bytes)}`;

  // Rate = delta of wire bytes / delta of time, tracked per row. A smoothed
  // (EMA) copy drives a steadier ETA.
  const now = performance.now();
  if (entry.lastStamp > 0) {
    const dt = (now - entry.lastStamp) / 1000;
    const db = s.wire_bytes - entry.lastWireBytes;
    if (dt > 0) {
      const raw = Math.max(0, db / dt);
      $(".rate", el).textContent = `${fmtBytes(raw)}/s`;
      entry.smoothedRate = entry.smoothedRate > 0 ? 0.3 * raw + 0.7 * entry.smoothedRate : raw;
    }
  }
  entry.lastWireBytes = s.wire_bytes;
  entry.lastStamp = now;

  // ETA = remaining content bytes / smoothed download rate.
  const remaining = Math.max(0, s.total_bytes - s.bytes_done);
  let eta = "—";
  if (s.complete) eta = "done";
  else if (!s.paused && entry.smoothedRate > 1 && remaining > 0) {
    eta = `~${fmtDuration(remaining / entry.smoothedRate)}`;
  }
  $(".eta", el).textContent = eta;

  // Drive the badge straight from the snapshot so completion is reflected even
  // if the `finished` event is delayed, reordered, or missed. Completion is
  // monotonic in the engine, so a later snapshot won't revert it.
  if (s.complete) applyStatus(entry, "complete");
  else applyStatus(entry, s.paused ? "paused" : "downloading");

  renderPeers(el, s.peers);
}

function renderPeers(el, peers) {
  const tbody = $(".peer-rows", el);
  if (!peers || peers.length === 0) {
    tbody.innerHTML = '<tr class="empty"><td colspan="4">No peers connected</td></tr>';
    return;
  }
  tbody.innerHTML = peers
    .map((p) => {
      const dot = `<span class="dot ${p.choked ? "choked" : ""}">●</span>`;
      const state = `${dot} ${p.choked ? "choked" : "unchoked"}`;
      return `<tr><td>${p.addr}</td><td>${state}</td><td>${p.have_pieces}</td><td>${fmtBytes(p.downloaded_bytes)}</td></tr>`;
    })
    .join("");
}

function addLog(id, level, message) {
  const log = $("#log");
  const entry = rows.get(id);
  const prefix = entry ? `${entry.name}: ` : "";
  const time = new Date().toLocaleTimeString();
  const line = document.createElement("div");
  line.className = level;
  line.textContent = `${time}  ${prefix}${message}`;
  log.appendChild(line);
  log.scrollTop = log.scrollHeight;
}

async function withErr(fn) {
  try {
    await fn();
  } catch (e) {
    addLog(null, "error", String(e));
  }
}

async function togglePause(id) {
  const entry = rows.get(id);
  if (!entry) return;
  const isPaused = $(".badge", entry.el).textContent === "paused";
  await withErr(() => invoke(isPaused ? "resume_download" : "pause_download", { id }));
}

async function browse() {
  await withErr(async () => {
    const path = await invoke("pick_torrent");
    if (path) $("#torrent").value = path;
  });
}

async function browseOutput() {
  await withErr(async () => {
    const path = await invoke("pick_folder");
    if (path) $("#output").value = path;
  });
}

async function add() {
  const torrent = $("#torrent").value.trim();
  const output = $("#output").value.trim() || ".";
  if (!torrent) {
    addLog(null, "error", "Please provide a path to a .torrent file.");
    return;
  }
  await withErr(async () => {
    const record = await invoke("add_torrent", { torrent, output });
    createRow(record);
    addLog(record.id, "info", "added");
    $("#torrent").value = "";
  });
}

// ----- Remove modal -----

function openRemoveModal(id, name) {
  removeTarget = id;
  $("#modal-name").textContent = name;
  $("#modal").classList.remove("hidden");
}

function closeRemoveModal() {
  removeTarget = null;
  $("#modal").classList.add("hidden");
}

async function doRemove(mode) {
  const id = removeTarget;
  closeRemoveModal();
  if (!id) return;
  await withErr(() => invoke("remove_torrent", { id, mode }));
  if (mode !== "data_only") {
    const entry = rows.get(id);
    if (entry) entry.el.remove();
    rows.delete(id);
    updateEmptyNote();
  } else {
    const entry = rows.get(id);
    if (entry) applyStatus(entry, "stopped");
  }
}

// ----- Event wiring -----

listen("stats", (e) => render(e.payload));
listen("log", (e) => addLog(e.payload.id, e.payload.level, e.payload.message));
listen("finished", (e) => {
  const { id, status } = e.payload; // "complete" | "stopped" | "stalled" | "error"
  const entry = rows.get(id);
  if (!entry) return;
  applyStatus(entry, status);
  entry.smoothedRate = 0;
  $(".rate", entry.el).textContent = "0 B/s";
  $(".eta", entry.el).textContent = status === "complete" ? "done" : "—";
});

window.addEventListener("DOMContentLoaded", async () => {
  $("#add").addEventListener("click", add);
  $("#browse").addEventListener("click", browse);
  $("#browse-output").addEventListener("click", browseOutput);

  $("#rm-list").addEventListener("click", () => doRemove("list"));
  $("#rm-both").addEventListener("click", () => doRemove("list_and_data"));
  $("#rm-data").addEventListener("click", () => doRemove("data_only"));
  $("#rm-cancel").addEventListener("click", closeRemoveModal);
  $("#modal").addEventListener("click", (e) => {
    if (e.target === $("#modal")) closeRemoveModal();
  });

  // Restore the persisted list.
  await withErr(async () => {
    const records = await invoke("list_torrents");
    for (const r of records) createRow(r);
    updateEmptyNote();
  });
});
