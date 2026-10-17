// Grab Download Manager — MV3 background service worker.
//
// Responsibilities:
//   1. Intercept downloads Chrome is about to perform and hand them to the
//      native host instead (so the desktop app does the heavy lifting).
//   2. Add a right-click "Download with Grab" context menu for links/media.
//   3. Forward cookies + a few headers so authenticated downloads work.
//   4. Relay progress/errors from the host to notifications and the popup.
//
// The native host is registered under this name (see the host manifest).
const HOST_NAME = "com.grab.download_manager";

// A single long-lived connection to the native host. Reconnect on demand.
let port = null;
// Tracks downloads we've already handed off, so onDeterminingFilename doesn't
// loop when we cancel Chrome's own download.
const handedOff = new Set();

function connectHost() {
  if (port) return port;
  try {
    port = chrome.runtime.connectNative(HOST_NAME);
    port.onMessage.addListener(onHostMessage);
    port.onDisconnect.addListener(() => {
      const err = chrome.runtime.lastError;
      console.warn("Native host disconnected:", err && err.message);
      port = null;
    });
  } catch (e) {
    console.error("Failed to connect to native host:", e);
    port = null;
  }
  return port;
}

function sendToHost(message) {
  const p = connectHost();
  if (!p) {
    notify("Grab app not found", "Install and run the Grab desktop app, then try again.");
    return false;
  }
  try {
    p.postMessage(message);
    return true;
  } catch (e) {
    console.error("postMessage failed:", e);
    port = null;
    return false;
  }
}

// ---- Download interception --------------------------------------------------

// Fires when Chrome has resolved a filename and is about to write to disk.
// We cancel Chrome's download and send the URL to the native host instead.
chrome.downloads.onDeterminingFilename?.addListener((item, suggest) => {
  handoff(item);
  // Let Chrome proceed with its own download only if handoff failed.
  // We return nothing (no suggest()) and cancel below.
});

// Fallback for browsers/versions without onDeterminingFilename.
chrome.downloads.onCreated.addListener((item) => {
  // Only handle http(s) downloads; skip blobs, data URIs, extensions.
  if (!/^https?:/i.test(item.finalUrl || item.url || "")) return;
  handoff(item);
});

async function handoff(item) {
  const url = item.finalUrl || item.url;
  if (!url || !/^https?:/i.test(url)) return;
  if (handedOff.has(item.id)) return;
  handedOff.add(item.id);

  // Cancel and erase Chrome's own download of this item.
  try {
    await chrome.downloads.cancel(item.id);
    await chrome.downloads.erase({ id: item.id });
  } catch (e) {
    // If we couldn't cancel, bail rather than double-download.
    console.warn("Could not cancel Chrome download:", e);
  }

  const headers = await buildHeaders(url, item.referrer);
  const filename = item.filename ? baseName(item.filename) : undefined;
  sendToHost({ type: "download", url, filename, headers });
}

// ---- Context menu -----------------------------------------------------------

chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({
    id: "grab-download-link",
    title: "Download with Grab",
    contexts: ["link", "image", "video", "audio"],
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId !== "grab-download-link") return;
  const url = info.linkUrl || info.srcUrl;
  if (!url || !/^https?:/i.test(url)) return;
  const headers = await buildHeaders(url, tab && tab.url);
  sendToHost({ type: "download", url, headers });
});

// ---- Header / cookie forwarding ---------------------------------------------

async function buildHeaders(url, referrer) {
  const headers = {};
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (cookies && cookies.length) {
      headers["Cookie"] = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    }
  } catch (e) {
    // cookies permission may be denied for some URLs; proceed without them.
  }
  if (referrer) headers["Referer"] = referrer;
  // A realistic UA avoids servers that reject the default.
  headers["User-Agent"] = navigator.userAgent;
  return headers;
}

function baseName(path) {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || undefined;
}

// ---- Host -> extension messages ---------------------------------------------

function onHostMessage(msg) {
  // Relay to any open popup.
  chrome.runtime.sendMessage({ source: "grab-host", payload: msg }).catch(() => {});

  if (msg.type === "accepted") {
    notify("Download started", `Grab is downloading (${msg.id}).`);
  } else if (msg.status === "completed") {
    notify("Download complete", `Finished ${formatBytes(msg.downloaded)}.`);
  } else if (msg.status === "failed") {
    notify("Download failed", msg.error || "Unknown error");
  } else if (msg.type === "error") {
    notify("Grab error", msg.message || "Unknown error");
  }
}

function notify(title, message) {
  try {
    chrome.notifications.create({
      type: "basic",
      iconUrl: "icons/icon48.png",
      title,
      message,
    });
  } catch (e) {
    console.log(title, "-", message);
  }
}

function formatBytes(n) {
  if (!n && n !== 0) return "";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(i === 0 ? 0 : 1)} ${units[i]}`;
}

// ---- Popup command relay ----------------------------------------------------

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (message && message.source === "grab-popup") {
    const ok = sendToHost(message.payload);
    sendResponse({ ok });
  }
  return true;
});
