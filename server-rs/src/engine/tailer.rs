use crate::engine::stream::{parse_line, ClaudeEvent};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Reads complete newline-delimited lines appended to a file since a byte offset, parsing each into
/// `ClaudeEvent`s via `parse_line`. Stateful: tracks the byte offset consumed + a carry buffer for a
/// partial last line.
pub struct EventTailer {
    path: PathBuf,
    offset: u64,
    buffer: Vec<u8>, // carry: bytes after the last newline
}

impl EventTailer {
    pub fn new(path: impl AsRef<Path>, start_offset: u64) -> Self {
        EventTailer {
            path: path.as_ref().to_path_buf(),
            offset: start_offset,
            buffer: Vec::new(),
        }
    }

    fn drain_lines(&mut self) -> Vec<ClaudeEvent> {
        let mut events = Vec::new();
        while let Some(nl) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=nl).collect(); // includes the '\n'
            let s = String::from_utf8_lossy(&line[..line.len() - 1]); // strip the '\n'
            events.extend(parse_line(&s));
        }
        events
    }

    pub fn poll(&mut self) -> Vec<ClaudeEvent> {
        if !self.path.exists() {
            return Vec::new();
        }
        let mut f = match File::open(&self.path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let size = match f.metadata() {
            Ok(m) => m.len(),
            Err(_) => return Vec::new(),
        };
        if size <= self.offset {
            return Vec::new();
        }
        let len = (size - self.offset) as usize;
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        // read_to_end loops over short reads (a single read() may return fewer bytes than requested
        // — e.g. EINTR under heavy parallel load) until EOF. A single read() in the final drain could
        // silently drop the tail (e.g. a large final `result` line), failing turn-completion.
        let mut buf = Vec::with_capacity(len);
        let read = match f.read_to_end(&mut buf) {
            Ok(n) => n,
            Err(_) => return Vec::new(),
        };
        self.offset += read as u64;
        self.buffer.extend_from_slice(&buf);
        self.drain_lines()
    }

    pub fn flush(&mut self) -> Vec<ClaudeEvent> {
        let rest = String::from_utf8_lossy(&self.buffer);
        let rest = rest.trim().to_string();
        if rest.is_empty() {
            return Vec::new();
        }
        self.buffer.clear();
        parse_line(&rest)
    }

    pub fn offset(&self) -> u64 {
        self.offset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const INIT: &str = r#"{"type":"system","subtype":"init","session_id":"s1"}"#;
    const TEXT: &str =
        r#"{"type":"stream_event","event":{"delta":{"type":"text_delta","text":"hi"}}}"#;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-tail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d.join("log.jsonl")
    }
    fn append(p: &Path, s: &str) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap();
        f.write_all(s.as_bytes()).unwrap();
    }

    #[test]
    fn missing_file_yields_empty() {
        let p = std::env::temp_dir()
            .join("nope-xyz-agentic")
            .join("x.jsonl");
        assert!(EventTailer::new(&p, 0).poll().is_empty());
    }

    #[test]
    fn parses_appended_lines_and_advances_offset() {
        let p = tmp();
        append(&p, &format!("{INIT}\n"));
        let mut t = EventTailer::new(&p, 0);
        assert!(
            matches!(t.poll().as_slice(), [ClaudeEvent::Init { session_id, .. }] if session_id == "s1")
        );
        assert!(t.poll().is_empty()); // nothing new
        append(&p, &format!("{TEXT}\n"));
        assert!(
            matches!(t.poll().as_slice(), [ClaudeEvent::Text { text, .. }] if text == "hi")
        );
    }

    #[test]
    fn carries_partial_line_across_polls() {
        let p = tmp();
        let mut t = EventTailer::new(&p, 0);
        append(&p, &INIT[..10]); // half a line, no newline
        assert!(t.poll().is_empty());
        append(&p, &format!("{}\n", &INIT[10..])); // complete it
        assert!(
            matches!(t.poll().as_slice(), [ClaudeEvent::Init { session_id, .. }] if session_id == "s1")
        );
    }

    #[test]
    fn flush_parses_trailing_newlineless_line() {
        let p = tmp();
        append(&p, INIT); // no trailing newline
        let mut t = EventTailer::new(&p, 0);
        assert!(t.poll().is_empty()); // incomplete → buffered
        assert!(
            matches!(t.flush().as_slice(), [ClaudeEvent::Init { session_id, .. }] if session_id == "s1")
        );
    }

    #[test]
    fn honours_non_zero_start_offset() {
        let p = tmp();
        append(&p, &format!("{INIT}\n"));
        let start = format!("{INIT}\n").len() as u64;
        let mut t = EventTailer::new(&p, start);
        assert!(t.poll().is_empty()); // skip what was already there
        append(&p, &format!("{TEXT}\n"));
        assert!(
            matches!(t.poll().as_slice(), [ClaudeEvent::Text { text, .. }] if text == "hi")
        );
    }
}
