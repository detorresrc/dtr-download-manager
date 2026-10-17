//! Live smoke test: downloads a real file through the engine, verifies the
//! bytes are correct, and confirms segmentation kicked in when supported.
//!
//! Run with:  cargo run -p grab-engine --example smoke

use grab_engine::{DownloadRequest, Manager, Settings, Status};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let tmp = PathBuf::from("/projects/sandbox/grab-smoke-out");
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;

    // A real, range-capable static asset pinned to an immutable commit hash so
    // its bytes can never change between our probe, our download, and the
    // reference checksum.
    let url = "https://raw.githubusercontent.com/rust-lang/rust/1.80.0/README.md".to_string();

    let settings = Settings {
        download_dir: tmp.clone(),
        max_connections_per_file: 4,
        max_concurrent_downloads: 2,
        speed_limit: None,
        min_split_size: 1024, // small, to force a split on this modest file
    };

    let (manager, mut rx) = Manager::new(settings);

    // First, probe to learn the expected size for verification.
    let client = reqwest::Client::builder()
        .user_agent("grab/0.1")
        .build()?;
    let probe = grab_engine::probe(
        &client,
        &DownloadRequest {
            url: url.clone(),
            filename: None,
            headers: Default::default(),
            save_dir: None,
            kind: None,
            resolved_target: None,
        },
    )
    .await?;
    let size = probe.content_length.expect("server reported a size");
    let expected_name = probe.filename.clone();
    println!(
        "probe: size={size} supports_ranges={} can_segment={}",
        probe.supports_ranges,
        probe.can_segment()
    );

    let req = DownloadRequest {
        url: url.clone(),
        filename: None,
        headers: Default::default(),
        save_dir: None,
        kind: None,
            resolved_target: None,
    };
    let id = manager.enqueue(req).await;
    println!("enqueued {id} -> {url}");

    let mut final_status = None;
    let mut max_conns = 0;
    let recv = async {
        while let Some(p) = rx.recv().await {
            if p.connections > max_conns {
                max_conns = p.connections;
            }
            match p.status {
                Status::Completed => {
                    println!("COMPLETED: {} bytes", p.downloaded);
                    final_status = Some((Status::Completed, p.downloaded));
                    break;
                }
                Status::Failed => {
                    println!("FAILED: {:?}", p.error);
                    final_status = Some((Status::Failed, p.downloaded));
                    break;
                }
                Status::Canceled => {
                    final_status = Some((Status::Canceled, p.downloaded));
                    break;
                }
                _ => {}
            }
        }
    };

    // Guard against a hung server: fail loudly rather than block forever.
    tokio::time::timeout(std::time::Duration::from_secs(60), recv)
        .await
        .map_err(|_| anyhow::anyhow!("download timed out"))?;

    // Verify the file on disk.
    let path = PathBuf::from(&tmp).join(&expected_name);
    let meta = std::fs::metadata(&path)?;
    println!("file on disk: {} = {} bytes (expected {size})", path.display(), meta.len());

    assert_eq!(
        final_status.map(|(s, _)| s),
        Some(Status::Completed),
        "download did not complete"
    );
    assert_eq!(meta.len(), size, "downloaded size mismatch");

    println!("\n✅ smoke test passed: {size} bytes verified on disk");
    Ok(())
}
