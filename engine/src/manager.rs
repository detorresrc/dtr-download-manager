//! The download manager: owns settings, enforces the max-concurrent-downloads
//! limit via a semaphore, assigns ids, and exposes pause/resume/cancel per id.

use crate::download::{download, Handle};
use crate::types::{DownloadRequest, Progress, Settings};
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
    progress_tx: mpsc::UnboundedSender<Progress>,
}

impl Manager {
    /// Create a manager. Progress events are delivered on the returned receiver.
    pub fn new(settings: Settings) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let client = Client::builder()
            .user_agent(concat!("grab/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build HTTP client");

        let permits = Semaphore::new(settings.max_concurrent_downloads.max(1));
        let inner = Inner {
            client,
            settings: Mutex::new(settings),
            permits,
            handles: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            progress_tx: tx,
        };
        (Self { inner: Arc::new(inner) }, rx)
    }

    /// Queue a download. Returns its id immediately; work runs in the background
    /// once a concurrency permit is available.
    pub async fn enqueue(&self, req: DownloadRequest) -> String {
        let id = format!("dl-{}", self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let handle = Handle::new();
        self.inner
            .handles
            .lock()
            .await
            .insert(id.clone(), handle.clone());

        let inner = self.inner.clone();
        let id_for_task = id.clone();
        tokio::spawn(async move {
            // Wait for a slot so we honor max_concurrent_downloads.
            let _permit = inner.permits.acquire().await.expect("semaphore open");
            let settings = inner.settings.lock().await.clone();
            let _ = download(
                inner.client.clone(),
                id_for_task.clone(),
                req,
                settings,
                handle,
                inner.progress_tx.clone(),
            )
            .await;
            inner.handles.lock().await.remove(&id_for_task);
        });

        id
    }

    pub async fn pause(&self, id: &str) {
        if let Some(h) = self.inner.handles.lock().await.get(id) {
            h.pause();
        }
    }

    pub async fn resume(&self, id: &str) {
        if let Some(h) = self.inner.handles.lock().await.get(id) {
            h.resume();
        }
    }

    pub async fn cancel(&self, id: &str) {
        if let Some(h) = self.inner.handles.lock().await.get(id) {
            h.cancel();
        }
    }

    pub async fn update_settings(&self, settings: Settings) {
        *self.inner.settings.lock().await = settings;
    }

    pub async fn settings(&self) -> Settings {
        self.inner.settings.lock().await.clone()
    }
}
