// Grab Download Manager — MV3 background service worker (HTTP transport).
//
// The Grab desktop app runs a localhost HTTP API (default 127.0.0.1:17654).
// This extension intercepts downloads and POSTs them to that API instead of
// letting Chrome download them. There is no native-messaging host anymore — the
// app must be running (it lives in the system tray and can auto-start on boot).
//
// Config (port + token) is stored in chrome.storage.local and set via the
// options page. Requests carry `Authorization: Bearer <token>`.

const DEFAULT_PORT = 17654;

// Tracks Chrome download ids we've already handed off, to avoid loops.
const handedOff = new Set();

// ---- config -----------------------------------------------------------------

async function getConfig() {
  const { grabPort, grabToken } = await chrome.storage.local.get(["grabPort", "grabToken"]);
  return {
    port: Number(grabPort) || DEFAULT_PORT,
    token: grabToken || "",
  };
}

function baseUrl(port) {
  return `http://127.0.0.1:${port}`;
}

// ---- HTTP client -------------------------------------------------------------

/// POST/GET the app's API. Returns { ok, status, data } or { ok:false, error }.
async function apiFetch(path, { method = "GET", body } = {}) {
  const { port, token } = await getConfig();
  try {
    const resp = await fetch(`${baseUrl(port)}${path}`, {
      method,
      headers: {
        "content-type": "application/json",
        ...(token ? { authorization: `Bearer ${token}` } : {}),
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    let data = null;
    try {
      data = await resp.json();
    } catch {
      /* some endpoints may return empty */
    }
    return { ok: resp.ok, status: resp.status, data };
  } catch (e) {
    // fetch throws (TypeError) when the app isn't running / port closed.
    return { ok: false, error: String(e), appDown: true };
  }
}

/// Health check for the popup's status indicator. No auth needed.
async function pingApp() {
  const { port } = await getConfig();
  try {
    const resp = await fetch(`${baseUrl(port)}/ping`, { method: "GET" });
    if (!resp.ok) return { up: false };
    const data = await resp.json();
    return { up: true, version: data.version };
  } catch {
    return { up: false };
  }
}

/// Send a download to the app. Handles the "app not running" + auth cases with
/// clear user feedback. Returns true if the app ACCEPTED the download.
async function sendDownload(payload) {
  const r = await apiFetch("/downloads", { method: "POST", body: payload });
  if (r.appDown) {
    notify(
      "Grab isn't running",
      "Open the Grab app (it can auto-start on boot). Letting Chrome download this file for now."
    );
    return false;
  }
  if (r.status === 401) {
    notify(
      "Grab not paired",
      "Set the Grab API token in the extension's options, then try again."
    );
    return false;
  }
  if (!r.ok) {
    notify("Grab error", `The Grab app rejected the request (HTTP ${r.status}).`);
    return false;
  }
  notify("Download started", "Grab is handling your download.");
  return true;
}

// ---- Download interception ---------------------------------------------------

// Fires when Chrome has resolved the suggested filename and is about to write
// to disk. This is the BEST place to grab the real name (item.filename is set
// here, unlike onCreated where it's often empty).
chrome.downloads.onDeterminingFilename?.addListener((item, suggest) => {
  handoff(item);
  // Don't call suggest() — we intend to take the download over if the app is up.
});

// Fallback for versions/cases without onDeterminingFilename.
chrome.downloads.onCreated.addListener((item) => {
  if (!/^https?:/i.test(item.finalUrl || item.url || "")) return;
  handoff(item);
});

async function handoff(item) {
  const url = item.finalUrl || item.url;
  if (!url || !/^https?:/i.test(url)) return;
  if (handedOff.has(item.id)) return;
  handedOff.add(item.id);

  const headers = await buildHeaders(url, item.referrer);
  // Prefer Chrome's resolved suggested filename; fall back to the URL path.
  // Chrome's `filename` already reflects Content-Disposition / the real name.
  let filename;
  if (item.filename) filename = baseName(item.filename);
  else filename = filenameFromUrl(url);

  // CRITICAL: only cancel Chrome's own download AFTER the app accepts it. If
  // the app is down, we let Chrome finish normally so the file is never lost.
  const accepted = await sendDownload({ url, filename, headers });
  if (accepted) {
    try {
      await chrome.downloads.cancel(item.id);
      await chrome.downloads.erase({ id: item.id });
    } catch (e) {
      console.warn("Could not cancel Chrome download (Grab already took over):", e);
    }
  }
  // If not accepted: do nothing — Chrome downloads it normally. Fallback intact.
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
  await sendDownload({ url, headers });
});

// ---- Header / cookie forwarding ---------------------------------------------

async function buildHeaders(url, referrer) {
  const headers = {};
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (cookies && cookies.length) {
      headers["Cookie"] = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
    }
  } catch {
    /* cookies may be denied for some URLs; proceed without them */
  }
  if (referrer) headers["Referer"] = referrer;
  headers["User-Agent"] = navigator.userAgent;
  return headers;
}

function baseName(path) {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || undefined;
}

// Derive a filename from a URL path (drops query/fragment). Returns undefined
// if there's nothing usable — the app will then fall back to its own logic.
function filenameFromUrl(url) {
  try {
    const p = new URL(url).pathname;
    const last = p.split("/").filter(Boolean).pop();
    return last ? decodeURIComponent(last) : undefined;
  } catch {
    return undefined;
  }
}

// ---- notifications -----------------------------------------------------------

function notify(title, message) {
  try {
    chrome.notifications.create({
      type: "basic",
      iconUrl: "icons/icon48.png",
      title,
      message,
    });
  } catch {
    console.log(title, "-", message);
  }
}

// ---- messages from popup -----------------------------------------------------

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || message.source !== "grab-popup") return false;
  (async () => {
    switch (message.action) {
      case "ping":
        sendResponse(await pingApp());
        break;
      case "download":
        sendResponse({ ok: await sendDownload(message.payload) });
        break;
      case "apiFetch":
        sendResponse(await apiFetch(message.path, message.opts));
        break;
      default:
        sendResponse({ ok: false, error: "unknown action" });
    }
  })();
  return true; // keep the channel open for the async response
});
