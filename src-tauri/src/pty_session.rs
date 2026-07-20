//! Per-tab PTY session built on `alacritty_terminal::tty`.
//!
//! Each session runs a dedicated thread that:
//!   * creates a PTY (ConPTY on Windows) wrapping the user's shell,
//!   * polls the PTY reader for output, scanning through the OSC 7 cwd parser,
//!   * emits `pty-data:<id>` (base64), `cwd-changed:<id>` events,
//!   * on child exit: **respawns** a fresh shell in the same tab (preserving
//!     the last known working directory), emitting `pty-restart:<id>` so the
//!     frontend can reset its input-line tracker,
//!   * on kill / fatal error: emits `pty-exit:<id>` and stops.
//!
//! Frontend keystrokes come in via `write()`; resize via `resize()`.

use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use alacritty_terminal::event::{OnResize, WindowSize};
use alacritty_terminal::tty::{
    self, ChildEvent, EventedPty, EventedReadWrite, Options, Shell,
};
use alacritty_terminal::tty::windows::{PTY_CHILD_EVENT_TOKEN, PTY_READ_WRITE_TOKEN};
use base64::Engine;
use parking_lot::Mutex;
use polling::{Event, PollMode, Poller};
use tauri::{AppHandle, Emitter, Manager};
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::TerminateProcess;

use crate::cwd_hook::Osc7Scanner;
use crate::state::AppState;

// alacritty_terminal's Pty::register overwrites the event key with these
// constants, so we must match against them (not our own).
const READ_TOKEN: usize = PTY_READ_WRITE_TOKEN;
const CHILD_TOKEN: usize = PTY_CHILD_EVENT_TOKEN;

/// A nullable, swappable PTY — the reader/respawn thread replaces it when
/// the child process exits and a new shell is spawned.
type PtySlot = Arc<Mutex<Option<tty::Pty>>>;

pub struct PtySession {
    #[allow(dead_code)]
    pub id: u32,
    pty: PtySlot,
    /// Kept alive for the session's lifetime so the poller registration
    /// persists across respawns.
    #[allow(dead_code)]
    poller: Arc<Poller>,
    pub cwd: Mutex<Option<String>>,
    killed: Arc<AtomicBool>,
}

impl PtySession {
    /// Spawn a new shell session. The reader/respawn thread is started
    /// internally; the first PTY is created there.
    pub fn spawn(
        app: AppHandle,
        id: u32,
        working_dir: Option<String>,
        hook_path: std::path::PathBuf,
    ) -> std::io::Result<Self> {
        let pty: PtySlot = Arc::new(Mutex::new(None));
        let poller = Arc::new(Poller::new()?);
        let killed = Arc::new(AtomicBool::new(false));

        let session = Self {
            id,
            pty: pty.clone(),
            poller: poller.clone(),
            cwd: Mutex::new(working_dir.clone()),
            killed: killed.clone(),
        };

        let app_t = app.clone();
        thread::Builder::new()
            .name(format!("pty-reader-{}", id))
            .spawn(move || {
                run_respawn_loop(app_t, id, pty, poller, hook_path, working_dir, killed);
            })?;

        Ok(session)
    }

    /// Write user keystrokes to the shell.
    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        if let Some(p) = self.pty.lock().as_mut() {
            p.writer().write_all(data)
        } else {
            Ok(())
        }
    }

    /// Resize the underlying PTY (SIGWINCH-equivalent).
    pub fn resize(&self, cols: u16, rows: u16) {
        if let Some(p) = self.pty.lock().as_mut() {
            p.on_resize(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 8,
                cell_height: 16,
            });
        }
    }

    /// Forcefully terminate the child process. Signals the reader/respawn
    /// thread to stop (no respawn).
    pub fn kill(&self) {
        self.killed.store(true, Ordering::SeqCst);
        if let Some(p) = self.pty.lock().as_mut() {
            let handle = p.child_watcher().raw_handle();
            unsafe {
                TerminateProcess(handle, 0);
                CloseHandle(handle);
            }
        }
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.lock().clone()
    }

    pub fn set_cwd(&self, cwd: String) {
        *self.cwd.lock() = Some(cwd);
    }
}

// ---------------------------------------------------------------------------
// Reader / respawn loop
// ---------------------------------------------------------------------------

fn build_config(hook_path: &std::path::Path, working_dir: &Option<String>) -> Options {
    let program = "powershell.exe".to_string();
    let args = vec![
        "-NoLogo".to_string(),
        "-NoExit".to_string(),
        "-ExecutionPolicy".to_string(),
        "Bypass".to_string(),
        "-File".to_string(),
        hook_path.to_string_lossy().into_owned(),
    ];
    let mut env = std::collections::HashMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
    env.insert("COLORTERM".to_string(), "truecolor".to_string());
    Options {
        shell: Some(Shell::new(program, args)),
        working_directory: working_dir.as_ref().map(|s| std::path::PathBuf::from(s)),
        drain_on_exit: true,
        env,
        escape_args: true,
    }
}

