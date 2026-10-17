//! Persistent download state for crash-safe resume.
//!
//! A sidecar file (`<target>.grabpart`) stores the segment layout and how many
//! bytes of each segment have been written. On restart we only resume if the
//! server's validators (size + ETag/Last-Modified) still match, so we never
//! stitch together bytes from two different versions of a file.

use crate::segment::Segment;
use crate::types::ProbeResult;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadState {
    pub url: String,
    pub content_length: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub segments: Vec<Segment>,
}

impl DownloadState {
    pub fn new(probe: &ProbeResult, segments: Vec<Segment>) -> Self {
        Self {
            url: probe.final_url.clone(),
            content_length: probe.content_length,
            etag: probe.etag.clone(),
            last_modified: probe.last_modified.clone(),
            segments,
        }
    }

    /// Total bytes already downloaded across all segments.
    pub fn total_completed(&self) -> u64 {
        self.segments.iter().map(|s| s.completed).sum()
    }

    /// Only resume if the remote file looks identical to the one we started.
    pub fn matches(&self, probe: &ProbeResult) -> bool {
        if self.content_length != probe.content_length {
            return false;
        }
        // If both have an ETag, they must match. If neither does, fall back to
        // Last-Modified when present. Missing validators => allow (best effort).
        match (&self.etag, &probe.etag) {
            (Some(a), Some(b)) => a == b,
            _ => match (&self.last_modified, &probe.last_modified) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            },
        }
    }

    pub async fn load(path: &Path) -> Option<Self> {
        let bytes = tokio::fs::read(path).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub async fn save(&self, path: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec(self).expect("serializable state");
        tokio::fs::write(path, json).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(len: Option<u64>, etag: Option<&str>) -> ProbeResult {
        ProbeResult {
            final_url: "https://ex.com/f".into(),
            content_length: len,
            supports_ranges: true,
            filename: "f".into(),
            etag: etag.map(|s| s.to_string()),
            last_modified: None,
        }
    }

    #[test]
    fn matches_on_same_size_and_etag() {
        let p = probe(Some(100), Some("abc"));
        let s = DownloadState::new(&p, vec![]);
        assert!(s.matches(&p));
    }

    #[test]
    fn rejects_on_size_change() {
        let s = DownloadState::new(&probe(Some(100), Some("abc")), vec![]);
        assert!(!s.matches(&probe(Some(200), Some("abc"))));
    }

    #[test]
    fn rejects_on_etag_change() {
        let s = DownloadState::new(&probe(Some(100), Some("abc")), vec![]);
        assert!(!s.matches(&probe(Some(100), Some("xyz"))));
    }

    #[test]
    fn total_completed_sums_segments() {
        let p = probe(Some(100), None);
        let s = DownloadState::new(
            &p,
            vec![
                Segment { start: 0, end: 49, completed: 20 },
                Segment { start: 50, end: 99, completed: 30 },
            ],
        );
        assert_eq!(s.total_completed(), 50);
    }
}
