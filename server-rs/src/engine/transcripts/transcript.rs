use std::path::Path;
use tokio::task::spawn_blocking;

/// The `type` values that belong in the rendered (replayable) projection — the lines the Android
/// client replays as the transcript (`stream_event`/`assistant`/`result` from claude, plus the
/// engine's own `agentic_prompt`/`agent_result` markers and the bridge's `agentic_perm`/
/// `agentic_perm_resolved` permission-card markers). Single source of truth.
///
/// `agentic_perm` / `agentic_perm_resolved` MUST be here: the live WS protocol delivers cards by
/// re-reading this rendered projection on a poke (they are not `is_live_only`), and the Android
/// `buildFromLog` reconstructs the (decided) cards from these same lines on reseed. Omit them and the
/// permission/plan card never reaches the app even though the turn is correctly parked.
///
/// `workflowRun` (the card→run link marker, engine/mod.rs) is here for the same reason: the client
/// fills `WorkflowNode.runId` from it both live (cursor poke) and on reseed (`buildFromLog`); omit it
/// and a workflow card can only fall back to name matching.
fn rendered_type(ty: &str) -> bool {
    matches!(
        ty,
        "agentic_prompt"
            | "stream_event"
            | "assistant"
            | "result"
            | "agent_result"
            | "agentic_file"
            | "agentic_perm"
            | "agentic_perm_resolved"
            | "pr"
            | "workflowRun"
    )
}

/// True if a raw log line belongs in the rendered projection.
///
/// Classifies by the line's DESERIALIZED `type` field — never by a byte prefix. The engine builds its
/// own markers with `serde_json::json!`, and serde_json (no `preserve_order` feature) serializes object
/// keys ALPHABETICALLY, so `type` is NOT first — an agentic_prompt lands as
/// `{"at":…,"text":…,"type":"agentic_prompt"}`. A `starts_with("{\"type\":…")` check missed exactly
/// those lines and silently dropped every user prompt out of the projection (they vanished from the app
/// and never replayed on reseed). Keying off the parsed `type` is order-proof — and matches how
/// `parse_line` and the Android `buildFromLog` already classify these same lines.
///
/// Pulls out ONLY the borrowed `type` field — serde skips the rest of the object without building a
/// `Value` tree — so it stays cheap on large `assistant`/`result` bodies and the many `stream_event`
/// deltas. A malformed or type-less line deserializes to `None` → not rendered.
pub fn is_rendered(line: &str) -> bool {
    #[derive(serde::Deserialize)]
    struct TypeTag<'a> {
        #[serde(default, borrow, rename = "type")]
        ty: Option<&'a str>,
    }
    serde_json::from_str::<TypeTag>(line)
        .ok()
        .and_then(|t| t.ty)
        .is_some_and(rendered_type)
}

/// True if a rendered line is a streaming-text delta (`stream_event`) rather than a LOGICAL event
/// (a prompt / assistant message / tool / ask / delegate / permission card / result). One streamed
/// token = one `stream_event`, so these dominate a long session's rendered projection (often >90%).
/// [RenderedProjection::window_tail_events] uses this to budget by cards/turns, not delta noise.
/// Same cheap borrowed-`type`-only parse as [is_rendered]; a type-less/malformed line is not a delta.
pub fn is_stream_event(line: &str) -> bool {
    // Fast path: the substring is a NECESSARY condition for `"type":"stream_event"`, so its absence
    // means "not a delta" without invoking serde — cheap for the event lines window_tail_events walks.
    // A line that merely mentions the phrase in its content (e.g. an assistant message about
    // stream_event handling) still falls through to the parse below, which checks the real `type`.
    if !line.contains("stream_event") {
        return false;
    }
    #[derive(serde::Deserialize)]
    struct TypeTag<'a> {
        #[serde(default, borrow, rename = "type")]
        ty: Option<&'a str>,
    }
    serde_json::from_str::<TypeTag>(line)
        .ok()
        .and_then(|t| t.ty)
        == Some("stream_event")
}

