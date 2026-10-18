# Grab Download Manager

A cross-platform (Windows + Linux) download manager with multi-connection
("segmented") downloading and a Chrome extension — like Internet Download
Manager, built with **Rust + Tauri**.

## Architecture

The **desktop app is the single engine**. It runs a tiny localhost HTTP API and
lives in the system tray (and can auto-start on boot), so it's always ready. The
Chrome extension intercepts downloads and `fetch()`es them to that API — no
native-messaging host, no separate background process.

```
Chrome extension ──HTTP + bearer token──► Grab app (tray, always running)
  intercept + forward   POST /downloads      • the download engine
  cookies / headers                          • localhost HTTP API (127.0.0.1:17654)
                                             • window = dashboard
```

### HTTP API

The app serves these on `http://127.0.0.1:17654`. Every endpoint except `/ping`
requires `Authorization: Bearer <token>` (the token from Settings → pairing).

| Method & path | Purpose |
|---|---|
| `GET /ping` | Health check (no auth) → `{ ok, version }` |
| `POST /downloads` | Queue a download → `{ id }`. Body: `url`, `filename?`, `headers?`, `save_dir?`, `kind?`, `queue?` |
| `GET /downloads` | List all download records |
| `POST /downloads/:id/pause` | Pause a download |
| `POST /downloads/:id/resume` | Resume a download |
| `POST /downloads/:id/cancel` | Cancel a download |
| `DELETE /downloads/:id` | Remove a download from the queue |

CORS is enabled, so any extension (Chrome, Firefox, …) can call it with the token.

## Status

| Component | State |
|-----------|-------|
| `engine/` — segmented engine + queue + HLS/DASH + HTTP API | ✅ working, tested (unit + live) |
| `app/` — Tauri 2 desktop app (single engine, tray, autostart, HTTP API) | ✅ code complete (build on a desktop) |
| `extension/` — Chrome MV3 extension (HTTP transport) | ✅ built (validated; load to test in Chrome) |
| YouTube / site videos (yt-dlp) | ⏳ planned — see [TODO.md](TODO.md) |
| Firefox extension | ⏳ planned (now trivial — same HTTP API) |

> Progress + remaining work is tracked in **[TASKS.md](TASKS.md)**; the longer
> backlog is in **[TODO.md](TODO.md)**.

## Features

- **Multi-connection downloads** — splits a file into N parallel segments.
- **Automatic single-connection fallback** — detects when a server doesn't
  support byte ranges (or lies about it mid-download) and switches seamlessly.
- **Configurable (IDM-style)** — max connections per file (1–32), max concurrent
  downloads, download folder (global + per-request override), **global speed
  limit**, **download categories** (auto-sort by file type into subfolders), and
  **theme** (system / light / dark).
