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
    /// Which queue this download belongs to. Defaults to the main queue.
    #[serde(default)]
    pub queue: Option<String>,
}

impl DownloadRequest {
    /// The effective kind: the explicit one, or a guess from the URL.
    pub fn effective_kind(&self) -> DownloadKind {
        self.kind.unwrap_or_else(|| DownloadKind::guess_from_url(&self.url))
    }
    /// The queue id this download targets, defaulting to the main queue.
    pub fn queue_id(&self) -> String {
        self.queue.clone().unwrap_or_else(|| DEFAULT_QUEUE_ID.to_string())
    }
}

/// UI theme preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the operating system's light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

/// A named download queue with its own concurrency limit and optional schedule.
/// Downloads are assigned to a queue; the scheduler starts them subject to the
/// queue's `max_concurrent`, its `running` flag, and its `schedule` window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    /// Stable identifier used by downloads to reference this queue.
    pub id: String,
    /// Human-readable name shown in the UI.
    pub name: String,
    /// Max downloads from THIS queue that may transfer at once.
    pub max_concurrent: usize,
    /// When false, the queue is manually paused and starts nothing.
    #[serde(default = "default_true")]
    pub running: bool,
    /// Optional time-window schedule gating automatic starts.
    #[serde(default)]
    pub schedule: crate::schedule::Schedule,
}

impl Queue {
    pub fn new(id: impl Into<String>, name: impl Into<String>, max_concurrent: usize) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            max_concurrent: max_concurrent.max(1),
            running: true,
            schedule: crate::schedule::Schedule::default(),
        }
    }
}

/// The id of the built-in default queue that always exists.
pub const DEFAULT_QUEUE_ID: &str = "main";

/// A download category: files whose extension matches `extensions` are saved
/// into `<download_dir>/<subfolder>` automatically (IDM-style auto-foldering).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    /// Display name, e.g. "Video".
    pub name: String,
    /// Subfolder under the base download dir, e.g. "Video". Empty = base dir.
    pub subfolder: String,
    /// Lowercase extensions WITHOUT the dot, e.g. ["mp4", "mkv"].
    pub extensions: Vec<String>,
}

impl Category {
    fn matches_ext(&self, ext: &str) -> bool {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        self.extensions.iter().any(|e| e.eq_ignore_ascii_case(&ext))
    }
}

/// Engine-wide, user-configurable settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Where files are saved by default (the base dir; categories nest under it).
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
    /// Auto-foldering rules by file extension. First match wins.
    #[serde(default = "default_categories")]
    pub categories: Vec<Category>,
    /// When true, files are sorted into category subfolders automatically.
    #[serde(default = "default_true")]
    pub use_categories: bool,
    /// UI theme preference (consumed by the app; stored here for one source of truth).
    #[serde(default)]
    pub theme: Theme,
    /// Configured download queues. Always contains at least the main queue.
    #[serde(default = "default_queues")]
    pub queues: Vec<Queue>,
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
            categories: default_categories(),
            use_categories: true,
            theme: Theme::System,
            queues: default_queues(),
        }
    }
}

/// The default set of queues: a single "main" queue using the global concurrency.
fn default_queues() -> Vec<Queue> {
    vec![Queue::new(DEFAULT_QUEUE_ID, "Main", 4)]
}

impl Settings {
    /// Resolve the directory a file with `filename` should be saved into,
    /// honoring category rules. Returns the base dir when categories are off or
    /// nothing matches. Does NOT create the directory.
    pub fn category_dir_for(&self, filename: &str) -> PathBuf {
        if !self.use_categories {
            return self.download_dir.clone();
        }
        let ext = filename.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
        if ext.is_empty() {
            return self.download_dir.clone();
        }
        for cat in &self.categories {
            if cat.matches_ext(ext) && !cat.subfolder.is_empty() {
                return self.download_dir.join(&cat.subfolder);
            }
        }
        self.download_dir.clone()
    }

    /// Clamp the connection count to a sane range.
    pub fn effective_connections(&self) -> usize {
        self.max_connections_per_file.clamp(1, 32)
    }

    /// Look up a queue by id, falling back to the main queue, then to a
    /// synthesized default if even that is missing.
    pub fn queue(&self, id: &str) -> Queue {
        self.queues
            .iter()
            .find(|q| q.id == id)
            .or_else(|| self.queues.iter().find(|q| q.id == DEFAULT_QUEUE_ID))
            .cloned()
            .unwrap_or_else(|| Queue::new(DEFAULT_QUEUE_ID, "Main", self.max_concurrent_downloads))
    }

    /// Ensure the invariant that a main queue always exists; call after loading
    /// or editing settings.
    pub fn ensure_main_queue(&mut self) {
        if !self.queues.iter().any(|q| q.id == DEFAULT_QUEUE_ID) {
            self.queues.insert(
                0,
                Queue::new(DEFAULT_QUEUE_ID, "Main", self.max_concurrent_downloads),
            );
        }
    }
}