/// Convert a RAW log offset (as the engine counts log lines) into the FILTERED offset the
/// client uses as its stream cursor: the number of rendered lines that precede `raw_offset`.
/// `POST /api/sessions/:id/messages` returns this so a resumed turn's backfill lines up with the
/// filtered log the client counted.
pub fn raw_to_rendered_offset(raw: &[String], raw_offset: usize) -> usize {
    let end = raw_offset.min(raw.len());
    raw[..end].iter().filter(|l| is_rendered(l)).count()
}

/// Rendered view; falls back to the raw lines if filtering yields nothing.
pub fn filter_rendered(raw: &[String]) -> Vec<String> {
    let filtered: Vec<String> = raw.iter().filter(|l| is_rendered(l)).cloned().collect();
    if filtered.is_empty() {
        raw.to_vec()
    } else {
        filtered
    }
}

pub struct Window {
    pub start: usize,
    pub lines: Vec<String>,
    pub total: usize,
}

/// Per-session append-only rendered projection with its own file byte cursor.
///
/// # Equivalence invariant
///
/// After every `sync()` that reads complete lines (i.e. lines terminated by `'\n'`),
/// `self.rendered == filter_rendered(complete_lines_of_file)`.
///
/// # Partial-line handling
///
/// A non-empty final line that lacks a trailing `\n` is a half-written in-flight event.
/// We intentionally do NOT include that partial line in `rendered`: a half-written line is an
/// in-flight event that should be delivered via the live stream, not the cursor-indexed rendered
/// list. The cursor converges on the next `sync()` once the `\n` lands. This is safe because the
/// Android client's rendered-coordinate `since` cursor only advances past lines that appear in
/// `rendered`, so a partial line never corrupts the cursor — it simply appears in the next sync
/// window.
///
/// # UTF-8 multibyte safety
///
/// The partial-line carry is `Vec<u8>` (raw bytes). We decode to `&str` only at newline
/// (`b'\n'`) boundaries — never on a partial byte buffer — so a multibyte UTF-8 character
/// (CJK, emoji) split across a read boundary is never corrupted.
pub struct RenderedProjection {
    rendered: Vec<String>,
    byte_offset: u64,
    raw_count: u64,
    /// Partial trailing bytes of the last incomplete line (no `\n` yet). Raw bytes — decoded
    /// only when a `\n` arrives.
    carry: Vec<u8>,
    bytes: usize, // approx memory = sum of rendered line lengths
}

impl RenderedProjection {
    pub fn new() -> Self {
        RenderedProjection {
            rendered: Vec::new(),
            byte_offset: 0,
            raw_count: 0,
            carry: Vec::new(),
            bytes: 0,
        }
    }

