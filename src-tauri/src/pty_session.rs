//! Per-tab PTY session built on `alacritty_terminal::tty`.
//!
//! Each session owns a `Pty` (ConPTY on Windows) and runs a dedicated reader
//! thread that:
//!   * polls the PTY reader for new output,
//!   * scans output through the OSC 7 cwd parser (stripping cwd markers),
//!   * emits `pty-data:<id>` Tauri events with base64-encoded cleaned bytes,
//!   * emits `cwd-changed:<id>` events when the shell reports a new cwd,
//!   * emits `pty-exit:<id>` when the child process exits.
//!
//! Frontend keystrokes come in via `write()`; resize via `resize()`.

use std::io::{Read as _, Write as _};
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

pub struct PtySession {
    #[allow(dead_code)]
    pub id: u32,
    pty: Arc<Mutex<tty::Pty>>,
    // Kept alive for the lifetime of the session so registration persists.
    #[allow(dead_code)]
    poller: Arc<Poller>,
    pub cwd: Mutex<Option<String>>,
}

impl PtySession {
    /// Spawn a new shell session.
    ///
    /// `working_dir` overrides the shell's startup directory (used to inherit
    /// the parent tab's last known cwd). `hook_path` is the PowerShell prompt
    /// hook script to inject.
    pub fn spawn(
        app: AppHandle,
        id: u32,
        working_dir: Option<String>,
        hook_path: &std::path::Path,
    ) -> std::io::Result<Self> {
        // Default shell is PowerShell (architected so cmd / WSL could be added
        // later). The prompt hook is only injected for PowerShell.
        let program = "powershell.exe".to_string();
        let is_powershell = true;
        let args: Vec<String> = if is_powershell {
            vec![
                "-NoLogo".to_string(),
                "-NoExit".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-File".to_string(),
                hook_path.to_string_lossy().into_owned(),
            ]
        } else {
            // cmd /k doesn't support prompt hooks the same way; spawn plain.
            Vec::new()
        };

    let mut env = std::collections::HashMap::new();
    env.insert("TERM".to_string(), "xterm-256color".to_string());
        env.insert("COLORTERM".to_string(), "truecolor".to_string());

        let config = Options {
            shell: Some(Shell::new(program, args)),
            working_directory: working_dir.as_ref().map(|s| std::path::PathBuf::from(s)),
            drain_on_exit: true,
            env,
            escape_args: true,
        };

        let size = WindowSize {
            num_lines: 24,
            num_cols: 80,
            cell_width: 8,
            cell_height: 16,
        };

        let pty = tty::new(&config, size, 0)?;
        let pty = Arc::new(Mutex::new(pty));
        let poller = Arc::new(Poller::new()?);

        {
            let mut p = pty.lock();
            // SAFETY: the Pty outlives its registration (it's owned by the Arc
            // and kept alive for the session's lifetime). Pty::register also
            // registers the child-watcher internally with PTY_CHILD_EVENT_TOKEN.
            //
            // Use PollMode::Level (matching alacritty's event_loop) so the
            // registration stays active — no reregister needed after each read.
            unsafe {
                p.register(&poller, Event::readable(READ_TOKEN), PollMode::Level)?;
            }
        }

        let session = Self {
            id,
            pty: pty.clone(),
            poller: poller.clone(),
            cwd: Mutex::new(working_dir.clone()),
        };

        // Reader thread.
        let pty_t = pty.clone();
        let poller_t = poller.clone();
        let app_t = app.clone();
        thread::Builder::new()
            .name(format!("pty-reader-{}", id))
            .spawn(move || {
                run_reader_loop(app_t, id, pty_t, poller_t);
            })?;

        Ok(session)
    }

    /// Write user keystrokes to the shell.
    pub fn write(&self, data: &[u8]) -> std::io::Result<()> {
        let mut p = self.pty.lock();
        p.writer().write_all(data)
    }

    /// Resize the underlying PTY (SIGWINCH-equivalent).
    pub fn resize(&self, cols: u16, rows: u16) {
        let mut p = self.pty.lock();
        let size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 8,
            cell_height: 16,
        };
        p.on_resize(size);
    }

    /// Forcefully terminate the child process and release PTY resources.
    pub fn kill(&self) {
        let p = self.pty.lock();
        let handle = p.child_watcher().raw_handle();
        unsafe {
            TerminateProcess(handle, 0);
            CloseHandle(handle);
        }
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.lock().clone()
    }

    pub fn set_cwd(&self, cwd: String) {
        *self.cwd.lock() = Some(cwd);
    }
}

fn run_reader_loop(
    app: AppHandle,
    id: u32,
    pty: Arc<Mutex<tty::Pty>>,
    poller: Arc<Poller>,
) {
    let mut scanner = Osc7Scanner::new();
    let mut events = polling::Events::new();
    let data_event = format!("pty-data:{}", id);
    let cwd_event = format!("cwd-changed:{}", id);
    let exit_event = format!("pty-exit:{}", id);

    loop {
        events.clear();
        match poller.wait(&mut events, Some(Duration::from_millis(500))) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }

        let mut exit = false;
        for ev in events.iter() {
            match ev.key {
                READ_TOKEN => {
                    if ev.is_interrupt() {
                        continue;
                    }
                    if !ev.readable {
                        continue;
                    }
                    // Drain the piper pipe in a tight loop until empty.
                    let mut p = pty.lock();
                    let mut buf = [0u8; 0x10000];
                    loop {
                        match p.reader().read(&mut buf) {
                            // No data available right now — back to polling.
                            Ok(0) => break,
                            Ok(n) => {
                                let (clean, cwd) = scanner.feed(&buf[..n]);
                                if let Some(cwd) = cwd {
                                    if let Some(state) =
                                        app.try_state::<std::sync::Arc<AppState>>()
                                    {
                                        if let Some(session) =
                                            state.sessions.lock().get(&id)
                                        {
                                            session.set_cwd(cwd.clone());
                                        }
                                    }
                                    let _ = app.emit(&cwd_event, &cwd);
                                }
                                if !clean.is_empty() {
                                    let b64 = base64::engine::general_purpose::STANDARD
                                        .encode(&clean);
                                    let _ = app.emit(&data_event, &b64);
                                }
                            }
                            Err(e)
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    || e.kind() == std::io::ErrorKind::Interrupted =>
                            {
                                break;
                            }
                            Err(_) => {
                                exit = true;
                                break;
                            }
                        }
                    }
                    // Level mode: registration stays active, no reregister needed.
                }
                CHILD_TOKEN => {
                    let mut p = pty.lock();
                    if let Some(ChildEvent::Exited(_)) = p.next_child_event() {
                        exit = true;
                    }
                }
                _ => {}
            }
        }

        if exit {
            break;
        }
    }

    // Deregister the PTY from the poller so it doesn't reference a closed handle.
    {
        let mut p = pty.lock();
        let _ = p.deregister(&poller);
    }

    let _ = app.emit(&exit_event, id);
}
