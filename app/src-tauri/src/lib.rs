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
    /// Bearer token the extension must send; shown in Settings for pairing.
    api_token: String,
    /// Localhost port the HTTP API listens on.
    api_port: u16,
}

/// The default localhost port for the extension-facing HTTP API.
const DEFAULT_API_PORT: u16 = 17654;

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

// Control commands apply to BOTH engines:
//   1. the app's own manager  — handles downloads started in the app window;
//   2. a pending action written to the shared queue file — the native host
//      (which owns browser-started downloads) polls and applies it.
// Whichever process owns the download acts on it; the other no-ops. This is why
// The app is now the single engine, so control commands act on it DIRECTLY.
// (The old interim `pending_action` dance existed only to reach a separate
// host process, which no longer exists.)

#[tauri::command]
async fn remove(state: State<'_, Arc<AppState>>, id: String) -> Result<(), String> {
    let manager = state.manager.lock().await;
    // Stop the transfer, then forget the record so it's gone from queue.json
    // for good — otherwise the next poll would re-add the row.
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
        // Single instance: a second launch focuses the already-running window
        // instead of starting a second engine that would fight over the queue.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        // Autostart: launch at login (controlled via the set_autostart command).
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--autostarted"]),
        ))
        .setup(|app| {
            let settings_path = config_dir().join("settings.json");
            let queue_path = config_dir().join("queue.json");
            let settings = load_settings(&settings_path);
            let api_token = load_or_create_token();
            let api_port = load_api_port();

            // The app is now the SINGLE engine, so it resumes unfinished
            // downloads itself (no separate host process anymore).
            let (manager, mut progress_rx) = tauri::async_runtime::block_on(async {
                let (m, rx) = Manager::with_persistence(settings, queue_path.clone()).await;
                let restored = m.restore().await;
                if !restored.is_empty() {
                    eprintln!("restored {} unfinished download(s)", restored.len());
                }
                (m, rx)
            });

            // Clone the manager for the HTTP server BEFORE it's moved into the
            // shared state (avoids a nested lock/block_on inside setup).
            let http_manager = manager.clone();

            let state = Arc::new(AppState {
                manager: Mutex::new(manager),
                settings_path,
                queue_path,
                api_token: api_token.clone(),
                api_port,
            });
            app.manage(state);

            // Start the localhost HTTP API the browser extension calls.
            {
                let token = api_token.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) =
                        grab_engine::http_api::serve(http_manager, token, api_port).await
                    {
                        eprintln!("HTTP API failed to start on port {api_port}: {e}");
                    }
                });
            }

            // Pump engine progress events to the webview. Also raise the window
            // the first time we see a given download id, so a download started
            // from the browser surfaces the UI for the user to see/accept.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                while let Some(p) = progress_rx.recv().await {
                    if seen.insert(p.id.clone()) {
                        // First event for this download → surface the window.
                        show_main(&handle);
                    }
                    let _ = handle.emit("progress", ProgressEvent::from(&p));
                }
            });

            // System tray: quick actions + show/hide the window.
            build_tray(app.handle())?;

            // Self-heal autostart: if the user enabled it but the OS entry is
            // gone (Windows sometimes drops the Run-key after a boot —
            // plugins-workspace#771), re-assert it now.
            {
                use tauri_plugin_autostart::ManagerExt;
                if wants_autostart() {
                    let al = app.autolaunch();
                    if let Ok(false) = al.is_enabled() {
                        if let Err(e) = al.enable() {
                            eprintln!("could not re-assert autostart: {e}");
                        }
                    }
                }
            }

            // If launched at login (--autostarted), start hidden in the tray so
            // it's not intrusive; otherwise show the window.
            let autostarted = std::env::args().any(|a| a == "--autostarted");
            if let Some(w) = app.get_webview_window("main") {
                if autostarted {
                    let _ = w.hide();
                } else {
                    let _ = w.show();
                }
            }
            Ok(())
        })
        // Close button hides to tray instead of quitting, so the engine + HTTP
        // API keep running in the background to catch browser downloads.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
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
            start_queue,
            get_api_info,
            set_api_port,
            set_autostart,
            is_autostart_enabled,
            quit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while running Grab");
}

