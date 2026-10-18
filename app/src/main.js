// Grab desktop frontend. Talks to the Rust backend via Tauri's invoke/event
// Tauri 2 exposes these on window.__TAURI__ only when `app.withGlobalTauri` is
// true in tauri.conf.json. Guard it so that, if the global is somehow missing,
// we show a clear on-screen error instead of a silently blank window (the whole
// script would otherwise die on the first undefined access).
const TAURI = window.__TAURI__;
if (!TAURI || !TAURI.core) {
  document.addEventListener("DOMContentLoaded", () => {
    document.body.innerHTML =
      '<div style="font-family:system-ui;padding:24px;color:#e25555">' +
      "<h2>Grab failed to initialize</h2>" +
      "<p>The Tauri API (<code>window.__TAURI__</code>) is not available. " +
      "This usually means <code>app.withGlobalTauri</code> is not enabled in " +
      "<code>tauri.conf.json</code>, or the app is being opened as a plain file " +
      "instead of run via <code>tauri dev</code> / the built app.</p></div>";
  });
  throw new Error("window.__TAURI__ unavailable (withGlobalTauri not enabled?)");
}
const { invoke } = TAURI.core;
const { listen } = TAURI.event;
const { open } = (TAURI.dialog || {});

const downloads = new Map(); // id -> { el, name }

const urlInput = document.getElementById("url-input");
const addBtn = document.getElementById("add-btn");
const list = document.getElementById("downloads");
const empty = document.getElementById("empty");

// ---- Adding downloads -------------------------------------------------------

async function addDownload() {
  const url = urlInput.value.trim();
  if (!/^https?:\/\//i.test(url)) {
    flash(urlInput);
    return;
  }
  const kind = document.getElementById("kind-select").value || null;
  const queue = document.getElementById("queue-select").value || null;
  try {
    const id = await invoke("start_download", { url, kind, queue });
    const name = guessName(url);
    createItem(id, name);
    urlInput.value = "";
  } catch (e) {
    alert("Failed to start download: " + e);
  }
}

// Global queue controls.
document.getElementById("pause-all-btn").onclick = () => invoke("pause_all");
document.getElementById("start-all-btn").onclick = () => invoke("start_all");

// Populate the queue picker from settings.
async function loadQueues() {
  try {
    const s = await invoke("get_settings");
    const sel = document.getElementById("queue-select");
    sel.innerHTML = "";
    for (const q of s.queues || [{ id: "main", name: "Main" }]) {
      const opt = document.createElement("option");
      opt.value = q.id;
      opt.textContent = q.name + (q.running === false ? " (paused)" : "");
      sel.appendChild(opt);
    }
  } catch (e) {
    console.warn("Could not load queues:", e);
  }
}

addBtn.addEventListener("click", addDownload);
urlInput.addEventListener("keydown", (e) => {
  if (e.key === "Enter") addDownload();
});

// ---- Rendering --------------------------------------------------------------

function createItem(id, name) {
  empty.style.display = "none";
  const el = document.createElement("div");
  el.className = "item";
  el.innerHTML = `
    <div class="top">
      <span class="name"></span>
      <span class="status">queued</span>
    </div>
    <div class="bar"><span></span></div>
    <div class="meta"><span class="size">—</span><span class="speed"></span></div>
    <div class="actions">
      <button class="ghost" data-act="pause">Pause</button>
      <button class="ghost" data-act="resume">Resume</button>
      <button class="ghost" data-act="cancel">Cancel</button>
      <button class="ghost" data-act="remove">Remove</button>
    </div>`;
  el.querySelector(".name").textContent = name;
  el.querySelector('[data-act="pause"]').onclick = () => invoke("pause", { id });
  el.querySelector('[data-act="resume"]').onclick = () => invoke("resume", { id });
  el.querySelector('[data-act="cancel"]').onclick = () => invoke("cancel", { id });
  el.querySelector('[data-act="remove"]').onclick = async () => {
    await invoke("remove", { id });
    el.remove();
    downloads.delete(id);
    // Briefly suppress re-adding this row in case a queue-file poll is already
    // in flight with the pre-removal snapshot.
    recentlyRemoved.set(id, Date.now());
    if (downloads.size === 0) empty.style.display = "";
  };
  list.appendChild(el);
  downloads.set(id, { el, name });
}

