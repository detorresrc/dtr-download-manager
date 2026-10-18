//! Grab desktop app backend. Wraps `grab-engine` and exposes it to the web
//! frontend via Tauri commands, streaming progress back as `progress` events.

use grab_engine::{DownloadKind, DownloadRecord, DownloadRequest, Manager, Progress, Settings};
use std::path::PathBuf;
use std::sync::Arc;
use tauri::{Emitter, Manager as _, State};
use tokio::sync::Mutex;

/// Shared app state: the engine manager plus the paths to the shared files.
struct AppState {
    manager: Mutex<Manager>,
    settings_path: PathBuf,
    queue_path: PathBuf,
}

#[tauri::command]
async fn start_download(
    state: State<'_, Arc<AppState>>,
    url: String,
    filename: Option<String>,
    save_dir: Option<String>,
    kind: Option<String>,
    queue: Option<String>,
) -> Result<String, String> {
    let kind = kind.and_then(|k| match k.to_ascii_lowercase().as_str() {
        "hls" => Some(DownloadKind::Hls),
        "dash" => Some(DownloadKind::Dash),
        "file" => Some(DownloadKind::File),
        _ => None,
    });
    let req = DownloadRequest {
        url,
        filename,
        headers: Default::default(),
        save_dir: save_dir.map(PathBuf::from),
        kind,
        resolved_target: None,
        queue,
    };
    let manager = state.manager.lock().await;
    Ok(manager.enqueue(req).await)
}

#[tauri::command]
async fn pause_all(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.manager.lock().await.pause_all().await;
    Ok(())
}

#[tauri::command]
async fn start_all(state: State<'_, Arc<AppState>>) -> Result<(), String> {
    state.manager.lock().await.start_all().await;
    Ok(())
}

#[tauri::command]
async fn pause_queue(state: State<'_, Arc<AppState>>, queue: String) -> Result<(), String> {
    state.manager.lock().await.pause_queue(&queue).await;
    Ok(())
}

#[tauri::command]
async fn start_queue(state: State<'_, Arc<AppState>>, queue: String) -> Result<(), String> {
    state.manager.lock().await.start_queue(&queue).await;
    Ok(())
}

#[tauri::command]
async fn list_downloads(state: State<'_, Arc<AppState>>) -> Result<Vec<DownloadRecord>, String> {
    Ok(state.manager.lock().await.records().await)
}

/// Read the SHARED queue file directly from disk. This surfaces downloads
/// started by the native host (Chrome) — which runs in a separate process and
/// writes the same `queue.json` — so the GUI can act as a live dashboard. The
/// app polls this. Returns an empty list if the file doesn't exist yet.
#[tauri::command]
async fn read_queue_file(state: State<'_, Arc<AppState>>) -> Result<Vec<DownloadRecord>, String> {
    match tokio::fs::read(&state.queue_path).await {
        Ok(bytes) => {
            serde_json::from_slice::<Vec<DownloadRecord>>(&bytes).map_err(|e| e.to_string())
        }
        Err(_) => Ok(Vec::new()),
    }
}

#[tauri::command]
async fn remove(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    let manager = state.manager.lock().await;
    manager.cancel(&id).await;
    manager.forget(&id).await;
    Ok(())
}

#[tauri::command]
async fn pause(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.manager.lock().await.pause(&id).await;
    Ok(())
}

#[tauri::command]
async fn resume(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.manager.lock().await.resume(&id).await;
    Ok(())
}

#[tauri::command]
async fn cancel(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    state.manager.lock().await.cancel(&id).await;
    Ok(())
}

#[tauri::command]
async fn get_settings(state: State<'_, Arc<AppState>>) -> Result<Settings, String> {
    Ok(state.manager.lock().await.settings().await)
}

#[tauri::command]
async fn save_settings(
    state: State<'_, Arc<AppState>>,
    settings: Settings,
) -> Result<(), String> {
    // Persist to disk and update the running engine.
    if let Some(parent) = state.settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(&state.settings_path, json).map_err(|e| e.to_string())?;
    state.manager.lock().await.update_settings(settings).await;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Resolve where settings + queue live (shared with the host format).
            let settings_path = config_dir().join("settings.json");
            let queue_path = config_dir().join("queue.json");
            let settings = load_settings(&settings_path);

            // Build a persistent manager for downloads started FROM the app.
            //
            // We deliberately do NOT call `restore()` here: the native host
            // (launched by Chrome) is a separate process that owns the same
            // `queue.json` and resumes unfinished downloads itself. If the app
            // also restored them, two engines would race over the same files.
            // The app instead polls `read_queue_file` to DISPLAY the host's
            // downloads as a live dashboard.
            let (manager, mut progress_rx) = tauri::async_runtime::block_on(async {
                Manager::with_persistence(settings, queue_path.clone()).await
            });

            let state = Arc::new(AppState {
                manager: Mutex::new(manager),
                settings_path,
                queue_path,
            });
            app.manage(state);

            // Pump engine progress events to the webview.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                while let Some(p) = progress_rx.recv().await {
                    let _ = handle.emit("progress", ProgressEvent::from(&p));
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_download,
            pause,
            resume,
            cancel,
            get_settings,
            save_settings,
            list_downloads,
            read_queue_file,
            remove,
            pause_all,
            start_all,
            pause_queue,
            start_queue
        ])
        .run(tauri::generate_context!())
        .expect("error while running Grab");
}

/// Serializable progress payload sent to the frontend.
#[derive(serde::Serialize, Clone)]
struct ProgressEvent {
    id: String,
    status: String,
    downloaded: u64,
    total: Option<u64>,
    speed: u64,
    connections: usize,
    error: Option<String>,
}

impl From<&Progress> for ProgressEvent {
    fn from(p: &Progress) -> Self {
        Self {
            id: p.id.clone(),
            status: format!("{:?}", p.status).to_lowercase(),
            downloaded: p.downloaded,
            total: p.total,
            speed: p.speed,
            connections: p.connections,
            error: p.error.clone(),
        }
    }
}

fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("grab");
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(xdg).join("grab");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".config").join("grab");
        }
    }
    PathBuf::from(".")
}

fn load_settings(path: &PathBuf) -> Settings {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Settings::default(),
    }
}
