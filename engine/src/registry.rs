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

    /// Update the mutable progress/status fields of a record, then persist.
    /// No-ops if the id is unknown.
    pub async fn update(
        &self,
        id: &str,
        status: Status,
        downloaded: u64,
        total: Option<u64>,
        error: Option<String>,
    ) {
        let mut inner = self.inner.lock().await;
        if let Some(r) = inner.records.get_mut(id) {
            r.status = status;
            r.downloaded = downloaded;
            if total.is_some() {
                r.total = total;
            }
            r.error = error;
            let _ = save(&inner.path, &inner.records).await;
        }
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

async fn load(path: &Path) -> Option<HashMap<String, DownloadRecord>> {
    let bytes = tokio::fs::read(path).await.ok()?;
    let records: Vec<DownloadRecord> = serde_json::from_slice(&bytes).ok()?;
    Some(records.into_iter().map(|r| (r.id.clone(), r)).collect())
}

/// Atomically persist the registry: write to `<path>.tmp` then rename over the
/// real file so readers never see a half-written file.
async fn save(path: &Path, records: &HashMap<String, DownloadRecord>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
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
            reg.update("dl-1", Status::Completed, 100, Some(100), None).await;
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
}
