use std::path::Path;
use std::time::Duration;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const SEP: char = '\u{1f}';

/// Compiled once for parse_name_status; the regex is a compile-time constant and cannot fail.
static NAME_STATUS_RE: std::sync::LazyLock<regex::Regex> =
    std::sync::LazyLock::new(|| regex::Regex::new(r"^([ACDMRT])\d*\t(.+)").expect("valid regex"));

async fn git(cwd: &Path, args: &[&str]) -> String {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(cwd).args(args).kill_on_drop(true);
    let fut = cmd.output();
    match tokio::time::timeout(GIT_TIMEOUT, fut).await {
        Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).into_owned(),
        _ => String::new(),
    }
}

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Unknown,
}

/// Kind of a ref label decorating a commit (from `git log %D`).
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RefKind {
    Head,
    Branch,
    Tag,
    Remote,
}

/// A ref pointing at a commit: a branch/tag/remote name + its kind. Rendered as a
/// chip in the client's commit-graph rows so branches and tags are visible.
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct GitRef {
    pub name: String,
    pub kind: RefKind,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct CommitNode {
    pub sha: String,
    #[serde(rename = "shortSha")]
    pub short_sha: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub author: String,
    pub at: i64,
    #[serde(rename = "isSession")]
    pub is_session: bool,
    /// Branch/tag/HEAD labels pointing at this commit (from `git log %D`). Empty
    /// for the vast majority of commits; non-empty at branch tips / tagged commits.
    pub refs: Vec<GitRef>,
}

/// Parse the `%D` decoration field into structured refs. The log call passes
/// `--decorate=full`, so names arrive fully-qualified
/// (`HEAD -> refs/heads/x, refs/remotes/origin/master, refs/heads/master, refs/tags/v1.0`).
/// Full-qualification makes classification config-independent (a user's
/// `log.decorate` setting can't change the shape) and supports any remote name —
/// not just `origin`. Short-form arms remain as a fallback. Order is preserved.
fn parse_refs(decoration: &str) -> Vec<GitRef> {
    decoration
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|tok| {
            if let Some(target) = tok.strip_prefix("HEAD -> ") {
                let name = target.strip_prefix("refs/heads/").unwrap_or(target);
                GitRef {
                    name: name.to_string(),
                    kind: RefKind::Head,
                }
            } else if tok == "HEAD" {
                GitRef {
                    name: "HEAD".to_string(),
                    kind: RefKind::Head,
                }
            } else if let Some(tag) = tok.strip_prefix("tag: ") {
                // git keeps the `tag: ` marker in BOTH modes; under --decorate=full
                // the inner name is `refs/tags/<name>`, so strip that too.
                let name = tag.strip_prefix("refs/tags/").unwrap_or(tag);
                GitRef {
                    name: name.to_string(),
                    kind: RefKind::Tag,
                }
            } else if let Some(remote) = tok.strip_prefix("refs/remotes/") {
                GitRef {
                    name: remote.to_string(),
                    kind: RefKind::Remote,
                }
            } else if let Some(branch) = tok.strip_prefix("refs/heads/") {
                GitRef {
                    name: branch.to_string(),
                    kind: RefKind::Branch,
                }
            } else if tok.starts_with("origin/") {
                GitRef {
                    name: tok.to_string(),
                    kind: RefKind::Remote,
                }
            } else {
                GitRef {
                    name: tok.to_string(),
                    kind: RefKind::Branch,
                }
            }
        })
        .collect()
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct Uncommitted {
    pub added: u32,
    pub modified: u32,
    pub deleted: u32,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct RepoGraph {
    pub commits: Vec<CommitNode>,
    pub uncommitted: Option<Uncommitted>,
}

#[derive(serde::Serialize, Clone, Debug)]
pub struct CommitFile {
    pub path: String,
    pub status: FileStatus,
    pub additions: u32,
    pub deletions: u32,
}

fn letter_to_status(l: char) -> FileStatus {
    match l {
        'A' => FileStatus::Added,
        'D' => FileStatus::Deleted,
        'R' | 'C' => FileStatus::Renamed,
        'M' => FileStatus::Modified,
        _ => FileStatus::Unknown,
    }
}

pub async fn commit_graph_for_repo(worktree: &Path, base_sha: Option<&str>) -> RepoGraph {
    use std::collections::HashSet;
    let mut session_set: HashSet<String> = HashSet::new();
    if let Some(base) = base_sha.filter(|b| !b.is_empty()) {
        let out = git(worktree, &["rev-list", &format!("{base}..HEAD")]).await;
        for line in out.lines() {
            let s = line.trim();
            if !s.is_empty() {
                session_set.insert(s.to_string());
            }
        }
    }
    // Tips to walk. Always HEAD; additionally the mainline branches (master/main,
    // local + remote) WHEN they exist, so a session branch that has diverged from
    // master renders as a separate lane instead of one straight line. We deliberately
    // do NOT use `--all`: in these shared worktrees it would surface dozens of OTHER
    // sessions' agentic/* branches. Each candidate is existence-checked first, because
    // `git log` aborts on an unknown ref and would then return nothing at all.
    let mut tips: Vec<String> = vec!["HEAD".to_string()];
    // Existence-check the mainline candidates concurrently — one process-spawn round
    // instead of four sequential ones. Each is added (in a fixed order) only when it
    // resolves to a commit.
    let (master, main, origin_master, origin_main) = tokio::join!(
        git(
            worktree,
            &["rev-parse", "--verify", "--quiet", "master^{commit}"]
        ),
        git(
            worktree,
            &["rev-parse", "--verify", "--quiet", "main^{commit}"]
        ),
        git(
            worktree,
            &["rev-parse", "--verify", "--quiet", "origin/master^{commit}"]
        ),
        git(
            worktree,
            &["rev-parse", "--verify", "--quiet", "origin/main^{commit}"]
        ),
    );
    for (resolved, name) in [
        (master, "master"),
        (main, "main"),
        (origin_master, "origin/master"),
        (origin_main, "origin/main"),
    ] {
        if !resolved.trim().is_empty() {
            tips.push(name.to_string());
        }
    }
    // `%D` adds ref decorations (branch/tag/HEAD labels) so the client can render them
    // as chips. `--date-order` keeps a stable, intuitive ordering for the multi-lane
    // graph layout. `--decorate=full` forces fully-qualified ref names in %D regardless
    // of the user's log.decorate config, so parse_refs classification is deterministic.
    let fmt = format!("--pretty=%H{SEP}%P{SEP}%s{SEP}%an{SEP}%at{SEP}%D");
    let mut args: Vec<&str> = vec![
        "log",
        "--no-color",
        "--decorate=full",
        "--date-order",
        "-n",
        "40",
        &fmt,
    ];
    args.extend(tips.iter().map(|s| s.as_str()));
    let log = git(worktree, &args).await;
    let mut commits = Vec::new();
    for line in log.split('\n') {
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(6, SEP).collect();
        let sha = parts.first().copied().unwrap_or("");
        if sha.is_empty() {
            continue;
        }
        let parents_str = parts.get(1).copied().unwrap_or("");
        let subject = parts.get(2).copied().unwrap_or("").to_string();
        let author = parts.get(3).copied().unwrap_or("").to_string();
        let at_sec: i64 = parts
            .get(4)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let refs = parse_refs(parts.get(5).copied().unwrap_or(""));
        commits.push(CommitNode {
            sha: sha.to_string(),
            short_sha: sha.chars().take(7).collect(),
            parents: parents_str
                .split(' ')
                .filter(|p| !p.is_empty())
                .map(|s| s.to_string())
                .collect(),
            subject,
            author,
            at: at_sec * 1000,
            is_session: session_set.contains(sha),
            refs,
        });
    }
    let status = git(worktree, &["status", "--porcelain"]).await;
    let (mut added, mut modified, mut deleted) = (0u32, 0u32, 0u32);
    for line in status.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let code: String = line.chars().take(2).collect();
        if code == "??" || code.contains('A') {
            added += 1;
        } else if code.contains('D') {
            deleted += 1;
        } else {
            modified += 1;
        }
    }
    let uncommitted = if added > 0 || modified > 0 || deleted > 0 {
        Some(Uncommitted {
            added,
            modified,
            deleted,
        })
    } else {
        None
    };
    RepoGraph {
        commits,
        uncommitted,
    }
}

fn parse_name_status(out: &str) -> std::collections::HashMap<String, FileStatus> {
    use std::collections::HashMap;
    let re = &*NAME_STATUS_RE;
    let mut map = HashMap::new();
    for line in out.split('\n') {
        if let Some(c) = re.captures(line) {
            let letter = c.get(1).unwrap().as_str().chars().next().unwrap();
            let rest = c.get(2).unwrap().as_str();
            let path = rest.split('\t').next_back().unwrap().to_string(); // renames: <old>\t<new> → new
            map.insert(path, letter_to_status(letter));
        }
    }
    map
}

fn parse_numstat(out: &str) -> std::collections::HashMap<String, (u32, u32)> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    for line in out.split('\n') {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        let adds = if parts[0] == "-" {
            0
        } else {
            parts[0].parse().unwrap_or(0)
        };
        let dels = if parts[1] == "-" {
            0
        } else {
            parts[1].parse().unwrap_or(0)
        };
        let path = parts[2..].join("\t");
        map.insert(path, (adds, dels));
    }
    map
}

