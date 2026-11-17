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
