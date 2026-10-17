//! grab-engine: a cross-platform segmented download engine.
//!
//! The engine is UI-agnostic: it's consumed by the Tauri desktop app and by the
//! native-messaging host that talks to the browser extension.

pub mod download;
pub mod manager;
pub mod media;
pub mod media_download;
pub mod probe;
pub mod registry;
pub mod segment;
pub mod state;
pub mod types;

pub use download::{download, Handle};
pub use manager::Manager;
pub use probe::probe;
pub use registry::Registry;
pub use segment::{plan, Segment};
pub use state::DownloadState;
pub use types::{
    DownloadKind, DownloadRecord, DownloadRequest, Progress, ProbeResult, Settings, Status,
};