/// Build the system-tray icon and menu.
fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::{TrayIconBuilder, TrayIconEvent};

    let open = MenuItemBuilder::with_id("open", "Open Grab").build(app)?;
    let pause_all = MenuItemBuilder::with_id("pause_all", "Pause all").build(app)?;
    let resume_all = MenuItemBuilder::with_id("resume_all", "Resume all").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
    let menu = MenuBuilder::new(app)
        .items(&[&open, &pause_all, &resume_all])
        .separator()
        .item(&quit)
        .build()?;

    let _tray = TrayIconBuilder::with_id("main")
        .tooltip("Grab Download Manager")
        .icon(app.default_window_icon().unwrap().clone())
        .menu(&menu)
        // Left-click shows the window (don't pop the menu on left click).
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => show_main(app),
            "quit" => app.exit(0),
            "pause_all" => {
                if let Some(state) = app.try_state::<Arc<AppState>>() {
                    let state = state.inner().clone();
                    tauri::async_runtime::spawn(async move {
                        state.manager.lock().await.pause_all().await;
                    });
                }
            }
            "resume_all" => {
                if let Some(state) = app.try_state::<Arc<AppState>>() {
                    let state = state.inner().clone();
                    tauri::async_runtime::spawn(async move {
                        state.manager.lock().await.start_all().await;
                    });
                }
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // Left-click the tray icon to show the window.
            if let TrayIconEvent::Click { button, .. } = event {
                if button == tauri::tray::MouseButton::Left {
                    show_main(tray.app_handle());
                }
            }
        })
        .build(app)?;
    Ok(())
}

/// Show, unminimize and focus the main window.
fn show_main(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Serializable progress payload sent to the frontend.
#[derive(serde::Serialize, Clone)]
struct ProgressEvent {
    id: String,
    status: String,
    downloaded: u64,
    total: Option<u64>,
    speed: u64,
    eta_secs: Option<u64>,
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
            eta_secs: p.eta_secs,
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

/// Load the API token from `<config>/api_token`, generating + persisting a new
/// random one the first time. The extension pairs with this token.
fn load_or_create_token() -> String {
    let path = config_dir().join("api_token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim().to_string();
        if !t.is_empty() {
            return t;
        }
    }
    // Generate a 32-char alphanumeric token.
    use rand::Rng;
    let token: String = rand::thread_rng()
        .sample_iter(rand::distributions::Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, &token);
    token
}

/// Load the HTTP API port from `<config>/api_port`, defaulting to the fixed
/// default. Invalid/empty contents fall back to the default.
fn load_api_port() -> u16 {
    std::fs::read_to_string(config_dir().join("api_port"))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .filter(|&p| p >= 1024) // avoid privileged ports
        .unwrap_or(DEFAULT_API_PORT)
}

fn save_api_port(port: u16) -> Result<(), String> {
    let path = config_dir().join("api_port");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, port.to_string()).map_err(|e| e.to_string())
}

/// Pairing info the Settings UI shows so the user can connect the extension.
#[tauri::command]
async fn get_api_info(state: State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "port": state.api_port,
        "token": state.api_token,
        "base_url": format!("http://127.0.0.1:{}", state.api_port),
    }))
}

/// Change the HTTP API port. Persisted immediately; takes effect after a
/// restart (the server binds the port once at launch). Returns the new port.
#[tauri::command]
async fn set_api_port(port: u16) -> Result<u16, String> {
    if port < 1024 {
        return Err("Port must be 1024 or higher.".into());
    }
    save_api_port(port)?;
    Ok(port)
}

/// Path of the small file that records whether the user wants autostart on.
/// Used to re-assert the OS entry on startup (defends against Windows removing
/// the Run-key after a boot — plugins-workspace#771).
fn autostart_pref_path() -> PathBuf {
    config_dir().join("autostart_enabled")
}

fn save_autostart_pref(enabled: bool) {
    let path = autostart_pref_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, if enabled { "1" } else { "0" });
}

fn wants_autostart() -> bool {
    std::fs::read_to_string(autostart_pref_path())
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Enable or disable launching Grab at login.
#[tauri::command]
async fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    let result = if enabled {
        mgr.enable()
    } else {
        mgr.disable()
    };
    result.map_err(|e| e.to_string())?;
    // Remember the user's intent so we can re-assert it on next startup.
    save_autostart_pref(enabled);
    Ok(())
}

/// Whether autostart is currently enabled. Reflects the OS state, but also the
/// saved intent so the checkbox isn't misleading if the OS entry was dropped
/// (startup self-heal re-asserts it anyway).
#[tauri::command]
async fn is_autostart_enabled(app: tauri::AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt;
    let os_enabled = app.autolaunch().is_enabled().unwrap_or(false);
    Ok(os_enabled || wants_autostart())
}

/// Quit the whole app (used by the tray "Quit" item — the window close button
/// only hides to tray so the engine keeps running).
#[tauri::command]
async fn quit_app(app: tauri::AppHandle) -> Result<(), String> {
    app.exit(0);
    Ok(())
}
