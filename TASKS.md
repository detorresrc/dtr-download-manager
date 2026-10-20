# Grab — Task Tracker

Living checklist. `[x]` done & merged, `[~]` partial, `[ ]` not started.
"Verified" notes say where it was tested: *unit*/*live* (proven in CI here) or
*needs-desktop* (code done, must be confirmed on a real Linux/Windows GUI).

**Last updated:** after PR #20. Engine: 51 unit tests passing, clippy clean.
**Repo:** https://github.com/detorresrc/dtr-download-manager

---

## 🟢 Current state (TL;DR for a new session)

Grab is a **working cross-platform download manager** with a **single-engine
architecture**:

- The **Tauri desktop app** (`app/`) IS the download engine. It runs a localhost
  HTTP API, lives in the system tray, and can auto-start on boot.
- The **Chrome extension** (`extension/`) intercepts downloads and `fetch()`es
  them to that API (bearer token + CORS). No native-messaging host anymore.
- The **engine crate** (`engine/`) holds all download logic + the HTTP API
  (feature `http`), and is fully unit/live tested in CI.

Confirmed working end-to-end on Linux by the user (incl. a 17.8 GB file-host
download that survived connection drops via retry/resume).

### Layout
```
engine/   Rust lib: segmented downloads, queue, HLS/DASH, HTTP API (grep http_api.rs)
app/      Tauri 2 app = the single engine + tray + autostart + GUI dashboard
extension/ Chrome MV3 extension (HTTP transport): background.js, popup, options
scripts/  (host installers were deleted in Phase 4 — dir may be empty)
TASKS.md / TODO.md / README.md
```

### Build / test (works in CI, no GUI needed for the engine)
```
export CARGO_HOME=... (if the default registry is read-only)
cargo test -p grab-engine                        # 51 unit tests
cargo run -p grab-engine --example smoke         # live download + checksum
cargo run -p grab-engine --example http_smoke    # live HTTP API test
cargo clippy -p grab-engine
# App (needs a desktop w/ WebView libs — see README):  cd app && npm run build
```
> The GUI/extension cannot run in the headless CI sandbox. Engine + HTTP API are
> fully testable there; tray/autostart/extension round-trip are verified by the
> user on their machine.

### Workflow conventions used so far
- Each change = its own branch → PR → merge to `main` (20 PRs merged).
- `git`/`gh` are pre-configured; open PRs with `gh api repos/.../pulls` (not `gh pr`).

---

## ✅ Done (all merged to `main`)

### Engine core (unit + live verified)
- [x] Multi-connection segmented downloads + single-connection fallback (+ runtime 200-vs-206 fallback)
- [x] Pause / resume / cancel; crash-safe resume via `.grabpart` sidecar
- [x] **Retry + resume on dropped connections** — single-stream (5 attempts) AND per-segment (10 attempts), backoff, resumes from exact offset  *(#17, #19)*
- [x] Correct filename resolution — Content-Disposition (RFC 5987) via probe, hint-with-extension, URL path; rejects opaque proxy tokens  *(#17, #20)*
- [x] Atomic filename reservation (no clobber on concurrent same-name)
- [x] Persistent queue (`queue.json`) + `.bak` recovery + orphaned-`.grabpart` reconciliation
- [x] HLS (.m3u8) + DASH (.mpd) capture (best variant, segment concat)
- [x] Download categories (auto-folder by extension)
- [x] Global speed limiting (token bucket); speed + ETA (smoothed)
- [x] Multiple named queues + scheduler (per-queue concurrency + time windows); fixed double-spawn race  *(#10)*

### HTTP API (`engine/src/http_api.rs`, feature `http`) — live verified
- [x] `GET /ping` (no auth) · `POST/GET /downloads` · `POST /downloads/:id/{pause,resume,cancel}` · `DELETE /downloads/:id`
- [x] Bearer-token auth (401 without) + CORS (OPTIONS preflight)

### App (`app/`) — code done; user-confirmed working on Linux
- [x] Single engine: starts HTTP API on launch, generates/persists `api_token`, calls `restore()`
- [x] System tray (Open / Pause all / Resume all / Quit); close → hide to tray
- [x] Autostart plugin + Settings toggle; `--autostarted` starts hidden
- [x] Single-instance (2nd launch focuses running window)
- [x] Raise/focus window on new download
- [x] GUI dashboard: list, progress, speed/ETA, categories, theme, queue picker, start/pause-all, API pairing panel
- [x] Control commands act on the engine directly (removed dead `pending_action` host-era code)  *(#18)*
- [x] GUI relabels id-placeholder rows (`dl-2`) with the real filename once known  *(#20)*

### Extension (`extension/`) — HTTP transport; user-confirmed
- [x] `fetch()` the API with bearer token (no native messaging)
- [x] `host_permissions` for 127.0.0.1/localhost; options page to pair port+token + "Test connection"
- [x] App-down → notify + let Chrome download normally (only cancels Chrome's dl after app accepts)
- [x] Popup: 🟢/🔴 status + **live download list** with progress/speed/ETA + pause/resume/cancel/remove  *(#16)*
- [x] Forwards cookies / Referer / User-Agent + suggested filename

### Build/infra fixes
- [x] Workspace detach for tauri build (#3), valid Windows .ico (#4), withGlobalTauri blank-window (#6), Linux WebKitGTK deps documented in README

---

## 📋 Backlog / not started (pick up here in the new session)

Ordered by suggested priority:

1. [ ] **Firefox extension** — now trivial: same HTTP API, mostly a manifest
       variant (`browser_specific_settings`), reuse background/popup/options.
       Highest value-for-effort.
2. [ ] **Podcast / RSS-feed downloading** — point Grab at a podcast RSS feed →
       enqueue episode MP3s. (Legal audio; discussed as the Spotify alternative.)
3. [ ] **yt-dlp / site-video downloading** — shell out to `yt-dlp` (+ `ffmpeg`)
       for YouTube/SoundCloud/Bandcamp etc. Full design in TODO.md. Compliance:
       legitimate content only; no DRM circumvention.
4. [x] Configurable API port in Settings — persisted to `<config>/api_port`,
       server binds it on launch, Settings has a port field (restart to apply).
       *(engine custom-port bind verified live)*
5. [ ] First-run "Start Grab on boot?" prompt (toggle exists in Settings).
6. [ ] "Ask before downloading" review popup (auto-accept is default).
7. [ ] Per-download speed limit (global done).
8. [ ] HLS/DASH segment-level resume (regular files already byte-resume).
9. [ ] Local-time scheduling (engine uses UTC now).
10. [ ] Checksum verification, disk-space pre-check, clipboard monitor, proxy.
11. [ ] Optional: WebSocket/SSE progress stream (replace the 1s polling).

### Explicitly NOT doing
- ❌ Spotify (or any DRM-protected service) track downloading — Widevine DRM +
  copyright + ToS; circumvention is illegal. Offer legal audio sources instead
  (Bandcamp/Free Music Archive/podcast RSS/yt-dlp for permitted content).

---

## ⚠️ Notes for the next session
- The engine's `pending_action` field/methods are retained but UNUSED in the
  single-engine world (kept as potentially-useful; harmless, serde-default).
- Scheduling time is UTC in the engine; the app could pass local time if precise
  local schedules matter (item #9).
- Some file-hosts use expiring/tokenized URLs — retry/resume covers genuine
  mid-stream drops, but a truly dead URL can't be resumed by any downloader.