fn merge_files(
    status_map: std::collections::HashMap<String, FileStatus>,
    numstat_map: std::collections::HashMap<String, (u32, u32)>,
) -> Vec<CommitFile> {
    use std::collections::BTreeSet;
    let mut paths: BTreeSet<String> = BTreeSet::new();
    paths.extend(status_map.keys().cloned());
    paths.extend(numstat_map.keys().cloned());
    paths
        .into_iter()
        .map(|path| {
            let (additions, deletions) = numstat_map.get(&path).copied().unwrap_or((0, 0));
            CommitFile {
                status: status_map
                    .get(&path)
                    .cloned()
                    .unwrap_or(FileStatus::Modified),
                additions,
                deletions,
                path,
            }
        })
        .collect()
}

pub async fn commit_files_for_repo(worktree: &Path, sha: &str) -> Vec<CommitFile> {
    if sha == "working" {
        let mut status_map = parse_name_status(
            &git(worktree, &["diff", "--no-color", "--name-status", "HEAD"]).await,
        );
        let numstat_map =
            parse_numstat(&git(worktree, &["diff", "--no-color", "--numstat", "HEAD"]).await);
        let status = git(worktree, &["status", "--porcelain"]).await;
        for line in status.split('\n') {
            if line.chars().take(2).collect::<String>() == "??" {
                let path = line.chars().skip(3).collect::<String>().trim().to_string();
                if !path.is_empty() && !status_map.contains_key(&path) {
                    status_map.insert(path, FileStatus::Added);
                }
            }
        }
        return merge_files(status_map, numstat_map);
    }
    let status_map = parse_name_status(
        &git(
            worktree,
            &["show", "--no-color", "--name-status", "--format=", sha],
        )
        .await,
    );
    let numstat_map = parse_numstat(
        &git(
            worktree,
            &["show", "--no-color", "--numstat", "--format=", sha],
        )
        .await,
    );
    merge_files(status_map, numstat_map)
}

