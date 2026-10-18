// Options page: store the Grab app port + token, and test the connection.

const portEl = document.getElementById("port");
const tokenEl = document.getElementById("token");
const statusEl = document.getElementById("status");

const DEFAULT_PORT = 17654;

// Load saved values.
chrome.storage.local.get(["grabPort", "grabToken"]).then((cfg) => {
  portEl.value = cfg.grabPort || DEFAULT_PORT;
  tokenEl.value = cfg.grabToken || "";
});

document.getElementById("save").addEventListener("click", async () => {
  const grabPort = Number(portEl.value) || DEFAULT_PORT;
  const grabToken = tokenEl.value.trim();
  await chrome.storage.local.set({ grabPort, grabToken });
  setStatus("✅ Saved.", true);
});

document.getElementById("test").addEventListener("click", async () => {
  const port = Number(portEl.value) || DEFAULT_PORT;
  const token = tokenEl.value.trim();
  setStatus("Testing…", true);

  // 1. Is the app reachable at all? (ping needs no auth)
  try {
    const ping = await fetch(`http://127.0.0.1:${port}/ping`);
    if (!ping.ok) throw new Error("ping failed");
    const info = await ping.json();
    // 2. Does the token work? (hit an authed endpoint)
    const authed = await fetch(`http://127.0.0.1:${port}/downloads`, {
      headers: { authorization: `Bearer ${token}` },
    });
    if (authed.status === 401) {
      setStatus(`🟡 App reachable (v${info.version}) but token is wrong.`, false);
    } else if (authed.ok) {
      setStatus(`🟢 Connected to Grab v${info.version}. Token OK.`, true);
    } else {
      setStatus(`🟡 App reachable, unexpected status ${authed.status}.`, false);
    }
  } catch {
    setStatus("🔴 Can't reach Grab. Is the app running on this port?", false);
  }
});

function setStatus(text, ok) {
  statusEl.textContent = text;
  statusEl.style.color = ok ? "#2d8a4e" : "#e25555";
}
