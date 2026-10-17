//! Downloading a single file: either segmented (multi-connection) when the
//! server supports byte ranges, or a single stream as a fallback.
//!
//! Correctness guarantees:
//!   - Every segment writes to its own absolute file offset, so concurrent
//!     writers never collide.
//!   - The file is pre-allocated to the final size for segmented downloads.
//!   - Progress (`completed` per segment) is persisted so a paused or crashed
//!     download can resume without re-fetching bytes.

use crate::probe::probe;
use crate::segment::{plan, Segment};
use crate::state::DownloadState;
use crate::types::{DownloadRequest, Progress, Settings, Status};
use anyhow::{Context, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, RANGE};
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::mpsc;

/// Errors specific to segment fetching that the orchestrator inspects to decide
/// whether to fall back to a single connection.
#[derive(Debug, thiserror::Error)]
enum SegmentError {
    #[error("server ignored Range header (responded 200 instead of 206)")]
    RangeIgnored,
}

/// Control signals a caller can send to a running download.
#[derive(Clone)]
pub struct Handle {
    pause: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl Handle {
    pub fn new() -> Self {
        Self {
            pause: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn pause(&self) {
        self.pause.store(true, Ordering::SeqCst);
    }
    pub fn resume(&self) {
        self.pause.store(false, Ordering::SeqCst);
    }
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
    /// Whether a pause has been requested (public so other download strategies
    /// like the media downloader can honor it).
    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }
    /// Whether a cancel has been requested.
    pub fn is_canceled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

impl Default for Handle {
    fn default() -> Self {
        Self::new()
    }
}

/// Download `req` to disk per `settings`, reporting progress on `tx`.
///
/// `id` identifies this download in progress events. `handle` allows
/// pause/cancel. Returns the final path on success.
pub async fn download(
    client: Client,
    id: String,
    req: DownloadRequest,
    settings: Settings,
    handle: Handle,
    tx: mpsc::UnboundedSender<Progress>,
) -> Result<PathBuf> {
    download_with_target_sink(client, id, req, settings, handle, tx, None).await
}

/// Type of the optional callback invoked once with the resolved output path, so
/// a caller (the Manager) can persist it for deterministic resume.
pub type TargetSink = std::sync::Arc<dyn Fn(PathBuf) + Send + Sync>;

/// Like [`download`], but also reports the resolved target path to `target_sink`
/// as soon as it's known (before bytes are written).
pub async fn download_with_target_sink(
    client: Client,
    id: String,
    req: DownloadRequest,
    settings: Settings,
    handle: Handle,
    tx: mpsc::UnboundedSender<Progress>,
    target_sink: Option<TargetSink>,
) -> Result<PathBuf> {
    let emit = |status: Status, downloaded: u64, total: Option<u64>, speed: u64, conns: usize, err: Option<String>| {
        let _ = tx.send(Progress {
            id: id.clone(),
            status,
            downloaded,
            total,
            speed,
            connections: conns,
            error: err,
        });
    };

    // Streaming media (HLS/DASH) follows an entirely different path: fetch and
    // parse a manifest, then download+concatenate its segments.
    if matches!(
        req.effective_kind(),
        crate::types::DownloadKind::Hls | crate::types::DownloadKind::Dash
    ) {
        return crate::media_download::download_media(
            client, id, req, settings, handle, tx, target_sink,
        )
        .await;
    }

    emit(Status::Probing, 0, None, 0, 0, None);
    let probe_result = probe(&client, &req).await?;

    // Resolve the output path. When resuming, the caller passes the exact path
    // via `resolved_target` so we write to the SAME file; otherwise we resolve
    // dir + filename and avoid collisions.
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
        unique_path(&dir, &probe_result.filename).await
    };
    // Report the resolved path so it can be persisted for resume.
    if let Some(sink) = &target_sink {
        sink(target.clone());
    }

    let state_path = state_path_for(&target);

    // Try to resume from a prior partial download of the same URL/size.
    let mut state = DownloadState::load(&state_path)
        .await
        .filter(|s| s.matches(&probe_result))
        .unwrap_or_else(|| {
            let segments = if probe_result.can_segment() {
                plan(
                    probe_result.content_length.unwrap(),
                    settings.max_connections_per_file,
                    settings.min_split_size,
                )
            } else {
                // Single stream: one open-ended segment.
                vec![Segment { start: 0, end: 0, completed: 0 }]
            };
            DownloadState::new(&probe_result, segments)
        });

    let total = probe_result.content_length;
    let downloaded = Arc::new(AtomicU64::new(state.total_completed()));

    // Pre-create / size the output file.
    // Open (or create) without truncating: resuming must preserve bytes already
    // written on a prior run.
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(&target)
        .await
        .with_context(|| format!("opening output file {}", target.display()))?;
    if let Some(len) = total {
        file.set_len(len).await.ok();
    }
    let file = Arc::new(tokio::sync::Mutex::new(file));

    emit(
        Status::Downloading,
        downloaded.load(Ordering::SeqCst),
        total,
        0,
        0,
        None,
    );

    // Spawn a progress reporter that samples the counter once per second.
    let reporter = spawn_reporter(id.clone(), tx.clone(), downloaded.clone(), total);

    let mut result = if probe_result.can_segment() {
        run_segmented(
            &client,
            &req,
            &probe_result,
            &mut state,
            &state_path,
            file.clone(),
            downloaded.clone(),
            &handle,
            settings.max_connections_per_file,
        )
        .await
    } else {
        run_single_stream(
            &client,
            &req,
            &probe_result,
            file.clone(),
            downloaded.clone(),
            &handle,
        )
        .await
    };

    // Runtime fallback: the probe said ranges were supported, but a segment got
    // a 200 instead of 206. Reset progress and redo as a single connection.
    if let Err(e) = &result {
        if e.downcast_ref::<SegmentError>().is_some() {
            tracing::warn!("falling back to single connection: {e}");
            downloaded.store(0, Ordering::SeqCst);
            {
                let f = file.lock().await;
                f.set_len(0).await.ok();
                if let Some(len) = total {
                    f.set_len(len).await.ok();
                }
            }
            result = run_single_stream(
                &client,
                &req,
                &probe_result,
                file.clone(),
                downloaded.clone(),
                &handle,
            )
            .await;
        }
    }

    reporter.abort();

    match result {
        Ok(()) => {
            file.lock().await.flush().await.ok();
            // Success: discard the resume sidecar.
            let _ = tokio::fs::remove_file(&state_path).await;
            emit(
                Status::Completed,
                downloaded.load(Ordering::SeqCst),
                total,
                0,
                0,
                None,
            );
            Ok(target)
        }
        Err(e) if handle.is_canceled() => {
            file.lock().await.flush().await.ok();
            let _ = tokio::fs::remove_file(&target).await;
            let _ = tokio::fs::remove_file(&state_path).await;
            emit(Status::Canceled, downloaded.load(Ordering::SeqCst), total, 0, 0, None);
            Err(e)
        }
        Err(e) if handle.is_paused() => {
            file.lock().await.flush().await.ok();
            state.save(&state_path).await.ok();
            emit(Status::Paused, downloaded.load(Ordering::SeqCst), total, 0, 0, None);
            Err(e)
        }
        Err(e) => {
            file.lock().await.flush().await.ok();
            state.save(&state_path).await.ok();
            emit(
                Status::Failed,
                downloaded.load(Ordering::SeqCst),
                total,
                0,
                0,
                Some(e.to_string()),
            );
            Err(e)
        }
    }
}

/// Run all unfinished segments, bounded to `max_parallel` at a time.
#[allow(clippy::too_many_arguments)]
async fn run_segmented(
    client: &Client,
    req: &DownloadRequest,
    probe: &crate::types::ProbeResult,
    state: &mut DownloadState,
    state_path: &Path,
    file: Arc<tokio::sync::Mutex<File>>,
    downloaded: Arc<AtomicU64>,
    handle: &Handle,
    max_parallel: usize,
) -> Result<()> {
    let base_headers = build_headers(req)?;
    let mut in_flight = FuturesUnordered::new();
    let mut pending: Vec<usize> = (0..state.segments.len())
        .filter(|&i| !state.segments[i].is_done())
        .collect();
    pending.reverse(); // pop from the front order

    let progress = Arc::new(tokio::sync::Mutex::new(SegmentProgress::new(state.segments.clone())));

    loop {
        if handle.is_canceled() {
            anyhow::bail!("canceled");
        }
        if handle.is_paused() {
            // Persist and stop; caller will mark as paused.
            sync_state(state, &progress).await;
            state.save(state_path).await.ok();
            anyhow::bail!("paused");
        }

        while in_flight.len() < max_parallel {
            let Some(idx) = pending.pop() else { break };
            let seg = { progress.lock().await.segments[idx] };
            in_flight.push(fetch_segment(
                client.clone(),
                probe.final_url.clone(),
                base_headers.clone(),
                idx,
                seg,
                file.clone(),
                downloaded.clone(),
                progress.clone(),
                handle.clone(),
            ));
        }

        if in_flight.is_empty() {
            break;
        }

        if let Some(res) = in_flight.next().await {
            // Checkpoint progress to disk after each segment finishes.
            sync_state(state, &progress).await;
            state.save(state_path).await.ok();
            res?;
        }
    }

    sync_state(state, &progress).await;
    Ok(())
}

/// Fetch (the remaining bytes of) a single segment, writing at its offset.
#[allow(clippy::too_many_arguments)]
async fn fetch_segment(
    client: Client,
    url: String,
    mut headers: HeaderMap,
    idx: usize,
    seg: Segment,
    file: Arc<tokio::sync::Mutex<File>>,
    downloaded: Arc<AtomicU64>,
    progress: Arc<tokio::sync::Mutex<SegmentProgress>>,
    handle: Handle,
) -> Result<()> {
    headers.insert(
        RANGE,
        HeaderValue::from_str(&seg.range_header()).expect("valid range header"),
    );
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let resp = client
        .get(&url)
        .headers(headers)
        .send()
        .await
        .with_context(|| format!("segment {idx} request failed"))?
        .error_for_status()
        .with_context(|| format!("segment {idx} bad status"))?;

    // CRITICAL: a server may ignore our Range header and reply 200 with the
    // FULL body. If we streamed that into a segment's offset we'd corrupt the
    // file (overlapping writes, wrong size). Only 206 Partial Content is safe
    // for segmented writes; anything else aborts so the caller can fall back to
    // a single stream.
    if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(SegmentError::RangeIgnored.into());
    }

    let mut resp = resp;
    let mut offset = seg.write_offset();
    // Never write past this segment's boundary, even if the server over-sends.
    let limit = seg.end;
    while let Some(chunk) = resp.chunk().await.context("reading chunk")? {
        if handle.is_canceled() {
            anyhow::bail!("canceled");
        }
        if handle.is_paused() {
            anyhow::bail!("paused");
        }
        if offset > limit {
            break;
        }
        // Clamp the chunk so it cannot spill into the next segment's bytes.
        let remaining = limit - offset + 1;
        let take = (chunk.len() as u64).min(remaining) as usize;
        let slice = &chunk[..take];
        {
            let mut f = file.lock().await;
            f.seek(SeekFrom::Start(offset)).await?;
            f.write_all(slice).await?;
        }
        let n = take as u64;
        offset += n;
        downloaded.fetch_add(n, Ordering::SeqCst);
        progress.lock().await.add(idx, n);
    }
    // The stream ended. If we didn't reach the end of the segment, the server
    // closed the connection early (short read). Treating this as success would
    // leave an unwritten gap in the file, so fail loudly — the download will be
    // marked failed and can be resumed (the bytes we did write are checkpointed).
    if offset <= limit {
        anyhow::bail!(
            "segment {idx} ended early at offset {offset}, expected through {limit} \
             (server closed connection; {} bytes short)",
            limit - offset + 1
        );
    }
    Ok(())
}

/// Fallback path: single connection, streamed sequentially from offset 0.
async fn run_single_stream(
    client: &Client,
    req: &DownloadRequest,
    probe: &crate::types::ProbeResult,
    file: Arc<tokio::sync::Mutex<File>>,
    downloaded: Arc<AtomicU64>,
    handle: &Handle,
) -> Result<()> {
    let mut headers = build_headers(req)?;
    headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    let mut resp = client
        .get(&probe.final_url)
        .headers(headers)
        .send()
        .await
        .context("single-stream request failed")?
        .error_for_status()
        .context("single-stream bad status")?;

    {
        let mut f = file.lock().await;
        f.seek(SeekFrom::Start(0)).await?;
    }
    while let Some(chunk) = resp.chunk().await.context("reading chunk")? {
        if handle.is_canceled() {
            anyhow::bail!("canceled");
        }
        if handle.is_paused() {
            anyhow::bail!("paused");
        }
        {
            let mut f = file.lock().await;
            f.write_all(&chunk).await?;
        }
        downloaded.fetch_add(chunk.len() as u64, Ordering::SeqCst);
    }
    Ok(())
}

/// Tracks per-segment completed bytes during a run, mirrored back into state.
struct SegmentProgress {
    segments: Vec<Segment>,
}

impl SegmentProgress {
    fn new(segments: Vec<Segment>) -> Self {
        Self { segments }
    }
    fn add(&mut self, idx: usize, n: u64) {
        self.segments[idx].completed += n;
    }
}

async fn sync_state(state: &mut DownloadState, progress: &Arc<tokio::sync::Mutex<SegmentProgress>>) {
    let p = progress.lock().await;
    state.segments = p.segments.clone();
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

/// Emit a progress event ~once per second with a computed speed.
fn spawn_reporter(
    id: String,
    tx: mpsc::UnboundedSender<Progress>,
    downloaded: Arc<AtomicU64>,
    total: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = downloaded.load(Ordering::SeqCst);
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let now = downloaded.load(Ordering::SeqCst);
            let speed = now.saturating_sub(last);
            last = now;
            let _ = tx.send(Progress {
                id: id.clone(),
                status: Status::Downloading,
                downloaded: now,
                total,
                speed,
                connections: 0,
                error: None,
            });
        }
    })
}

/// Produce a non-colliding path: `name.ext`, then `name (1).ext`, etc.
async fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = split_ext(filename);
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

fn split_ext(filename: &str) -> (String, Option<String>) {
    match filename.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), Some(ext.to_string())),
        _ => (filename.to_string(), None),
    }
}

fn state_path_for(target: &Path) -> PathBuf {
    let mut s = target.as_os_str().to_os_string();
    s.push(".grabpart");
    PathBuf::from(s)
}
