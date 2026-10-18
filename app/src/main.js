// Grab desktop frontend. Talks to the Rust backend via Tauri's invoke/event
// APIs (exposed on window.__TAURI__ when withGlobalTauri is on, but we import
// from the module to be explicit).
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const { open } = window.__TAURI__.dialog;

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
    if (downloads.size === 0) empty.style.display = "";
  };
  list.appendChild(el);
  downloads.set(id, { el, name });
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
    const name = r.target ? r.target.split(/[\\/]/).pop() : guessName(r.request.url);
    if (!downloads.has(r.id)) {
      createItem(r.id, name);
    }
    // Skip if a live event updated this item in the last 2s (fresher than disk).
    if (Date.now() - (lastEventAt.get(r.id) || 0) < 2000) continue;
    updateItem({
      id: r.id,
      status: r.status,
      downloaded: r.downloaded || 0,
      total: r.total,
      speed: 0,
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
  el.querySelector(".speed").textContent =
    p.status === "downloading" && p.speed ? `${fmt(p.speed)}/s` : "";

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
