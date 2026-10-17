//! Orchestrates downloading an HLS/DASH stream: fetch the manifest, resolve the
//! ordered segment list, download segments concurrently (bounded), and
//! concatenate them in order into one output file.
//!
//! Progress is reported against segment *count* (we usually don't know total
//! bytes up front for streaming media), so `total` in the emitted `Progress`
//! reflects the number of segments and `downloaded` the number completed. A
//! byte counter is also tracked and reported where available.

use crate::download::Handle;
use crate::media::{self, HlsResolved, MediaPlan};
use crate::types::{DownloadKind, DownloadRequest, Progress, Settings, Status};
use anyhow::{Context, Result};
use futures::stream::{FuturesOrdered, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, Semaphore};

pub async fn download_media(
    client: Client,
    id: String,
    req: DownloadRequest,
    settings: Settings,
    handle: Handle,
    tx: mpsc::UnboundedSender<Progress>,
    target_sink: Option<crate::download::TargetSink>,
) -> Result<PathBuf> {
    let emit = |status: Status, done: u64, total: Option<u64>, speed: u64, conns: usize, err: Option<String>| {
        let _ = tx.send(Progress {
            id: id.clone(),
            status,
            downloaded: done,
            total,
            speed,
            connections: conns,
            error: err,
        });
    };

    emit(Status::Probing, 0, None, 0, 0, None);

    let headers = build_headers(&req)?;
    let kind = req.effective_kind();

    // 1. Resolve the manifest into an ordered media plan.
    let (plan, final_url) = match resolve_plan(&client, &headers, &req.url, kind).await {
        Ok(v) => v,
        Err(e) => {
            emit(Status::Failed, 0, None, 0, 0, Some(e.to_string()));
            return Err(e);
        }
    };

    // 2. Determine the output path (reuse the exact path when resuming).
    let target = if let Some(t) = req.resolved_target.clone() {
        if let Some(parent) = t.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        t
    } else {
        let dir = req
            .save_dir
            .clone()
            .unwrap_or_else(|| settings.download_dir.clone());
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating download dir {}", dir.display()))?;
        let filename = media_filename(&req, &final_url, plan.suggested_ext);
        unique_path(&dir, &filename).await
    };
    if let Some(sink) = &target_sink {
        sink(target.clone());
    }

    let urls = plan.all_urls();
    let total_segments = urls.len() as u64;
    let completed = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    emit(Status::Downloading, 0, Some(total_segments), 0, 0, None);

    // 3. Download all segments concurrently into memory-bounded temp buffers,
    //    but WRITE them in strict order so the output is a valid stream.
    //    FuturesOrdered yields results in submission order regardless of which
    //    finishes first, giving us correct concatenation with concurrency.
    let max_parallel = settings.max_connections_per_file.max(1);
    let sem = Arc::new(Semaphore::new(max_parallel));

    let mut file = tokio::fs::File::create(&target)
        .await
        .with_context(|| format!("creating {}", target.display()))?;

    let mut ordered = FuturesOrdered::new();
    for url in urls.iter() {
        let client = client.clone();
        let headers = headers.clone();
        let sem = sem.clone();
        let url = url.to_string();
        let handle = handle.clone();
        ordered.push_back(async move {
            let _permit = sem.acquire().await.expect("semaphore open");
            if handle.is_canceled() {
                anyhow::bail!("canceled");
            }
            fetch_bytes(&client, &headers, &url).await
        });
    }

    // Progress reporter (1s cadence) over segment counts + bytes.
    let reporter = {
        let tx = tx.clone();
        let id = id.clone();
        let completed = completed.clone();
        let bytes = bytes.clone();
        tokio::spawn(async move {
            let mut last = 0u64;
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                ticker.tick().await;
                let now = bytes.load(Ordering::SeqCst);
                let speed = now.saturating_sub(last);
                last = now;
                let _ = tx.send(Progress {
                    id: id.clone(),
                    status: Status::Downloading,
                    downloaded: completed.load(Ordering::SeqCst),
                    total: Some(total_segments),
                    speed,
                    connections: 0,
                    error: None,
                });
            }
        })
    };

    let mut idx = 0u64;
    while let Some(result) = ordered.next().await {
        if handle.is_paused() {
            reporter.abort();
            file.flush().await.ok();
            emit(
                Status::Paused,
                completed.load(Ordering::SeqCst),
                Some(total_segments),
                0,
                0,
                None,
            );
            // Partial media file is left in place; a future resume would need
            // segment-level checkpointing (tracked as a known limitation).
            anyhow::bail!("paused");
        }
        match result {
            Ok(data) => {
                if let Err(e) = file.write_all(&data).await {
                    reporter.abort();
                    let _ = tokio::fs::remove_file(&target).await;
                    emit(Status::Failed, idx, Some(total_segments), 0, 0, Some(e.to_string()));
                    return Err(e.into());
                }
                bytes.fetch_add(data.len() as u64, Ordering::SeqCst);
                completed.fetch_add(1, Ordering::SeqCst);
                idx += 1;
            }
            Err(e) => {
                reporter.abort();
                if handle.is_canceled() {
                    let _ = tokio::fs::remove_file(&target).await;
                    emit(Status::Canceled, idx, Some(total_segments), 0, 0, None);
                    return Err(e);
                }
                // One bad segment fails the whole media download (a gap would
                // corrupt playback).
                let _ = tokio::fs::remove_file(&target).await;
                emit(
                    Status::Failed,
                    idx,
                    Some(total_segments),
                    0,
                    0,
                    Some(format!("segment download failed: {e}")),
                );
                return Err(e);
            }
        }
    }

    reporter.abort();
    file.flush().await.ok();
    emit(
        Status::Completed,
        total_segments,
        Some(total_segments),
        0,
        0,
        None,
    );
    Ok(target)
}

