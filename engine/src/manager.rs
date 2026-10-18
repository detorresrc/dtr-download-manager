//! The download manager: owns settings and queues, assigns ids, persists the
//! queue, and runs a **scheduler** that starts downloads subject to each
//! queue's concurrency limit, running flag, and time-window schedule.
//!
//! ## Start model (Tier 2)
//! `enqueue` no longer spawns a transfer immediately. Instead it records the
//! download as `Queued`. A background scheduler loop (`tick`) decides which
//! queued downloads may start: for each queue, if it's running and its schedule
//! is active, it promotes the oldest queued downloads until the queue's
//! `max_concurrent` is reached. This gives IDM-style queues + scheduling while
//! reusing the same per-download engine, resume, rate limiting, and persistence.

use crate::download::Handle;
use crate::ratelimit::RateLimiter;
use crate::registry::Registry;
use crate::schedule::now_local_weekday_minute;
use crate::types::{DownloadRecord, DownloadRequest, Progress, Settings, Status};
use reqwest::Client;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Shared, cloneable handle to the running download system.
#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

struct Inner {
    client: Client,
    settings: Mutex<Settings>,
    /// Live handles for currently-running downloads (id -> pause/cancel handle).
    handles: Mutex<HashMap<String, Handle>>,
    /// Ids that are actively transferring right now (promoted by the scheduler).
    running: Mutex<std::collections::HashSet<String>>,
    next_id: AtomicU64,
    progress_tx: mpsc::UnboundedSender<Progress>,
    registry: Registry,
    limiter: Arc<RateLimiter>,
}

