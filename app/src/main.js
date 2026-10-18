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
  try {
    const id = await invoke("start_download", { url, kind });
    const name = guessName(url);
    createItem(id, name);
    urlInput.value = "";
  } catch (e) {
    alert("Failed to start download: " + e);
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

// Repopulate the list from the persisted queue on startup.
async function restoreList() {
  try {
    const records = await invoke("list_downloads");
    for (const r of records) {
      const name = r.target
        ? r.target.split(/[\\/]/).pop()
        : guessName(r.request.url);
      createItem(r.id, name);
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
  } catch (e) {
    console.warn("Could not load saved downloads:", e);
  }
}

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

listen("progress", (event) => updateItem(event.payload));

// Load any downloads persisted from a previous session, and apply the theme.
restoreList();
invoke("get_settings")
  .then((s) => applyTheme(s.theme || "system"))
  .catch(() => {});

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
