//! grab-engine: a cross-platform segmented download engine.
//!
//! The engine is UI-agnostic: it's consumed by the Tauri desktop app and by the
//! native-messaging host that talks to the browser extension.

pub mod download;
pub mod manager;
pub mod media;
pub mod media_download;
pub mod probe;
pub mod ratelimit;
pub mod registry;
pub mod schedule;
pub mod segment;
pub mod state;
pub mod types;

pub use download::{download, Handle};
pub use manager::Manager;
pub use probe::probe;
pub use ratelimit::RateLimiter;
pub use registry::Registry;
pub use schedule::Schedule;
pub use segment::{plan, Segment};
pub use state::DownloadState;
pub use types::{
    Category, DownloadKind, DownloadRecord, DownloadRequest, PendingAction, Progress, ProbeResult,
    Queue,
    Settings, Status, Theme, DEFAULT_QUEUE_ID,
};