    pub fn count(&self) -> usize {
        self.rendered.len()
    }
    pub fn raw_count(&self) -> u64 {
        self.raw_count
    }
    pub fn byte_offset(&self) -> u64 {
        self.byte_offset
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn rendered_clone(&self) -> Vec<String> {
        self.rendered.clone()
    }

    /// Read only the new bytes `[byte_offset, EOF)` and ingest complete (newline-terminated)
    /// lines. Missing file = no-op.
    ///
    /// If the file has shrunk (rotation), the projection is reset and re-read from the start so a
    /// rotated log re-hydrates instead of freezing.
    ///
    /// `byte_offset` is advanced by the number of bytes ACTUALLY read, not by a metadata snapshot.
    pub async fn sync(&mut self, path: &Path) -> std::io::Result<()> {
        let path = path.to_path_buf();
        let offset = self.byte_offset;
        // Returns Ok(None) = file not found; Ok(Some((buf, rotated))) otherwise.
        // `rotated = true` means the file shrank and `buf` is the full file from byte 0.
        let read = spawn_blocking(move || -> std::io::Result<Option<(Vec<u8>, bool)>> {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = match std::fs::File::open(&path) {
                Ok(f) => f,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e),
            };
            let size = f.metadata()?.len();
            if size < offset {
                // Rotation: re-read whole file from the start.
                let mut buf = Vec::with_capacity(size as usize);
                f.read_to_end(&mut buf)?;
                return Ok(Some((buf, true)));
            }
            if size == offset {
                return Ok(Some((Vec::new(), false)));
            }
            f.seek(SeekFrom::Start(offset))?;
            let to_read = size - offset;
            let mut buf = Vec::with_capacity(to_read as usize);
            f.take(to_read).read_to_end(&mut buf)?;
            Ok(Some((buf, false)))
        })
        .await
        .map_err(std::io::Error::other)??;

        let Some((buf, rotated)) = read else {
            return Ok(());
        };

        if rotated {
            // Reset projection state; re-process from scratch below.
            self.rendered.clear();
            self.carry.clear();
            self.raw_count = 0;
            self.bytes = 0;
            self.byte_offset = 0;
        }

        if buf.is_empty() {
            // No new bytes (file at same size, or rotation resulted in empty file).
            return Ok(());
        }

        // Append new bytes to the byte carry, then split on b'\n'.
        // Decode only at newline boundaries — never on a partial byte buffer — so a multibyte
        // UTF-8 character (CJK, emoji) split across a read boundary is never corrupted.
        let mut carry = std::mem::take(&mut self.carry);
        carry.extend_from_slice(&buf);

        let mut start = 0;
        for i in 0..carry.len() {
            if carry[i] == b'\n' {
                let line_bytes = &carry[start..i];
                if !line_bytes.is_empty() {
                    // Safe to decode: we are at a complete line boundary.
                    let line = String::from_utf8_lossy(line_bytes);
                    self.raw_count += 1;
                    if is_rendered(&line) {
                        self.bytes += line.len();
                        self.rendered.push(line.into_owned());
                    }
                }
                start = i + 1;
            }
        }
        // Keep the trailing bytes after the last '\n' as the new byte carry.
        self.carry = carry[start..].to_vec();

        // Advance byte_offset by the number of bytes actually read (not by metadata snapshot).
        self.byte_offset += buf.len() as u64;
        Ok(())
    }

    pub fn window_tail(&self, limit: usize) -> Window {
        let start = self.rendered.len().saturating_sub(limit);
        Window {
            start,
            lines: self.rendered[start..].to_vec(),
            total: self.rendered.len(),
        }
    }

    /// Tail window budgeted by LOGICAL events (deltas included in suffix). Keeps streaming-text
    /// deltas that fall within the retained suffix. Used by the old `GET /api/sessions/:id` endpoint
    /// and the WebSocket cursor — see [window_tail_logical_events] for the Discord-style filtered variant.
    pub fn window_tail_events(&self, max_events: usize, max_lines: usize) -> Window {
        let r = &self.rendered;
        let n = r.len();
        let mut events = 0usize;
        let mut start = n;
        let mut idx = n;
        while idx > 0 {
            let i = idx - 1;
            if n - i > max_lines {
                break;
            }
            if !is_stream_event(&r[i]) {
                if events >= max_events {
                    break;
                }
                events += 1;
            }
            start = i;
            idx = i;
        }
        Window {
            start,
            lines: r[start..].to_vec(),
            total: n,
        }
    }

    /// Tail window filtered to NON-delta events only (Discord-style sealed-event tail).
    /// `max_events` = max logical events to return; `max_lines` = hard safety cap on
    /// total rendered lines to scan backward. Returns a Window whose `total` is the FULL
    /// **rendered-line count** (compatible with the WS cursor), not the logical event count.
    /// The returned `start` is the rendered-line offset of the first non-delta event.
    pub fn window_tail_logical_events(&self, max_events: usize, max_lines: usize) -> Window {
        let r = &self.rendered;
        let n = r.len();
        let total = n; // rendered-line count — same cursor space as WS
        let mut events = 0usize;
        let mut start = n;
        let mut idx = n;
        while idx > 0 {
            let i = idx - 1;
            if n - i > max_lines {
                break;
            }
            if !is_stream_event(&r[i]) {
                if events >= max_events {
                    break;
                }
                events += 1;
            }
            start = i;
            idx = i;
        }
        let filtered: Vec<String> = r[start..]
            .iter()
            .filter(|l| !is_stream_event(l))
            .take(max_events)
            .cloned()
            .collect();
        Window {
            start,
            lines: filtered,
            total,
        }
    }

