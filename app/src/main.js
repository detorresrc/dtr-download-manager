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
  try {
    const id = await invoke("start_download", { url });
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
    </div>`;
  el.querySelector(".name").textContent = name;
  el.querySelector('[data-act="pause"]').onclick = () => invoke("pause", { id });
  el.querySelector('[data-act="resume"]').onclick = () => invoke("resume", { id });
  el.querySelector('[data-act="cancel"]').onclick = () => invoke("cancel", { id });
  list.appendChild(el);
  downloads.set(id, { el, name });
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

// ---- Settings ---------------------------------------------------------------

const dialog = document.getElementById("settings-dialog");
document.getElementById("settings-btn").onclick = async () => {
  const s = await invoke("get_settings");
  document.getElementById("s-dir").value = s.download_dir;
  document.getElementById("s-conns").value = s.max_connections_per_file;
  document.getElementById("s-concurrent").value = s.max_concurrent_downloads;
  dialog.showModal();
};

document.getElementById("s-browse").onclick = async () => {
  const picked = await open({ directory: true, multiple: false });
  if (picked) document.getElementById("s-dir").value = picked;
};

dialog.addEventListener("close", async () => {
  if (dialog.returnValue !== "save") return;
  const current = await invoke("get_settings");
  const settings = {
    ...current,
    download_dir: document.getElementById("s-dir").value,
    max_connections_per_file: parseInt(document.getElementById("s-conns").value, 10) || 8,
    max_concurrent_downloads: parseInt(document.getElementById("s-concurrent").value, 10) || 4,
  };
  try {
    await invoke("save_settings", { settings });
  } catch (e) {
    alert("Failed to save settings: " + e);
  }
});

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