/// Fetch a manifest, following an HLS master playlist to its best variant, and
/// return the resolved plan plus the final manifest URL used.
async fn resolve_plan(
    client: &Client,
    headers: &HeaderMap,
    url: &str,
    kind: DownloadKind,
) -> Result<(MediaPlan, String)> {
    let body = fetch_text(client, headers, url).await?;

    // If the URL extension was ambiguous, sniff the body.
    let kind = match kind {
        DownloadKind::File => media::sniff_kind(&body).unwrap_or(DownloadKind::File),
        k => k,
    };

    match kind {
        DownloadKind::Hls => match media::resolve_hls(&body, url)? {
            HlsResolved::Plan(plan) => Ok((plan, url.to_string())),
            HlsResolved::FollowVariant(variant_url) => {
                let variant_body = fetch_text(client, headers, &variant_url).await?;
                let plan = media::parse_hls_media(&variant_body, &variant_url)?;
                Ok((plan, variant_url))
            }
        },
        DownloadKind::Dash => {
            let plan = media::parse_dash(&body, url)?;
            Ok((plan, url.to_string()))
        }
        DownloadKind::File => {
            anyhow::bail!("URL does not look like an HLS or DASH manifest")
        }
    }
}

async fn fetch_text(client: &Client, headers: &HeaderMap, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .headers(headers.clone())
        .send()
        .await
        .with_context(|| format!("fetching manifest {url}"))?
        .error_for_status()
        .with_context(|| format!("manifest {url} returned an error status"))?;
    resp.text().await.context("reading manifest body")
}

async fn fetch_bytes(client: &Client, headers: &HeaderMap, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .headers(headers.clone())
        .send()
        .await
        .with_context(|| format!("fetching segment {url}"))?
        .error_for_status()
        .with_context(|| format!("segment {url} returned an error status"))?;
    Ok(resp.bytes().await.context("reading segment body")?.to_vec())
}

fn build_headers(req: &DownloadRequest) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (k, v) in &req.headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid header name: {k}"))?;
        let value =
            HeaderValue::from_str(v).with_context(|| format!("invalid header value for {k}"))?;
        map.insert(name, value);
    }
    Ok(map)
}

fn media_filename(req: &DownloadRequest, url: &str, ext: &str) -> String {
    if let Some(name) = &req.filename {
        return ensure_ext(name, ext);
    }
    // Derive a base name from the manifest URL path.
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let stem = path
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or("video")
        .split('.')
        .next()
        .unwrap_or("video");
    let stem = if stem.is_empty() { "video" } else { stem };
    format!("{stem}.{ext}")
}

fn ensure_ext(name: &str, ext: &str) -> String {
    if name.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    }
}

async fn unique_path(dir: &std::path::Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), Some(e.to_string())),
        _ => (filename.to_string(), None),
    };
    for i in 1..10_000 {
        let name = match &ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        let candidate = dir.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(filename)
}
