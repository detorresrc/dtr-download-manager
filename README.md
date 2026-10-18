# Grab Download Manager

A cross-platform (Windows + Linux) download manager with multi-connection
("segmented") downloading and a Chrome extension — like Internet Download
Manager, built with **Rust + Tauri**.

The browser extension intercepts downloads and hands them to a fast native
engine that splits each file into parallel byte-range segments, with automatic
fallback to a single connection when the server doesn't support ranges.

## Status

| Component | State |
|-----------|-------|
| `engine/` — segmented engine + queue persistence + HLS/DASH | ✅ working, tested (unit + live end-to-end) |
| `host/` — native messaging host (Chrome ⇄ engine) | ✅ working, protocol verified |
| `extension/` — Chrome MV3 extension | ✅ built (syntax-validated; load to test in Chrome) |
| `scripts/` — host installers (Linux + Windows) | ✅ |
| `app/` — Tauri 2 desktop GUI | ✅ code complete (build on a desktop) |
| YouTube / site videos (yt-dlp) | ⏳ planned — see [TODO.md](TODO.md) |
| Firefox extension | ⏳ planned — see [TODO.md](TODO.md) |

> **Planned work** (YouTube/site-video support via yt-dlp, Firefox, speed
> limiting, HLS/DASH resume, and more) is tracked in **[TODO.md](TODO.md)**.

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
  survives app/host restarts. Unfinished downloads are automatically re-queued
  and resumed on startup, writing to the *same* file (no collision copies).
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

## Architecture

```
┌──────────────┐   native messaging    ┌────────────────────┐
│ Chrome       │  (length-prefixed     │ grab-host          │
│ extension    │◄──── JSON / stdio ────►│ (native host)      │
│ (MV3)        │                       │        │           │
└──────────────┘                       │        ▼           │
                                       │   grab-engine      │
┌──────────────┐    Tauri commands     │ (segmented         │
│ Tauri GUI    │◄─────────────────────►│  download engine)  │
│ (app/)       │                       └────────────────────┘
└──────────────┘
```

The `grab-engine` crate is UI-agnostic and shared by both the native host and
the Tauri app.

## Build

Requires a recent stable Rust toolchain.

### Engine + native host (no GUI deps)

```bash
# Engine + host
cargo build --release -p grab-host

# Run the engine's test suite
cargo test -p grab-engine

# Live end-to-end smoke test (downloads a real file and verifies it)
cargo run -p grab-engine --example smoke
```

### Desktop app (Tauri)

The GUI needs the Tauri CLI and platform WebView libraries.

```bash
# One-time: install the Tauri CLI
npm install -g @tauri-apps/cli    # or: cargo install tauri-cli --version '^2'

cd app
npm install        # installs @tauri-apps/cli locally
npm run dev        # run the app in development
npm run build      # produce a bundled installer
```

**Linux build prerequisites** (Debian/Ubuntu example):

```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
  libayatana-appindicator3-dev librsvg2-dev build-essential
```

On Fedora: `webkit2gtk4.1-devel gtk3-devel`. On Windows: the Microsoft
Edge WebView2 runtime (preinstalled on Windows 11) and the MSVC build tools.

> Note: the app was developed in a headless CI sandbox without WebView
> libraries, so the GUI must be built/run on a real desktop. The engine and
> native host are fully tested there; the Tauri layer is a thin wrapper around
> the verified `grab-engine` API.

## Install the browser integration

### 1. Load the Chrome extension

1. Open `chrome://extensions`.
2. Enable **Developer mode** (top right).
3. Click **Load unpacked** and select the `extension/` folder.
4. Copy the extension's **ID** (shown on its card).

### 2. Register the native host

The browser needs to know where the `grab-host` binary lives. Build it first
(`cargo build --release -p grab-host`), then:

**Linux**
```bash
./scripts/install-host-linux.sh <EXTENSION_ID>
```

**Windows (PowerShell)**
```powershell
.\scripts\install-host-windows.ps1 -ExtensionId <EXTENSION_ID>
```

Restart Chrome afterward. Now downloads you start in the browser (or via the
extension popup / right-click **Download with Grab**) are handled by the engine.

## Configuration

Settings live in a JSON file managed by the host:

- Linux: `~/.config/grab/settings.json`
- Windows: `%APPDATA%\grab\settings.json`

You can also change them from the extension popup. Keys:

| Key | Meaning | Default |
|-----|---------|---------|
| `download_dir` | Base folder where files are saved | `~/Downloads` |
| `max_connections_per_file` | Segments per file (clamped 1–32) | `8` |
| `max_concurrent_downloads` | Files downloading at once | `4` |
| `speed_limit` | Global bytes/sec cap (`null` = unlimited) | `null` |
| `use_categories` | Auto-sort files into category subfolders | `true` |
| `categories` | Extension → subfolder rules (first match wins) | Video/Audio/Documents/Programs/Archives/Images |
| `theme` | UI theme: `system` / `light` / `dark` | `system` |

### Download categories

When `use_categories` is on, a file is saved into `download_dir/<subfolder>` based
on its extension — e.g. `movie.mkv` → `Downloads/Video/`, `report.pdf` →
`Downloads/Documents/`. Unknown extensions stay in the base folder. An explicit
per-request save folder always overrides categories. Edit the rules in settings.

## License

MIT