// ── Line-level diff (single file) ─────────────────────────────────────────────
//
// `commit_files_for_repo` only returns per-file +N/-M counts. To actually review what
// changed on a phone we also need the patch text, parsed into hunks with old/new line
// numbers so the client can render a gutter without re-implementing a diff parser.
//
// Caps: the raw `git` output is truncated at MAX_DIFF_BYTES (sets `truncated`), and binary
// files are flagged (no hunks) instead of streaming megabytes of unreadable bytes to a phone.

/// Hard cap on the raw `git diff`/`git show` output we will parse for one file. A diff bigger
/// than this is truncated at the last newline before the cap and `FileDiff.truncated` is set,
/// so the client can show "diff too large — truncated" rather than the server OOMing on a
/// generated-file blowup.
const MAX_DIFF_BYTES: usize = 1_000_000;

/// Hunk-header parser: `@@ -<oldStart>[,<oldLines>] +<newStart>[,<newLines>] @@<section heading>`.
static HUNK_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(.*)$").expect("valid regex")
});

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DiffLineKind {
    Context,
    Add,
    Del,
}

/// One physical line of a hunk. `old_line`/`new_line` are 1-based and present only on the
/// side(s) the line exists in (context = both, add = new only, del = old only).
#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub content: String,
}

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// Section heading git prints after the second `@@` (often the enclosing fn/decl). May be empty.
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(serde::Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct FileDiff {
    pub path: String,
    pub status: FileStatus,
    /// True when git reported a binary file — `hunks` is then empty.
    pub binary: bool,
    /// True when the raw diff exceeded MAX_DIFF_BYTES and was cut short.
    pub truncated: bool,
    pub hunks: Vec<DiffHunk>,
}

/// Truncate `raw` to at most MAX_DIFF_BYTES, cutting at the last newline before the cap so we
/// never hand a half-line to the parser. Truncates in place (no extra 1 MB allocation) and returns
/// (possibly-shortened text, was_truncated).
fn cap_diff(mut raw: String) -> (String, bool) {
    if raw.len() <= MAX_DIFF_BYTES {
        return (raw, false);
    }
    let mut cut = MAX_DIFF_BYTES;
    while cut > 0 && !raw.is_char_boundary(cut) {
        cut -= 1;
    }
    // Prefer the last newline before the cap (keep it) so the parser never sees a half line.
    let end = raw[..cut].rfind('\n').map(|i| i + 1).unwrap_or(cut);
    raw.truncate(end);
    (raw, true)
}

/// Parse one file's unified-diff text (from `git diff` / `git show`, single path) into hunks.
fn parse_file_diff(path: &str, raw: String) -> FileDiff {
    let (raw, truncated) = cap_diff(raw);
    let mut status = FileStatus::Modified;
    let mut binary = false;
    let mut hunks: Vec<DiffHunk> = Vec::new();
    let mut cur: Option<DiffHunk> = None;
    let (mut old_no, mut new_no) = (0u32, 0u32);

    for raw_line in raw.split('\n') {
        // Strip a trailing CR so CRLF repos parse like LF ones (the marker is column 0, but the
        // hunk-header `(.*)$` and line content would otherwise keep a stray '\r').
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        // Pre-hunk metadata lines tell us the file's status / binariness.
        if line.starts_with("new file mode") {
            status = FileStatus::Added;
            continue;
        }
        if line.starts_with("deleted file mode") {
            status = FileStatus::Deleted;
            continue;
        }
        if line.starts_with("rename from") || line.starts_with("rename to") {
            status = FileStatus::Renamed;
            continue;
        }
        if line.starts_with("Binary files") || line.starts_with("GIT binary patch") {
            binary = true;
            continue;
        }

        if let Some(c) = HUNK_RE.captures(line) {
            if let Some(h) = cur.take() {
                hunks.push(h);
            }
            let g = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
            let old_start = g(1).unwrap_or(0);
            let new_start = g(3).unwrap_or(0);
            old_no = old_start;
            new_no = new_start;
            cur = Some(DiffHunk {
                old_start,
                old_lines: g(2).unwrap_or(1),
                new_start,
                new_lines: g(4).unwrap_or(1),
                header: c
                    .get(5)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_default(),
                lines: Vec::new(),
            });
            continue;
        }

        // Until the first @@, skip git's file headers (diff --git / index / --- / +++).
        let Some(h) = cur.as_mut() else {
            continue;
        };

        if let Some(rest) = line.strip_prefix('+') {
            h.lines.push(DiffLine {
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(new_no),
                content: rest.to_string(),
            });
            new_no += 1;
        } else if let Some(rest) = line.strip_prefix('-') {
            h.lines.push(DiffLine {
                kind: DiffLineKind::Del,
                old_line: Some(old_no),
                new_line: None,
                content: rest.to_string(),
            });
            old_no += 1;
        } else if let Some(rest) = line.strip_prefix(' ') {
            h.lines.push(DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(old_no),
                new_line: Some(new_no),
                content: rest.to_string(),
            });
            old_no += 1;
            new_no += 1;
        }
        // "\ No newline at end of file" and any stray blank line between hunks: ignore.
    }
    if let Some(h) = cur.take() {
        hunks.push(h);
    }
    FileDiff {
        path: path.to_string(),
        status,
        binary,
        truncated,
        hunks,
    }
}

