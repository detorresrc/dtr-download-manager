//! Localhost HTTP API for the browser extension.
//!
//! The engine runs inside the desktop app (always-on, via tray + autostart).
//! This module exposes its `Manager` over `http://127.0.0.1:<port>` so the
//! Chrome extension can queue downloads and control them with simple `fetch()`
//! calls — replacing the old native-messaging host.
//!
//! ## Security
//! A localhost server is reachable by ANY process or web page on the machine
//! (any site can `fetch("http://127.0.0.1:…")`). So every endpoint except the
//! health check requires `Authorization: Bearer <token>`, where the token is a
//! random secret the app generates and the user pairs into the extension once.
//! Requests without the token get `401`.
//!
//! ## CORS
//! The extension's origin is `chrome-extension://…`; we allow any origin but
//! gate on the bearer token, which is what actually protects the API.

use crate::manager::Manager;
use crate::types::DownloadRequest;
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

/// Shared state handed to every request handler.
#[derive(Clone)]
struct ApiState {
    manager: Manager,
    token: Arc<String>,
}

/// Build the router. Exposed separately from `serve` so it can be unit-tested
/// with an in-process client (no real socket needed).
pub fn router(manager: Manager, token: String) -> Router {
    let state = ApiState {
        manager,
        token: Arc::new(token),
    };
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/ping", get(ping)) // no auth
        .route("/downloads", post(create_download).get(list_downloads))
        .route("/downloads/:id/pause", post(pause))
        .route("/downloads/:id/resume", post(resume))
        .route("/downloads/:id/cancel", post(cancel))
        .route("/downloads/:id", delete(remove))
        .layer(cors)
        .with_state(state)
}

/// Start the server on `127.0.0.1:port`. Returns an error if the port is taken
/// (so the app can fall back to another port or report it).
pub async fn serve(manager: Manager, token: String, port: u16) -> std::io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("grab HTTP API listening on http://{addr}");
    axum::serve(listener, router(manager, token))
        .await
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

// ---- auth helper ------------------------------------------------------------

/// Returns `true` if the request carries the correct bearer token.
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(|t| t == token)
        .unwrap_or(false)
}

/// Shorthand 401 response.
fn unauthorized() -> axum::response::Response {
    (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response()
}

// ---- handlers ---------------------------------------------------------------

/// Health check — no auth, so the extension can show a 🟢/🔴 status.
async fn ping() -> impl IntoResponse {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

/// Body for `POST /downloads`.
#[derive(Debug, Deserialize)]
struct CreateBody {
    url: String,
    #[serde(default)]
    filename: Option<String>,
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    save_dir: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    queue: Option<String>,
}

async fn create_download(
    State(st): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<CreateBody>,
) -> axum::response::Response {
    if !authorized(&headers, &st.token) {
        return unauthorized();
    }
    let kind = body.kind.as_deref().and_then(|k| match k.to_ascii_lowercase().as_str() {
        "hls" => Some(crate::types::DownloadKind::Hls),
        "dash" => Some(crate::types::DownloadKind::Dash),
        "file" => Some(crate::types::DownloadKind::File),
        _ => None,
    });
    let req = DownloadRequest {
        url: body.url,
        filename: body.filename,
        headers: body.headers,
        save_dir: body.save_dir.map(std::path::PathBuf::from),
        kind,
        resolved_target: None,
        queue: body.queue,
    };
    let id = st.manager.enqueue(req).await;
    (StatusCode::OK, Json(json!({ "id": id }))).into_response()
}

async fn list_downloads(State(st): State<ApiState>, headers: HeaderMap) -> axum::response::Response {
    if !authorized(&headers, &st.token) {
        return unauthorized();
    }
    let records = st.manager.records().await;
    (StatusCode::OK, Json(records)).into_response()
}

async fn pause(
    State(st): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !authorized(&headers, &st.token) {
        return unauthorized();
    }
    st.manager.pause(&id).await;
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

async fn resume(
    State(st): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !authorized(&headers, &st.token) {
        return unauthorized();
    }
    st.manager.resume(&id).await;
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

async fn cancel(
    State(st): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !authorized(&headers, &st.token) {
        return unauthorized();
    }
    st.manager.cancel(&id).await;
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

async fn remove(
    State(st): State<ApiState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !authorized(&headers, &st.token) {
        return unauthorized();
    }
    st.manager.cancel(&id).await;
    st.manager.forget(&id).await;
    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}
