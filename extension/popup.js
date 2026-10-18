// Popup: Grab app connection status, a quick-add URL box, and a LIVE list of
// downloads (polled from the app's HTTP API via the background worker). All
// HTTP goes through the background so the popup reuses one config/transport.

const dot = document.getElementById("dot");
const statusText = document.getElementById("status-text");
const msg = document.getElementById("msg");
const urlEl = document.getElementById("url");
const listEl = document.getElementById("list");
const emptyEl = document.getElementById("empty");

function bg(message) {
  return chrome.runtime.sendMessage({ source: "grab-popup", ...message });
}

function setMsg(text, ok = true) {
  msg.textContent = text;
  msg.style.color = ok ? "#2d8a4e" : "#e25555";
}

// ---- connection status + live list -----------------------------------------

async function refresh() {
  const res = await bg({ action: "ping" });
  const up = !!(res && res.up);
  dot.className = "dot " + (up ? "up" : "down");
  statusText.textContent = up
    ? `Connected to Grab${res.version ? " v" + res.version : ""}`
    : "Grab app not running";

  if (!up) {
    // App down: can't list. Show the hint, clear the list.
    listEl.innerHTML = "";
    emptyEl.textContent = "Open the Grab app to see downloads.";
    emptyEl.style.display = "";
    return;
  }
  // GET /downloads via the background (adds the bearer token).
  const r = await bg({ action: "apiFetch", path: "/downloads", opts: { method: "GET" } });
  if (!r || !r.ok || !Array.isArray(r.data)) {
    emptyEl.textContent = "Couldn't load downloads (check pairing/token).";
    emptyEl.style.display = "";
    return;
  }
  renderList(r.data);
}

function renderList(records) {
  // Newest first.
  records = [...records].sort((a, b) => (b.created_at || 0) - (a.created_at || 0));
  if (records.length === 0) {
    listEl.innerHTML = "";
    emptyEl.textContent = "No downloads yet.";
    emptyEl.style.display = "";
    return;
  }
  emptyEl.style.display = "none";

  // Rebuild the list (simple + robust for a small popup list).
  listEl.innerHTML = "";
  for (const d of records) {
    listEl.appendChild(renderItem(d));
  }
}

function renderItem(d) {
  const el = document.createElement("div");
  el.className = "item";

  const name = d.target ? baseName(d.target) : guessName(d.request && d.request.url);
  const pct =
    d.total && d.total > 0 ? Math.min(100, (d.downloaded / d.total) * 100) : d.status === "completed" ? 100 : 0;

  const metaLeft = d.total
    ? `${fmt(d.downloaded)} / ${fmt(d.total)} (${pct.toFixed(0)}%)`
    : fmt(d.downloaded || 0);
  const metaRight =
    d.status === "downloading" && d.speed
      ? `${fmt(d.speed)}/s${d.eta_secs != null ? " · " + fmtEta(d.eta_secs) + " left" : ""}`
      : "";

  el.innerHTML = `
    <div class="top">
      <span class="name"></span>
      <span class="st ${d.status}">${d.status}</span>
    </div>
    <div class="bar"><span style="width:${pct}%"></span></div>
    <div class="meta"><span></span><span></span></div>
    <div class="acts"></div>`;
  el.querySelector(".name").textContent = name;
  const metas = el.querySelectorAll(".meta span");
  metas[0].textContent = d.status === "failed" && d.error ? "Error: " + d.error : metaLeft;
  metas[1].textContent = metaRight;

  // Buttons depend on state.
  const acts = el.querySelector(".acts");
  const active = ["downloading", "queued", "probing"].includes(d.status);
  if (active) acts.appendChild(btn("Pause", () => control(d.id, "pause")));
  if (d.status === "paused" || d.status === "failed")
    acts.appendChild(btn("Resume", () => control(d.id, "resume")));
  if (active || d.status === "paused") acts.appendChild(btn("Cancel", () => control(d.id, "cancel")));
  acts.appendChild(btn("Remove", () => removeItem(d.id)));
  return el;
}

function btn(label, onClick) {
  const b = document.createElement("button");
  b.className = "ghost";
  b.textContent = label;
  b.onclick = onClick;
  return b;
}

async function control(id, action) {
  await bg({ action: "apiFetch", path: `/downloads/${id}/${action}`, opts: { method: "POST" } });
  refresh();
}

async function removeItem(id) {
  await bg({ action: "apiFetch", path: `/downloads/${id}`, opts: { method: "DELETE" } });
  refresh();
}

// ---- quick add ---------------------------------------------------------------

document.getElementById("go").addEventListener("click", sendUrl);
urlEl.addEventListener("keydown", (e) => {
  if (e.key === "Enter") sendUrl();
});

async function sendUrl() {
  const url = urlEl.value.trim();
  if (!/^https?:\/\//i.test(url)) {
    setMsg("Enter a valid http(s) URL.", false);
    return;
  }
  const res = await bg({ action: "download", payload: { url } });
  if (res && res.ok) {
    setMsg("Sent to Grab.");
    urlEl.value = "";
    refresh();
  } else {
    setMsg("Couldn't reach the Grab app. Check it's running / paired.", false);
  }
}

document.getElementById("open-options").addEventListener("click", () => {
  chrome.runtime.openOptionsPage();
});

// ---- helpers -----------------------------------------------------------------

function fmt(n) {
  if (n == null) return "—";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${u[i]}`;
}

function fmtEta(secs) {
  if (secs == null) return "";
  secs = Math.max(0, Math.round(secs));
  if (secs < 60) return `${secs}s`;
  const m = Math.floor(secs / 60);
  const s = secs % 60;
  if (m < 60) return s ? `${m}m ${s}s` : `${m}m`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

function baseName(p) {
  const parts = String(p).split(/[\\/]/);
  return parts[parts.length - 1] || "download";
}

function guessName(url) {
  try {
    return decodeURIComponent(new URL(url).pathname.split("/").pop() || "download");
  } catch {
    return "download";
  }
}

// Poll while the popup is open (popups unload on close, so this auto-stops).
refresh();
const timer = setInterval(refresh, 1000);
window.addEventListener("unload", () => clearInterval(timer));
