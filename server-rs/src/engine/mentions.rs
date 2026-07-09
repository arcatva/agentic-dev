//! `@session:<id-prefix>` mention expansion.
//!
//! The Android composer lets the user @-mention another session; picking a candidate inserts a
//! literal `@session:<first-8-uuid-chars>` token into the prompt text. The logged/displayed
//! prompt keeps the raw token (the user bubble shows exactly what was typed) — expansion happens
//! ONLY on the text delivered to claude, at the two write sites in `engine::mod` (live inject +
//! spawn). For each token that resolves to a known session, a resolution block is appended after
//! the user's text giving claude that session's identity and on-disk locations (worktree,
//! transcript log) so it can inspect the mentioned session's files, diffs and conversation — or
//! act on them when asked.
//!
//! Trust boundary: this is a SINGLE-USER platform — one HMAC token guards the whole instance and
//! `Store::list()` has no per-user scoping, so every session is legitimately mentionable. If the
//! platform ever grows multi-user auth, this resolver must filter to the caller's sessions.

use crate::engine::store::Session;
use std::path::PathBuf;

/// Char budget for the mentioned session's title inside the expansion block. Titles are
/// user prompts and can be huge; the block only needs enough to identify the session.
const TITLE_MAX_CHARS: usize = 120;

/// Scan `text` for `@session:<prefix>` tokens and return the distinct prefixes in first-seen
/// order. A token starts at an `@` that is NOT preceded by an alphanumeric char (so
/// `mail@session:...`-style strings don't fire) and the prefix is 4–36 chars drawn from the
/// uuid alphabet (ascii hex + `-`), longest match first.
fn scan_prefixes(text: &str) -> Vec<String> {
    const MARK: &str = "@session:";
    let bytes = text.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut at = 0usize;
    while let Some(rel) = text[at..].find(MARK) {
        let start = at + rel;
        // Word boundary: the char immediately before '@' must not be alphanumeric.
        let boundary_ok = start == 0
            || !text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric());
        let mut end = start + MARK.len();
        while end < bytes.len()
            && end - (start + MARK.len()) < 36
            && (bytes[end].is_ascii_hexdigit() || bytes[end] == b'-')
        {
            end += 1;
        }
        let prefix = &text[start + MARK.len()..end];
        if boundary_ok && prefix.len() >= 4 {
            let p = prefix.to_ascii_lowercase();
            if !out.contains(&p) {
                out.push(p);
            }
        }
        at = end.max(start + MARK.len());
    }
    out
}

/// Expand `@session:<prefix>` mentions in `text` against `sessions`.
///
/// Returns `text` unchanged when no token resolves. Otherwise returns the text with a
/// resolution appendix: one block per uniquely-resolved mention (identity + paths), a short
/// ambiguity note when a prefix matches several sessions, and nothing at all for prefixes that
/// match none (those may be false positives — e.g. the token quoted inside pasted text).
/// `log_path` maps a session id to its transcript JSONL location (`Store::log_path`).
pub fn expand_session_mentions(
    text: &str,
    sessions: &[Session],
    log_path: &dyn Fn(&str) -> PathBuf,
) -> String {
    let prefixes = scan_prefixes(text);
    if prefixes.is_empty() {
        return text.to_string();
    }

    let mut blocks: Vec<String> = Vec::new();
    let mut any_resolved = false;
    for prefix in &prefixes {
        let matches: Vec<&Session> = sessions
            .iter()
            .filter(|s| s.id.to_ascii_lowercase().starts_with(prefix.as_str()))
            .collect();
        match matches.as_slice() {
            [] => {} // unknown prefix — likely a false positive; leave the raw token alone
            [s] => {
                any_resolved = true;
                blocks.push(render_block(prefix, s, log_path));
            }
            many => {
                let ids: Vec<&str> = many.iter().map(|s| s.id.as_str()).collect();
                blocks.push(format!(
                    "[@session:{prefix} is ambiguous — it matches these session ids: {}. Ask the user which one they meant.]",
                    ids.join(", ")
                ));
            }
        }
    }
    if blocks.is_empty() {
        return text.to_string();
    }
    if any_resolved {
        blocks.push(
            "[The paths above belong to OTHER sessions running on this host. You may read their \
worktrees, diffs and transcript logs directly to answer questions about them or to carry their \
work forward — but do not write into another session's worktree unless the user explicitly \
asked for that.]"
                .to_string(),
        );
    }
    format!(
        "{text}\n\n---\n[@session mentions in the message above, resolved by the server]\n\n{}",
        blocks.join("\n\n")
    )
}

