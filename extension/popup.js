// Popup: show Grab app connection status and let the user paste a URL to queue.
// All real work goes through the background service worker (which talks HTTP to
// the app) so the popup stays simple and reuses one config/transport.

const dot = document.getElementById("dot");
const statusText = document.getElementById("status-text");
const msg = document.getElementById("msg");
const urlEl = document.getElementById("url");

function bg(message) {
  return chrome.runtime.sendMessage({ source: "grab-popup", ...message });
}

function setMsg(text, ok = true) {
  msg.textContent = text;
  msg.style.color = ok ? "#2d8a4e" : "#e25555";
}

// Connection status via the background's /ping.
async function refreshStatus() {
  const res = await bg({ action: "ping" });
  if (res && res.up) {
    dot.className = "dot up";
    statusText.textContent = `Connected to Grab${res.version ? " v" + res.version : ""}`;
  } else {
    dot.className = "dot down";
    statusText.textContent = "Grab app not running";
  }
}

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
  } else {
    setMsg("Couldn't reach the Grab app. Check it's running / paired.", false);
  }
}

document.getElementById("open-options").addEventListener("click", () => {
  chrome.runtime.openOptionsPage();
});

refreshStatus();
