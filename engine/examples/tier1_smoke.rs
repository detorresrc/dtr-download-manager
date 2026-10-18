//! Live verification of Tier-1 features against a LOCAL throttle-free server:
//!   1. Speed limiting actually caps throughput to ~the configured rate.
//!   2. Category auto-foldering places a file in the right subfolder.
//!
//!   cargo run -p grab-engine --example tier1_smoke

use grab_engine::{DownloadRequest, Manager, Settings, Status};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Instant;

/// Serve a fixed-size, range-capable body as fast as possible (no artificial
/// delay) so the ONLY throttle is our rate limiter.
fn start_server(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 2048];
            let _ = stream.read(&mut buf);
            let req = String::from_utf8_lossy(&buf);
            let range = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                .and_then(|l| l.split('=').nth(1))
                .map(|s| s.trim().to_string());
            let total = body.len();
            if let Some(r) = range {
                let mut p = r.split('-');
                let start: usize = p.next().unwrap_or("0").trim().parse().unwrap_or(0);
                let end: usize = p
                    .next()
                    .and_then(|e| e.trim().parse().ok())
                    .unwrap_or(total - 1)
                    .min(total - 1);
                let slice = &body[start..=end];
                let h = format!(
                    "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nContent-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    start, end, total, slice.len()
                );
                let _ = stream.write_all(h.as_bytes());
                let _ = stream.write_all(slice);
            } else {
                let h = format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    total
                );
                let _ = stream.write_all(h.as_bytes());
                let _ = stream.write_all(&body);
            }
        }
    });
    format!("http://{addr}/clip.mp4")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = PathBuf::from("/projects/sandbox/tier1-out");
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base)?;

    // 4 MiB payload, a ".mp4" so it should land in the Video category folder.
    let body: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let size = body.len() as u64;
    let url = start_server(body);

    // 1 MiB/s cap.
    let rate = 1024 * 1024u64;
    let settings = Settings {
        download_dir: base.clone(),
        max_connections_per_file: 8,
        max_concurrent_downloads: 2,
        speed_limit: Some(rate),
        min_split_size: 64 * 1024,
        ..Settings::default() // keeps default categories + use_categories=true
    };

    let (manager, mut rx) = Manager::new(settings);
    let id = manager
        .enqueue(DownloadRequest {
            url: url.clone(),
            filename: Some("clip.mp4".into()),
            headers: Default::default(),
            save_dir: None, // let categories decide the folder
            kind: None,
            resolved_target: None,
        })
        .await;
    println!("enqueued {id}, cap = {} KiB/s", rate / 1024);

    let start = Instant::now();
    let mut done = false;
    let recv = async {
        while let Some(p) = rx.recv().await {
            if p.status == Status::Completed {
                done = true;
                break;
            }
            if p.status == Status::Failed {
                panic!("failed: {:?}", p.error);
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), recv)
        .await
        .map_err(|_| anyhow::anyhow!("timed out"))?;
    let elapsed = start.elapsed().as_secs_f64();
    assert!(done);

    // --- Verify speed limiting ---
    // 4 MiB at 1 MiB/s should take ~4s (minus the ~0.5s initial burst → ~3.5s).
    // Assert it was clearly throttled but not absurdly slow.
    let effective_mibps = (size as f64 / 1024.0 / 1024.0) / elapsed;
    println!("downloaded {} MiB in {:.2}s → {:.2} MiB/s", size / 1024 / 1024, elapsed, effective_mibps);
    assert!(elapsed > 2.5, "not throttled (finished in {elapsed:.2}s)");
    assert!(effective_mibps < 2.0, "throughput {effective_mibps:.2} MiB/s exceeds the 1 MiB/s cap");

    // --- Verify category foldering ---
    let expected = base.join("Video").join("clip.mp4");
    println!("expecting file at {}", expected.display());
    assert!(expected.exists(), "file was not placed in the Video category folder");
    assert_eq!(std::fs::metadata(&expected)?.len(), size);

    println!("\n✅ Tier-1 smoke passed: speed limited to ~1 MiB/s AND auto-foldered to Video/");
    let _ = std::fs::remove_dir_all(&base);
    Ok(())
}
