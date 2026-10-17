//! grab-engine: a cross-platform segmented download engine.
//!
//! The engine is UI-agnostic: it's consumed by the Tauri desktop app and by the
//! native-messaging host that talks to the browser extension.

pub mod download;
pub mod manager;
pub mod probe;
pub mod segment;
pub mod state;
pub mod types;

pub use download::{download, Handle};
pub use manager::Manager;
pub use probe::probe;
pub use segment::{plan, Segment};
pub use state::DownloadState;
pub use types::{DownloadRequest, Progress, ProbeResult, Settings, Status};
