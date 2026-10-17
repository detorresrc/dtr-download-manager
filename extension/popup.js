// Popup logic: send a URL to download, and push settings to the native host
// via the background service worker.

const $ = (id) => document.getElementById(id);
const statusEl = $("status");

function setStatus(text, ok = true) {
  statusEl.textContent = text;
  statusEl.style.color = ok ? "#2d8a4e" : "#c23";
}

function sendToBackground(payload) {
  return chrome.runtime.sendMessage({ source: "grab-popup", payload });
}

$("go").addEventListener("click", async () => {
  const url = $("url").value.trim();
  if (!/^https?:\/\//i.test(url)) {
    setStatus("Enter a valid http(s) URL.", false);
    return;
  }
  const resp = await sendToBackground({ type: "download", url });
  if (resp && resp.ok) {
    setStatus("Sent to Grab.");
    $("url").value = "";
  } else {
    setStatus("Could not reach the Grab app.", false);
  }
});

$("save").addEventListener("click", async () => {
  const payload = { type: "settings" };
  const dir = $("dir").value.trim();
  const conns = parseInt($("conns").value.trim(), 10);
  const concurrent = parseInt($("concurrent").value.trim(), 10);
  if (dir) payload.download_dir = dir;
  if (Number.isFinite(conns)) payload.max_connections_per_file = conns;
  if (Number.isFinite(concurrent)) payload.max_concurrent_downloads = concurrent;

  const resp = await sendToBackground(payload);
  if (resp && resp.ok) {
    setStatus("Settings saved.");
    // Remember locally so the fields repopulate next time.
    chrome.storage.local.set({ grabSettings: { dir, conns, concurrent } });
  } else {
    setStatus("Could not reach the Grab app.", false);
  }
});

// Restore saved settings into the form.
chrome.storage.local.get("grabSettings").then((data) => {
  const s = data.grabSettings;
  if (!s) return;
  if (s.dir) $("dir").value = s.dir;
  if (Number.isFinite(s.conns)) $("conns").value = s.conns;
  if (Number.isFinite(s.concurrent)) $("concurrent").value = s.concurrent;
});

// Show live progress relayed from the host.
chrome.runtime.onMessage.addListener((message) => {
  if (message && message.source === "grab-host") {
    const m = message.payload;
    if (m.status === "downloading" && m.total) {
      const pct = ((m.downloaded / m.total) * 100).toFixed(0);
      setStatus(`Downloading ${m.id}: ${pct}%`);
    } else if (m.status === "completed") {
      setStatus(`Completed ${m.id}.`);
    } else if (m.status === "failed") {
      setStatus(`Failed: ${m.error || "error"}`, false);
    }
  }
});
