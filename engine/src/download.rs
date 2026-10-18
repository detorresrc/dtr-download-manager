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
use crate::ratelimit::RateLimiter;
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
    // Standalone callers get a limiter derived from the settings' speed_limit.
    let limiter = Arc::new(RateLimiter::new(settings.speed_limit));
    download_full(client, id, req, settings, handle, tx, None, limiter).await
}

/// Type of the optional callback invoked once with the resolved output path, so
/// a caller (the Manager) can persist it for deterministic resume.
pub type TargetSink = std::sync::Arc<dyn Fn(PathBuf) + Send + Sync>;

/// Like [`download`], but also reports the resolved target path to `target_sink`.
pub async fn download_with_target_sink(
    client: Client,
    id: String,
    req: DownloadRequest,
    settings: Settings,
    handle: Handle,
    tx: mpsc::UnboundedSender<Progress>,
    target_sink: Option<TargetSink>,
) -> Result<PathBuf> {
    let limiter = Arc::new(RateLimiter::new(settings.speed_limit));
    download_full(client, id, req, settings, handle, tx, target_sink, limiter).await
}

/// The full download entry point, with an explicit shared rate limiter so the
/// `Manager` can enforce a single global bandwidth cap across all downloads.
#[allow(clippy::too_many_arguments)]
pub async fn download_full(
    client: Client,
    id: String,
    req: DownloadRequest,
    settings: Settings,
    handle: Handle,
    tx: mpsc::UnboundedSender<Progress>,
    target_sink: Option<TargetSink>,
    limiter: Arc<RateLimiter>,
) -> Result<PathBuf> {
    let emit = |status: Status, downloaded: u64, total: Option<u64>, speed: u64, conns: usize, err: Option<String>| {
        let _ = tx.send(Progress {
            id: id.clone(),
            status,
            downloaded,
            total,
            speed,
            eta_secs: Progress::estimate_eta(downloaded, total, speed),
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
            client, id, req, settings, handle, tx, target_sink, limiter,
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
        // An explicit per-request save_dir always wins. Otherwise apply the
        // category auto-foldering rules to the base download dir.
        let dir = match &req.save_dir {
            Some(d) => d.clone(),
            None => settings.category_dir_for(&probe_result.filename),
        };
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
            settings.effective_connections(),
            &limiter,
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
            &limiter,
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
                &limiter,
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
    limiter: &Arc<RateLimiter>,
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
                limiter.clone(),
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
    base_headers: HeaderMap,
    idx: usize,
    seg: Segment,
    file: Arc<tokio::sync::Mutex<File>>,
    downloaded: Arc<AtomicU64>,
    progress: Arc<tokio::sync::Mutex<SegmentProgress>>,
    handle: Handle,
    limiter: Arc<RateLimiter>,
) -> Result<()> {
    // Flaky hosts drop the connection mid-segment ("reading chunk"). Retry and
    // RESUME from wherever we got to: re-request `Range: bytes={offset}-{end}`.
    // `offset` persists across attempts so each retry picks up exactly where the
    // previous one died — no re-downloading and no gaps.
    const MAX_ATTEMPTS: usize = 10;
    let limit = seg.end; // never write past this segment's last byte
    let mut offset = seg.write_offset();

    for attempt in 0..MAX_ATTEMPTS {
        if handle.is_canceled() {
            anyhow::bail!("canceled");
        }
        if handle.is_paused() {
            anyhow::bail!("paused");
        }
        if offset > limit {
            return Ok(()); // already complete
        }

        let mut headers = base_headers.clone();
        headers.insert(
            RANGE,
            HeaderValue::from_str(&format!("bytes={offset}-{limit}")).expect("valid range"),
        );
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));

        let send = client.get(&url).headers(headers).send().await;
        let resp = match send.and_then(|r| r.error_for_status()) {
            Ok(r) => r,
            Err(e) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    tracing::warn!("segment {idx} request failed (attempt {}), retrying: {e}", attempt + 1);
                    backoff(attempt).await;
                    continue;
                }
                return Err(anyhow::Error::new(e).context(format!("segment {idx} request failed")));
            }
        };

        // A server may ignore Range and reply 200 with the FULL body — unsafe to
        // write at a segment offset. Only 206 is valid for segmented writes;
        // bail so the orchestrator falls back to a single stream. (Only treat
        // the FIRST attempt's 200 as a range-ignored signal; mid-download we've
        // already proven ranges work, so a non-206 there is just a bad retry.)
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            if offset == seg.write_offset() {
                return Err(SegmentError::RangeIgnored.into());
            }
            if attempt + 1 < MAX_ATTEMPTS {
                backoff(attempt).await;
                continue;
            }
            anyhow::bail!("segment {idx}: server stopped honoring Range mid-download");
        }

        let mut resp = resp;
        let mut dropped = false;
        loop {
            if handle.is_canceled() {
                anyhow::bail!("canceled");
            }
            if handle.is_paused() {
                anyhow::bail!("paused");
            }
            if offset > limit {
                break;
            }
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    let remaining = limit - offset + 1;
                    let take = (chunk.len() as u64).min(remaining) as usize;
                    limiter.acquire(take as u64).await;
                    {
                        let mut f = file.lock().await;
                        f.seek(SeekFrom::Start(offset)).await?;
                        f.write_all(&chunk[..take]).await?;
                    }
                    let n = take as u64;
                    offset += n;
                    downloaded.fetch_add(n, Ordering::SeqCst);
                    progress.lock().await.add(idx, n);
                }
                Ok(None) => break,          // stream ended for this attempt
                Err(_) => {
                    dropped = true;
                    break;                   // connection dropped → retry from offset
                }
            }
        }

        if offset > limit {
            return Ok(()); // segment fully downloaded
        }
        // Not done: either a clean-but-short end or a dropped connection. Retry
        // from the current offset (resume) unless we're out of attempts.
        if attempt + 1 < MAX_ATTEMPTS {
            tracing::warn!(
                "segment {idx} {} at {offset}/{limit} (attempt {}), resuming…",
                if dropped { "connection dropped" } else { "ended early" },
                attempt + 1
            );
            backoff(attempt).await;
            continue;
        }
        anyhow::bail!(
            "segment {idx} incomplete after {MAX_ATTEMPTS} attempts: reached {offset}, needed {limit}"
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
    limiter: &Arc<RateLimiter>,
) -> Result<()> {
    // Flaky file-hosts often drop the connection mid-stream ("reading chunk").
    // Retry up to N times; when the server supports ranges, RESUME from the
    // bytes we already have (Range header) instead of restarting from zero.
    const MAX_ATTEMPTS: usize = 5;
    let base_headers = build_headers(req)?;

    for attempt in 0..MAX_ATTEMPTS {
        if handle.is_canceled() {
            anyhow::bail!("canceled");
        }
        if handle.is_paused() {
            anyhow::bail!("paused");
        }

        let start = downloaded.load(Ordering::SeqCst);
        let mut headers = base_headers.clone();
        headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
        // Resume from `start` if possible (and we've made progress).
        let resuming = start > 0 && probe.supports_ranges;
        if resuming {
            headers.insert(
                RANGE,
                HeaderValue::from_str(&format!("bytes={start}-")).expect("valid range"),
            );
        }

        let send_result = client.get(&probe.final_url).headers(headers).send().await;
        let resp = match send_result.and_then(|r| r.error_for_status()) {
            Ok(r) => r,
            Err(e) => {
                if attempt + 1 < MAX_ATTEMPTS {
                    backoff(attempt).await;
                    continue;
                }
                return Err(anyhow::Error::new(e).context("single-stream request failed"));
            }
        };

        // Position the file: if we're resuming via range the server returns the
        // tail from `start`, so seek there; otherwise (re)start at 0.
        let write_from = if resuming && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            start
        } else {
            // Server ignored the range (or first attempt): rewrite from scratch.
            downloaded.store(0, Ordering::SeqCst);
            0
        };
        {
            let mut f = file.lock().await;
            f.seek(SeekFrom::Start(write_from)).await?;
        }

        let mut resp = resp;
        let stream_err = loop {
            if handle.is_canceled() {
                anyhow::bail!("canceled");
            }
            if handle.is_paused() {
                anyhow::bail!("paused");
            }
            match resp.chunk().await {
                Ok(Some(chunk)) => {
                    limiter.acquire(chunk.len() as u64).await;
                    {
                        let mut f = file.lock().await;
                        f.write_all(&chunk).await?;
                    }
                    downloaded.fetch_add(chunk.len() as u64, Ordering::SeqCst);
                }
                Ok(None) => return Ok(()),     // finished cleanly
                Err(e) => break e,             // connection dropped → retry
            }
        };

        // Stream broke. Retry (resuming) unless we're out of attempts.
        if attempt + 1 < MAX_ATTEMPTS {
            tracing::warn!(
                "stream dropped at {} bytes (attempt {}/{}), retrying…",
                downloaded.load(Ordering::SeqCst),
                attempt + 1,
                MAX_ATTEMPTS
            );
            backoff(attempt).await;
            continue;
        }
        return Err(anyhow::Error::new(stream_err).context("reading chunk (gave up after retries)"));
    }
    Ok(())
}

