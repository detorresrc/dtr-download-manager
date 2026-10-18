# Grab — Task Tracker

Living checklist for the project. `[x]` = done & merged, `[~]` = in progress,
`[ ]` = not started. "Verified" notes say where a task was actually tested
(unit test / live test here, or needs-your-Windows-machine).

---

## ✅ Already built (merged to `main`)

### Core engine (`engine/`)
- [x] Multi-connection segmented downloader — *verified: live byte-for-byte checksum*
- [x] Range-support probe + automatic single-connection fallback — *verified: live*
- [x] Runtime fallback when a server lies (200 instead of 206) — *verified: live*
- [x] Pause / resume / cancel (cooperative `Handle`) — *verified: live pause→resume→complete, checksum match*
- [x] Crash-safe resume via `.grabpart` sidecar (ETag/size validated) — *verified: unit + live*
- [x] Correct filenames (Content-Disposition RFC 5987, URL, sanitize) — *verified: unit*
- [x] Atomic filename reservation (O_EXCL) — no clobber on concurrent same-name — *verified: live*
- [x] Persistent download queue (`queue.json`) across restarts — *verified: live crash+resume*
- [x] Queue file `.bak` recovery + orphaned `.grabpart` reconciliation — *verified: unit*
- [x] HLS (`.m3u8`) capture — master→best variant, segment concat — *verified: live (64 seg, valid MPEG-TS)*
- [x] DASH (`.mpd`) capture — SegmentTemplate, highest bandwidth — *verified: unit*
- [x] Download categories (auto-folder by extension) — *verified: live + unit*
- [x] Global speed limiting (shared token-bucket) — *verified: live ~throttle*
- [x] Download speed (smoothed EMA) + ETA — *verified: live*
- [x] Persist speed/ETA so cross-process rows show stats — *verified: live via host*
- [x] Multiple named queues + scheduler (concurrency + time windows) — *verified: live concurrency cap*
- [x] Fixed scheduler double-spawn race — *verified: unit + live*

### Current integration (to be replaced in the rewrite)
- [x] Native messaging host `grab-host` (JSON over stdio) — *WILL BE REMOVED (Phase 4)*
- [x] Chrome MV3 extension (intercept, cookies, context menu, popup) — *WILL BE REWRITTEN (Phase 3)*
- [x] Host installer scripts (Linux + Windows) — *WILL BE REMOVED (Phase 4)*
- [x] Cross-process control channel via `pending_action` in queue.json — *interim; superseded by single engine*

### Tauri desktop app (`app/`)
- [x] Tauri 2 app wrapping the engine (commands + progress events)
- [x] UI: download list, progress bars, settings dialog
- [x] `withGlobalTauri` fix (blank-window bug)
- [x] Live dashboard polling the shared queue file
- [x] Theme (system/light/dark); speed/ETA display; queue picker; start/pause-all

### Build / repo
- [x] Cargo workspace; app detached as its own workspace (tauri build fix)
- [x] Valid multi-resolution Windows `.ico` (MSI bundling fix)
- [x] README, TODO roadmap, static UI preview

---

## 🎯 Architecture rewrite: single-engine app + HTTP API

Goal: drop `grab-host.exe`; the Tauri app becomes the single engine, always
running (tray + autostart), exposing a localhost HTTP API the extension calls.

### Decisions (locked)
- Transport: **localhost HTTP** `127.0.0.1:17654` (configurable), **bearer token**, **CORS**
- App-not-running: extension notifies + falls back to Chrome's own download
- New download: **auto-accept** default + optional "Ask before downloading"
- Autostart: **opt-in** via first-run prompt
- Pairing: **manual token paste** (one-time)

---

### Phase 1 — HTTP API in the app  `[ ]`  (fully testable here w/ curl)
- [ ] Add `axum` localhost server bound to `127.0.0.1:<port>` in `grab-app`
- [ ] Generate + persist a random auth token on first run
- [ ] Bearer-token auth middleware (reject all but `/ping` without valid token)
- [ ] CORS: handle `OPTIONS` preflight + allow headers/methods/origin
- [ ] `GET /ping` → `{ok, version}` (no auth)
- [ ] `POST /downloads` → `{id}` (url, headers, filename, kind, queue)
- [ ] `GET /downloads` → `[records]`
- [ ] `POST /downloads/:id/pause` | `/resume` | `/cancel`
- [ ] `DELETE /downloads/:id`
- [ ] Port configurable via Settings
- [ ] Verify with curl: happy path, 401 without token, CORS preflight, "app down" (connection refused)

### Phase 2 — Tray + lifecycle  `[ ]`  (code here; visible behavior = your Windows)
- [ ] System tray icon + menu (Open · Pause all · Resume all · Quit)
- [ ] Close button → hide to tray (engine keeps running)
- [ ] `tauri-plugin-single-instance` (second launch focuses running one)
- [ ] `tauri-plugin-autostart` + Settings toggle
- [ ] Start hidden in tray when launched via autostart
- [ ] Raise/focus window on new `POST /downloads` (point 4)
- [ ] First-run prompt: "Start Grab on boot?"
- [ ] "Ask before downloading" setting + review popup

### Phase 3 — Rework the Chrome extension  `[ ]`  (code here; verify in Chrome)
- [ ] Replace native messaging with `fetch()` to the HTTP API
- [ ] `host_permissions: ["http://127.0.0.1:17654/*"]`
- [ ] Send bearer token; options page to paste/store it
- [ ] Only cancel Chrome's own download AFTER the app accepts the handoff
- [ ] App-down: catch fetch error → notify + let Chrome download normally
- [ ] Popup: 🟢/🔴 status via `GET /ping`
- [ ] Keep forwarding cookies/headers/referer + suggested filename

### Phase 4 — Remove the old host  `[ ]`  (here)
- [ ] Delete `host/` crate and workspace member
- [ ] Delete native-messaging manifest template
- [ ] Delete host installer scripts (Linux + Windows)
- [ ] Remove interim `pending_action` control-channel code (no longer needed)
- [ ] Update README (new setup: run app, pair token, no host install)
- [ ] Update TODO/TASKS

---

## 📋 Backlog (from TODO.md, unchanged)
- [ ] YouTube / site videos via yt-dlp (+ffmpeg)
- [ ] Firefox extension (now trivial: just points at the same HTTP API)
- [ ] Per-download speed limit (global done)
- [ ] HLS/DASH segment-level resume
- [ ] Local-time scheduling (currently UTC in engine)
- [ ] Checksum verification, disk-space pre-check, clipboard monitor, proxy
- [ ] Optional: WebSocket/SSE progress stream for the extension popup
