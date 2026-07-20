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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_osc7_bel_terminated() {
        let mut s = Osc7Scanner::new();
        let (out, cwd) = s.feed(b"hello \x1b]7;file://host/C:/Users/dog\x07 world");
        assert_eq!(out, b"hello  world");
        assert_eq!(cwd.as_deref(), Some("C:/Users/dog"));
    }

    #[test]
    fn parses_osc7_st_terminated() {
        let mut s = Osc7Scanner::new();
        let (out, cwd) = s.feed(b"\x1b]7;file:///D:/foo\x1b\\");
        assert_eq!(out, b"");
        assert_eq!(cwd.as_deref(), Some("D:/foo"));
    }

    #[test]
    fn handles_split_chunks() {
        let mut s = Osc7Scanner::new();
        let (out1, c1) = s.feed(b"abc \x1b]7;file://h/");
        assert_eq!(out1, b"abc ");
        assert!(c1.is_none());
        let (out2, c2) = s.feed(b"X:/path\x07 def");
        assert_eq!(out2, b" def");
        assert_eq!(c2.as_deref(), Some("X:/path"));
    }

    #[test]
    fn passes_other_osc_through() {
        let mut s = Osc7Scanner::new();
        let (out, cwd) = s.feed(b"\x1b]0;title\x07");
        assert_eq!(out, b"\x1b]0;title\x07");
        assert!(cwd.is_none());
    }
}
