//! A shared token-bucket rate limiter for throttling download bandwidth.
//!
//! One `RateLimiter` is shared (via `Arc`) across every active segment/stream
//! in the engine, so the configured limit is a TRUE global cap regardless of
//! how many connections or files are running at once.
//!
//! Design: a classic token bucket. Tokens (= bytes) refill continuously at
//! `rate` bytes/sec up to a small burst capacity. Before writing a chunk, a
//! worker calls `acquire(n)`, which grants what it can immediately and sleeps
//! for the time needed to accrue the remainder. A `rate` of 0 / unlimited makes
//! `acquire` a no-op, so there's zero overhead when throttling is off.

use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    inner: Mutex<Inner>,
}

struct Inner {
    /// Bytes per second. `None` = unlimited.
    rate: Option<u64>,
    /// Currently available tokens (bytes).
    tokens: f64,
    /// Max tokens we let accumulate (burst), to smooth bursty writers.
    capacity: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// Create a limiter. `rate` is bytes/sec; `None` or `Some(0)` = unlimited.
    pub fn new(rate: Option<u64>) -> Self {
        let rate = rate.filter(|r| *r > 0);
        // Allow up to ~0.5s worth of burst, min 64 KiB, so small chunks aren't
        // starved and speed stays close to the target without being spiky.
        let capacity = rate
            .map(|r| (r as f64 * 0.5).max(64.0 * 1024.0))
            .unwrap_or(0.0);
        Self {
            inner: Mutex::new(Inner {
                rate,
                tokens: capacity, // start full so the first chunk isn't delayed
                capacity,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Change the limit at runtime (e.g. the user edits the setting).
    pub fn set_rate(&self, rate: Option<u64>) {
        let rate = rate.filter(|r| *r > 0);
        let mut inner = self.inner.lock().unwrap();
        inner.rate = rate;
        inner.capacity = rate
            .map(|r| (r as f64 * 0.5).max(64.0 * 1024.0))
            .unwrap_or(0.0);
        inner.tokens = inner.tokens.min(inner.capacity);
        inner.last_refill = Instant::now();
    }

    /// Block until `n` bytes may be sent. Returns immediately when unlimited.
    /// Splits large requests so one huge chunk can't monopolize the bucket.
    pub async fn acquire(&self, n: u64) {
        let mut remaining = n;
        loop {
            let wait = {
                let mut inner = self.inner.lock().unwrap();
                let Some(rate) = inner.rate else {
                    return; // unlimited
                };
                let rate = rate as f64;

                // Refill based on elapsed time.
                let now = Instant::now();
                let elapsed = now.duration_since(inner.last_refill).as_secs_f64();
                inner.last_refill = now;
                inner.tokens = (inner.tokens + elapsed * rate).min(inner.capacity);

                if inner.tokens >= remaining as f64 {
                    inner.tokens -= remaining as f64;
                    return;
                }

                // Consume what's available; compute how long to wait for the rest.
                let still_needed = remaining as f64 - inner.tokens;
                remaining = still_needed as u64;
                inner.tokens = 0.0;
                Duration::from_secs_f64((still_needed / rate).min(1.0))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn unlimited_is_instant() {
        let rl = RateLimiter::new(None);
        let start = Instant::now();
        rl.acquire(10 * 1024 * 1024).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn throttles_to_approximately_the_rate() {
        // 1 MiB/s limit; request ~2 MiB beyond the initial burst.
        let rate = 1024 * 1024u64;
        let rl = RateLimiter::new(Some(rate));
        let start = Instant::now();
        // Drain in 256 KiB chunks, ~2.5 MiB total.
        let mut sent = 0u64;
        while sent < rate * 2 + rate / 2 {
            rl.acquire(256 * 1024).await;
            sent += 256 * 1024;
        }
        let elapsed = start.elapsed().as_secs_f64();
        // With a 0.5s burst, ~2.5 MiB at 1 MiB/s should take ~2s (2.5 - 0.5 burst).
        // Allow generous slack for CI timing: it must be clearly throttled (>1s)
        // but not absurdly slow (<5s).
        assert!(elapsed > 1.0, "did not throttle: {elapsed}s");
        assert!(elapsed < 5.0, "throttled too hard: {elapsed}s");
    }

    #[tokio::test]
    async fn set_rate_to_unlimited_stops_throttling() {
        let rl = RateLimiter::new(Some(1024));
        rl.set_rate(None);
        let start = Instant::now();
        rl.acquire(5 * 1024 * 1024).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
