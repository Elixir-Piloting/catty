//! catty - a minimal fast terminal emulator.
//! Tauri command surface + per-tab PTY management backed by alacritty_terminal.

mod cwd_hook;
mod history;
mod pty_session;
mod state;

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::history::{History, SharedHistory};
use crate::pty_session::PtySession;
use crate::state::{AppState, get};

#[derive(Serialize)]
struct TabInfo {
    id: u32,
    cwd: Option<String>,
}

#[tauri::command]
fn tab_new(app: AppHandle, parent_tab_id: Option<u32>) -> Result<TabInfo, String> {
    let state = get(&app).ok_or("state missing")?;

    // New tab inherits the parent tab's last known cwd, if any.
    let cwd = parent_tab_id.and_then(|pid| {
        state
            .sessions
            .lock()
            .get(&pid)
            .and_then(|s| s.cwd())
    });

    let id = state.alloc_id();
    let session = PtySession::spawn(app.clone(), id, cwd.clone(), &state.hook_path)
        .map_err(|e| format!("failed to spawn pty: {e}"))?;

    state.sessions.lock().insert(id, Arc::new(session));

    Ok(TabInfo { id, cwd })
}

#[tauri::command]
fn tab_close(app: AppHandle, id: u32) -> Result<(), String> {
    let state = get(&app).ok_or("state missing")?;
    if let Some(session) = state.sessions.lock().remove(&id) {
        session.kill();
    }
    Ok(())
}

#[tauri::command]
fn tab_input(app: AppHandle, id: u32, data: String) -> Result<(), String> {
    let state = get(&app).ok_or("state missing")?;
    let sessions = state.sessions.lock();
    if let Some(session) = sessions.get(&id) {
        session
            .write(data.as_bytes())
            .map_err(|e| format!("pty write failed: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
fn tab_resize(app: AppHandle, id: u32, cols: u16, rows: u16) -> Result<(), String> {
    let state = get(&app).ok_or("state missing")?;
    let sessions = state.sessions.lock();
    if let Some(session) = sessions.get(&id) {
        session.resize(cols, rows);
    }
    Ok(())
}

#[tauri::command]
fn tab_cwd(app: AppHandle, id: u32) -> Result<Option<String>, String> {
    let state = get(&app).ok_or("state missing")?;
    let cwd = state
        .sessions
        .lock()
        .get(&id)
        .and_then(|s| s.cwd());
    Ok(cwd)
}

#[derive(Deserialize)]
struct HistoryAddArgs {
    command: String,
}

#[tauri::command]
fn history_add(app: AppHandle, request: HistoryAddArgs) -> Result<(), String> {
    let state = get(&app).ok_or("state missing")?;
    state.history.add(&request.command);
    Ok(())
}

#[tauri::command]
fn history_match(app: AppHandle, prefix: String) -> Result<Option<String>, String> {
    let state = get(&app).ok_or("state missing")?;
    Ok(state.history.match_prefix(&prefix))
}

/// Build the shared history + hook path at startup.
fn build_state() -> Result<(SharedHistory, std::path::PathBuf), Box<dyn std::error::Error>> {
    let data_dir = dirs::data_dir()
        .or_else(|| std::env::temp_dir().into())
        .ok_or("no data dir")?;
    let app_dir = data_dir.join("catty");
    std::fs::create_dir_all(&app_dir)?;
    let db_path = app_dir.join("history.sqlite3");
    let history: SharedHistory = Arc::new(History::open(db_path)?);
    let hook_path = cwd_hook::write_hook_script()?;
    Ok((history, hook_path))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let (history, hook_path) = build_state().expect("failed to initialize catty state");
    let app_state = Arc::new(AppState::new(history, hook_path));

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            tab_new,
            tab_close,
            tab_input,
            tab_resize,
            tab_cwd,
            history_add,
            history_match,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