- **Pause / resume / cancel**, including crash-safe resume (validated against
  the server's size + ETag so you never stitch together mismatched bytes).
- **Persistent download queue** — the list of downloads is saved to disk, so it
  survives app restarts. Unfinished downloads are automatically re-queued and
  resumed on startup, writing to the *same* file (no collision copies).
- **Multiple queues + scheduler (IDM-style)** — organize downloads into named
  queues, each with its own concurrency limit and optional time-window schedule
  (start/stop times, active weekdays). Pause/start individual queues or all at
  once. A background scheduler starts queued downloads only when their queue is
  running, schedule-active, and under its concurrency cap.
- **Video / stream capture (HLS & DASH)** — point it at an `.m3u8` or `.mpd`
  and it parses the manifest, picks the highest-bandwidth rendition, downloads
  all media segments concurrently, and concatenates them into one file.
- **Cookie & header forwarding** from the browser, so authenticated/behind-login
  downloads work.
- **Correct filenames** — parses `Content-Disposition` (incl. RFC 5987),
  URL paths, sanitizes illegal characters, and avoids collisions.
- **Saves files exactly as served** — requests `Accept-Encoding: identity` so
  byte ranges and on-disk bytes always agree.

### Streaming capture details

- **HLS**: master (multivariant) playlists are resolved to the highest-bandwidth
  variant; media playlists with `#EXTINF` segments and `#EXT-X-MAP` init segments
  are supported. Output is the raw MPEG-TS or fMP4 segment concatenation (a valid,
  playable stream).
- **DASH**: `SegmentTemplate` with `$Number$` / `$RepresentationID$`
  substitution, `initialization`, `startNumber`, `SegmentTimeline`, and
  `BaseURL`; highest-`bandwidth` `Representation` is chosen.
- **Known limitations** (reported as errors, not silent corruption): HLS
  AES-128 / SAMPLE-AES decryption is not performed; DASH live/dynamic manifests
  are treated as their current snapshot; segments aren't remuxed into MP4.

### Queue persistence details

The queue lives at `~/.config/grab/queue.json` (Linux) / `%APPDATA%\grab\queue.json`
(Windows). Each record stores the request, resolved target path, and last known
status. Byte-level resume continues to use the per-file `.grabpart` sidecar next
to each output file.

## Build

Requires a recent stable Rust toolchain.

### Engine (no GUI deps — fully testable in CI)

```bash
cargo test -p grab-engine                 # unit tests
cargo run -p grab-engine --example smoke  # live end-to-end download + verify
cargo run -p grab-engine --example http_smoke  # live HTTP API test
```

### Desktop app (Tauri)

The app needs the Tauri CLI and platform WebView libraries.

```bash
# One-time: install the Tauri CLI
npm install -g @tauri-apps/cli    # or: cargo install tauri-cli --version '^2'

cd app
npm install
npm run dev        # run in development
npm run build      # produce a bundled installer
```

**Linux build prerequisites** (Debian/Ubuntu):

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev build-essential
```

On Fedora: `webkit2gtk4.1-devel gtk3-devel`. On Windows: the Microsoft Edge
WebView2 runtime (preinstalled on Windows 11) + the MSVC build tools.

> Note: the app is developed in a headless CI sandbox without WebView libraries,
> so the GUI must be built/run on a real desktop. The `grab-engine` crate
> (including the HTTP API) is fully tested there; the Tauri layer is a thin
> wrapper around the verified engine.

## Setup (run the app, then pair the extension)

No native-messaging host to install anymore — the app runs the HTTP API itself.

### 1. Run the Grab app

Build and launch it (`cd app && npm run build`, then run the produced binary).
It sits in the **system tray**; closing the window hides it there so it keeps
running to catch browser downloads. Enable **Start on login** in Settings so
it's always ready. On first run it generates an API token.

### 2. Load the Chrome extension

1. `chrome://extensions` → enable **Developer mode** → **Load unpacked** →
   select the `extension/` folder.

### 3. Pair the extension with the app

1. In the Grab app: **Settings → Browser extension pairing** — note the **API
   URL port** and **token**.
2. In Chrome: the Grab extension → **Options** (or the popup's "Pairing /
   options" link) → enter the port and token → **Test connection** (should go
   🟢).

Now downloads you start in the browser (or via the extension popup / right-click
**Download with Grab**) are handed to the app. If the app isn't running, the
extension notifies you and lets Chrome download the file normally so nothing is
lost.

## Configuration

Settings are managed by the app (Settings dialog) and stored in a JSON file:

- Linux: `~/.config/grab/settings.json`
- Windows: `%APPDATA%\grab\settings.json`

The app also keeps two more files in the same folder: `queue.json` (the download
list) and `api_token` (the bearer token the extension pairs with). Settings keys:

| Key | Meaning | Default |
|-----|---------|---------|
| `download_dir` | Base folder where files are saved | `~/Downloads` |
| `max_connections_per_file` | Segments per file (clamped 1–32) | `8` |
| `max_concurrent_downloads` | Files downloading at once | `4` |
| `speed_limit` | Global bytes/sec cap (`null` = unlimited) | `null` |
| `use_categories` | Auto-sort files into category subfolders | `true` |
| `categories` | Extension → subfolder rules (first match wins) | Video/Audio/Documents/Programs/Archives/Images |
| `theme` | UI theme: `system` / `light` / `dark` | `system` |
| `queues` | Named queues: `id`, `name`, `max_concurrent`, `running`, `schedule` | one "Main" queue |

### Queues & scheduling

Each download belongs to a queue (default: **Main**). A queue has its own
`max_concurrent` limit and an optional `schedule` (`enabled`, `start_minute`,
`stop_minute`, active `days` Mon=0..Sun=6; windows may wrap past midnight). The
engine's scheduler starts a queued download only when its queue is `running`,
the schedule is active, and the queue is under its concurrency cap. Use the
app's **Start all / Pause all** controls or per-queue start/pause.

### Download categories

When `use_categories` is on, a file is saved into `download_dir/<subfolder>` based
on its extension — e.g. `movie.mkv` → `Downloads/Video/`, `report.pdf` →
`Downloads/Documents/`. Unknown extensions stay in the base folder. An explicit
per-request save folder always overrides categories. Edit the rules in settings.

## License

MIT
