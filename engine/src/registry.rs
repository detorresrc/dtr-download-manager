//! Persistent download queue.
//!
//! The engine keeps the *list* of downloads (not their bytes) in a single JSON
//! file so the queue survives restarts. Byte-level resume is still handled by
//! the per-file `.grabpart` sidecars; this registry records which downloads
//! exist, their target paths, and their last known status.
//!
//! Writes are atomic (write to a temp file, then rename) so a crash mid-write
//! can never corrupt the queue file.

use crate::types::{DownloadRecord, Status};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A thread-safe, persistent registry of download records.
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    path: PathBuf,
    /// id -> record. Insertion order is not preserved; `created_at` orders them.
    records: HashMap<String, DownloadRecord>,
}

impl Registry {
    /// Open (or create) the registry at `path`, loading any existing records.
    pub async fn open(path: PathBuf) -> Self {
        let records = load(&path).await.unwrap_or_default();
        Self {
            inner: Arc::new(Mutex::new(Inner { path, records })),
        }
    }

    /// An empty, in-memory registry that still persists to `path` once written
    /// to. Does not read any existing file. Safe to build from a sync context.
    pub fn empty(path: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                path,
                records: HashMap::new(),
            })),
        }
    }

    /// Insert or replace a record, then persist.
    pub async fn upsert(&self, record: DownloadRecord) {
        let mut inner = self.inner.lock().await;
        inner.records.insert(record.id.clone(), record);
        let _ = save(&inner.path, &inner.records).await;
    }

    /// Update the mutable progress/status fields of a record from a live
    /// `Progress` event, then persist. No-ops if the id is unknown.
    ///
    /// Persisting `speed`/`eta_secs` here is what lets the GUI display live
    /// stats for downloads run by a DIFFERENT process (the browser's native
    /// host): the app reads this file and sees the latest speed/ETA.
    pub async fn update(&self, p: &crate::types::Progress) {
        let mut inner = self.inner.lock().await;
        if let Some(r) = inner.records.get_mut(&p.id) {
            r.status = p.status;
            r.downloaded = p.downloaded;
            if p.total.is_some() {
                r.total = p.total;
            }
            r.speed = p.speed;
            r.eta_secs = p.eta_secs;
            r.error = p.error.clone();
            let _ = save(&inner.path, &inner.records).await;
        }
    }

    /// Set just the status of a record (and clear transient speed/ETA, since a
    /// non-downloading state isn't moving), then persist. Used for queue/pause
    /// transitions. No-ops if the id is unknown.
    pub async fn set_status(&self, id: &str, status: Status) {
        let mut inner = self.inner.lock().await;
        if let Some(r) = inner.records.get_mut(id) {
            r.status = status;
            r.speed = 0;
            r.eta_secs = None;
            let _ = save(&inner.path, &inner.records).await;
        }
    }

    /// Request a control action on a download from another process (the GUI).
    /// Written into the shared file; the owning process (the host) picks it up.
    pub async fn request_action(&self, id: &str, action: crate::types::PendingAction) {
        let mut inner = self.inner.lock().await;
        if let Some(r) = inner.records.get_mut(id) {
            r.pending_action = Some(action);
            let _ = save(&inner.path, &inner.records).await;
        }
    }

    /// Reload the file from disk (to observe actions written by another process),
    /// then return and CLEAR any pending actions. The owning process calls this
    /// periodically and applies each `(id, action)` to its live downloads.
    pub async fn take_pending_actions(&self) -> Vec<(String, crate::types::PendingAction)> {
        let mut inner = self.inner.lock().await;

        // Merge actions written to disk by another process into our in-memory
        // view. We only adopt the `pending_action` field from disk so we don't
        // clobber our own fresher status/progress for downloads we own.
        if let Some(on_disk) = load(&inner.path).await {
            for (id, dr) in on_disk {
                if let Some(action) = dr.pending_action {
                    if let Some(r) = inner.records.get_mut(&id) {
                        r.pending_action = Some(action);
                    }
                }
            }
        }

        let pending: Vec<(String, crate::types::PendingAction)> = inner
            .records
            .iter()
            .filter_map(|(id, r)| r.pending_action.map(|a| (id.clone(), a)))
            .collect();

        if !pending.is_empty() {
            for (id, _) in &pending {
                if let Some(r) = inner.records.get_mut(id) {
                    r.pending_action = None;
                }
            }
            let _ = save(&inner.path, &inner.records).await;
        }
        pending
    }

    /// Record the resolved on-disk target path for a download.
    pub async fn set_target(&self, id: &str, target: PathBuf) {
        let mut inner = self.inner.lock().await;
        if let Some(r) = inner.records.get_mut(id) {
            r.target = Some(target);
            let _ = save(&inner.path, &inner.records).await;
        }
    }

    /// Remove a record (e.g. user cleared it from the list), then persist.
    pub async fn remove(&self, id: &str) {
        let mut inner = self.inner.lock().await;
        inner.records.remove(id);
        let _ = save(&inner.path, &inner.records).await;
    }

    /// All records, sorted oldest-first by creation time.
    pub async fn all(&self) -> Vec<DownloadRecord> {
        let inner = self.inner.lock().await;
        let mut v: Vec<DownloadRecord> = inner.records.values().cloned().collect();
        v.sort_by_key(|r| r.created_at);
        v
    }

    /// Records that were unfinished at last shutdown (to resume on startup).
    pub async fn unfinished(&self) -> Vec<DownloadRecord> {
        let inner = self.inner.lock().await;
        let mut v: Vec<DownloadRecord> = inner
            .records
            .values()
            .filter(|r| r.is_unfinished())
            .cloned()
            .collect();
        v.sort_by_key(|r| r.created_at);
        v
    }

    /// Scan `dir` for orphaned `.grabpart` sidecars — partial downloads whose
    /// queue record was lost (e.g. queue.json was deleted) — and re-add a record
    /// for each so the download can be resumed instead of being stranded.
    ///
    /// `next_id` supplies fresh ids (e.g. from `max_numeric_id() + 1`); returns
    /// the ids added. Safe to call on startup.
    pub async fn reconcile_orphans(&self, dir: &Path, mut next_id: u64) -> Vec<String> {
        // Collect targets we already track so we don't duplicate them.
        let known_targets: std::collections::HashSet<PathBuf> = {
            let inner = self.inner.lock().await;
            inner
                .records
                .values()
                .filter_map(|r| r.target.clone())
                .collect()
        };

        let mut added = Vec::new();
        let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
            return added;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let part = entry.path();
            if part.extension().map(|e| e == "grabpart").unwrap_or(false) {
                // The real target is the sidecar path without the ".grabpart".
                let target = strip_grabpart(&part);
                if known_targets.contains(&target) {
                    continue; // already tracked
                }
                // Parse the sidecar to recover the URL and progress.
                let Ok(bytes) = tokio::fs::read(&part).await else { continue };
                let Ok(state) = serde_json::from_slice::<crate::state::DownloadState>(&bytes) else {
                    continue;
                };
                let id = format!("dl-{next_id}");
                next_id += 1;
                let mut req = crate::types::DownloadRequest {
                    url: state.url.clone(),
                    filename: target
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string()),
                    headers: Default::default(),
                    save_dir: None,
                    kind: None,
                    // Resume to the SAME file.
                    resolved_target: Some(target.clone()),
                    queue: None,
                };
                // Keep category routing from re-moving the file.
                let _ = &mut req;
                let mut record = DownloadRecord::new(id.clone(), req);
                record.target = Some(target);
                record.status = Status::Paused; // unfinished; resume will pick it up
                record.downloaded = state.total_completed();
                record.total = state.content_length;
                self.upsert(record).await;
                tracing::info!("recovered orphaned partial download as {id}");
                added.push(id);
            }
        }
        added
    }

    /// The highest numeric suffix seen in existing `dl-N` ids, so a restarted
    /// manager can continue the id sequence instead of colliding from 1.
    pub async fn max_numeric_id(&self) -> u64 {
        let inner = self.inner.lock().await;
        inner
            .records
            .keys()
            .filter_map(|id| id.strip_prefix("dl-"))
            .filter_map(|n| n.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
    }
}