    /// Filtered range: scan forward from rendered-line `start_line`, collect up to `limit`
    /// non-delta events, stop at `max_lines` total lines scanned. `start` = first event's
    /// rendered-line offset; `total` = full rendered-line count (WS-compatible cursor).
    pub fn window_filtered_range(
        &self,
        start_line: usize,
        limit: usize,
        max_lines: usize,
    ) -> Window {
        let r = &self.rendered;
        let total = r.len();
        let start_line = start_line.min(total);
        let mut found = Vec::new();
        let mut first_start = None;
        let mut scanned = 0usize;
        for (i, line) in r.iter().enumerate().skip(start_line) {
            if scanned >= max_lines {
                break;
            }
            scanned += 1;
            if !is_stream_event(line) {
                if first_start.is_none() {
                    first_start = Some(i);
                }
                found.push(line.clone());
                if found.len() >= limit {
                    break;
                }
            }
        }
        Window {
            start: first_start.unwrap_or(start_line),
            lines: found,
            total,
        }
    }

    /// Filtered backward range: walk backward from `before_line`, collect up to `limit`
    /// non-delta events. `start` = rendered-line offset of the first (oldest) returned event.
    pub fn window_filtered_before(
        &self,
        before_line: usize,
        limit: usize,
        max_lines: usize,
    ) -> Window {
        let r = &self.rendered;
        let total = r.len();
        let end = before_line.min(total);
        let mut found = Vec::new();
        let mut last_start = end;
        let mut scanned = 0usize;
        let mut idx = end;
        while idx > 0 && scanned < max_lines {
            idx -= 1;
            scanned += 1;
            if !is_stream_event(&r[idx]) {
                found.push(r[idx].clone());
                last_start = idx;
                if found.len() >= limit {
                    break;
                }
            }
        }
        found.reverse();
        Window {
            start: last_start,
            lines: found,
            total,
        }
    }
    pub fn range(&self, before: usize, limit: usize) -> &[String] {
        let before = before.min(self.rendered.len());
        let start = before.saturating_sub(limit);
        &self.rendered[start..before]
    }
    /// Return up to `limit` events whose index is STRICTLY GREATER than `after` (forward pagination).
    pub fn range_after(&self, after: usize, limit: usize) -> &[String] {
        let start = (after + 1).min(self.rendered.len());
        let end = (start + limit).min(self.rendered.len());
        &self.rendered[start..end]
    }
    pub fn slice_from(&self, since: usize) -> &[String] {
        &self.rendered[since.min(self.rendered.len())..]
    }
}

use parking_lot::Mutex;
use std::collections::HashMap;

struct Entry {
    proj: RenderedProjection,
    last_access: u64,
}

/// Per-session rendered projections, LRU-evicted by a byte budget. The big cold read inside
/// `RenderedProjection::sync` is off the executor (spawn_blocking); the map lock is only held for the
/// short in-memory bookkeeping, never across the sync's await.
pub struct TranscriptCache {
    map: Mutex<HashMap<String, Entry>>,
    budget: usize,
    tick: std::sync::atomic::AtomicU64,
}

