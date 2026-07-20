//! Shared application state: per-tab PTY sessions, id counter, history store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use parking_lot::Mutex;
use tauri::{AppHandle, Manager};

use crate::history::SharedHistory;
use crate::pty_session::PtySession;

pub struct AppState {
    pub sessions: Mutex<HashMap<u32, std::sync::Arc<PtySession>>>,
    pub next_id: AtomicU32,
    pub history: SharedHistory,
    pub hook_path: PathBuf,
}

impl AppState {
    pub fn new(history: SharedHistory, hook_path: PathBuf) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            history,
            hook_path,
        }
    }

    pub fn alloc_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::SeqCst)
    }
}

use std::sync::Arc;

/// Convenience accessor for the AppState through Tauri's managed state.
pub fn get(app: &AppHandle) -> Option<Arc<AppState>> {
    app.try_state::<Arc<AppState>>()
        .map(|s| s.inner().clone())
}