fn run_respawn_loop(
    app: AppHandle,
    id: u32,
    pty_slot: PtySlot,
    poller: Arc<Poller>,
    hook_path: std::path::PathBuf,
    initial_cwd: Option<String>,
    killed: Arc<AtomicBool>,
) {
    let mut current_cwd = initial_cwd;
    let data_event = format!("pty-data:{}", id);
    let cwd_event = format!("cwd-changed:{}", id);
    let restart_event = format!("pty-restart:{}", id);
    let exit_event = format!("pty-exit:{}", id);

    loop {
        if killed.load(Ordering::SeqCst) {
            break;
        }

        // Create a fresh PTY wrapping the shell, starting in the last known cwd.
        let config = build_config(&hook_path, &current_cwd);
        let size = WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 8,
            cell_height: 16,
        };

        let mut pty = match tty::new(&config, size, 0) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[catty pty {}] PTY creation failed: {:?}", id, e);
                break;
            }
        };

        // Register with the poller before storing so we don't miss early output.
        // SAFETY: the Pty outlives its registration (stored in the Arc<Mutex>).
        unsafe {
            if let Err(e) = pty.register(&poller, Event::readable(READ_TOKEN), PollMode::Level) {
                eprintln!("[catty pty {}] PTY register failed: {:?}", id, e);
                break;
            }
        }

        *pty_slot.lock() = Some(pty);

        // Tell the frontend a new shell is starting so it can reset its
        // input-line tracker / ghost-text overlay.
        let _ = app.emit(&restart_event, ());

        // Run the read loop for this PTY instance.
        let result = run_read_loop(
            &app,
            id,
            &pty_slot,
            &poller,
            &data_event,
            &cwd_event,
            &mut current_cwd,
        );

        // Deregister and drop the old PTY.
        {
            let mut slot = pty_slot.lock();
            if let Some(p) = slot.as_mut() {
                let _ = p.deregister(&poller);
            }
            *slot = None;
        }

        match result {
            ReadResult::ChildExited => {
                if killed.load(Ordering::SeqCst) {
                    break; // user killed the tab — don't respawn
                }
                // Shell exited (e.g. a TUI app took it down). Respawn.
                eprintln!(
                    "[catty pty {}] shell exited, respawning in {:?}",
                    id, current_cwd
                );
                continue;
            }
            ReadResult::Killed | ReadResult::Fatal => {
                break;
            }
        }
    }

    let _ = app.emit(&exit_event, id);
}

enum ReadResult {
    /// The child process exited — respawn if not killed.
    ChildExited,
    /// The session was killed externally.
    Killed,
    /// An unrecoverable I/O error occurred.
    Fatal,
}

fn run_read_loop(
    app: &AppHandle,
    id: u32,
    pty_slot: &PtySlot,
    poller: &Arc<Poller>,
    data_event: &str,
    cwd_event: &str,
    cwd: &mut Option<String>,
) -> ReadResult {
    let mut scanner = Osc7Scanner::new();
    let mut events = polling::Events::new();

    // Log the first non-empty chunk to detect clear-screen sequences.
    let mut first_data = true;

    loop {
        if pty_slot.lock().is_none() {
            return ReadResult::Killed;
        }

        events.clear();
        match poller.wait(&mut events, Some(Duration::from_millis(500))) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return ReadResult::Fatal,
        }

        for ev in events.iter() {
            match ev.key {
                READ_TOKEN => {
                    if ev.is_interrupt() || !ev.readable {
                        continue;
                    }
                    let mut slot = pty_slot.lock();
                    let p = match slot.as_mut() {
                        Some(p) => p,
                        None => return ReadResult::Killed,
                    };
                    let mut buf = [0u8; 0x10000];
                    loop {
                        match p.reader().read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                let (clean, new_cwd) = scanner.feed(&buf[..n]);
                                if let Some(c) = new_cwd {
                                    *cwd = Some(c.clone());
                                    if let Some(state) =
                                        app.try_state::<std::sync::Arc<AppState>>()
                                    {
                                        if let Some(session) =
                                            state.sessions.lock().get(&id)
                                        {
                                            session.set_cwd(c.clone());
                                        }
                                    }
                                    let _ = app.emit(cwd_event, &c);
                                }
                                if !clean.is_empty() {
                                    if first_data {
                                        first_data = false;
                                        eprintln!(
                                            "[catty pty {}] FIRST_DATA chunk ({} bytes, clean={})",
                                            id,
                                            n,
                                            String::from_utf8_lossy(&clean)
                                                .escape_debug()
                                                .take(200)
                                                .collect::<String>()
                                        );
                                    }
                                    let b64 = base64::engine::general_purpose::STANDARD
                                        .encode(&clean);
                                    let _ = app.emit(data_event, &b64);
                                }
                            }
                            Err(e)
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    || e.kind() == std::io::ErrorKind::Interrupted =>
                            {
                                break;
                            }
                            Err(_) => return ReadResult::Fatal,
                        }
                    }
                }
                CHILD_TOKEN => {
                    let mut slot = pty_slot.lock();
                    let p = match slot.as_mut() {
                        Some(p) => p,
                        None => return ReadResult::Killed,
                    };
                    if let Some(ChildEvent::Exited(code)) = p.next_child_event() {
                        eprintln!(
                            "[catty pty {}] CHILD_EXITED code={:?}",
                            id, code
                        );
                        // Drain any remaining output before respawning.
                        let mut buf = [0u8; 0x10000];
                        loop {
                            match p.reader().read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    let (clean, new_cwd) = scanner.feed(&buf[..n]);
                                    if let Some(c) = new_cwd {
                                        *cwd = Some(c.clone());
                                        if let Some(state) =
                                            app.try_state::<std::sync::Arc<AppState>>()
                                        {
                                            if let Some(session) =
                                                state.sessions.lock().get(&id)
                                            {
                                                session.set_cwd(c.clone());
                                            }
                                        }
                                        let _ = app.emit(cwd_event, &c);
                                    }
                                    if !clean.is_empty() {
                                        let b64 = base64::engine::general_purpose::STANDARD
                                            .encode(&clean);
                                        let _ = app.emit(data_event, &b64);
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        return ReadResult::ChildExited;
                    }
                }
                _ => {}
            }
        }
    }
}
