//! Shared data types for the download engine.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

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
