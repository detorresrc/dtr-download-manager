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

## 2. Firefox extension  — PLANNED

The engine + native host already support it; only a second extension manifest
and the Firefox native-host registration path are needed.

- [ ] `extension/manifest.firefox.json` (MV3 with `browser_specific_settings`).
- [ ] `scripts/install-host-firefox-linux.sh` → `~/.mozilla/native-messaging-hosts/`.
- [ ] Windows: register under `HKCU\Software\Mozilla\NativeMessagingHosts\…`.
- [ ] Note Firefox's `allowed_extensions` (by extension ID) vs Chrome's
      `allowed_origins` in the host manifest.

## 3. Speed limiting (enforce `Settings.speed_limit`)  — PLANNED

The setting is plumbed through but not enforced in the download loop.

- [ ] Global token-bucket / rate limiter shared across active segments.
- [ ] Optional per-download limit.
- [ ] Apply in both the segmented and media-segment download paths.

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