/// One resolved mention block: identity first, then every on-disk handle claude needs to
/// inspect the session. Optional fields (worktree, branch) are omitted when absent
/// (e.g. skill-only sessions have no worktree).
fn render_block(prefix: &str, s: &Session, log_path: &dyn Fn(&str) -> PathBuf) -> String {
    let mut title: String = s.prompt.chars().take(TITLE_MAX_CHARS).collect();
    if s.prompt.chars().nth(TITLE_MAX_CHARS).is_some() {
        title.push('…');
    }
    let mut b = format!("[@session:{prefix}] → session {} — \"{title}\"\n", s.id);
    b.push_str(&format!("- status: {}\n", s.status));
    if !s.repos.is_empty() {
        b.push_str(&format!("- repos: {}\n", s.repos.join(", ")));
    }
    if let Some(wt) = s.worktree_path.as_deref() {
        match s.branch.as_deref() {
            Some(br) => b.push_str(&format!("- worktree: {wt} (branch {br})\n")),
            None => b.push_str(&format!("- worktree: {wt}\n")),
        }
    }
    b.push_str(&format!(
        "- transcript log (stream-json of that session's whole conversation): {}",
        log_path(&s.id).display()
    ));
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(id: &str, prompt: &str) -> Session {
        Session {
            id: id.to_string(),
            prompt: prompt.to_string(),
            status: "running".to_string(),
            repos: vec!["agentic-dev".to_string()],
            worktree_path: Some(format!("/tmp/worktrees/{id}/agentic-dev")),
            branch: Some(format!("agentic/{id}")),
            ..Default::default()
        }
    }

    fn lp(id: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/logs/{id}.jsonl"))
    }

    #[test]
    fn scans_tokens_with_boundaries() {
        assert_eq!(scan_prefixes("hi @session:abcd1234 x"), vec!["abcd1234"]);
        // uppercase hex normalized, hyphens allowed, full uuid ok
        assert_eq!(
            scan_prefixes("@session:ABCD-1234-ef"),
            vec!["abcd-1234-ef"]
        );
        // too short, missing prefix char, or embedded in a word → no token
        assert!(scan_prefixes("@session:abc").is_empty());
        assert!(scan_prefixes("mail@session:abcd1234").is_empty());
        assert!(scan_prefixes("no mentions here").is_empty());
        // duplicates collapse
        assert_eq!(
            scan_prefixes("@session:abcd1234 and @session:abcd1234"),
            vec!["abcd1234"]
        );
    }

    #[test]
    fn unique_match_appends_block_and_keeps_original_text() {
        let sessions = vec![sess("abcd1234-0000-0000-0000-000000000000", "fix the login bug")];
        let out = expand_session_mentions("look at @session:abcd1234 please", &sessions, &|id| lp(id));
        assert!(out.starts_with("look at @session:abcd1234 please\n\n---\n"));
        assert!(out.contains("session abcd1234-0000-0000-0000-000000000000"));
        assert!(out.contains("\"fix the login bug\""));
        assert!(out.contains("/tmp/worktrees/abcd1234-0000-0000-0000-000000000000/agentic-dev"));
        assert!(out.contains("(branch agentic/abcd1234-0000-0000-0000-000000000000)"));
        assert!(out.contains("/tmp/logs/abcd1234-0000-0000-0000-000000000000.jsonl"));
        assert!(out.contains("do not write into another session's worktree"));
    }

    #[test]
    fn no_match_returns_text_unchanged() {
        let sessions = vec![sess("abcd1234-0000-0000-0000-000000000000", "t")];
        let text = "ping @session:ffff0000 nothing";
        assert_eq!(expand_session_mentions(text, &sessions, &|id| lp(id)), text);
    }

    #[test]
    fn ambiguous_prefix_lists_candidates_without_paths() {
        let sessions = vec![
            sess("abcd1234-0000-0000-0000-000000000000", "one"),
            sess("abcd1234-1111-1111-1111-111111111111", "two"),
        ];
        let out = expand_session_mentions("see @session:abcd1234", &sessions, &|id| lp(id));
        assert!(out.contains("is ambiguous"));
        assert!(out.contains("abcd1234-0000-0000-0000-000000000000"));
        assert!(out.contains("abcd1234-1111-1111-1111-111111111111"));
        // ambiguity alone must not add the "paths above" hint
        assert!(!out.contains("paths above"));
    }

    #[test]
    fn skill_only_session_omits_worktree_line() {
        let mut s = sess("abcd1234-0000-0000-0000-000000000000", "skill run");
        s.worktree_path = None;
        s.branch = None;
        s.repos = vec![];
        let out = expand_session_mentions("@session:abcd1234", &[s], &|id| lp(id));
        assert!(!out.contains("- worktree:"));
        assert!(!out.contains("- repos:"));
        assert!(out.contains("- transcript log"));
    }

    #[test]
    fn long_title_is_truncated_by_chars_not_bytes() {
        let long = "标题".repeat(200); // multi-byte chars — must not slice mid-char
        let s = sess("abcd1234-0000-0000-0000-000000000000", &long);
        let out = expand_session_mentions("@session:abcd1234", &[s], &|id| lp(id));
        assert!(out.contains('…'));
    }
}