/// Exponential-ish backoff between retry attempts (0.5s, 1s, 2s, 4s…).
async fn backoff(attempt: usize) {
    let secs = (1u64 << attempt.min(3)) as f64 * 0.5;
    tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
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

/// Emit a progress event ~once per second with a smoothed speed and an ETA.
///
/// Raw per-second deltas are spiky (segments start/finish, buffers flush), which
/// makes both the displayed speed and a naive ETA jump around. We smooth the
/// speed with an exponential moving average, then derive the ETA from the
/// smoothed speed so the "time remaining" is stable and useful.
fn spawn_reporter(
    id: String,
    tx: mpsc::UnboundedSender<Progress>,
    downloaded: Arc<AtomicU64>,
    total: Option<u64>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut last = downloaded.load(Ordering::SeqCst);
        // EMA smoothing factor: 0.3 reacts reasonably fast but still steadies.
        const ALPHA: f64 = 0.3;
        let mut avg_speed: f64 = 0.0;
        let mut seeded = false;
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let now = downloaded.load(Ordering::SeqCst);
            let sample = now.saturating_sub(last) as f64; // bytes in the last ~1s
            last = now;

            // Seed the average with the first real sample so early ETAs aren't wild.
            if !seeded {
                avg_speed = sample;
                seeded = true;
            } else {
                avg_speed = ALPHA * sample + (1.0 - ALPHA) * avg_speed;
            }
            let speed = avg_speed.round() as u64;
            let eta = Progress::estimate_eta(now, total, speed);

            let _ = tx.send(Progress {
                id: id.clone(),
                status: Status::Downloading,
                downloaded: now,
                total,
                speed,
                eta_secs: eta,
                connections: 0,
                error: None,
            });
        }
    })
}

/// Produce a non-colliding path: `name.ext`, then `name (1).ext`, etc.
///
/// Reserving is ATOMIC: we create the file with `create_new` (O_EXCL), which
/// fails if it already exists. This closes a race where two concurrent
/// downloads with the same name both pass an `exists()` check and then clobber
/// each other. The created (empty) file is reused as the real output.
pub(crate) async fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    use tokio::fs::OpenOptions;
    // Try the plain name first, then numbered variants, claiming each atomically.
    let try_claim = |path: PathBuf| async move {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(_) => Some(path),         // we won the race; file now exists (empty)
            Err(_) => None,              // taken (or unwritable); try the next name
        }
    };

    if let Some(p) = try_claim(dir.join(filename)).await {
        return p;
    }
    let (stem, ext) = split_ext(filename);
    for i in 1..100_000 {
        let name = match &ext {
            Some(e) => format!("{stem} ({i}).{e}"),
            None => format!("{stem} ({i})"),
        };
        if let Some(p) = try_claim(dir.join(name)).await {
            return p;
        }
    }
    // Extremely unlikely fallback.
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