// Update a row's displayed filename (and remembered name). Used to replace an
// id placeholder like "dl-2" once the real filename is known. Only relabels
// when the new name is a real filename and differs from what's shown, so we
// never overwrite a good name with a guess.
function setName(id, name) {
  const entry = downloads.get(id);
  if (!entry || !name) return;
  const looksReal = name !== id && /\.[A-Za-z0-9]{1,8}$/.test(name);
  if (looksReal && entry.name !== name) {
    entry.name = name;
    entry.el.querySelector(".name").textContent = name;
  }
}

// Refresh the list from the SHARED queue file on disk. This surfaces downloads
// started by the browser (handled by the native host, a separate process that
// writes the same queue.json). Called on startup and then polled every second,
// so host-driven and pending downloads appear and update live in the window.
//
// Live progress events (from downloads started IN the app) still arrive via the
// "progress" listener; for those the event is fresher, so we don't overwrite a
// just-updated item with a slightly stale file read.
let lastEventAt = new Map(); // id -> timestamp of last live progress event
const recentlyRemoved = new Map(); // id -> timestamp; suppresses re-adding after Remove

async function refreshFromQueue() {
  let records;
  try {
    records = await invoke("read_queue_file");
  } catch (e) {
    return; // file may not exist yet; try again next tick
  }
  const seen = new Set();
  for (const r of records) {
    seen.add(r.id);
    // Don't resurrect a row the user just removed (host clears it shortly).
    if (Date.now() - (recentlyRemoved.get(r.id) || 0) < 5000) continue;
    // Prefer the resolved on-disk filename; fall back to a URL guess.
    const name = r.target ? r.target.split(/[\\/]/).pop() : guessName(r.request && r.request.url);
    if (!downloads.has(r.id)) {
      createItem(r.id, name);
    } else {
      // Relabel if the real filename is now known (the row may have been
      // created from a live progress event before `target` existed, using the
      // id as a placeholder — e.g. "dl-2").
      setName(r.id, name);
    }
    // Skip if a live event updated this item in the last 2s (fresher than disk).
    if (Date.now() - (lastEventAt.get(r.id) || 0) < 2000) continue;
    updateItem({
      id: r.id,
      status: r.status,
      downloaded: r.downloaded || 0,
      total: r.total,
      // speed/eta are persisted in the queue file by whichever process is
      // running the download (e.g. the browser's native host), so browser-
      // driven rows show live stats too.
      speed: r.speed || 0,
      eta_secs: r.eta_secs ?? null,
      connections: 0,
      error: r.error,
    });
  }
}

// Alias kept for the startup call site.
const restoreList = refreshFromQueue;

function updateItem(p) {
  let entry = downloads.get(p.id);
  if (!entry) {
    // A download we didn't create in this window (e.g. from the browser).
    createItem(p.id, p.id);
    entry = downloads.get(p.id);
  }
  const { el } = entry;
  const statusEl = el.querySelector(".status");
  statusEl.textContent = p.status + (p.connections ? ` · ${p.connections} conn` : "");
  statusEl.className = "status " + p.status;

  const barFill = el.querySelector(".bar > span");
  if (p.total) {
    const pct = Math.min(100, (p.downloaded / p.total) * 100);
    barFill.style.width = pct.toFixed(1) + "%";
    el.querySelector(".size").textContent =
      `${fmt(p.downloaded)} / ${fmt(p.total)} (${pct.toFixed(0)}%)`;
  } else {
    el.querySelector(".size").textContent = fmt(p.downloaded);
    if (p.status === "completed") barFill.style.width = "100%";
  }
  // Speed + ETA, shown only while actively downloading.
  if (p.status === "downloading" && p.speed) {
    const parts = [`${fmt(p.speed)}/s`];
    if (p.eta_secs != null) parts.push(`${fmtEta(p.eta_secs)} left`);
    el.querySelector(".speed").textContent = parts.join(" · ");
  } else {
    el.querySelector(".speed").textContent = "";
  }

  if (p.status === "failed" && p.error) {
    el.querySelector(".size").textContent = "Error: " + p.error;
  }
}

