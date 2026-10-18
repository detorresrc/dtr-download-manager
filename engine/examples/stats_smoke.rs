//! Verify speed + ETA are emitted and sane during a throttled download.
//!   cargo run -p grab-engine --example stats_smoke

use grab_engine::{DownloadRequest, Manager, Settings, Status};
use std::io::{Read, Write};
use std::net::TcpListener;

/// Range-capable, throttled server (handles the probe's `Range: bytes=0-0` and
/// real ranged GETs). Each connection is served on its own thread.
fn start_server(body: Vec<u8>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let mut b = [0u8; 2048];
                let _ = s.read(&mut b);
                let req = String::from_utf8_lossy(&b);
                let total = body.len();
                let range = req
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                    .and_then(|l| l.split('=').nth(1))
                    .map(|x| x.trim().to_string());
                let (start, end, code, extra) = if let Some(r) = range {
                    let mut p = r.split('-');
                    let st: usize = p.next().unwrap_or("0").trim().parse().unwrap_or(0);
                    let en: usize = p.next().and_then(|e| e.trim().parse().ok())
                        .unwrap_or(total - 1).min(total - 1);
                    (st, en, "206 Partial Content",
                     format!("Content-Range: bytes {st}-{en}/{total}\r\n"))
                } else {
                    (0, total - 1, "200 OK", String::new())
                };
                let slice = body[start..=end].to_vec();
                let h = format!(
                    "HTTP/1.1 {code}\r\nAccept-Ranges: bytes\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                let _ = s.write_all(h.as_bytes());
                // Throttle to ~640 KiB/s so the download lasts a few seconds.
                for chunk in slice.chunks(64 * 1024) {
                    if s.write_all(chunk).is_err() { return; }
                    let _ = s.flush();
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            });
        }
    });
    format!("http://{addr}/big.bin")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from("/projects/sandbox/stats-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let size = 3 * 1024 * 1024; // 3 MiB
    let url = start_server(vec![9u8; size]);

    let settings = Settings { download_dir: dir.clone(), ..Settings::default() };
    let (m, mut rx) = Manager::new(settings);
    m.enqueue(DownloadRequest {
        url, filename: Some("big.bin".into()), headers: Default::default(),
        save_dir: None, kind: None, resolved_target: None, queue: None,
    }).await;

    let mut saw_speed = false;
    let mut saw_eta = false;
    let mut samples = Vec::new();
    let recv = async {
        while let Some(p) = rx.recv().await {
            if p.status == Status::Downloading && p.speed > 0 {
                saw_speed = true;
                if let Some(eta) = p.eta_secs {
                    saw_eta = true;
                    samples.push((p.downloaded, p.speed, eta));
                    println!("  {:>7} KiB  speed={:>6} KiB/s  eta={}s",
                        p.downloaded/1024, p.speed/1024, eta);
                }
            }
            if p.status == Status::Completed { break; }
            if p.status == Status::Failed { panic!("failed: {:?}", p.error); }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), recv).await
        .map_err(|_| anyhow::anyhow!("timed out"))?;

    assert!(saw_speed, "never reported a non-zero speed");
    assert!(saw_eta, "never reported an ETA");
    // ETA should decrease overall as the download progresses.
    let first = samples.first().unwrap().2;
    let last = samples.last().unwrap().2;
    println!("ETA first={first}s last={last}s (expect last <= first)");
    assert!(last <= first + 1, "ETA should trend down, not up");

    println!("\n✅ stats smoke passed: speed + ETA emitted and sane");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
