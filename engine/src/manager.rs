//! The download manager: owns settings, enforces the max-concurrent-downloads
//! limit via a semaphore, assigns ids, persists the queue, and exposes
//! pause/resume/cancel per id.

use crate::download::Handle;
use crate::ratelimit::RateLimiter;
use crate::registry::Registry;
use crate::types::{DownloadRecord, DownloadRequest, Progress, Settings};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, Semaphore};

/// Shared, cloneable handle to the running download system.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    settings: Mutex<Settings>,
    /// Caps how many downloads actively transfer at once.
    permits: Semaphore,
    handles: Mutex<HashMap<String, Handle>>,
    next_id: AtomicU64,
    /// Progress goes out to the caller here…
    progress_tx: mpsc::UnboundedSender<Progress>,
    /// …and is mirrored into the persistent queue here.
    registry: Registry,
    /// Shared global bandwidth cap applied across every active download.
    limiter: Arc<RateLimiter>,
}

impl Manager {
    /// Create a manager WITHOUT queue persistence (in-memory only). Kept for
    /// tests and simple embeddings.
    pub fn new(settings: Settings) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        // An ephemeral registry pointed at a temp path that we never reload.
        let tmp = std::env::temp_dir().join(format!("grab-queue-{}.json", std::process::id()));
        Self::with_registry(settings, Registry::empty(tmp))
    }

    /// Create a manager that persists its queue to `registry`. Use
    /// [`Manager::restore`] afterward to re-enqueue unfinished downloads.
    pub async fn with_persistence(
        settings: Settings,
        registry_path: std::path::PathBuf,
    ) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        let registry = Registry::open(registry_path).await;
        // Continue the id sequence past anything already on disk.
        let start = registry.max_numeric_id().await + 1;
        let (mgr, rx) = Self::with_registry(settings, registry);
        mgr.inner.next_id.store(start, Ordering::SeqCst);
        (mgr, rx)
    }

    fn with_registry(
        settings: Settings,
        registry: Registry,
    ) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        // `internal_tx` receives every progress event from download tasks. A
        // relay task mirrors each event into the persistent registry, then
        // forwards it to the public receiver the caller holds (`rx`).
        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<Progress>();
        let (public_tx, rx) = mpsc::unbounded_channel::<Progress>();

        let client = Client::builder()
            .user_agent(concat!("grab/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build HTTP client");

        let permits = Semaphore::new(settings.max_concurrent_downloads.max(1));
        let limiter = Arc::new(RateLimiter::new(settings.speed_limit));
        let inner = Arc::new(Inner {
            client,
            settings: Mutex::new(settings),
            permits,
            handles: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            progress_tx: internal_tx,
            registry,
            limiter,
        });

        // Relay: persist-then-forward. Keeps the queue file in sync with the
        // last known status/progress of every download, automatically.
        let registry = inner.registry.clone();
        tokio::spawn(async move {
            while let Some(p) = internal_rx.recv().await {
                registry
                    .update(&p.id, p.status, p.downloaded, p.total, p.error.clone())
                    .await;
                // If the public receiver is gone, keep draining so download
                // tasks don't block on a full-less unbounded channel (no-op).
                let _ = public_tx.send(p);
            }
        });

        (Self { inner }, rx)
    }

    /// Re-enqueue every download that was unfinished at last shutdown. Byte-level
    /// progress is picked up automatically from each file's `.grabpart` sidecar.
    /// Returns the ids that were restarted.
    pub async fn restore(&self) -> Vec<String> {
        let unfinished = self.inner.registry.unfinished().await;
        let mut restarted = Vec::new();
        for record in unfinished {
            let mut req = record.request.clone();
            // Resume to the SAME file that was being written, if we recorded it.
            if req.resolved_target.is_none() {
                req.resolved_target = record.target.clone();
            }
            self.spawn_download(record.id.clone(), req).await;
            restarted.push(record.id);
        }
        restarted
    }

    /// Queue a download. Returns its id immediately; work runs in the background
    /// once a concurrency permit is available.
    pub async fn enqueue(&self, req: DownloadRequest) -> String {
        let id = format!("dl-{}", self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        // Persist the record before any work starts, so a crash right after
        // enqueue still leaves the download in the queue to be resumed.
        self.inner
            .registry
            .upsert(DownloadRecord::new(id.clone(), req.clone()))
            .await;
        self.spawn_download(id.clone(), req).await;
        id
    }

    /// Spawn the background task for a single download id (shared by enqueue and
    /// restore).
    async fn spawn_download(&self, id: String, req: DownloadRequest) {
        let handle = Handle::new();
        self.inner
            .handles
            .lock()
            .await
            .insert(id.clone(), handle.clone());

        let inner = self.inner.clone();
        tokio::spawn(async move {
            // Wait for a slot so we honor max_concurrent_downloads.
            let _permit = inner.permits.acquire().await.expect("semaphore open");
            let settings = inner.settings.lock().await.clone();
            // Record the resolved target path into the registry as soon as the
            // download knows it, so a later restart resumes the same file.
            let registry = inner.registry.clone();
            let sink_id = id.clone();
            let target_sink: crate::download::TargetSink =
                std::sync::Arc::new(move |path: std::path::PathBuf| {
                    let registry = registry.clone();
                    let sink_id = sink_id.clone();
                    tokio::spawn(async move {
                        registry.set_target(&sink_id, path).await;
                    });
                });
            let _ = crate::download::download_full(
                inner.client.clone(),
                id.clone(),
                req,
                settings,
                handle,
                inner.progress_tx.clone(),
                Some(target_sink),
                inner.limiter.clone(),
            )
            .await;
            inner.handles.lock().await.remove(&id);
        });
    }

    pub async fn pause(&self, id: &str) {
        if let Some(h) = self.inner.handles.lock().await.get(id) {
            h.pause();
        }
    }

    /// Resume a download. If it's still live (just paused), flip the flag.
    /// If it's no longer running (e.g. paused then the app restarted, or it
    /// failed), re-enqueue it from its persisted record.
    pub async fn resume(&self, id: &str) {
        let live = {
            let handles = self.inner.handles.lock().await;
            if let Some(h) = handles.get(id) {
                h.resume();
                true
            } else {
                false
            }
        };
        if !live {
            // Not running: look it up in the registry and start it again.
            if let Some(record) = self.inner.registry.all().await.into_iter().find(|r| r.id == id) {
                self.spawn_download(id.to_string(), record.request).await;
            }
        }
    }

    pub async fn cancel(&self, id: &str) {
        if let Some(h) = self.inner.handles.lock().await.get(id) {
            h.cancel();
        }
    }

    /// Set the resolved on-disk target path for a download in the registry.
    pub async fn record_target(&self, id: &str, target: std::path::PathBuf) {
        self.inner.registry.set_target(id, target).await;
    }

    /// Remove a download from the persistent queue (does not stop it; call
    /// `cancel` first if it may be running).
    pub async fn forget(&self, id: &str) {
        self.inner.registry.remove(id).await;
    }

    /// Snapshot of all persisted download records (oldest first).
    pub async fn records(&self) -> Vec<DownloadRecord> {
        self.inner.registry.all().await
    }

    pub async fn update_settings(&self, settings: Settings) {
        // Apply the new speed limit to the live shared limiter immediately.
        self.inner.limiter.set_rate(settings.speed_limit);
        *self.inner.settings.lock().await = settings;
    }

    pub async fn settings(&self) -> Settings {
        self.inner.settings.lock().await.clone()
    }
}


