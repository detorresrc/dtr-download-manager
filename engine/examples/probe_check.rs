//! Quick probe-only check against a known range-capable host.
use grab_engine::{probe, DownloadRequest};
use reqwest::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::builder()
        .user_agent("grab/0.1")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;

    let url = "https://raw.githubusercontent.com/rust-lang/rust/master/README.md";
    let req = DownloadRequest {
        url: url.into(),
        filename: None,
        headers: Default::default(),
        save_dir: None,
        kind: None,
            resolved_target: None,
    };
    let r = tokio::time::timeout(std::time::Duration::from_secs(20), probe(&client, &req)).await??;
    println!("final_url     = {}", r.final_url);
    println!("content_length= {:?}", r.content_length);
    println!("supports_range= {}", r.supports_ranges);
    println!("can_segment   = {}", r.can_segment());
    println!("filename      = {}", r.filename);
    println!("etag          = {:?}", r.etag);
    Ok(())
}