/// Turn `/path/to/file.zip.grabpart` back into `/path/to/file.zip`.
fn strip_grabpart(part: &Path) -> PathBuf {
    let s = part.to_string_lossy();
    match s.strip_suffix(".grabpart") {
        Some(base) => PathBuf::from(base),
        None => part.to_path_buf(),
    }
}

/// Path of the backup copy kept alongside the queue file.
fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".bak");
    PathBuf::from(s)
}

/// Load the registry, tolerating an accidentally deleted or corrupted main
/// file by falling back to the `.bak` copy written on the previous save.
async fn load(path: &Path) -> Option<HashMap<String, DownloadRecord>> {
    if let Some(map) = load_file(path).await {
        return Some(map);
    }
    // Main file missing or unparseable: try the backup.
    let bak = backup_path(path);
    if let Some(map) = load_file(&bak).await {
        tracing::warn!(
            "queue file {} missing/corrupt; recovered from backup {}",
            path.display(),
            bak.display()
        );
        return Some(map);
    }
    None
}

async fn load_file(path: &Path) -> Option<HashMap<String, DownloadRecord>> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let records: Vec<DownloadRecord> = serde_json::from_slice(&bytes).ok()?;
    Some(records.into_iter().map(|r| (r.id.clone(), r)).collect())
}

