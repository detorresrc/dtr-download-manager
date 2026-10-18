//! grab-host: the native messaging host.
//!
//! Chrome launches this binary as a child process and speaks the Native
//! Messaging protocol over stdin/stdout:
//!
//!   [ 4-byte message length, native byte order ][ UTF-8 JSON message ]
//!
//! Messages are capped at 1 MiB by Chrome (for messages *to* the host) and
//! historically limited both directions, so we keep payloads small and stream
//! progress as a series of short messages rather than one large blob.
//!
//! Incoming messages (from the extension) look like:
//!   { "type": "download", "url": "...", "filename": "...",
//!     "headers": { "Cookie": "...", "Referer": "..." } }
//!   { "type": "pause",  "id": "dl-3" }
//!   { "type": "resume", "id": "dl-3" }
//!   { "type": "cancel", "id": "dl-3" }
//!   { "type": "settings", "download_dir": "...", "max_connections_per_file": 8, ... }
//!   { "type": "ping" }
//!
//! Outgoing messages (to the extension):
//!   { "type": "accepted", "id": "dl-3" }
//!   { "type": "progress", "id": "dl-3", "status": "downloading",
//!     "downloaded": 123, "total": 456, "speed": 789 }
//!   { "type": "pong" }
//!   { "type": "error", "message": "..." }

use grab_engine::{DownloadKind, DownloadRequest, Manager, Progress, Settings};
use serde::Deserialize;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;

/// A message received from the browser extension.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Incoming {
    Download {
        url: String,
        #[serde(default)]
        filename: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        save_dir: Option<PathBuf>,
        /// Optional explicit kind ("file" | "hls" | "dash"); guessed if absent.
        #[serde(default)]
        kind: Option<DownloadKind>,
    },
    Pause {
        id: String,
    },
    Resume {
        id: String,
    },
    Cancel {
        id: String,
    },
    /// List all persisted downloads (the queue, including history).
    List,
    /// Remove a download from the persistent queue.
    Remove {
        id: String,
    },
    Settings {
        #[serde(default)]
        download_dir: Option<PathBuf>,
        #[serde(default)]
        max_connections_per_file: Option<usize>,
        #[serde(default)]
        max_concurrent_downloads: Option<usize>,
        /// Double-Option so we can tell "omitted" (None → leave unchanged) from
        /// "null" (Some(None) → set to unlimited) from a value (Some(Some(n))).
        #[serde(default)]
        speed_limit: Option<Option<u64>>,
        #[serde(default)]
        use_categories: Option<bool>,
        #[serde(default)]
        theme: Option<String>,
    },
    Ping,
}

fn main() {
    // Log to a file (never stdout — that's reserved for the protocol).
    init_file_logging();
    tracing::info!("grab-host starting (pid {})", std::process::id());

    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(run()) {
        tracing::error!("fatal: {e:#}");
        let _ = write_message(&serde_json::json!({
            "type": "error",
            "message": e.to_string(),
        }));
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let settings = load_settings();
    tracing::info!("settings: {:?}", settings);
    // Persist the queue so downloads survive host restarts.
    let queue_path = config_dir().join("queue.json");
    let (manager, mut progress_rx) =
        Manager::with_persistence(settings, queue_path).await;
    // Re-enqueue anything left unfinished last time (byte-level resume is
    // handled per-file via .grabpart sidecars).
    let restored = manager.restore().await;
    if !restored.is_empty() {
        tracing::info!("restored {} unfinished download(s)", restored.len());
    }

    // Forward engine progress events to the extension over stdout.
    // stdout isn't Send-friendly to share, so we funnel everything through one
    // channel and do all writing from a single place.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    let progress_tx = out_tx.clone();
    let progress_task = tokio::spawn(async move {
        while let Some(p) = progress_rx.recv().await {
            let _ = progress_tx.send(progress_to_json(&p));
        }
    });

    // Writer task: serialize all outgoing protocol writes.
    let writer = tokio::task::spawn_blocking(move || {
        while let Some(msg) = out_rx.blocking_recv() {
            if let Err(e) = write_message(&msg) {
                tracing::error!("write failed: {e}");
                break;
            }
        }
    });

    // Reader loop: block on stdin in a dedicated thread, hand messages to async.
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || loop {
        match read_message() {
            Ok(Some(buf)) => {
                if in_tx.send(buf).is_err() {
                    break;
                }
            }
            Ok(None) => {
                tracing::info!("stdin closed; browser disconnected");
                break;
            }
            Err(e) => {
                tracing::error!("read error: {e}");
                break;
            }
        }
    });

    while let Some(buf) = in_rx.recv().await {
        let parsed: Result<Incoming, _> = serde_json::from_slice(&buf);
        match parsed {
            Ok(msg) => handle_message(msg, &manager, &out_tx).await,
            Err(e) => {
                tracing::warn!("bad message: {e}");
                let _ = out_tx.send(serde_json::json!({
                    "type": "error",
                    "message": format!("invalid message: {e}"),
                }));
            }
        }
    }

    // Shutdown: stdin closed (browser disconnected). Tear down the forwarding
    // and writer tasks so the process can exit instead of lingering. We abort
    // the progress forwarder (it holds a clone of the outgoing sender and would
    // otherwise keep the writer alive), drop our own sender, and let the writer
    // drain any queued messages before finishing.
    progress_task.abort();
    drop(out_tx);
    let _ = writer.await;
    tracing::info!("grab-host exiting");
    Ok(())
}

