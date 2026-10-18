//! Verifies queue persistence + restore against a LOCAL http server we control,
//! so the test is deterministic (no flaky public hosts).
//!
//!   cargo run -p grab-engine --example persist_smoke
//!
//! Flow:
//!   1. Start a tiny local server that serves a range-capable file slowly.
//!   2. Manager A (persistent) enqueues a download, we let it start, then DROP
//!      manager A without finishing (simulating a crash/exit).
//!   3. Manager B opens the SAME queue file, calls restore(), and the download
//!      completes. We verify the bytes and that the queue recorded completion.

use grab_engine::{Manager, Settings, Status};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;

fn start_server(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let req = String::from_utf8_lossy(&buf);
            // Parse a Range header if present.
            let range = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                .and_then(|l| l.split('=').nth(1))
                .map(|s| s.trim().to_string());

            let total = body.len();
            if let Some(r) = range {
                let mut parts = r.split('-');
                let start: usize = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
                let end: usize = parts
                    .next()
                    .and_then(|e| e.trim().parse().ok())
                    .unwrap_or(total - 1)
                    .min(total - 1);
                let slice = &body[start..=end];
                let header = format!(
                    "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    start, end, total, slice.len()
                );
                let _ = stream.write_all(header.as_bytes());
                // Throttle: dribble the body so a download can't finish instantly,
                // guaranteeing there's something to interrupt and resume.
                for chunk in slice.chunks(16 * 1024) {
                    if stream.write_all(chunk).is_err() {
                        break;
                    }
                    let _ = stream.flush();
                    std::thread::sleep(std::time::Duration::from_millis(40));
                }
            } else {
                let header = format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    total
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        }
    });
    format!("http://{addr}/file.bin")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from("/projects/sandbox/persist-smoke");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let queue_path = dir.join("queue.json");

    // Deterministic 2 MiB payload.
    let body: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let expected = body.clone();
    let url = start_server(body);

    let settings = Settings {
        download_dir: dir.clone(),
        max_connections_per_file: 4,
        max_concurrent_downloads: 2,
        speed_limit: None,
        min_split_size: 64 * 1024,
        ..Settings::default()
    };

    // --- Manager A runs in its OWN runtime on a separate thread, which we
    // shut down mid-download. Shutting down the runtime aborts A's background
    // tasks, faithfully simulating the process crashing/exiting. ---
    {
        let settings_a = settings.clone();
        let queue_a = queue_path.clone();
        let url_a = url.clone();
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (mgr_a, _rx) =
                    Manager::with_persistence(settings_a, queue_a).await;
                let _ = mgr_a
                    .enqueue(grab_engine::DownloadRequest {
                        url: url_a,
                        filename: Some("file.bin".into()),
                        headers: Default::default(),
                        save_dir: None,
                        kind: None,
            resolved_target: None,
            queue: None,
                    })
                    .await;
                // Let a little data flow (server is throttled), then return,
                // which drops `rt` at the end of the thread and aborts tasks.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            });
            // Dropping `rt` here aborts all of A's in-flight download tasks.
            drop(rt);
        });
        handle.join().unwrap();
        println!("manager A interrupted mid-download (runtime shut down)");
    }

    // Confirm the queue file recorded the (unfinished) download.
    let raw = std::fs::read(&queue_path)?;
    let records: serde_json::Value = serde_json::from_slice(&raw)?;
    println!("queue.json after A: {}", records);
    let arr = records.as_array().expect("array");
    assert!(!arr.is_empty(), "queue should contain the download");
    let status_a = arr[0]["status"].as_str().unwrap_or("");
    println!("status after interruption: {status_a}");
    assert_ne!(status_a, "completed", "A should NOT have finished");

    // --- Manager B: reopen the same queue and restore. ---
    let (mgr_b, mut rx) = Manager::with_persistence(settings, queue_path.clone()).await;
    let restored = mgr_b.restore().await;
    println!("manager B restored: {:?}", restored);
    assert!(!restored.is_empty(), "B should have an unfinished download to restore");

    let mut done = false;
    let recv = async {
        while let Some(p) = rx.recv().await {
            if p.status == Status::Completed {
                done = true;
                break;
            }
            if p.status == Status::Failed {
                panic!("download failed: {:?}", p.error);
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), recv)
        .await
        .map_err(|_| anyhow::anyhow!("restore did not complete in time"))?;
    assert!(done, "did not observe completion");

    // Verify bytes.
    let got = std::fs::read(dir.join("file.bin"))?;
    assert_eq!(got.len(), expected.len(), "size mismatch");
    if got != expected {
        // Find and report the first divergence to aid debugging.
        let first = got
            .iter()
            .zip(expected.iter())
            .position(|(a, b)| a != b)
            .unwrap_or(got.len());
        let n_diff = got.iter().zip(expected.iter()).filter(|(a, b)| a != b).count();
        panic!(
            "content mismatch: first differ at offset {first} (got {:?}, want {:?}); {n_diff} bytes differ of {}",
            got.get(first),
            expected.get(first),
            got.len()
        );
    }

    // The queue should now show the download completed.
    let raw = std::fs::read(&queue_path)?;
    let records: serde_json::Value = serde_json::from_slice(&raw)?;
    let status = records[0]["status"].as_str().unwrap_or("");
    println!("final queue status: {status}");
    assert_eq!(status, "completed");

    println!("\n✅ persistence smoke passed: queue survived restart, resumed, verified {} bytes", got.len());
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
