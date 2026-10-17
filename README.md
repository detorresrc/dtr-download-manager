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
| `engine/` — segmented download engine (Rust library) | ✅ working, tested (unit + live end-to-end) |
| `host/` — native messaging host (Chrome ⇄ engine) | ✅ working, protocol verified |
| `extension/` — Chrome MV3 extension | ✅ built (syntax-validated; load to test in Chrome) |
| `scripts/` — host installers (Linux + Windows) | ✅ |
| `app/` — Tauri 2 desktop GUI | 🚧 in progress |
| Firefox extension | ⏳ planned |

## Features

- **Multi-connection downloads** — splits a file into N parallel segments.
- **Automatic single-connection fallback** — detects when a server doesn't
  support byte ranges (or lies about it mid-download) and switches seamlessly.
- **Configurable** — max connections per file, max concurrent downloads,
  download folder (global + per-request override).
- **Pause / resume / cancel**, including crash-safe resume (validated against
  the server's size + ETag so you never stitch together mismatched bytes).
- **Cookie & header forwarding** from the browser, so authenticated/behind-login
  downloads work.
- **Correct filenames** — parses `Content-Disposition` (incl. RFC 5987),
  URL paths, sanitizes illegal characters, and avoids collisions.
- **Saves files exactly as served** — requests `Accept-Encoding: identity` so
  byte ranges and on-disk bytes always agree.

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

```bash
# Engine + host
cargo build --release -p grab-host

# Run the engine's test suite
cargo test -p grab-engine

# Live end-to-end smoke test (downloads a real file and verifies it)
cargo run -p grab-engine --example smoke
```

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
| `download_dir` | Where files are saved | `~/Downloads` |
| `max_connections_per_file` | Segments per file | `8` |
| `max_concurrent_downloads` | Files downloading at once | `4` |
| `speed_limit` | Global bytes/sec cap (`null` = unlimited) | `null` |

## License

MIT
