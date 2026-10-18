# Grab — Roadmap / TODO

Tracked future work. Items are ordered roughly by priority.

## 1. YouTube / site-video downloading (via yt-dlp)  — PLANNED

Download videos from YouTube and other sites that don't expose a plain
`.m3u8` / `.mpd` URL.

**Why it's not just HLS/DASH:** YouTube has no static manifest URL; stream URLs
are buried in page JSON, protected by a JavaScript-based signature / `n`-param
cipher that changes often, split into separate video-only + audio-only tracks
(need muxing), and guarded by throttling / bot checks / PO tokens. Reimplementing
all of this natively is high-maintenance and breaks constantly.

**Approach (recommended): shell out to `yt-dlp` (+ `ffmpeg` for muxing).**
`yt-dlp` already solves signature descrambling, format selection, and muxing, and
is actively maintained. Keep the native Rust engine for direct files and open
HLS/DASH; delegate site videos to `yt-dlp`.

**Implementation sketch:**
- [ ] Add `DownloadKind::SiteVideo` (and/or auto-detect by hostname: youtube.com,
      youtu.be, vimeo.com, etc.).
- [ ] New `engine/src/ytdlp.rs` module that:
  - [ ] Locates a bundled/`PATH` `yt-dlp` and `ffmpeg` binary (configurable path
        in `Settings`); surface a clear error if missing.
  - [ ] Invokes `yt-dlp` with `--newline --progress-template …` and parses its
        progress output into the existing `Progress` events (map % / bytes / speed).
  - [ ] Honors the configured download dir (`-P`/`-o`), supports format selection
        (best video+audio, or a chosen height), and forwards cookies/headers.
  - [ ] Supports pause/cancel via the existing `Handle` (terminate the child
        process on cancel; pause = stop + resume later with yt-dlp's `--continue`).
  - [ ] Routes through the persistent queue like every other download.
- [ ] `download()` routes `SiteVideo` to the new module.
- [ ] Host (`Incoming::Download`) and Tauri `start_download` accept the new kind.
- [ ] UI: add "Site video (yt-dlp)" to the type dropdown; show a format picker.
- [ ] Packaging: document/bundle `yt-dlp` + `ffmpeg` per OS; version-check on start.
- [ ] Tests: verify binary discovery + progress parsing against recorded
      `yt-dlp` output; live test against a Creative-Commons / self-owned video.

**Compliance:** position for legitimate use only (own uploads, Creative-Commons,
content the user has rights to, archival). Do NOT add DRM-circumvention features.
Downloading copyrighted YouTube content generally violates YouTube's ToS — make
that clear in the UI/README.

## 2. Firefox extension  — PLANNED (now trivial)

Since the extension talks to the app over plain HTTP (no native messaging), a
Firefox port is mostly a manifest variant pointing at the same API.

- [ ] `extension/manifest.firefox.json` (MV3 + `browser_specific_settings`).
- [ ] Confirm Firefox allows `fetch()` to `http://127.0.0.1` with the declared
      `host_permissions` (same as Chrome).
- [ ] Reuse the same `background.js`/`options.js`/`popup.js` unchanged.
- [ ] No host registration needed — just pair the port + token in options.

## 3. Speed limiting (enforce `Settings.speed_limit`)  — ✅ DONE

Implemented as a shared token-bucket `RateLimiter` (`engine/src/ratelimit.rs`).

- [x] Global token-bucket / rate limiter shared across active segments.
- [x] Applied in both the segmented and media-segment download paths.
- [x] Adjustable at runtime via `Manager::update_settings`.
- [ ] Optional *per-download* limit (currently global only).

## Tier 1 — IDM-style configurability  — ✅ DONE

- [x] **Download categories** — `Settings.categories` + `category_dir_for()`
      auto-sort files into subfolders (Video/Audio/Documents/Programs/Archives/
      Images) by extension; toggle with `use_categories`.
- [x] **Speed limiting** — see #3 above.
- [x] **Connection count** — `effective_connections()` clamps to 1–32; exposed
      in host/app settings.
- [x] **Theme** — `Settings.theme` (system/light/dark); applied by the app UI.

## Tier 2 — Scheduler + multiple queues  — ✅ DONE

- [x] **Multiple named queues** — `Settings.queues` (`Queue { id, name,
      max_concurrent, running, schedule }`); each download has an optional
      `queue` id (defaults to "main").
- [x] **Scheduler** — the `Manager` runs a background tick loop that promotes
      `Queued` downloads to running only when the queue is running,
      schedule-active, and under its `max_concurrent`.
- [x] **Time-window schedules** — `engine/src/schedule.rs`: enabled flag,
      start/stop minute (wraps past midnight), active weekdays.
- [x] **Queue controls** — `pause_all` / `start_all` / `pause_queue` /
      `start_queue`, exposed via host messages and Tauri commands + UI buttons.
- [x] Fixed a concurrency bug: `unique_path` now reserves names ATOMICALLY
      (O_EXCL), so simultaneous downloads with the same name no longer clobber
      each other.
- [ ] Per-queue time-of-day in LOCAL time (currently UTC in the headless engine;
      the app can pass local time explicitly).

## 4. Resume for HLS/DASH media downloads  — PLANNED

Regular file downloads resume at the byte level (`.grabpart`). Media downloads
currently restart from the first segment.

- [ ] Checkpoint completed segment indices to a sidecar.
- [ ] On resume, skip already-downloaded segments and append the rest.

## 5. Smaller improvements

- [ ] Re-apply `max_concurrent_downloads` at runtime (currently the semaphore is
      fixed at `Manager` creation; changing the setting later has no effect).
- [ ] Checksum verification (user-supplied hash, or `Content-MD5` / digest headers).
- [ ] Disk-space pre-check before starting a large download.
- [ ] Clipboard URL monitoring in the app.
- [ ] Proxy support.
