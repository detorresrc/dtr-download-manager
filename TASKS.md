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

### Phase 1 — HTTP API in the app  `[x]`  (done & verified here)
- [x] Add `axum` localhost server (in engine crate, feature `http`, reusable by app) — *verified: live*
- [x] Bearer-token auth (reject all but `/ping` without valid token) — *verified: live 401*
- [x] CORS: `OPTIONS` preflight + allow headers/methods/origin — *verified: live preflight 200*
- [x] `GET /ping` → `{ok, version}` (no auth) — *verified: live*
- [x] `POST /downloads` → `{id}` (url, headers, filename, kind, queue) — *verified: live*
- [x] `GET /downloads` → `[records]` — *verified: live*
- [x] `POST /downloads/:id/pause` | `/resume` | `/cancel` — *verified: live*
- [x] `DELETE /downloads/:id` — *verified: live*
- [x] Wire the server into `grab-app` startup (done in Phase 2)
- [x] Generate + persist a random auth token on first run (done in Phase 2)
- [ ] Port configurable via Settings (small follow-up; fixed 17654 for now)
- Note: core server + auth + CORS fully proven via `examples/http_smoke.rs`.

### Phase 2 — Tray + lifecycle  `[~]`  (code done; visible behavior = your Windows)
- [x] Wire HTTP API into app startup (serves on 127.0.0.1:17654)
- [x] Generate + persist random auth token (`<config>/api_token`); shown in Settings
- [x] App is now the single engine — calls `restore()` on startup
- [x] System tray icon + menu (Open · Pause all · Resume all · Quit)
- [x] Close button → hide to tray (engine + API keep running)
- [x] `tauri-plugin-single-instance` (second launch focuses running one)
- [x] `tauri-plugin-autostart` + Settings toggle (`set_autostart`/`is_autostart_enabled`)
- [x] Start hidden in tray when launched via `--autostarted`
- [x] Raise/focus window on first progress event of a new download (point 4)
- [x] Settings UI: autostart toggle + API pairing (URL + token) display
- [ ] First-run prompt "Start Grab on boot?" — deferred (toggle in Settings for now)
- [ ] "Ask before downloading" setting + review popup — deferred to Phase 3 polish
- Note: Tauri 2 API usage reviewed/confirmed; **needs your Windows machine** to
  verify tray, close-to-tray, autostart, single-instance visually (can't run GUI here).
- [ ] Make API port configurable in Settings (currently fixed 17654)

### Phase 3 — Rework the Chrome extension  `[~]`  (code done; verify in Chrome)
- [x] Replace native messaging with `fetch()` to the HTTP API — *contract verified here*
- [x] `host_permissions` for `http://127.0.0.1/*` + `localhost`; dropped `nativeMessaging`
- [x] Send bearer token; `options.html`/`options.js` to paste/store port + token + "Test connection"
- [x] Only cancel Chrome's own download AFTER the app accepts the handoff — *fallback intact*
- [x] App-down: catch fetch error → notify + let Chrome download normally
- [x] Popup: 🟢/🔴 status via `GET /ping`
- [x] Keep forwarding cookies/headers/referer + suggested filename
- Note: manifest + all JS validated; the POST/ping/401 contract exercised against
  the real server here. **Needs your Chrome to verify the live round-trip.**

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
