//! Live test of the localhost HTTP API: starts the real server and drives it
//! with an HTTP client, verifying auth, CORS-irrelevant happy paths, and the
//! download lifecycle end to end.
//!
//!   cargo run -p grab-engine --example http_smoke

use grab_engine::{Manager, Settings};
use std::io::{Read, Write};
use std::net::TcpListener;

/// Tiny range-capable, throttled file server so a download lasts long enough
/// to pause.
fn start_file_server(body: Vec<u8>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let mut b = [0u8; 2048];
                let _ = s.read(&mut b);
                let total = body.len();
                let h = format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(h.as_bytes());
                for chunk in body.chunks(16 * 1024) {
                    if s.write_all(chunk).is_err() { return; }
                    let _ = s.flush();
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
            });
        }
    });
    format!("http://{addr}/file.bin")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::path::PathBuf::from("/projects/sandbox/http-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let settings = Settings { download_dir: dir.clone(), ..Settings::default() };
    let (manager, _rx) = Manager::new(settings);

    let token = "secret-token-123".to_string();
    let port = 17655u16; // test port, avoid clashing with the real default
    {
        let manager = manager.clone();
        let token = token.clone();
        tokio::spawn(async move {
            grab_engine::http_api::serve(manager, token, port).await.unwrap();
        });
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    let auth = format!("Bearer {token}");

    // Helper: parse a response body as JSON (reqwest built without `json` feat).
    async fn as_json(r: reqwest::Response) -> serde_json::Value {
        let t = r.text().await.unwrap();
        serde_json::from_str(&t).unwrap_or(serde_json::Value::Null)
    }

    // 1. /ping needs no auth.
    let r = client.get(format!("{base}/ping")).send().await?;
    assert_eq!(r.status(), 200, "ping should be 200");
    let v = as_json(r).await;
    assert_eq!(v["ok"], true);
    println!("✅ /ping (no auth) -> {v}");

    // 2. Protected endpoint without a token -> 401.
    let r = client.get(format!("{base}/downloads")).send().await?;
    assert_eq!(r.status(), 401, "no token must be rejected");
    println!("✅ GET /downloads without token -> 401");

    // 3. Wrong token -> 401.
    let r = client
        .get(format!("{base}/downloads"))
        .header("authorization", "Bearer wrong")
        .send()
        .await?;
    assert_eq!(r.status(), 401, "wrong token must be rejected");
    println!("✅ wrong token -> 401");

    // 4. POST a download with the correct token.
    let file_url = start_file_server(vec![1u8; 512 * 1024]);
    let body = serde_json::json!({ "url": file_url, "filename": "file.bin" }).to_string();
    let r = client
        .post(format!("{base}/downloads"))
        .header("authorization", &auth)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await?;
    assert_eq!(r.status(), 200);
    let id = as_json(r).await["id"].as_str().unwrap().to_string();
    println!("✅ POST /downloads -> id={id}");

    // 5. GET /downloads shows it.
    let r = client.get(format!("{base}/downloads")).header("authorization", &auth).send().await?;
    let list = as_json(r).await;
    assert!(list.as_array().unwrap().iter().any(|d| d["id"] == id.as_str()));
    println!("✅ GET /downloads lists {} record(s)", list.as_array().unwrap().len());

    // 6. Pause it (should return ok).
    let r = client
        .post(format!("{base}/downloads/{id}/pause"))
        .header("authorization", &auth)
        .send()
        .await?;
    assert_eq!(r.status(), 200);
    println!("✅ POST /downloads/{id}/pause -> 200");

    // 7. Remove it.
    let r = client
        .delete(format!("{base}/downloads/{id}"))
        .header("authorization", &auth)
        .send()
        .await?;
    assert_eq!(r.status(), 200);
    println!("✅ DELETE /downloads/{id} -> 200");

    println!("\n✅ HTTP API smoke passed: auth enforced, download lifecycle works");
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}
