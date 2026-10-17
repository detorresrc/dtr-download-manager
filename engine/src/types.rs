//! Shared data types for the download engine.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// What kind of resource a request points at. Chooses the download strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadKind {
    /// A regular file served over HTTP (the default path: probe + segment).
    #[default]
    File,
    /// An HLS playlist (`.m3u8`): a list of media segments to fetch and join.
    Hls,
    /// A DASH manifest (`.mpd`): adaptation sets of segments to fetch and join.
    Dash,
}

impl DownloadKind {
    /// Guess the kind from a URL, using the path extension and common markers.
    /// Callers may override this explicitly on the request.
    pub fn guess_from_url(url: &str) -> DownloadKind {
        let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
        if path.ends_with(".m3u8") {
            DownloadKind::Hls
        } else if path.ends_with(".mpd") {
            DownloadKind::Dash
        } else {
            DownloadKind::File
        }
    }
}

/// A request to download a URL, typically originating from the browser extension.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
    /// Optional suggested filename (e.g. from Content-Disposition seen by the browser).
    #[serde(default)]
    pub filename: Option<String>,
    /// Headers to forward (cookies, referer, user-agent) so authenticated downloads work.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Override the default download directory for this request.
    #[serde(default)]
    pub save_dir: Option<PathBuf>,
    /// How to interpret `url`. When omitted, the engine guesses from the URL.
    #[serde(default)]
    pub kind: Option<DownloadKind>,
    /// A pre-resolved, exact output path. Set by the engine when resuming so a
    /// restarted download writes to the SAME file instead of a collision-renamed
    /// one. When `None`, the engine resolves the path (dir + filename + collision
    /// handling) itself.
    #[serde(default)]
    pub resolved_target: Option<PathBuf>,
}

impl DownloadRequest {
    /// The effective kind: the explicit one, or a guess from the URL.
    pub fn effective_kind(&self) -> DownloadKind {
        self.kind.unwrap_or_else(|| DownloadKind::guess_from_url(&self.url))
    }
}

/// Engine-wide, user-configurable settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Where files are saved by default.
    pub download_dir: PathBuf,
    /// Max number of connections used to split a single file (segments).
    pub max_connections_per_file: usize,
    /// Max number of files downloading at the same time.
    pub max_concurrent_downloads: usize,
    /// Global speed limit in bytes/sec. `None` = unlimited.
    #[serde(default)]
    pub speed_limit: Option<u64>,
    /// Minimum file size (bytes) before we bother splitting into segments.
    pub min_split_size: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            max_connections_per_file: 8,
            max_concurrent_downloads: 4,
            speed_limit: None,
            // Don't split files smaller than 1 MiB; overhead isn't worth it.
            min_split_size: 1024 * 1024,
        }
    }
}

fn default_download_dir() -> PathBuf {
    // Prefer $HOME/Downloads, fall back to current dir.
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let p = PathBuf::from(home).join("Downloads");
        return p;
    }
    PathBuf::from(".")
}

/// The result of probing a URL to see whether it supports segmented downloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    /// Final URL after following redirects.
    pub final_url: String,
    /// Total size in bytes, if the server reported Content-Length.
    pub content_length: Option<u64>,
    /// Whether the server advertises byte-range support (`Accept-Ranges: bytes`
    /// or a verified partial-content response).
    pub supports_ranges: bool,
    /// Resolved filename (from Content-Disposition, URL path, or the request hint).
    pub filename: String,
    /// Validator for safe resume (ETag or Last-Modified).
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
}

impl ProbeResult {
    /// Segmented download is only possible when the server supports ranges AND
    /// we know the total size up front. Otherwise we must use a single stream.
    pub fn can_segment(&self) -> bool {
        self.supports_ranges && self.content_length.is_some()
    }
}

/// Lifecycle status of a download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Probing,
    Downloading,
    Paused,
    Completed,
    Failed,
    Canceled,
}

/// A persisted record of a download, so the queue survives app/host restarts.
///
/// This holds everything needed to resume or re-run a download later: the
/// original request, the resolved target path (once known), and the last known
/// status/progress. Byte-level resume is still handled by the per-file
/// `.grabpart` sidecar; this record just tracks *which* downloads exist.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadRecord {
    pub id: String,
    pub request: DownloadRequest,
    /// Resolved on-disk path once the download has started. `None` until then.
    #[serde(default)]
    pub target: Option<PathBuf>,
    pub status: Status,
    #[serde(default)]
    pub downloaded: u64,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
    /// Unix epoch seconds when the record was created (for ordering/history).
    #[serde(default)]
    pub created_at: u64,
}

impl DownloadRecord {
    pub fn new(id: String, request: DownloadRequest) -> Self {
        Self {
            id,
            request,
            target: None,
            status: Status::Queued,
            downloaded: 0,
            total: None,
            error: None,
            created_at: now_secs(),
        }
    }

    /// A download that should be resumed on restart (was mid-flight or paused).
    pub fn is_unfinished(&self) -> bool {
        matches!(
            self.status,
            Status::Queued | Status::Probing | Status::Downloading | Status::Paused | Status::Failed
        )
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A progress snapshot emitted by the engine for UI / logging.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Progress {
    pub id: String,
    pub status: Status,
    pub downloaded: u64,
    pub total: Option<u64>,
    /// Instantaneous speed in bytes/sec.
    pub speed: u64,
    /// Number of active connections for this download.
    pub connections: usize,
    #[serde(default)]
    pub error: Option<String>,
}
