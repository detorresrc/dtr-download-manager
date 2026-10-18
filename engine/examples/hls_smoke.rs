//! Live HLS end-to-end test: resolves a real master playlist to its best
//! variant, downloads all segments, concatenates them, and verifies the output
//! file is non-trivial and that the engine reported Completed.
//!
//!   cargo run -p grab-engine --example hls_smoke

use grab_engine::{DownloadKind, DownloadRequest, Manager, Settings, Status};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let out = PathBuf::from("/projects/sandbox/hls-smoke-out");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    let settings = Settings {
        download_dir: out.clone(),
        max_connections_per_file: 6, // download 6 segments at a time
        max_concurrent_downloads: 1,
        speed_limit: None,
        min_split_size: 1,
        ..Settings::default()
    };

    let (manager, mut rx) = Manager::new(settings);

    let req = DownloadRequest {
        url: "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8".into(),
        filename: Some("mux-test".into()),
        headers: Default::default(),
        save_dir: None,
        kind: Some(DownloadKind::Hls),
            resolved_target: None,
            queue: None,
    };
    let id = manager.enqueue(req).await;
    println!("enqueued {id}");

    let mut final_status = None;
    let mut last_total = None;
    let recv = async {
        while let Some(p) = rx.recv().await {
            last_total = p.total;
            match p.status {
                Status::Downloading => {
                    if let Some(total) = p.total {
                        println!("  segments {}/{}", p.downloaded, total);
                    }
                }
                Status::Completed => {
                    println!("COMPLETED: {} segments", p.downloaded);
                    final_status = Some(Status::Completed);
                    break;
                }
                Status::Failed => {
                    println!("FAILED: {:?}", p.error);
                    final_status = Some(Status::Failed);
                    break;
                }
                _ => {}
            }
        }
    };

    tokio::time::timeout(std::time::Duration::from_secs(180), recv)
        .await
        .map_err(|_| anyhow::anyhow!("hls download timed out"))?;

    assert_eq!(final_status, Some(Status::Completed), "did not complete");

    // The mux test stream's top variant has many segments.
    let n = last_total.unwrap_or(0);
    assert!(n > 5, "expected many segments, got {n}");

    let path = out.join("mux-test.ts");
    let meta = std::fs::metadata(&path)?;
    println!("output: {} = {} bytes", path.display(), meta.len());
    assert!(meta.len() > 1_000_000, "output suspiciously small");

    println!("\n✅ HLS smoke test passed: {} segments, {} bytes", n, meta.len());
    Ok(())
}
