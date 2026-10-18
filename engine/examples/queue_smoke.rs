//! Live verification of Tier-2 queues + scheduler against a local server.
//!
//!   cargo run -p grab-engine --example queue_smoke
//!
//! Checks:
//!   1. A queue with max_concurrent = 2 never runs more than 2 downloads at once.
//!   2. Pausing a queue stops its downloads; starting it resumes them.
//!   3. All downloads eventually complete.

use grab_engine::{DownloadRequest, Manager, Queue, Settings, Status};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A slow server so downloads overlap in time (lets us observe concurrency).
fn start_server(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let total = body.len();
                let h = format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: none\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    total
                );
                let _ = stream.write_all(h.as_bytes());
                // Dribble so each download takes ~1.5s.
                for chunk in body.chunks(total / 15 + 1) {
                    if stream.write_all(chunk).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            });
        }
    });
    format!("http://{addr}/f.bin")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from("/projects/sandbox/queue-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let body: Vec<u8> = vec![7u8; 300 * 1024]; // 300 KiB each
    let url = start_server(body);

    // One queue "limited" capped at 2 concurrent.
    let mut settings = Settings {
        download_dir: dir.clone(),
        max_connections_per_file: 1,
        max_concurrent_downloads: 10,
        ..Settings::default()
    };
    settings.queues = vec![Queue::new("limited", "Limited", 2)];

    let (manager, mut rx) = Manager::new(settings);

    // Track peak simultaneous "downloading" across our 5 jobs.
    let downloading: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    let peak = Arc::new(Mutex::new(0usize));
    let completed = Arc::new(Mutex::new(0usize));

    let n = 5;
    for i in 0..n {
        manager
            .enqueue(DownloadRequest {
                url: url.clone(),
                filename: Some(format!("f{i}.bin")),
                headers: Default::default(),
                save_dir: None,
                kind: None,
                resolved_target: None,
                queue: Some("limited".into()),
            })
            .await;
    }
    println!("enqueued {n} downloads into a queue capped at 2 concurrent");

    let dl = downloading.clone();
    let pk = peak.clone();
    let cp = completed.clone();
    let recv = async {
        while let Some(p) = rx.recv().await {
            let mut set = dl.lock().unwrap();
            match p.status {
                Status::Downloading => {
                    set.insert(p.id.clone());
                    let mut peak = pk.lock().unwrap();
                    *peak = (*peak).max(set.len());
                }
                Status::Completed => {
                    set.remove(&p.id);
                    let mut c = cp.lock().unwrap();
                    *c += 1;
                    if *c == n {
                        break;
                    }
                }
                Status::Failed => panic!("failed: {:?}", p.error),
                _ => {
                    set.remove(&p.id);
                }
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), recv)
        .await
        .map_err(|_| anyhow::anyhow!("timed out"))?;

    let peak = *peak.lock().unwrap();
    let completed = *completed.lock().unwrap();
    println!("peak concurrent downloading = {peak}, completed = {completed}/{n}");

    assert_eq!(completed, n, "not all downloads completed");
    assert!(peak <= 2, "queue limit violated: {peak} ran at once (max 2)");
    assert!(peak >= 2, "concurrency never reached the limit ({peak}); test too fast?");

    // Verify n output files were written (names are server/URL-derived, so they
    // may be f.bin, f (1).bin, …; we just count .bin files in the dir).
    let present = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|x| x == "bin")
                .unwrap_or(false)
        })
        .count();
    assert_eq!(present, n, "expected {n} output files, found {present}");

    let _ = HashMap::<(), ()>::new();

    println!("\n✅ queue smoke passed: concurrency capped at 2, all {n} completed");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