listen("progress", (event) => {
  const p = event.payload;
  lastEventAt.set(p.id, Date.now()); // mark as freshly updated by a live event
  updateItem(p);
});

// Show the shared queue on startup, apply the theme, populate the queue picker.
refreshFromQueue();
loadQueues();
invoke("get_settings")
  .then((s) => applyTheme(s.theme || "system"))
  .catch(() => {});

// Poll the shared queue file so downloads started from the browser (handled by
// the separate native-host process) appear and update live in this window.
setInterval(refreshFromQueue, 1000);

// ---- Settings ---------------------------------------------------------------

const dialog = document.getElementById("settings-dialog");
document.getElementById("settings-btn").onclick = async () => {
  const s = await invoke("get_settings");
  document.getElementById("s-dir").value = s.download_dir;
  document.getElementById("s-conns").value = s.max_connections_per_file;
  document.getElementById("s-concurrent").value = s.max_concurrent_downloads;
  // speed_limit is bytes/sec in the engine; show it as KB/s (0 = unlimited).
  document.getElementById("s-speed").value = s.speed_limit ? Math.round(s.speed_limit / 1024) : 0;
  document.getElementById("s-categories").checked = s.use_categories !== false;
  document.getElementById("s-theme").value = s.theme || "system";

  // Autostart state + API pairing info (independent of engine settings).
  try {
    document.getElementById("s-autostart").checked = await invoke("is_autostart_enabled");
  } catch {}
  try {
    const api = await invoke("get_api_info");
    document.getElementById("s-api-url").value = api.base_url;
    document.getElementById("s-api-token").value = api.token;
  } catch {}

  dialog.showModal();
};

document.getElementById("s-browse").onclick = async () => {
  const picked = await open({ directory: true, multiple: false });
  if (picked) document.getElementById("s-dir").value = picked;
};

dialog.addEventListener("close", async () => {
  if (dialog.returnValue !== "save") return;
  const current = await invoke("get_settings");
  const speedKb = parseInt(document.getElementById("s-speed").value, 10) || 0;
  const theme = document.getElementById("s-theme").value;
  const settings = {
    ...current,
    download_dir: document.getElementById("s-dir").value,
    max_connections_per_file: Math.min(
      32,
      Math.max(1, parseInt(document.getElementById("s-conns").value, 10) || 8)
    ),
    max_concurrent_downloads: parseInt(document.getElementById("s-concurrent").value, 10) || 4,
    // Convert KB/s back to bytes/sec; 0 means unlimited (null).
    speed_limit: speedKb > 0 ? speedKb * 1024 : null,
    use_categories: document.getElementById("s-categories").checked,
    theme,
  };
  try {
    await invoke("save_settings", { settings });
    applyTheme(theme);
    // Apply the autostart toggle (separate from engine settings).
    await invoke("set_autostart", {
      enabled: document.getElementById("s-autostart").checked,
    });
  } catch (e) {
    alert("Failed to save settings: " + e);
  }
});

// Apply the chosen theme by forcing a color-scheme on the root element.
// "system" clears the override so the OS preference (via CSS) takes over.
function applyTheme(theme) {
  const root = document.documentElement;
  if (theme === "light" || theme === "dark") {
    root.style.colorScheme = theme;
    root.dataset.theme = theme;
  } else {
    root.style.colorScheme = "";
    delete root.dataset.theme;
  }
}

// ---- Helpers ----------------------------------------------------------------

function fmt(n) {
  if (n == null) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${u[i]}`;
}

// Human-readable "time remaining": 45s, 3m 20s, 1h 2m.
function fmtEta(secs) {
  if (secs == null) return "";
  secs = Math.max(0, Math.round(secs));
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  if (m < 60) return s ? `${m}m ${s}s` : `${m}m`;
  const h = Math.floor(m / 60);
  const mm = m % 60;
  return mm ? `${h}h ${mm}m` : `${h}h`;
}

function guessName(url) {
  try {
    const p = new URL(url).pathname.split("/").pop();
    return decodeURIComponent(p || "download");
  } catch {
    return "download";
  }
}

function flash(el) {
  el.style.outline = "2px solid #e25555";
  setTimeout(() => (el.style.outline = ""), 600);
}
