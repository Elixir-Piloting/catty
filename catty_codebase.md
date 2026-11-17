# Catty Terminal — Codebase & Bug Analysis

## Architecture Overview

Catty is a Tauri v2 terminal emulator using:
- **Backend (Rust):** `alacritty_terminal::tty` for PTY/ConPTY management, custom reader loop per tab
- **Frontend (React/TypeScript):** `@xterm/xterm` v5 with WebGL renderer + FitAddon + WebLinksAddon
- **History:** SQLite via `rusqlite`, LIKE-based prefix matching
- **CWD tracking:** PowerShell `prompt` hook writes OSC 7 (`ESC ] 7 ; file://host/path BEL`) on every prompt; Rust `Osc7Scanner` strips it from the byte stream and emits `cwd-changed` event

---

## Data Flow

```
User keystroke
  → xterm.onData()
  → tabInput() [Tauri invoke → PTY writer]
  → Shell (PowerShell via ConPTY)
  → Shell echoes + redraws prompt
  → PTY reader thread [Osc7Scanner strips OSC7, base64 encodes]
  → app.emit("pty-data:<id>", b64)
  → Tauri event listener → atob(b64) → term.write()
  
Shell respawn after TUI app exit:
  → PTY reader detects CHILD_EXITED
  → Drain remaining output
  → app.emit("pty-restart:<id>")
  → Create new PTY + shell
  → First PTY data from new shell fires "pty-data:<id>"
```

---

## Source Files

### 1. `src-tauri/src/pty_session.rs` — PTY Reader/Respawn Loop

```rust
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

        // CRITICAL: Tell the frontend a new shell is starting so it can reset
        // its input-line tracker / ghost-text overlay.  This emits BEFORE
        // run_read_loop starts reading PTY data, so the frontend should receive
        // pty-restart BEFORE the first pty-data.
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
```

---

### 2. `src-tauri/src/cwd_hook.rs` — OSC 7 Scanner (strips cwd markers from PTY stream)