fn default_true() -> bool {
    true
}

/// The default IDM-style category set.
fn default_categories() -> Vec<Category> {
    let mk = |name: &str, sub: &str, exts: &[&str]| Category {
        name: name.to_string(),
        subfolder: sub.to_string(),
        extensions: exts.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        mk("Video", "Video", &["mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mpg", "mpeg"]),
        mk("Audio", "Audio", &["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus"]),
        mk("Documents", "Documents", &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "odt", "epub"]),
        mk("Programs", "Programs", &["exe", "msi", "dmg", "pkg", "deb", "rpm", "appimage", "apk"]),
        mk("Archives", "Archives", &["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso"]),
        mk("Images", "Images", &["jpg", "jpeg", "png", "gif", "webp", "bmp", "svg", "tiff"]),
    ]
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
    /// Last known smoothed speed in bytes/sec (persisted so the GUI, which reads
    /// this file, can show speed for downloads run by another process e.g. the
    /// browser's native host). 0 when not actively downloading.
    #[serde(default)]
    pub speed: u64,
    /// Last known estimated seconds remaining. `None` when unknown/stalled.
    #[serde(default)]
    pub eta_secs: Option<u64>,
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
            speed: 0,
            eta_secs: None,
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
    /// Smoothed speed in bytes/sec (exponential moving average, less jittery
    /// than a raw 1-second sample — better for display and ETA).
    pub speed: u64,
    /// Estimated seconds remaining, if total size and a non-zero speed are
    /// known. `None` when the size is unknown or the download is stalled.
    #[serde(default)]
    pub eta_secs: Option<u64>,
    /// Number of active connections for this download.
    pub connections: usize,
    #[serde(default)]
    pub error: Option<String>,
}

impl Progress {
    /// Compute seconds remaining from the current counters. `None` if the total
    /// is unknown or speed is zero (can't estimate).
    pub fn estimate_eta(downloaded: u64, total: Option<u64>, speed: u64) -> Option<u64> {
        let total = total?;
        if speed == 0 || downloaded >= total {
            return None;
        }
        Some((total - downloaded) / speed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings_with_base(base: &str) -> Settings {
        Settings {
            download_dir: PathBuf::from(base),
            ..Settings::default()
        }
    }

    #[test]
    fn category_routes_video_to_subfolder() {
        let s = settings_with_base("/dl");
        assert_eq!(s.category_dir_for("movie.mp4"), PathBuf::from("/dl/Video"));
        assert_eq!(s.category_dir_for("song.mp3"), PathBuf::from("/dl/Audio"));
        assert_eq!(s.category_dir_for("report.pdf"), PathBuf::from("/dl/Documents"));
        assert_eq!(s.category_dir_for("app.exe"), PathBuf::from("/dl/Programs"));
        assert_eq!(s.category_dir_for("pack.zip"), PathBuf::from("/dl/Archives"));
    }

    #[test]
    fn category_is_case_insensitive() {
        let s = settings_with_base("/dl");
        assert_eq!(s.category_dir_for("CLIP.MKV"), PathBuf::from("/dl/Video"));
    }

    #[test]
    fn unknown_extension_stays_in_base() {
        let s = settings_with_base("/dl");
        assert_eq!(s.category_dir_for("data.xyz"), PathBuf::from("/dl"));
        assert_eq!(s.category_dir_for("noext"), PathBuf::from("/dl"));
    }

    #[test]
    fn categories_disabled_uses_base() {
        let mut s = settings_with_base("/dl");
        s.use_categories = false;
        assert_eq!(s.category_dir_for("movie.mp4"), PathBuf::from("/dl"));
    }

    #[test]
    fn connections_are_clamped() {
        let mut s = Settings::default();
        s.max_connections_per_file = 999;
        assert_eq!(s.effective_connections(), 32);
        s.max_connections_per_file = 0;
        assert_eq!(s.effective_connections(), 1);
    }

    #[test]
    fn settings_roundtrip_json_with_new_fields() {
        let s = Settings::default();
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.categories.len(), s.categories.len());
        assert!(back.use_categories);
        assert_eq!(back.theme, Theme::System);
    }

    #[test]
    fn old_settings_json_without_new_fields_still_loads() {
        // A settings.json written by an earlier version (no categories/theme).
        let old = r#"{
            "download_dir": "/dl",
            "max_connections_per_file": 8,
            "max_concurrent_downloads": 4,
            "min_split_size": 1048576
        }"#;
        let s: Settings = serde_json::from_str(old).unwrap();
        // Defaults fill in for the missing fields.
        assert!(s.use_categories);
        assert!(!s.categories.is_empty());
        assert_eq!(s.theme, Theme::System);
    }
}