/// Build a synthetic all-additions diff for an untracked working-tree file (git produces no
/// patch for these). Reads the file directly, flags binary on a NUL byte, and caps size.
async fn untracked_file_diff(worktree: &Path, path: &str) -> FileDiff {
    let bytes = tokio::fs::read(worktree.join(path))
        .await
        .unwrap_or_default();
    if bytes.contains(&0) {
        return FileDiff {
            path: path.to_string(),
            status: FileStatus::Added,
            binary: true,
            truncated: false,
            hunks: Vec::new(),
        };
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let (text, truncated) = cap_diff(text);
    // An empty file has no lines at all — `"".split('\n')` would otherwise yield one empty element
    // and produce a spurious single added line. Guard it explicitly.
    let mut rows: Vec<&str> = if text.is_empty() {
        Vec::new()
    } else {
        text.split('\n').collect()
    };
    // A trailing newline yields a spurious empty final element — drop it (git wouldn't show it).
    if text.ends_with('\n') {
        rows.pop();
    }
    let lines: Vec<DiffLine> = rows
        .iter()
        .enumerate()
        .map(|(i, content)| DiffLine {
            kind: DiffLineKind::Add,
            old_line: None,
            new_line: Some(i as u32 + 1),
            // Strip a trailing CR so a CRLF working-tree file displays without stray '\r'.
            content: content.strip_suffix('\r').unwrap_or(content).to_string(),
        })
        .collect();
    let count = lines.len() as u32;
    let hunks = if count == 0 {
        Vec::new()
    } else {
        vec![DiffHunk {
            old_start: 0,
            old_lines: 0,
            new_start: 1,
            new_lines: count,
            header: String::new(),
            lines,
        }]
    };
    FileDiff {
        path: path.to_string(),
        status: FileStatus::Added,
        binary: false,
        truncated,
        hunks,
    }
}

/// Line-level diff for ONE file in a commit (or the working tree). `path` is relative to the
/// repo worktree; callers must validate repo / sha / path before calling.
pub async fn commit_diff_for_repo(worktree: &Path, sha: &str, path: &str) -> FileDiff {
    if sha == "working" {
        // Untracked files have no patch; synthesize one from the file contents.
        let st = git(worktree, &["status", "--porcelain", "--", path]).await;
        if st
            .lines()
            .next()
            .map(|l| l.starts_with("??"))
            .unwrap_or(false)
        {
            return untracked_file_diff(worktree, path).await;
        }
        let raw = git(
            worktree,
            &["diff", "--no-color", "--unified=3", "HEAD", "--", path],
        )
        .await;
        return parse_file_diff(path, raw);
    }
    let raw = git(
        worktree,
        &[
            "show",
            "--no-color",
            "--format=",
            "--unified=3",
            sha,
            "--",
            path,
        ],
    )
    .await;
    parse_file_diff(path, raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }
    fn temp_repo() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-sd-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        for a in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@t"],
            vec!["config", "user.name", "t"],
        ] {
            Command::new("git")
                .args(&a)
                .current_dir(&d)
                .status()
                .unwrap();
        }
        std::fs::write(d.join("README.md"), "# r\n").unwrap();
        run(&d, &["add", "."]);
        run(&d, &["commit", "-q", "-m", "init"]);
        d
    }
    fn commit_file(wt: &Path, name: &str, content: &str, msg: &str) -> String {
        std::fs::write(wt.join(name), content).unwrap();
        run(wt, &["add", "."]);
        run(wt, &["commit", "-q", "-m", msg]);
        run(wt, &["rev-parse", "HEAD"])
    }

    #[tokio::test]
    async fn graph_newest_first_with_session_flags_and_uncommitted() {
        let repo = temp_repo();
        let base = run(&repo, &["rev-parse", "HEAD"]);
        let sha1 = commit_file(&repo, "a.txt", "a\n", "add a");
        let sha2 = commit_file(&repo, "b.txt", "b\n", "add b");
        let clean = commit_graph_for_repo(&repo, Some(&base)).await;
        assert!(clean.uncommitted.is_none());
        assert_eq!(clean.commits[0].sha, sha2);
        assert_eq!(clean.commits[1].sha, sha1);
        assert_eq!(clean.commits[2].sha, base);
        let by = |s: &str| clean.commits.iter().find(|c| c.sha == s).unwrap();
        assert!(by(&sha2).is_session);
        assert!(by(&sha1).is_session);
        assert!(!by(&base).is_session);
        assert_eq!(by(&sha2).parents, vec![sha1.clone()]);
        assert_eq!(by(&sha2).short_sha, sha2[..7]);
        assert!(by(&sha2).at > 0);
        // dirty
        std::fs::write(repo.join("a.txt"), "a changed\n").unwrap();
        let dirty = commit_graph_for_repo(&repo, Some(&base)).await;
        assert!(dirty.uncommitted.unwrap().modified > 0);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn graph_marks_nothing_session_when_base_falsy() {
        let repo = temp_repo();
        commit_file(&repo, "a.txt", "a\n", "add a");
        let g = commit_graph_for_repo(&repo, None).await;
        assert!(g.commits.iter().all(|c| !c.is_session));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn parse_refs_classifies_decorations() {
        // --decorate=full shape (what the log call actually produces — note git
        // keeps the `tag: ` marker even in full mode: `tag: refs/tags/v1.0`).
        let refs = parse_refs(
            "HEAD -> refs/heads/agentic/x, refs/remotes/origin/master, refs/heads/master, tag: refs/tags/v1.0",
        );
        assert_eq!(
            refs,
            vec![
                GitRef {
                    name: "agentic/x".into(),
                    kind: RefKind::Head
                },
                GitRef {
                    name: "origin/master".into(),
                    kind: RefKind::Remote
                },
                GitRef {
                    name: "master".into(),
                    kind: RefKind::Branch
                },
                GitRef {
                    name: "v1.0".into(),
                    kind: RefKind::Tag
                },
            ]
        );
        // Short-form fallback still classifies correctly.
        assert_eq!(
            parse_refs("HEAD -> main, tag: v2"),
            vec![
                GitRef {
                    name: "main".into(),
                    kind: RefKind::Head
                },
                GitRef {
                    name: "v2".into(),
                    kind: RefKind::Tag
                },
            ]
        );
        assert!(parse_refs("").is_empty());
        assert_eq!(
            parse_refs("HEAD"),
            vec![GitRef {
                name: "HEAD".into(),
                kind: RefKind::Head
            }]
        );
    }

    #[tokio::test]
    async fn graph_includes_branch_and_tag_refs() {
        let repo = temp_repo();
        let sha = commit_file(&repo, "a.txt", "a\n", "add a");
        run(&repo, &["tag", "v1.0"]);
        run(&repo, &["branch", "feature/x"]);
        let g = commit_graph_for_repo(&repo, None).await;
        let head = g.commits.iter().find(|c| c.sha == sha).unwrap();
        assert!(
            head.refs
                .iter()
                .any(|r| r.kind == RefKind::Tag && r.name == "v1.0"),
            "tag ref missing: {:?}",
            head.refs
        );
        assert!(
            head.refs
                .iter()
                .any(|r| r.kind == RefKind::Branch && r.name == "feature/x"),
            "branch ref missing: {:?}",
            head.refs
        );
        assert!(
            head.refs.iter().any(|r| r.kind == RefKind::Head),
            "HEAD ref missing: {:?}",
            head.refs
        );
        // A plain commit with no refs stays empty.
        let base = g.commits.iter().find(|c| c.sha != sha).unwrap();
        assert!(
            base.refs.is_empty(),
            "base should have no refs: {:?}",
            base.refs
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn graph_walks_mainline_divergence_not_just_head() {
        // A session branch that has diverged from master must show master's extra
        // commits as their own lane — i.e. the walk includes master, not only HEAD.
        let repo = temp_repo();
        // Pin the default branch name so the candidate list matches regardless of the
        // host git's init.defaultBranch (some default to `main`).
        run(&repo, &["branch", "-M", "master"]);
        let base = run(&repo, &["rev-parse", "HEAD"]);
        // master gains a commit the session branch will NOT contain.
        let master_only = commit_file(&repo, "m.txt", "m\n", "master only");
        // Session branch forks from base and adds its own commit → divergence.
        run(&repo, &["checkout", "-q", "-b", "agentic/x", &base]);
        let session_tip = commit_file(&repo, "s.txt", "s\n", "session only");

        let g = commit_graph_for_repo(&repo, Some(&base)).await;
        let shas: Vec<&str> = g.commits.iter().map(|c| c.sha.as_str()).collect();
        assert!(
            shas.contains(&session_tip.as_str()),
            "session tip missing: {shas:?}"
        );
        assert!(
            shas.contains(&master_only.as_str()),
            "master-only commit missing — mainline branch was not walked: {shas:?}",
        );
        let m = g.commits.iter().find(|c| c.sha == master_only).unwrap();
        assert!(
            m.refs.iter().any(|r| r.name == "master"),
            "master ref chip missing on the diverged commit: {:?}",
            m.refs,
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn files_for_a_real_sha() {
        let repo = temp_repo();
        let sha = commit_file(&repo, "new.txt", "one\ntwo\n", "add new");
        let files = commit_files_for_repo(&repo, &sha).await;
        let f = files.iter().find(|x| x.path == "new.txt").unwrap();
        assert_eq!(f.status, FileStatus::Added);
        assert_eq!(f.additions, 2);
        assert_eq!(f.deletions, 0);
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn files_for_working_includes_untracked_as_added() {
        let repo = temp_repo();
        std::fs::write(repo.join("README.md"), "# changed\n\nmore\n").unwrap();
        std::fs::write(repo.join("fresh.txt"), "x\n").unwrap();
        let files = commit_files_for_repo(&repo, "working").await;
        assert_eq!(
            files.iter().find(|x| x.path == "README.md").unwrap().status,
            FileStatus::Modified
        );
        assert_eq!(
            files.iter().find(|x| x.path == "fresh.txt").unwrap().status,
            FileStatus::Added
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    // ── Line-level diff ──────────────────────────────────────────────────────

    #[test]
    fn parse_file_diff_hunk_line_numbers() {
        // A modify: one context, one deletion, one addition. git emits @@ -1,2 +1,2 @@.
        let raw = "diff --git a/f.txt b/f.txt\nindex e69..abc 100644\n--- a/f.txt\n+++ b/f.txt\n@@ -1,2 +1,2 @@ ctx heading\n one\n-two\n+TWO\n".to_string();
        let d = parse_file_diff("f.txt", raw);
        assert!(!d.binary && !d.truncated);
        assert_eq!(d.status, FileStatus::Modified);
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (1, 2, 1, 2)
        );
        assert_eq!(h.header, "ctx heading");
        // context "one": both sides line 1.
        assert_eq!(
            h.lines[0],
            DiffLine {
                kind: DiffLineKind::Context,
                old_line: Some(1),
                new_line: Some(1),
                content: "one".into()
            }
        );
        // deletion "two": old line 2, no new.
        assert_eq!(
            h.lines[1],
            DiffLine {
                kind: DiffLineKind::Del,
                old_line: Some(2),
                new_line: None,
                content: "two".into()
            }
        );
        // addition "TWO": new line 2, no old.
        assert_eq!(
            h.lines[2],
            DiffLine {
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(2),
                content: "TWO".into()
            }
        );
    }

    #[test]
    fn parse_file_diff_detects_new_and_binary() {
        let added = parse_file_diff("n.txt",
            "diff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+hi\n".into());
        assert_eq!(added.status, FileStatus::Added);
        assert_eq!(added.hunks[0].lines[0].kind, DiffLineKind::Add);
        assert_eq!(added.hunks[0].lines[0].new_line, Some(1));

        let bin = parse_file_diff("img.png",
            "diff --git a/img.png b/img.png\nindex 0..1 100644\nBinary files a/img.png and b/img.png differ\n".into());
        assert!(bin.binary);
        assert!(bin.hunks.is_empty());
    }

    #[tokio::test]
    async fn diff_for_real_sha_and_working_and_untracked() {
        let repo = temp_repo();
        // README.md exists from temp_repo() with "# r\n". Modify + commit it.
        std::fs::write(repo.join("README.md"), "# r\nsecond line\n").unwrap();
        let sha = {
            run(&repo, &["add", "."]);
            run(&repo, &["commit", "-q", "-m", "edit"]);
            run(&repo, &["rev-parse", "HEAD"])
        };
        let d = commit_diff_for_repo(&repo, &sha, "README.md").await;
        assert!(!d.binary);
        assert!(
            d.hunks
                .iter()
                .flat_map(|h| &h.lines)
                .any(|l| l.kind == DiffLineKind::Add && l.content == "second line"),
            "expected an added 'second line', got {:?}",
            d.hunks
        );

        // Working-tree (tracked) modification not yet committed.
        std::fs::write(repo.join("README.md"), "# r\nsecond line\nthird\n").unwrap();
        let w = commit_diff_for_repo(&repo, "working", "README.md").await;
        assert!(w
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .any(|l| l.kind == DiffLineKind::Add && l.content == "third"));

        // Untracked file → synthetic all-additions diff.
        std::fs::write(repo.join("fresh.txt"), "alpha\nbeta\n").unwrap();
        let u = commit_diff_for_repo(&repo, "working", "fresh.txt").await;
        assert_eq!(u.status, FileStatus::Added);
        assert!(!u.binary);
        assert_eq!(u.hunks.len(), 1);
        let contents: Vec<&str> = u.hunks[0]
            .lines
            .iter()
            .map(|l| l.content.as_str())
            .collect();
        assert_eq!(contents, vec!["alpha", "beta"]);
        assert_eq!(u.hunks[0].lines[0].new_line, Some(1));
        assert_eq!(u.hunks[0].lines[1].new_line, Some(2));
        std::fs::remove_dir_all(&repo).ok();
    }

    #[tokio::test]
    async fn untracked_empty_file_has_no_hunks() {
        let repo = temp_repo();
        std::fs::write(repo.join("empty.txt"), "").unwrap();
        let d = commit_diff_for_repo(&repo, "working", "empty.txt").await;
        assert_eq!(d.status, FileStatus::Added);
        assert!(!d.binary);
        assert!(
            d.hunks.is_empty(),
            "an empty file must yield 0 hunks, got {:?}",
            d.hunks
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn parse_file_diff_strips_crlf() {
        // CRLF line endings: every line ends with \r before the \n the parser splits on.
        let raw = "diff --git a/f.txt b/f.txt\r\n--- a/f.txt\r\n+++ b/f.txt\r\n@@ -1 +1 @@ ctx\r\n-old\r\n+new\r\n".to_string();
        let d = parse_file_diff("f.txt", raw);
        let h = &d.hunks[0];
        assert_eq!(h.header, "ctx", "header must not keep a trailing CR");
        assert_eq!(
            h.lines[0],
            DiffLine {
                kind: DiffLineKind::Del,
                old_line: Some(1),
                new_line: None,
                content: "old".into()
            }
        );
        assert_eq!(
            h.lines[1],
            DiffLine {
                kind: DiffLineKind::Add,
                old_line: None,
                new_line: Some(1),
                content: "new".into()
            }
        );
    }

    #[test]
    fn cap_diff_truncates_at_newline() {
        let big = "x\n".repeat(MAX_DIFF_BYTES); // ~2x the cap
        let (kept, truncated) = cap_diff(big);
        assert!(truncated);
        assert!(kept.len() <= MAX_DIFF_BYTES);
        assert!(kept.ends_with('\n'), "must cut at a line boundary");
    }
}