async fn handle_message(
    msg: Incoming,
    manager: &Manager,
    out: &mpsc::UnboundedSender<serde_json::Value>,
) {
    match msg {
        Incoming::Download {
            url,
            filename,
            headers,
            save_dir,
            kind,
        } => {
            let req = DownloadRequest {
                url,
                filename,
                headers,
                save_dir,
                kind,
                resolved_target: None,
            };
            let id = manager.enqueue(req).await;
            tracing::info!("enqueued {id}");
            let _ = out.send(serde_json::json!({ "type": "accepted", "id": id }));
        }
        Incoming::Pause { id } => manager.pause(&id).await,
        Incoming::Resume { id } => manager.resume(&id).await,
        Incoming::Cancel { id } => manager.cancel(&id).await,
        Incoming::List => {
            let records = manager.records().await;
            let _ = out.send(serde_json::json!({ "type": "list", "downloads": records }));
        }
        Incoming::Remove { id } => {
            // Stop it if running, then drop it from the persistent queue.
            manager.cancel(&id).await;
            manager.forget(&id).await;
            let _ = out.send(serde_json::json!({ "type": "removed", "id": id }));
        }
        Incoming::Settings {
            download_dir,
            max_connections_per_file,
            max_concurrent_downloads,
            speed_limit,
            use_categories,
            theme,
        } => {
            // Patch only the fields that were provided; categories list and any
            // unspecified field are preserved from the current settings.
            let mut s = manager.settings().await;
            if let Some(d) = download_dir {
                s.download_dir = d;
            }
            if let Some(n) = max_connections_per_file {
                s.max_connections_per_file = n.clamp(1, 32);
            }
            if let Some(n) = max_concurrent_downloads {
                s.max_concurrent_downloads = n.max(1);
            }
            if let Some(limit) = speed_limit {
                s.speed_limit = limit; // Some(None) => unlimited, Some(Some(n)) => n
            }
            if let Some(uc) = use_categories {
                s.use_categories = uc;
            }
            if let Some(t) = theme {
                s.theme = match t.to_ascii_lowercase().as_str() {
                    "light" => grab_engine::Theme::Light,
                    "dark" => grab_engine::Theme::Dark,
                    _ => grab_engine::Theme::System,
                };
            }
            persist_settings(&s);
            manager.update_settings(s).await;
            let _ = out.send(serde_json::json!({ "type": "settings_saved" }));
        }
        Incoming::Ping => {
            let _ = out.send(serde_json::json!({ "type": "pong" }));
        }
    }
}

fn progress_to_json(p: &Progress) -> serde_json::Value {
    serde_json::to_value(p).unwrap_or_else(|_| {
        serde_json::json!({ "type": "error", "message": "progress serialize failed" })
    })
}

// ---- Native messaging framing ------------------------------------------------

/// Read one message: a 4-byte native-endian length prefix, then that many bytes.
/// Returns `Ok(None)` on clean EOF (browser closed the pipe).
fn read_message() -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    let mut stdin = io::stdin().lock();
    match stdin.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    // Native byte order per the spec; little-endian on all target platforms.
    let len = u32::from_ne_bytes(len_buf) as usize;
    // Chrome limits messages to 1 MiB; guard against a bogus huge length.
    if len > 64 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message length too large",
        ));
    }
    let mut buf = vec![0u8; len];
    stdin.read_exact(&mut buf)?;
    Ok(Some(buf))
}

/// Write one message with the 4-byte native-endian length prefix.
fn write_message(value: &serde_json::Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).expect("serializable json");
    let len = bytes.len() as u32;
    let mut stdout = io::stdout().lock();
    stdout.write_all(&len.to_ne_bytes())?;
    stdout.write_all(&bytes)?;
    stdout.flush()
}

// ---- Settings persistence ----------------------------------------------------

fn config_dir() -> PathBuf {
    // Keep config next to the host's own data dir, cross-platform.
    let base = dirs_config_home();
    base.join("grab")
}

/// Minimal cross-platform config-home resolver (avoids an extra crate).
fn dirs_config_home() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata);
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config");
        }
    }
    PathBuf::from(".")
}

fn settings_path() -> PathBuf {
    config_dir().join("settings.json")
}

fn load_settings() -> Settings {
    match std::fs::read(settings_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}

fn persist_settings(s: &Settings) {
    let dir = config_dir();
    if std::fs::create_dir_all(&dir).is_ok() {
        if let Ok(json) = serde_json::to_vec_pretty(s) {
            let _ = std::fs::write(settings_path(), json);
        }
    }
}

fn init_file_logging() {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("grab-host.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
    {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .try_init();
    }
}