impl TranscriptCache {
    pub fn new(budget_bytes: usize) -> Self {
        TranscriptCache {
            map: Mutex::new(HashMap::new()),
            budget: budget_bytes,
            tick: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn drop_session(&self, id: &str) {
        self.map.lock().remove(id);
    }

    /// Sync the session's projection to current EOF, then run `f` against it and return f's result.
    /// `f` returns an owned value so the projection borrow never escapes the lock.
    pub async fn with<R>(
        &self,
        id: &str,
        path: &std::path::Path,
        f: impl FnOnce(&RenderedProjection) -> R,
    ) -> std::io::Result<R> {
        // Take the projection out (or create) so the big sync runs without holding the map lock.
        let mut proj = {
            let mut m = self.map.lock();
            m.remove(id)
                .map(|e| e.proj)
                .unwrap_or_else(RenderedProjection::new)
        };
        proj.sync(path).await?;
        let out = f(&proj);
        let now = self.tick.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut m = self.map.lock();
        m.insert(
            id.to_string(),
            Entry {
                proj,
                last_access: now,
            },
        );
        // evict LRU while over budget (never evict the just-inserted id)
        let mut total: usize = m.values().map(|e| e.proj.bytes()).sum();
        while total > self.budget && m.len() > 1 {
            if let Some(victim) = m
                .iter()
                .filter(|(k, _)| k.as_str() != id)
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| k.clone())
            {
                if let Some(e) = m.remove(&victim) {
                    total -= e.proj.bytes();
                }
            } else {
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AO};

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn write_lines(path: &std::path::Path, lines: &[&str]) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }
    fn tmpfile(name: &str) -> std::path::PathBuf {
        let n = CTR.fetch_add(1, AO::SeqCst);
        let p = std::env::temp_dir().join(format!("agentic-tx-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    const RENDERED: [&str; 2] = [
        "{\"type\":\"agentic_prompt\",\"text\":\"hi\"}",
        "{\"type\":\"assistant\"}",
    ];
    const NOISE: [&str; 2] = ["{\"type\":\"tool_result\"}", "{\"type\":\"system\"}"];

    #[test]
    fn raw_to_rendered_offset_counts_rendered_lines_before_offset() {
        // raw[0]=rendered, raw[1]=noise, raw[2]=rendered, raw[3]=noise, raw[4]=rendered
        let raw: Vec<String> = vec![
            RENDERED[0].to_string(),
            NOISE[0].to_string(),
            RENDERED[1].to_string(),
            NOISE[1].to_string(),
            RENDERED[0].to_string(),
        ];
        // before raw index 0 → 0 rendered lines precede it
        assert_eq!(raw_to_rendered_offset(&raw, 0), 0);
        // before raw index 2 → raw[0] is the only rendered line that precedes → 1
        assert_eq!(raw_to_rendered_offset(&raw, 2), 1);
        // before raw index 3 → raw[0], raw[2] precede → 2
        assert_eq!(raw_to_rendered_offset(&raw, 3), 2);
        // offset past the end → all 3 rendered lines counted
        assert_eq!(raw_to_rendered_offset(&raw, 99), 3);
    }

    #[test]
    fn agent_result_marker_is_rendered_but_raw_tool_result_is_not() {
        // The engine persists a compact `agent_result` marker for a spawned-agent's result; it must be
        // rendered so it survives a reopen/reconnect and the agent card's body comes back.
        assert!(is_rendered(
            r#"{"type":"agent_result","toolUseId":"tu_1","text":"found it"}"#
        ));
        // The raw `user` tool_result line (every tool emits one) stays NON-rendered — we don't bloat the
        // replayable log with every Bash/Read output.
        assert!(!is_rendered(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_1"}]}}"#
        ));
    }

    #[test]
    fn agentic_perm_markers_are_rendered() {
        // The bridge writes these permission-card markers (type-first via JS JSON.stringify); they MUST
        // be rendered so the live WS poke re-reads them and the app shows / resolves the perm/plan card.
        assert!(is_rendered(
            r#"{"type":"agentic_perm","permKind":"perm","id":"p1","tool":"Bash","input":{}}"#
        ));
        assert!(is_rendered(
            r##"{"type":"agentic_perm","permKind":"plan","id":"p2","plan":"# do X"}"##
        ));
        assert!(is_rendered(
            r#"{"type":"agentic_perm_resolved","id":"p1","decision":"allow"}"#
        ));
        // Also robust to serde's alphabetical (type-last) key order, in case a marker is ever rebuilt
        // server-side with serde_json::json!.
        let resolved = serde_json::json!({"type":"agentic_perm_resolved","id":"p1","decision":"allow","at":1i64}).to_string();
        assert!(
            resolved.starts_with("{\"at\":"),
            "serde must put `type` last: {resolved}"
        );
        assert!(
            is_rendered(&resolved),
            "alphabetical-key agentic_perm_resolved must be rendered: {resolved}"
        );
    }

    #[test]
    fn engine_markers_render_in_real_serde_key_order_not_just_type_first() {
        // REGRESSION (cutover bug): the engine builds its markers with serde_json::json!, which —
        // without the `preserve_order` feature — serializes keys ALPHABETICALLY, so `type` is LAST.
        // is_rendered MUST still recognize them, or every user prompt / agent-result body silently
        // drops out of the rendered projection (vanishes from the app, never replays on reseed).
        // Build them exactly as the engine does (NOT a hand-written type-first literal that hides the
        // bug) and assert the real wire bytes are type-last AND still rendered.
        let prompt =
            serde_json::json!({"type":"agentic_prompt","text":"全量","at":1782044771008i64})
                .to_string();
        assert!(
            prompt.starts_with("{\"at\":"),
            "serde must put `type` last: {prompt}"
        );
        assert!(
            is_rendered(&prompt),
            "alphabetical-key agentic_prompt must be rendered: {prompt}"
        );

        let agent =
            serde_json::json!({"type":"agent_result","toolUseId":"tu_1","text":"x"}).to_string();
        assert!(
            agent.starts_with("{\"text\":"),
            "serde must put `type` last: {agent}"
        );
        assert!(
            is_rendered(&agent),
            "alphabetical-key agent_result must be rendered: {agent}"
        );

        // And the negatives still hold (no over-matching of non-rendered lines).
        assert!(!is_rendered(r#"{"type":"user","message":{}}"#));
        assert!(!is_rendered(r#"{"type":"system","subtype":"init"}"#));
        assert!(!is_rendered(
            r#"{"at":1,"text":"hi","type":"rate_limit_event"}"#
        ));
    }

    #[test]
    fn filter_keeps_rendered_drops_noise_with_fallback() {
        let all: Vec<String> = RENDERED
            .iter()
            .chain(NOISE.iter())
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            filter_rendered(&all),
            RENDERED.iter().map(|s| s.to_string()).collect::<Vec<_>>()
        );
        // empty-after-filter fallback: returns the raw lines
        let only_noise: Vec<String> = NOISE.iter().map(|s| s.to_string()).collect();
        assert_eq!(filter_rendered(&only_noise), only_noise);
    }

    #[tokio::test]
    async fn sync_is_incremental_and_matches_filter_rendered() {
        let path = tmpfile("inc");
        write_lines(&path, &[RENDERED[0], NOISE[0], RENDERED[1]]);
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 2); // 2 rendered, 1 noise dropped
        assert_eq!(p.raw_count(), 3);
        // equivalence invariant: split on '\n', filter empty lines
        let raw_bytes = std::fs::read(&path).unwrap();
        let whole: Vec<String> = raw_bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();
        assert_eq!(p.rendered_clone(), filter_rendered(&whole));
        // append more; a second sync reads only the new bytes
        let before = p.byte_offset();
        write_lines(&path, &[NOISE[1], RENDERED[0]]);
        p.sync(&path).await.unwrap();
        assert!(p.byte_offset() > before);
        assert_eq!(p.count(), 3);
    }

    #[tokio::test]
    async fn window_range_slice_count() {
        let path = tmpfile("win");
        let many: Vec<String> = (0..10)
            .map(|i| format!("{{\"type\":\"assistant\",\"i\":{i}}}"))
            .collect();
        write_lines(&path, &many.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 10);
        let w = p.window_tail(3);
        assert_eq!((w.start, w.total, w.lines.len()), (7, 10, 3));
        assert_eq!(p.range(7, 4).len(), 4); // [3,7)
        assert_eq!(p.range(2, 5).len(), 2); // [0,2) clamped
        assert_eq!(p.slice_from(8).len(), 2); // [8,10)
        assert_eq!(p.slice_from(100).len(), 0);
    }

    /// window_tail_events budgets by LOGICAL events (cards/turns), keeping cards a plain line-window
    /// would drop when streaming-text deltas dominate — the "ask/delegate card disappears on reseed"
    /// bug. Layout: 3 events (idx 0,1,23) with a 20-delta flood between the 2nd and 3rd event.
    #[tokio::test]
    async fn window_tail_events_budgets_by_cards_not_deltas() {
        let path = tmpfile("win_ev");
        let mut lines: Vec<String> = Vec::new();
        lines.push("{\"type\":\"assistant\",\"i\":0}".to_string()); // event 0
        lines.push("{\"type\":\"assistant\",\"i\":1}".to_string()); // event 1
        for k in 0..20 {
            lines.push(format!("{{\"type\":\"stream_event\",\"k\":{k}}}"));
        }
        lines.push("{\"type\":\"assistant\",\"i\":2}".to_string()); // event 2 (23 lines total)
        write_lines(&path, &lines.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 23);

        let events_in = |w: &Window| w.lines.iter().filter(|l| !is_stream_event(l)).count();

        // A plain last-5-LINES window reaches only into the delta flood: just 1 of 3 events survives.
        let line_win = p.window_tail(5);
        assert_eq!(
            events_in(&line_win),
            1,
            "line window drops older cards behind delta flood"
        );

        // Event budget 3 (ample line cap) keeps ALL 3 cards even across the 20-delta flood.
        let w3 = p.window_tail_events(3, 1000);
        assert_eq!((w3.start, w3.total, w3.lines.len()), (0, 23, 23));
        assert_eq!(events_in(&w3), 3);

        // Event budget 2 keeps the newest 2 cards (drops event 0); window stays a contiguous suffix.
        let w2 = p.window_tail_events(2, 1000);
        assert_eq!(events_in(&w2), 2);
        assert_eq!(w2.start, 1, "suffix begins at the 2nd-newest event");

        // Line cap wins when the two bounds conflict: max_lines=5 caps the suffix (events dropped).
        let capped = p.window_tail_events(3, 5);
        assert_eq!(capped.lines.len(), 5);
        assert_eq!(events_in(&capped), 1);

        // Fewer events than the budget → whole projection returned.
        let all = p.window_tail_events(100, 1000);
        assert_eq!((all.start, all.lines.len(), events_in(&all)), (0, 23, 3));
    }

    #[tokio::test]
    async fn missing_file_is_empty() {
        let mut p = RenderedProjection::new();
        p.sync(std::path::Path::new("/no/such/file.jsonl"))
            .await
            .unwrap();
        assert_eq!(p.count(), 0);
    }

    /// Regression for #2/#10: a partial final line (no trailing '\n') is NOT included in
    /// `rendered`. Once the '\n' is appended, count() converges and equals filter_rendered of the
    /// complete lines. The cursor never corrupts.
    #[tokio::test]
    async fn partial_final_line_excluded_then_converges() {
        let path = tmpfile("partial");
        // Write a complete rendered line followed by a partial line (no trailing '\n').
        let complete = "{\"type\":\"assistant\",\"text\":\"done\"}";
        let partial_prefix = "{\"type\":\"assistant\",\"text\":\"in-fli"; // no '\n'
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&path)
                .unwrap();
            write!(f, "{complete}\n{partial_prefix}").unwrap();
        }
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        // Only the complete line is in rendered; the partial is in carry.
        assert_eq!(p.count(), 1, "partial line must not be in rendered");
        assert_eq!(p.rendered_clone()[0], complete);

        // Now complete the partial line.
        let rest = "ght\"}";
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "{rest}").unwrap();
        }
        p.sync(&path).await.unwrap();
        // Both lines now complete — count must equal filter_rendered of the full file.
        assert_eq!(p.count(), 2, "after nl, both lines must be rendered");
        let raw_bytes = std::fs::read(&path).unwrap();
        let whole: Vec<String> = raw_bytes
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .collect();
        assert_eq!(
            p.rendered_clone(),
            filter_rendered(&whole),
            "cursor must converge: projection == filter_rendered(complete lines)"
        );
    }

    /// Regression for #1/#10: a multibyte UTF-8 character split across two sync() calls must
    /// decode without corruption (no U+FFFD replacement characters).
    #[tokio::test]
    async fn multibyte_char_split_across_reads_decodes_correctly() {
        let path = tmpfile("multibyte");
        // "你好" in UTF-8 is 6 bytes: [0xE4,0xBD,0xA0,0xE5,0xA5,0xBD].
        // We'll write a rendered line containing "你好" such that the first sync() ends
        // INSIDE the second character's bytes (after the first 4 bytes of the 6-byte sequence),
        // then the second sync() appends the rest and the '\n'.
        let line_prefix = "{\"type\":\"assistant\",\"text\":\""; // ASCII prefix
        let cjk = "你好";
        let line_suffix = "\"}";
        let full_line = format!("{line_prefix}{cjk}{line_suffix}");

        // Build the full line bytes, then split to simulate two reads.
        let full_bytes: Vec<u8> = format!("{full_line}\n").into_bytes();
        // Split after 4 bytes into "你好" (i.e. after the full prefix + first 4 bytes of CJK).
        let split_at = line_prefix.len() + 4; // cuts 你(3 bytes) + 1 byte of 好
        assert!(split_at < full_bytes.len(), "split must be within the line");

        // Write first chunk.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(&full_bytes[..split_at]).unwrap();
        }
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        // No complete line yet (no '\n' seen).
        assert_eq!(p.count(), 0, "no complete line before nl");

        // Append the rest.
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            f.write_all(&full_bytes[split_at..]).unwrap();
        }
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 1, "complete line must be counted after nl");
        let decoded = &p.rendered_clone()[0];
        assert!(
            decoded.contains("你好"),
            "multibyte char must decode correctly, got: {decoded:?}"
        );
        assert!(
            !decoded.contains('\u{FFFD}'),
            "must not contain replacement char, got: {decoded:?}"
        );
    }

    /// Regression for file rotation: if the file shrinks (rotated), projection resets and
    /// re-reads from the start.
    #[tokio::test]
    async fn file_rotation_resets_and_rehydrates() {
        let path = tmpfile("rotate");
        write_lines(&path, &[RENDERED[0], RENDERED[1]]);
        let mut p = RenderedProjection::new();
        p.sync(&path).await.unwrap();
        assert_eq!(p.count(), 2);
        let old_offset = p.byte_offset();
        assert!(old_offset > 0);

        // Simulate rotation: replace file with a new shorter one.
        {
            let mut f = std::fs::File::create(&path).unwrap();
            writeln!(f, "{}", RENDERED[0]).unwrap();
        }
        p.sync(&path).await.unwrap();
        // After rotation detection, projection should reflect only the new file's contents.
        assert_eq!(p.count(), 1, "after rotation, count must reflect new file");
    }
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AO};
    use std::sync::Arc;

    static CTR: AtomicU64 = AtomicU64::new(0);

    fn tmpfile(name: &str) -> std::path::PathBuf {
        let n = CTR.fetch_add(1, AO::SeqCst);
        let p =
            std::env::temp_dir().join(format!("agentic-cache-{}-{n}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }
    fn seed(path: &std::path::Path, n: usize) {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for i in 0..n {
            writeln!(f, "{{\"type\":\"assistant\",\"i\":{i}}}").unwrap();
        }
    }

    #[tokio::test]
    async fn hydrates_then_serves_count() {
        let path = tmpfile("c1");
        seed(&path, 5);
        let cache = TranscriptCache::new(1_000_000);
        let total = cache.with("s1", &path, |p| p.count()).await.unwrap();
        assert_eq!(total, 5);
    }

    #[tokio::test]
    async fn evicts_lru_over_budget_then_rehydrates() {
        let a = tmpfile("ca");
        seed(&a, 50);
        let b = tmpfile("cb");
        seed(&b, 50);
        let cache = TranscriptCache::new(200); // tiny budget → only one fits
        let n1 = cache.with("a", &a, |p| p.count()).await.unwrap();
        let n2 = cache.with("b", &b, |p| p.count()).await.unwrap();
        assert_eq!((n1, n2), (50, 50));
        // "a" was evicted by "b"; re-access re-hydrates from file to the same count
        assert_eq!(cache.with("a", &a, |p| p.count()).await.unwrap(), 50);
    }

    #[tokio::test]
    async fn concurrent_get_of_cold_session_is_consistent() {
        let path = tmpfile("cc");
        seed(&path, 20);
        let cache = Arc::new(TranscriptCache::new(1_000_000));
        let mut hs = Vec::new();
        for _ in 0..8 {
            let c = cache.clone();
            let p = path.clone();
            hs.push(tokio::spawn(async move {
                c.with("s", &p, |pr| pr.count()).await.unwrap()
            }));
        }
        for h in hs {
            assert_eq!(h.await.unwrap(), 20);
        }
    }
}