impl Manager {
    /// In-memory manager (no persisted queue). The scheduler still runs.
    pub fn new(settings: Settings) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        let tmp = std::env::temp_dir().join(format!("grab-queue-{}.json", std::process::id()));
        Self::with_registry(settings, Registry::empty(tmp))
    }

    /// Manager that persists its queue to disk. Call [`Manager::restore`] after.
    pub async fn with_persistence(
        settings: Settings,
        registry_path: std::path::PathBuf,
    ) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        let registry = Registry::open(registry_path).await;
        let start = registry.max_numeric_id().await + 1;
        let (mgr, rx) = Self::with_registry(settings, registry);
        mgr.inner.next_id.store(start, Ordering::SeqCst);
        (mgr, rx)
    }

    fn with_registry(
        mut settings: Settings,
        registry: Registry,
    ) -> (Self, mpsc::UnboundedReceiver<Progress>) {
        settings.ensure_main_queue();

        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<Progress>();
        let (public_tx, rx) = mpsc::unbounded_channel::<Progress>();

        let client = Client::builder()
            .user_agent(concat!("grab/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()
            .expect("failed to build HTTP client");

        let limiter = Arc::new(RateLimiter::new(settings.speed_limit));
        let inner = Arc::new(Inner {
            client,
            settings: Mutex::new(settings),
            handles: Mutex::new(HashMap::new()),
            running: Mutex::new(std::collections::HashSet::new()),
            next_id: AtomicU64::new(1),
            progress_tx: internal_tx,
            registry,
            limiter,
        });

        // Relay: mirror every progress event into the registry, then forward it.
        // Also clears the `running` set entry on terminal states so the
        // scheduler can promote the next queued download.
        {
            let registry = inner.registry.clone();
            let inner2 = inner.clone();
            tokio::spawn(async move {
                while let Some(p) = internal_rx.recv().await {
                    registry
                        .update(&p.id, p.status, p.downloaded, p.total, p.error.clone())
                        .await;
                    if matches!(
                        p.status,
                        Status::Completed | Status::Failed | Status::Canceled | Status::Paused
                    ) {
                        inner2.running.lock().await.remove(&p.id);
                    }
                    let _ = public_tx.send(p);
                }
            });
        }

        // Scheduler: periodically promote eligible queued downloads.
        {
            let inner = inner.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
                loop {
                    ticker.tick().await;
                    Manager { inner: inner.clone() }.tick().await;
                }
            });
        }

        (Self { inner }, rx)
    }

    /// One scheduler pass: for every queue, start queued downloads up to the
    /// queue's concurrency limit, if the queue is running and schedule-active.
    pub async fn tick(&self) {
        let settings = self.inner.settings.lock().await.clone();
        let (weekday, minute) = now_local_weekday_minute();

        // Snapshot current records and the running set.
        let records = self.inner.registry.all().await;
        let running = self.inner.running.lock().await.clone();

        // Count how many are running per queue right now.
        let mut running_per_queue: HashMap<String, usize> = HashMap::new();
        for r in &records {
            if running.contains(&r.id) {
                *running_per_queue.entry(r.request.queue_id()).or_default() += 1;
            }
        }

        // Promote queued downloads, oldest first (records() is sorted by created_at).
        for r in &records {
            if r.status != Status::Queued {
                continue;
            }
            if running.contains(&r.id) {
                continue; // already being started this tick
            }
            let qid = r.request.queue_id();
            let q = settings.queue(&qid);
            if !q.running || !q.schedule.is_active_at(weekday, minute) {
                continue;
            }
            let active = running_per_queue.get(&qid).copied().unwrap_or(0);
            if active >= q.max_concurrent.max(1) {
                continue;
            }
            // Eligible: mark running and spawn.
            self.inner.running.lock().await.insert(r.id.clone());
            *running_per_queue.entry(qid).or_default() += 1;
            let mut req = r.request.clone();
            if req.resolved_target.is_none() {
                req.resolved_target = r.target.clone();
            }
            self.spawn_download(r.id.clone(), req).await;
        }
    }

    /// Re-queue downloads that were unfinished at last shutdown. They enter the
    /// `Queued` state; the scheduler starts them subject to queue rules.
    pub async fn restore(&self) -> Vec<String> {
        let unfinished = self.inner.registry.unfinished().await;
        let mut ids = Vec::new();
        for record in &unfinished {
            // Reset to Queued so the scheduler governs the restart.
            self.inner
                .registry
                .update(&record.id, Status::Queued, record.downloaded, record.total, None)
                .await;
            ids.push(record.id.clone());
        }
        // Kick a scheduling pass right away.
        self.tick().await;
        ids
    }

    /// Queue a download. Returns its id immediately. The scheduler starts it
    /// when its queue has capacity and is active.
    pub async fn enqueue(&self, req: DownloadRequest) -> String {
        let id = format!("dl-{}", self.inner.next_id.fetch_add(1, Ordering::SeqCst));
        let mut record = DownloadRecord::new(id.clone(), req);
        record.status = Status::Queued;
        self.inner.registry.upsert(record).await;
        // Try to start immediately if the queue has room.
        self.tick().await;
        id
    }

    /// Spawn the background transfer task for a download id.
    async fn spawn_download(&self, id: String, req: DownloadRequest) {
        let handle = Handle::new();
        self.inner
            .handles
            .lock()
            .await
            .insert(id.clone(), handle.clone());

        let inner = self.inner.clone();
        tokio::spawn(async move {
            let settings = inner.settings.lock().await.clone();
            let registry = inner.registry.clone();
            let sink_id = id.clone();
            let target_sink: crate::download::TargetSink =
                Arc::new(move |path: std::path::PathBuf| {
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
            inner.running.lock().await.remove(&id);
        });
    }

    pub async fn pause(&self, id: &str) {
        if let Some(h) = self.inner.handles.lock().await.get(id) {
            h.pause();
        }
    }

    /// Resume a paused/failed download by returning it to the `Queued` state;
    /// the scheduler restarts it when the queue has room.
    pub async fn resume(&self, id: &str) {
        // If it's live and merely paused, flip the flag for an instant resume.
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
            if let Some(r) = self.inner.registry.all().await.into_iter().find(|r| r.id == id) {
                self.inner
                    .registry
                    .update(id, Status::Queued, r.downloaded, r.total, None)
                    .await;
                self.tick().await;
            }
        }
    }

    pub async fn cancel(&self, id: &str) {
        if let Some(h) = self.inner.handles.lock().await.get(id) {
            h.cancel();
        }
        self.inner.running.lock().await.remove(id);
    }

    // ---- Queue controls -----------------------------------------------------

    /// Pause a whole queue: stop new starts and pause everything running in it.
    pub async fn pause_queue(&self, queue_id: &str) {
        {
            let mut s = self.inner.settings.lock().await;
            if let Some(q) = s.queues.iter_mut().find(|q| q.id == queue_id) {
                q.running = false;
            }
        }
        // Pause currently-running downloads in that queue.
        for r in self.inner.registry.all().await {
            if r.request.queue_id() == queue_id {
                self.pause(&r.id).await;
            }
        }
    }

    /// Resume/start a whole queue.
    pub async fn start_queue(&self, queue_id: &str) {
        {
            let mut s = self.inner.settings.lock().await;
            if let Some(q) = s.queues.iter_mut().find(|q| q.id == queue_id) {
                q.running = true;
            }
        }
        // Return paused downloads in this queue to Queued, then schedule.
        for r in self.inner.registry.all().await {
            if r.request.queue_id() == queue_id && r.status == Status::Paused {
                self.inner
                    .registry
                    .update(&r.id, Status::Queued, r.downloaded, r.total, None)
                    .await;
            }
        }
        self.tick().await;
    }

    /// Pause every queue (global "pause all").
    pub async fn pause_all(&self) {
        let ids: Vec<String> = {
            let s = self.inner.settings.lock().await;
            s.queues.iter().map(|q| q.id.clone()).collect()
        };
        for id in ids {
            self.pause_queue(&id).await;
        }
    }

    /// Start every queue (global "start all").
    pub async fn start_all(&self) {
        let ids: Vec<String> = {
            let s = self.inner.settings.lock().await;
            s.queues.iter().map(|q| q.id.clone()).collect()
        };
        for id in ids {
            self.start_queue(&id).await;
        }
    }

    // ---- Registry passthroughs ----------------------------------------------

    pub async fn record_target(&self, id: &str, target: std::path::PathBuf) {
        self.inner.registry.set_target(id, target).await;
    }

    pub async fn forget(&self, id: &str) {
        self.inner.registry.remove(id).await;
    }

    pub async fn records(&self) -> Vec<DownloadRecord> {
        self.inner.registry.all().await
    }

    pub async fn update_settings(&self, mut settings: Settings) {
        settings.ensure_main_queue();
        self.inner.limiter.set_rate(settings.speed_limit);
        *self.inner.settings.lock().await = settings;
        // A settings change (queue limits/schedules) may free up capacity.
        self.tick().await;
    }

    pub async fn settings(&self) -> Settings {
        self.inner.settings.lock().await.clone()
    }
}