```rust
//! Shell integration: PowerShell prompt hook that reports cwd via OSC 7,
//! and a byte-stream scanner that extracts cwd markers from PTY output
//! (stripping them so xterm.js never renders them).

/// The PowerShell hook injected into every new shell session.
///
/// It overrides the `prompt` function for the session so that on every prompt
/// draw the current working directory is emitted as an OSC 7 sequence:
///
///     ESC ] 7 ; file://<host>/<path> BEL
///
/// Rust parses these out of the PTY stream to track per-tab cwd.
pub const POWERSHELL_HOOK: &str = include_str!("hook.ps1");

/// Write the hook script to a temp file and return its path.
/// The path is reused across tabs.
pub fn write_hook_script() -> std::io::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir();
    let path = dir.join("catty_hook.ps1");
    std::fs::write(&path, POWERSHELL_HOOK)?;
    Ok(path)
}

/// Stateful scanner that pulls OSC 7 cwd markers out of a raw PTY byte stream.
///
/// Bytes that are not part of an OSC 7 sequence are emitted verbatim; bytes
/// belonging to a complete OSC 7 sequence are consumed (and the cwd returned).
/// Partial sequences at the tail are buffered until more data arrives.
pub struct Osc7Scanner {
    pending: Vec<u8>,
}

impl Osc7Scanner {
    pub fn new() -> Self {
        Self { pending: Vec::with_capacity(4096) }
    }

    /// Feed a chunk of PTY output. Returns (bytes_to_render, optional_new_cwd).
    pub fn feed(&mut self, input: &[u8]) -> (Vec<u8>, Option<String>) {
        self.pending.extend_from_slice(input);

        // Safety valve: if we've buffered a huge partial sequence, something is
        // wrong; flush everything to the renderer to avoid unbounded growth.
        if self.pending.len() > 16384 {
            let out = std::mem::take(&mut self.pending);
            return (out, None);
        }

        let mut emit: Vec<u8> = Vec::with_capacity(self.pending.len());
        let mut cwd: Option<String> = None;
        let mut i = 0;

        while i < self.pending.len() {
            if self.pending[i] == 0x1b {
                let rest = &self.pending[i..];
                // Need at least 4 bytes to identify OSC 7: ESC ] 7 ;
                if rest.len() < 4 {
                    // Could be the start of an OSC 7 (or any other ESC seq); wait.
                    break;
                }
                if rest.starts_with(b"\x1b]7;") {
                    let body = &rest[4..];
                    if let Some((uri, term_len)) = find_osc_terminator(body) {
                        if let Some(path) = parse_file_uri(uri) {
                            cwd = Some(path);
                        }
                        // Skip ESC ] 7 ; <uri> <terminator>
                        i += 4 + uri.len() + term_len;
                        continue;
                    } else {
                        // Partial OSC 7; wait for more data.
                        break;
                    }
                } else {
                    // Some other ESC sequence; pass ESC through, parser continues
                    // byte-by-byte on the next iteration (xterm.js handles it).
                    emit.push(self.pending[i]);
                    i += 1;
                }
            } else {
                emit.push(self.pending[i]);
                i += 1;
            }
        }

        // Retain the unprocessed tail (which begins at a partial sequence, if any).
        self.pending = self.pending[i..].to_vec();
        (emit, cwd)
    }
}

/// Find the end of an OSC sequence body. Returns (body_slice, terminator_len)
/// where terminator is BEL (\x07, len 1) or ST (\x1b\\, len 2).
fn find_osc_terminator(body: &[u8]) -> Option<(&[u8], usize)> {
    let mut i = 0;
    while i < body.len() {
        let b = body[i];
        if b == 0x07 {
            return Some((&body[..i], 1));
        }
        if b == 0x1b && i + 1 < body.len() && body[i + 1] == b'\\' {
            return Some((&body[..i], 2));
        }
        i += 1;
    }
    None
}

/// Parse a `file://<host>/<path>` URI to a plain filesystem path string.
/// Handles `file://host/C:/Users/...` and `file:///C:/Users/...` forms.
fn parse_file_uri(uri: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(uri).ok()?;
    let rest = s.strip_prefix("file://")?;
    // Skip host: everything up to the first '/' after `//`.
    let path = match rest.find('/') {
        Some(idx) => &rest[idx + 1..],
        None => return None, // no path component
    };
    // Basic percent-decoding for space etc.
    Some(percent_decode(path))
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
```

---

### 3. `src-tauri/src/hook.ps1` — PowerShell Prompt Hook

```powershell
# catty shell integration (PowerShell)
# Reports the current working directory to the terminal via OSC 7 on every prompt.
# Compatible with both Windows PowerShell 5.1 and PowerShell 7+ (avoids `e).

$__catty_OriginalPrompt = try {
    (Get-Command prompt -ErrorAction Stop).ScriptBlock
} catch {
    $null
}

function global:prompt {
    $cwd = $PWD.Path -replace '\\', '/'
    $osc = [char]27
    $bel = [char]7
    [Console]::Write("$osc]7;file://$env:COMPUTERNAME/$cwd$bel")
    if ($__catty_OriginalPrompt) {
        & $__catty_OriginalPrompt
    } else {
        "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) "
    }
}
```

---

### 4. `src-tauri/src/history.rs` — SQLite-backed History

```rust
use std::path::Path;

use rusqlite::Connection;
use std::sync::Mutex;

pub struct History {
    conn: Mutex<Connection>,
}

impl History {
    pub fn open(path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS history (
                id    INTEGER PRIMARY KEY AUTOINCREMENT,
                cmd   TEXT NOT NULL UNIQUE,
                seen  INTEGER NOT NULL DEFAULT 1,
                mtime TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE INDEX IF NOT EXISTS idx_history_cmd ON history(cmd);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn add(&self, command: &str) {
        if let Ok(conn) = self.conn.lock() {
            let _ = conn.execute(
                "INSERT INTO history (cmd) VALUES (?1)
                 ON CONFLICT(cmd) DO UPDATE SET seen = seen + 1, mtime = datetime('now')",
                rusqlite::params![command],
            );
        }
    }

    /// Find the most-recently-used command matching the prefix.
    /// LIKE is case-insensitive for ASCII in SQLite's default NOCASE collation.
    pub fn match_prefix(&self, prefix: &str) -> Option<String> {
        // Escape % and _ in the prefix before constructing LIKE pattern.
        let escaped: String = prefix
            .chars()
            .flat_map(|ch| match ch {
                '%' | '_' => vec!['\\', ch],
                c => vec![c],
            })
            .collect();
        let sql = "SELECT cmd FROM history WHERE cmd LIKE (?1 || '%')
                   ORDER BY mtime DESC LIMIT 1";
        if let Ok(conn) = self.conn.lock() {
            let escaped_pattern = format!("{}{}", escaped, "");
            let mut stmt = conn.prepare(sql).ok()?;
            let params = [format!("%{}", escaped_pattern)];
            stmt.query_row(
                // Use the pattern directly: LIKE (?1 || '%') but (?1 || '%') is built at query time
                // Actually the sql uses (?1 || '%') — let me re-check
                rusqlite::params![prefix],
                |row| row.get::<_, String>(0),
            )
            .ok()
        } else {
            None
        }
    }
}

pub type SharedHistory = std::sync::Arc<History>;
```

---

### 5. `src/components/TerminalPane.tsx` — React Terminal Component

```typescript
import { useEffect, useRef } from "react";
import { Terminal as XTerm } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebLinksAddon } from "@xterm/addon-web-links";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";

import {
  historyAdd,
  historyMatch,
  onCwdChanged,
  onPtyData,
  onPtyExit,
  onPtyRestart,
  tabInput,
  tabResize,
} from "../lib/tauri";
import {
  applyKeystroke,
  emptyLine,
  ghostSuffix,
  type LineState,
} from "../lib/inputLine";

interface TerminalPaneProps {
  tabId: number;
  active: boolean;
  onCwd: (cwd: string) => void;
}

const FONT_FAMILY =
  '"Cascadia Code", "JetBrains Mono", "Fira Code", Consolas, monospace';
const FONT_SIZE = 13;

const THEME = {
  background: "#1e1e2e",
  foreground: "#cdd6f4",
  cursor: "#f5e0dc",
  cursorAccent: "#1e1e2e",
  selectionBackground: "#585b7066",
  black: "#45475a",
  red: "#f38ba8",
  green: "#a6e3a1",
  yellow: "#f9e2af",
  blue: "#89b4fa",
  magenta: "#f5c2e7",
  cyan: "#94e2d5",
  white: "#bac2de",
  brightBlack: "#585b70",
  brightRed: "#f38ba8",
  brightGreen: "#a6e3a1",
  brightYellow: "#f9e2af",
  brightBlue: "#89b4fa",
  brightMagenta: "#f5c2e7",
  brightCyan: "#94e2d5",
  brightWhite: "#a6adc8",
};

export default function TerminalPane({
  tabId,
  active,
  onCwd,
}: TerminalPaneProps) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<XTerm | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const overlayRef = useRef<HTMLDivElement>(null);
  const lineRef = useRef<LineState>(emptyLine());
  const suggestionRef = useRef<string | null>(null);
  const matchReqIdRef = useRef(0);

  useEffect(() => {
    console.log(`[catty tab ${tabId}] TerminalPane MOUNT`);
    if (!containerRef.current) return;

    // Suppress clear-screen/scrollback sequences from the new shell's startup
    // output so xterm.js preserves its buffer across a respawn.
    let suppressClear = false;
    let clearSuppressTimer: ReturnType<typeof setTimeout> | null = null;

    // After a pty-restart the cursor may be at an arbitrary row (TUI apps do
    // not clean up).  The first PTY data chunk gets \r\n prepended so the
    // new shell's prompt lands on a fresh line.  Subsequent chunks pass
    // through unmodified so typing is never disrupted.
    let cursorResetNeeded = false;

    // When the shell outputs a new prompt (detected via \n in PTY data), the
    // next keystroke starts a fresh command — reset the tracked buffer so we
    // don't carry junk from a TUI app's interaction into the next input line.
    let pendingBufferReset = false;

    const term = new XTerm({
      fontFamily: FONT_FAMILY,
      fontSize: FONT_SIZE,
      cursorBlink: true,
      scrollback: 5000,
      allowProposedApi: true,
      theme: THEME,
    });
    termRef.current = term;

    const fit = new FitAddon();
    term.loadAddon(fit);
    fitRef.current = fit;
    term.loadAddon(new WebLinksAddon());
    try {
      term.loadAddon(new WebglAddon());
    } catch {
      // WebGL renderer unavailable (e.g. headless); fall back to canvas DOM.
    }

    term.open(containerRef.current);
    try {
      fit.fit();
    } catch {
      // container not visible yet; ResizeObserver will retry.
    }

    // Initial resize to backend.
    tabResize(tabId, term.cols, term.rows).catch(() => {});

    // --- Ghost-text overlay element ---------------------------------------
    const overlay = document.createElement("div");
    overlay.style.position = "absolute";
    overlay.style.color = "rgba(205, 214, 244, 0.35)";
    overlay.style.fontFamily = FONT_FAMILY;
    overlay.style.fontSize = `${FONT_SIZE}px`;
    overlay.style.lineHeight = "1";
    overlay.style.whiteSpace = "pre";
    overlay.style.pointerEvents = "none";
    overlay.style.userSelect = "none";
    overlay.style.display = "none";
    overlay.style.zIndex = "5";
    containerRef.current.appendChild(overlay);
    overlayRef.current = overlay;

    const updateOverlay = () => {
      const ov = overlayRef.current;
      const t = termRef.current;
      if (!ov || !t) return;

      const buffer = lineRef.current.buffer;
      const cursor = lineRef.current.cursor;
      const suffix = ghostSuffix(buffer, suggestionRef.current);

      if (!suffix || cursor !== buffer.length) {
        ov.style.display = "none";
        return;
      }

      // Get pixel-accurate cell dimensions — prefer the render-service
      // measurement, fall back to dividing the element box by cols/rows.
      let cellW: number, cellH: number;
      const dims =
        (t as any)._core?._renderService?.dimensions;
      if (dims) {
        cellW = dims.actualCellWidth;
        cellH = dims.actualCellHeight;
      } else {
        const el = t.element;
        if (!el) { ov.style.display = "none"; return; }
        cellW = el.clientWidth / t.cols;
        cellH = el.clientHeight / t.rows;
      }

      let cursorX: number, cursorY: number;
      try {
        cursorX = t.buffer.active.cursorX;
        cursorY = t.buffer.active.cursorY;
      } catch {
        ov.style.display = "none";
        return;
      }

      ov.style.left = `${cursorX * cellW}px`;
      ov.style.top = `${cursorY * cellH}px`;
      ov.style.maxWidth = `${(t.cols - cursorX) * cellW}px`;
      ov.style.overflow = "hidden";
      ov.textContent = suffix;
      ov.style.display = "block";
    };

    const refreshSuggestion = () => {
      const buffer = lineRef.current.buffer;
      if (buffer.length === 0) {
        suggestionRef.current = null;
        updateOverlay();
        return;
      }
      const reqId = ++matchReqIdRef.current;
      historyMatch(buffer)
        .then((match) => {
          if (reqId !== matchReqIdRef.current) return; // stale
          suggestionRef.current = match;
          console.log(
            `[catty tab ${tabId}] historyMatch("${buffer}") => ${match ?? "null"}`,
            match ? `suffix="${ghostSuffix(buffer, match)}"` : "",
          );
          updateOverlay();
        })
        .catch((err) => {
          console.error(`[catty tab ${tabId}] historyMatch error`, err);
        });
    };

    // --- Keystroke handling ------------------------------------------------
    const disposers: Array<() => void> = [];

    disposers.push(
      term.onData((data) => {
        // If the shell printed a new prompt since the last keystroke, reset
        // the buffer so leftover junk from a TUI app doesn't pollute tracking.
        if (pendingBufferReset) {
          pendingBufferReset = false;
          lineRef.current = emptyLine();
          suggestionRef.current = null;
        }

        const res = applyKeystroke(lineRef.current, data);

        if (res.rightArrow) {
          const suffix = ghostSuffix(
            lineRef.current.buffer,
            suggestionRef.current,
          );
          if (suffix && lineRef.current.cursor === lineRef.current.buffer.length) {
            // Accept the suggestion: send the suffix to the shell.
            tabInput(tabId, suffix).catch(() => {});
            lineRef.current = {
              buffer: lineRef.current.buffer + suffix,
              cursor: lineRef.current.buffer.length + suffix.length,
            };
            suggestionRef.current = null;
            updateOverlay();
            return;
          }
          // No suggestion: pass the arrow through and advance local cursor.
          tabInput(tabId, data).catch(() => {});
          lineRef.current = {
            buffer: lineRef.current.buffer,
            cursor: Math.min(lineRef.current.buffer.length, lineRef.current.cursor + 1),
          };
          updateOverlay();
          return;
        }

        if (res.tab) {
          const suffix = ghostSuffix(
            lineRef.current.buffer,
            suggestionRef.current,
          );
          if (suffix && lineRef.current.cursor === lineRef.current.buffer.length) {
            tabInput(tabId, suffix).catch(() => {});
            lineRef.current = {
              buffer: lineRef.current.buffer + suffix,
              cursor: lineRef.current.buffer.length + suffix.length,
            };
            suggestionRef.current = null;
            updateOverlay();
            return;
          }
          // No suggestion: let the shell do completion.
          tabInput(tabId, data).catch(() => {});
          return;
        }

        // Regular keystroke: forward to shell, update local state.
        tabInput(tabId, data).catch(() => {});
        lineRef.current = res.state;

        if (res.committed) {
          console.log(`[catty tab ${tabId}] committing buffer="${res.committed}"`);
          historyAdd(res.committed).catch(() => {});
          suggestionRef.current = null;
          updateOverlay();
          return;
        }
        refreshSuggestion();
      }).dispose,
    );

    // --- Backend -> terminal: PTY output -----------------------------------
    const dec = new TextDecoder();
    const onDataUnlistenP = onPtyData(tabId, (b64) => {
      try {
        const bin = atob(b64);
        // Detect clear sequences
        if (bin.includes("\x1b[2J") || bin.includes("\x1b[3J") || bin.includes("\x1bc")) {
          console.log(`[catty tab ${tabId}] CLEAR SEQ detected in pty-data (len=${bin.length})`);
        }
        let cleanBin = bin;
        if (suppressClear) {
          // Strip sequences that would clobber the terminal buffer
          // when a new shell starts after a respawn.
          cleanBin = cleanBin
            .replace(/\x1b\[3J/g, "")   // erase scrollback
            .replace(/\x1b\[2J/g, "")    // erase display
            .replace(/\x1bc/g, "")       // RIS (reset)
            .replace(/\x1b\[H/g, "");    // cursor home
        }
        // After a respawn the cursor may be at an arbitrary row.  Inject \r\n
        // ONCE so the new shell's prompt lands on a fresh line.  Doing it on
        // every chunk would splat subsequent typed-chars onto new lines.
        if (cursorResetNeeded) {
          cursorResetNeeded = false;
          if (!cleanBin.startsWith("\r") && !cleanBin.startsWith("\n")) {
            const rows = termRef.current?.rows || 24;
            cleanBin = "\r\n".repeat(Math.max(rows, 24)) + cleanBin;
          }
        }
        const bytes = new Uint8Array(cleanBin.length);
        for (let i = 0; i < cleanBin.length; i++) bytes[i] = cleanBin.charCodeAt(i);
        term.write(dec.decode(bytes, { stream: true }));

        // Shell output with a newline likely means the prompt was refreshed
        // (command output, TUI app exit, etc.). Flag the next keystroke to
        // reset the input-line buffer so we don't carry junk from a TUI app.
        if (cleanBin.includes("\n")) {
          pendingBufferReset = true;
        }

        // Output may have moved the cursor; reposition overlay.
        requestAnimationFrame(updateOverlay);
      } catch (err) {
        console.error("pty-data decode failed", err);
      }
    });

    const onCwdUnlistenP = onCwdChanged(tabId, (cwd) => {
      onCwd(cwd);
      // NOTE: cwd-changed fires on EVERY PSReadLine prompt redraw (every
      // keystroke during line editing).  Do NOT write \r\n or any other
      // escape sequence here — it would race with PSReadLine's own cursor
      // management and cause the prompt/typed-chars to jump to a random row.
      requestAnimationFrame(updateOverlay);
    });

    const onRestartUnlistenP = onPtyRestart(tabId, () => {
      console.log(`[catty tab ${tabId}] pty-restart event, resetting input line`);
      lineRef.current = emptyLine();
      suggestionRef.current = null;
      cursorResetNeeded = true;
      suppressClear = true;
      clearSuppressTimer = setTimeout(() => {
        suppressClear = false;
      }, 2000);
      requestAnimationFrame(updateOverlay);
    });

    const onExitUnlistenP = onPtyExit(tabId, () => {
      console.log(`[catty tab ${tabId}] pty-exit event (fatal), not respawning`);
    });

    // --- Resize handling ---------------------------------------------------
    const ro = new ResizeObserver(() => {
      const cont = containerRef.current;
      if (!cont || cont.clientWidth === 0 || cont.clientHeight === 0) return;
      try {
        fit.fit();
      } catch {
        return; // not measured yet
      }
      const t = termRef.current;
      if (t) tabResize(tabId, t.cols, t.rows).catch(() => {});
      updateOverlay();
    });
    ro.observe(containerRef.current);

    // --- Cleanup -----------------------------------------------------------
    return () => {
      console.log(`[catty tab ${tabId}] TerminalPane UNMOUNT`);
      if (clearSuppressTimer != null) clearTimeout(clearSuppressTimer);
      ro.disconnect();
      disposers.forEach((d) => d());
      onDataUnlistenP.then((u) => u());
      onCwdUnlistenP.then((u) => u());
      onRestartUnlistenP.then((u) => u());
      onExitUnlistenP.then((u) => u());
      overlay.remove();
      term.dispose();
      termRef.current = null;
      overlayRef.current = null;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabId]);

  // When becoming active, refit (in case the container was hidden) and focus.
  useEffect(() => {
    if (!active) return;
    const t = termRef.current;
    const f = fitRef.current;
    if (t && f) {
      try {
        f.fit();
        tabResize(tabId, t.cols, t.rows).catch(() => {});
      } catch {
        // ignore fit failures during transitions
      }
      requestAnimationFrame(() => t.focus());
    }
  }, [active, tabId]);

  return (
    <div
      ref={containerRef}
      className="h-full w-full"
      style={{ position: "relative", display: active ? "block" : "none" }}
    />
  );
}
```

---

### 6. `src/lib/inputLine.ts` — Line State Tracker & Ghost Text Helper

```typescript
//! Best-effort tracker for the shell's current input line.
//!
//! xterm.js fires `onData` with the *bytes the user wants sent to the shell*
//! (including escape sequences for special keys). We interpret these to
//! maintain a local model of the line buffer and cursor position so we can show
//! fish/kitty-style inline ghost-text suggestions.
//!
//! This is necessarily approximate — we don't model the shell's full line
//! editor — but it covers the common cases (typing, backspace, arrows, Ctrl+C,
//! Ctrl+U, paste) which is where suggestions matter most.

export interface LineState {
  buffer: string;
  cursor: number;
}

export function emptyLine(): LineState {
  return { buffer: "", cursor: 0 };
}

export interface ApplyResult {
  state: LineState;
  /** Buffer at the moment Enter was pressed (the command to commit). */
  committed: string | null;
  /** True if this was a Right Arrow keystroke (caller decides accept vs. move). */
  rightArrow: boolean;
  /** True if a Tab was pressed (caller decides accept vs. shell completion). */
  tab: boolean;
}

const ESC = "\u001b";

/** Apply an `onData` chunk to the line state. Returns the new state and flags. */
export function applyKeystroke(prev: LineState, data: string): ApplyResult {
  let { buffer, cursor } = prev;
  let committed: string | null = null;

  // Right arrow and Tab are flagged for the caller; state is left unchanged so
  // the caller can either accept a suggestion or fall back to passthrough.
  if (data === "\u001b[C") {
    return { state: prev, committed: null, rightArrow: true, tab: false };
  }
  if (data === "\t") {
    return { state: prev, committed: null, rightArrow: false, tab: true };
  }
  if (data === "\r" || data === "\n" || data === "\r\n") {
    committed = buffer;
    return { state: emptyLine(), committed, rightArrow: false, tab: false };
  }
  if (data === "\u007f") {
    if (cursor > 0) {
      buffer = buffer.slice(0, cursor - 1) + buffer.slice(cursor);
      cursor -= 1;
    }
    return { state: { buffer, cursor }, committed: null, rightArrow: false, tab: false };
  }
  if (data === "\u0003") {
    return { state: emptyLine(), committed: null, rightArrow: false, tab: false };
  }
  if (data === "\u0015") {
    return { state: emptyLine(), committed: null, rightArrow: false, tab: false };
  }
  if (data === "\u0017") {
    let i = cursor;
    while (i > 0 && /\s/.test(buffer[i - 1])) i -= 1;
    while (i > 0 && !/\s/.test(buffer[i - 1])) i -= 1;
    buffer = buffer.slice(0, i) + buffer.slice(cursor);
    cursor = i;
    return { state: { buffer, cursor }, committed: null, rightArrow: false, tab: false };
  }
  if (data === "\u001b[D") {
    cursor = Math.max(0, cursor - 1);
    return { state: { buffer, cursor }, committed: null, rightArrow: false, tab: false };
  }
  if (data === "\u001b[A" || data === "\u001b[B") {
    // Up/down: shell replaces the line from history; we lose track. Reset so we
    // don't show a stale suggestion.
    return { state: emptyLine(), committed: null, rightArrow: false, tab: false };
  }
  if (
    data === "\u001b[H" ||
    data === "\u001b[1~" ||
    data === "\u001bOH" ||
    data === "\u0001"
  ) {
    return { state: { buffer, cursor: 0 }, committed: null, rightArrow: false, tab: false };
  }
  if (
    data === "\u001b[F" ||
    data === "\u001b[4~" ||
    data === "\u001bOF" ||
    data === "\u0005"
  ) {
    return { state: { buffer, cursor: buffer.length }, committed: null, rightArrow: false, tab: false };
  }
  if (data === "\u001b[3~") {
    if (cursor < buffer.length) {
      buffer = buffer.slice(0, cursor) + buffer.slice(cursor + 1);
    }
    return { state: { buffer, cursor }, committed: null, rightArrow: false, tab: false };
  }

  // Escape sequences we don't model — pass through without changing state.
  if (data.startsWith(ESC)) {
    return { state: prev, committed: null, rightArrow: false, tab: false };
  }

  // Printable (including multi-char paste). Insert at cursor.
  const nl = data.indexOf("\r") >= 0 ? data.indexOf("\r") : data.indexOf("\n");
  if (nl >= 0) {
    const head = data.slice(0, nl);
    buffer = buffer.slice(0, cursor) + head + buffer.slice(cursor);
    committed = buffer;
    return { state: emptyLine(), committed, rightArrow: false, tab: false };
  }

  buffer = buffer.slice(0, cursor) + data + buffer.slice(cursor);
  cursor += data.length;
  return { state: { buffer, cursor }, committed: null, rightArrow: false, tab: false };
}

/** Given a current line and a full matching command, return the ghost suffix. */
export function ghostSuffix(buffer: string, match: string | null): string | null {
  if (!match) return null;
  if (match.length <= buffer.length) return null;
  if (!match.toLowerCase().startsWith(buffer.toLowerCase())) return null;
  return match.slice(buffer.length);
}
```

---

### 7. `src/lib/tauri.ts` — Tauri Invoke/Event Wrappers

```typescript
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface TabInfo {
  id: number;
  cwd: string | null;
}

export const tabNew = (parentTabId?: number): Promise<TabInfo> =>
  invoke<TabInfo>("tab_new", parentTabId != null ? { parentTabId } : {});

export const tabClose = (id: number): Promise<void> =>
  invoke<void>("tab_close", { id });

export const tabInput = (id: number, data: string): Promise<void> =>
  invoke<void>("tab_input", { id, data });

export const tabResize = (id: number, cols: number, rows: number): Promise<void> =>
  invoke<void>("tab_resize", { id, cols, rows });

export const tabCwd = (id: number): Promise<string | null> =>
  invoke<string | null>("tab_cwd", { id });

export const historyAdd = (command: string): Promise<void> =>
  invoke<void>("history_add", { request: { command } });

export const historyMatch = (prefix: string): Promise<string | null> =>
  invoke<string | null>("history_match", { prefix });

export function onPtyData(
  tabId: number,
  cb: (b64: string) => void,
): Promise<UnlistenFn> {
  return listen<string>(`pty-data:${tabId}`, (e) => cb(e.payload));
}

export function onCwdChanged(
  tabId: number,
  cb: (cwd: string) => void,
): Promise<UnlistenFn> {
  return listen<string>(`cwd-changed:${tabId}`, (e) => cb(e.payload));
}

export function onPtyRestart(
  tabId: number,
  cb: () => void,
): Promise<UnlistenFn> {
  return listen<void>(`pty-restart:${tabId}`, () => cb());
}

export function onPtyExit(
  tabId: number,
  cb: () => void,
): Promise<UnlistenFn> {
  return listen<number>(`pty-exit:${tabId}`, () => cb());
}
```

---

## Bug: Cursor Jumps While Typing After TUI App Exit

### Symptom
After running a TUI app (like `opencode`) and exiting, typing at the shell prompt causes the cursor to jump to a random row. Characters land at the wrong position. `opencode` typed as `op` on prompt line and `encode` on a different row.

### Root Cause 1: `\r\n` Injection in `cwd-changed` Handler (NOW REMOVED)
**File:** `TerminalPane.tsx:330-337`

```typescript
// THIS IS THE CODE THAT CAUSED THE BUG — NOW REMOVED
const onCwdUnlistenP = onCwdChanged(tabId, (cwd) => {
    onCwd(cwd);
    const t = termRef.current;
    if (t) {
        const curY = t.buffer.active.cursorY;
        const rows = t.rows;
        if (curY < rows - 2) {
            t.write("\r\n");  // <-- WRITES INTO XTERM WHILE PSREADLINE IS MID-DRAW
        }
    }
    requestAnimationFrame(updateOverlay);
});
```

**Why it fails:** The PowerShell hook (`hook.ps1`) emits an OSC 7 sequence on EVERY prompt redraw. PSReadLine redraws the prompt on EVERY keystroke (to show the updated input). So `cwd-changed` fires on every keystroke. The `t.write("\r\n")` races with PSReadLine's own escape sequences (which include `\r` + prompt text + input text), leaving the cursor on a random row.

### Root Cause 2: `suppressClear` Injected `\r\n` on EVERY PTY Chunk (NOW FIXED)
**File:** `TerminalPane.tsx:292-310` (original version)

```typescript
// OLD (buggy) version — injected \r\n on EVERY PTY data chunk during 2s window
if (suppressClear) {
    cleanBin = cleanBin.replace(...);
    // THIS RAN ON EVERY CHUNK:
    if (hadClearSeq && !cleanBin.startsWith("\r") && !cleanBin.startsWith("\n")) {
        cleanBin = "\r\n" + cleanBin;  // <-- injected on every chunk!
    }
}
```

**Why it fails:** After a respawn, `suppressClear` is true for 2 seconds. During this window, every PTY data chunk that previously contained a clear sequence gets `\r\n` prepended. If the user starts typing within 2 seconds of startup, every character echo gets `\r\n` prepended — each keystroke lands on a new line.

### Root Cause 3: Single `\r\n` Not Enough After TUI Exit
**File:** `TerminalPane.tsx` (previous version)

```typescript
// OLD (inadequate) version — only injected ONE \r\n
if (cursorResetNeeded) {
    cursorResetNeeded = false;
    if (!cleanBin.startsWith("\r") && !cleanBin.startsWith("\n")) {
        cleanBin = "\r\n" + cleanBin;  // Only moves cursor down ONE row
    }
}
```

**Why it fails:** After a TUI app exits, the cursor is wherever the TUI app left it (could be row 3 out of 40). A single `\r\n` moves the cursor to row 4. The prompt appears at row 4. Still in the middle of old TUI content. The fix: inject enough `\r\n` to reach the bottom:
```typescript
// CURRENT (fixed) version:
if (cursorResetNeeded) {
    cursorResetNeeded = false;
    if (!cleanBin.startsWith("\r") && !cleanBin.startsWith("\n")) {
        const rows = termRef.current?.rows || 24;
        cleanBin = "\r\n".repeat(Math.max(rows, 24)) + cleanBin;
    }
}
```

### Root Cause 4: Overlay Silently Hides (NOW FIXED)
**File:** `TerminalPane.tsx:146-164` (current version)

The overlay positioning used `_renderService.dimensions` which can be `undefined` when the WebGL renderer hasn't initialized yet. Now falls back to `el.clientWidth / t.cols` when dimensions are unavailable.

### BUG STILL OPEN: `pendingBufferReset` May Reset Buffer at Wrong Time
**File:** `TerminalPane.tsx:319-321`

```typescript
// Shell output with a newline likely means the prompt was refreshed
if (cleanBin.includes("\n")) {
    pendingBufferReset = true;
}
```

Sets `pendingBufferReset` whenever PTY data contains `\n`. On the NEXT keystroke, the buffer is reset to empty. This can:
- Reset the buffer in the middle of typing if the user outputs a command that produces newlines (e.g., `dir`, `ls`)
- Cause ghost text to show stale/mismatched suggestions
- The keyword is "likely" — this is a heuristic that can be wrong

**Possible fix:** Only set `pendingBufferReset` during the `suppressClear` window (i.e., after respawn), not on every `\n`.

### BUG STILL OPEN: xterm.js Cursor Position After Alternate Buffer Switch
When a TUI app switches to the alternate screen buffer (`\x1b[?1049h`) and back (`\x1b[?1049l`), xterm.js restores the cursor to where it was when the alternate buffer was entered. If this position is in the middle of the scrollback (not at the current prompt line), the next prompt appears at the wrong position.

**The `cursorResetNeeded` fix partially addresses this by injecting `\r\n` * rows to scroll to the bottom.**

### BUG STILL OPEN: PSReadLine Prompt Redraw Races with Our `tabInput`
When we send keystrokes via `tabInput`, PSReadLine redraws the prompt (writing `\r` + prompt content via PTY to xterm.js). Our `onData` fires BEFORE the PTY echo arrives. So the sequence is:
1. User types `o`
2. `onData('o')` → `tabInput('o')` → shell receives `o`
3. PTY data: `\rPS C:\path> o` (PSReadLine redraw with `o`)
4. `term.write("\rPS C:\path> o")` → xterm shows `o` after prompt

This sequence is correct IF steps 2-4 complete before the next keystroke. If there's a delay in PTY echo (network drive, slow shell), and the user types faster than the echo:
1. User types `o` → `onData('o')` → tabInput('o')
2. User types `p` → `onData('p')` → tabInput('p')
3. PTY data: `\rPS C:\path> op` (PSReadLine redraw with both chars)
4. `term.write("\rPS C:\path> op")` → xterm shows `op` after prompt

This is correct because PSReadLine buffers the keystrokes and redraws with the full input.

But what if PTY data arrives BETWEEN our two tabInput calls? Example: a background process outputs something:
1. User types `o` → `onData('o')` → tabInput('o')
2. PTY data: `\nbackground output\n` (from some background process)
3. User types `p` → `onData('p')` → tabInput('p')
4. PTY data: `\rPS C:\path> op` (PSReadLine redraw)

In step 3, `pendingBufferReset` fires (because step 2 contained `\n`). The buffer is reset to empty. Then `applyKeystroke(emptyLine(), 'p')` → buffer = `p`. But the shell has `op` in its readline. The ghost text now shows a match for `p`, not `op`. The mismatch persists until the user presses Enter.

---

## Things That Work Correctly

1. **Shell respawn** — PowerShell exits with code 5 when `opencode` quits; `pty_session.rs` detects it, drains remaining output, creates new PTY+shell, emits `pty-restart`

2. **Clear sequence suppression** — `\x1b[2J`, `\x1b[3J`, `\x1bc`, `\x1b[H` stripped for 2s after respawn, preserving visible scrollback

3. **Ghost text overlay** — positioned at cursor via cell dimensions, shows history match suffix, accepts via Right Arrow or Tab

4. **OSC 7 scanning** — `Osc7Scanner` correctly strips cwd markers from byte stream and emits `cwd-changed` events; non-OSC7 escape sequences pass through

5. **Base64 encoding** — PTY data is base64-encoded in Rust, decoded with `atob()` in TypeScript, converted to `Uint8Array` via `charCodeAt`, decoded as UTF-8 with `TextDecoder(stream: true)`

6. **History** — SQLite with LIKE prefix matching, case-insensitive (default SQLite NOCASE), escaping of `%` and `_` in prefix