/// Atomically persist the registry: write to `<path>.tmp` then rename over the
/// real file so readers never see a half-written file. Before overwriting, the
/// previous good file is copied to `<path>.bak` so an accidental deletion or a
/// corrupt write can be recovered on the next load.
async fn save(path: &Path, records: &HashMap<String, DownloadRecord>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    // Keep the current good file as a backup before we replace it.
    if tokio::fs::try_exists(path).await.unwrap_or(false) {
        let _ = tokio::fs::copy(path, backup_path(path)).await;
    }

    let mut list: Vec<&DownloadRecord> = records.values().collect();
    list.sort_by_key(|r| r.created_at);
    let json = serde_json::to_vec_pretty(&list).expect("serializable records");

    let tmp = path.with_extension("json.tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, path).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::DownloadRequest;

    fn req(url: &str) -> DownloadRequest {
        DownloadRequest {
            url: url.into(),
            filename: None,
            headers: Default::default(),
            save_dir: None,
            kind: None,
            resolved_target: None,
            queue: None,
        }
    }

    #[tokio::test]
    async fn persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("grab-reg-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("queue.json");

        {
            let reg = Registry::open(path.clone()).await;
            reg.upsert(DownloadRecord::new("dl-1".into(), req("https://a/1"))).await;
            reg.upsert(DownloadRecord::new("dl-2".into(), req("https://a/2"))).await;
            reg.update(&crate::types::Progress {
                id: "dl-1".into(),
                status: Status::Completed,
                downloaded: 100,
                total: Some(100),
                speed: 0,
                eta_secs: None,
                connections: 0,
                error: None,
            })
            .await;
        }

        // Reopen: records should survive.
        let reg = Registry::open(path.clone()).await;
        let all = reg.all().await;
        assert_eq!(all.len(), 2);
        assert_eq!(reg.max_numeric_id().await, 2);

        // dl-2 was left Queued -> unfinished; dl-1 completed -> not.
        let unfinished = reg.unfinished().await;
        assert_eq!(unfinished.len(), 1);
        assert_eq!(unfinished[0].id, "dl-2");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn remove_deletes_record() {
        let dir = std::env::temp_dir().join(format!("grab-reg2-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("queue.json");

        let reg = Registry::open(path.clone()).await;
        reg.upsert(DownloadRecord::new("dl-1".into(), req("https://a/1"))).await;
        reg.remove("dl-1").await;
        assert!(reg.all().await.is_empty());

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn recovers_from_backup_when_main_file_deleted() {
        let dir = std::env::temp_dir().join(format!("grab-reg3-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("queue.json");

        {
            let reg = Registry::open(path.clone()).await;
            reg.upsert(DownloadRecord::new("dl-1".into(), req("https://a/1"))).await;
            // A second write creates the .bak from the first good file.
            reg.upsert(DownloadRecord::new("dl-2".into(), req("https://a/2"))).await;
        }
        // Simulate an accidental deletion of the main queue file.
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(backup_path(&path).exists(), "backup should exist");

        // Reopen: should transparently recover from the .bak.
        let reg = Registry::open(path.clone()).await;
        let all = reg.all().await;
        assert!(!all.is_empty(), "records should be recovered from backup");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn pending_actions_roundtrip_across_registries() {
        use crate::types::PendingAction;
        let dir = std::env::temp_dir().join(format!("grab-reg-act-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("queue.json");

        // Realistic sequence: the HOST creates the record first (via enqueue)...
        let host = Registry::open(path.clone()).await;
        host.upsert(DownloadRecord::new("dl-1".into(), req("https://a/1"))).await;

        // ...then the APP (separate instance, same file) requests an action.
        let app = Registry::open(path.clone()).await;
        app.upsert(DownloadRecord::new("dl-1".into(), req("https://a/1"))).await;
        app.request_action("dl-1", PendingAction::Pause).await;

        // The host polls, picks up the action from disk, and clears it.
        let actions = host.take_pending_actions().await;
        assert_eq!(actions, vec![("dl-1".to_string(), PendingAction::Pause)]);

        // Second take yields nothing (cleared).
        let again = host.take_pending_actions().await;
        assert!(again.is_empty());

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn reconciles_orphaned_grabpart() {
        use crate::segment::Segment;
        use crate::state::DownloadState;

        let dir = std::env::temp_dir().join(format!("grab-reg4-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let queue = dir.join("queue.json");

        // An orphaned partial: a .grabpart sidecar with no queue record.
        let target = dir.join("movie.mp4");
        let state = DownloadState {
            url: "https://cdn/movie.mp4".into(),
            content_length: Some(1000),
            etag: None,
            last_modified: None,
            segments: vec![Segment { start: 0, end: 999, completed: 400 }],
        };
        let part = {
            let mut s = target.clone().into_os_string();
            s.push(".grabpart");
            std::path::PathBuf::from(s)
        };
        tokio::fs::write(&part, serde_json::to_vec(&state).unwrap())
            .await
            .unwrap();

        let reg = Registry::open(queue.clone()).await;
        let added = reg.reconcile_orphans(&dir, 1).await;
        assert_eq!(added.len(), 1, "should recover the orphaned partial");

        let rec = reg.all().await.into_iter().next().unwrap();
        assert_eq!(rec.target.as_deref(), Some(target.as_path()));
        assert_eq!(rec.downloaded, 400);
        assert_eq!(rec.total, Some(1000));
        assert_eq!(rec.request.resolved_target.as_deref(), Some(target.as_path()));

        // Running it again must NOT duplicate the now-tracked target.
        let again = reg.reconcile_orphans(&dir, 2).await;
        assert!(again.is_empty(), "already-tracked target must not be re-added");

        tokio::fs::remove_dir_all(&dir).await.ok();
    }
}
