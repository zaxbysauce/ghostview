//! GhostView Pro — Tauri host library.
//!
//! This crate is the native Rust desktop agent for GhostView. It captures the
//! screen (on Windows) and streams the frames via WebRTC to a browser-based
//! viewer. Non-Windows builds fall back to synthetic capture so the scaffold
//! can be developed and smoke-tested cross-platform.

mod capture;
mod codec;
mod session;
mod signaling;
mod webrtc_host;

use serde::Serialize;
use session::AppState;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Information about a capturable monitor, surfaced to the frontend.
#[derive(Serialize, Clone, Debug)]
pub struct MonitorInfo {
    pub index: usize,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
}

/// Status payload emitted to the frontend via the `session-status` event.
#[derive(Serialize, Clone, Debug)]
pub struct SessionStatus {
    pub state: String,
    pub pin: Option<String>,
    pub message: Option<String>,
}

#[tauri::command]
async fn start_session(
    monitor_index: usize,
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
) -> Result<String, String> {
    let mut guard = state.lock().await;
    guard
        .start(monitor_index)
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn stop_session(
    state: tauri::State<'_, Arc<Mutex<AppState>>>,
) -> Result<(), String> {
    let mut guard = state.lock().await;
    guard.stop().await.map_err(|e| e.to_string())
}

#[tauri::command]
fn list_monitors() -> Vec<MonitorInfo> {
    capture::list_monitors()
}

/// Entry point invoked from `main.rs`.
pub fn run() {
    // Install tracing subscriber. Respect `RUST_LOG`, default to info.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let app_state = Arc::new(Mutex::new(AppState::new()));

    tauri::Builder::default()
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            start_session,
            stop_session,
            list_monitors,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
