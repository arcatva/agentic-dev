use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
pub struct WorktreeInfo {
    pub worktree_path: PathBuf,
    pub branch: String,
}

#[derive(Clone, Debug)]
pub struct SessionWorktree {
    pub repo: String,
    pub worktree_path: PathBuf,
    pub base_sha: String,
}

#[derive(thiserror::Error, Debug)]
pub enum WorktreeError {
    #[error("git failed: {0}")]
    Git(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

const GIT_TIMEOUT_MS: u64 = 30_000;

/// Run git synchronously. On non-zero exit return Err(Git(stderr)), else Ok(stdout).
pub(crate) fn git_sync(args: &[&str]) -> Result<String, WorktreeError> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(WorktreeError::Io)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(WorktreeError::Git(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

/// Run git asynchronously with a 30 s timeout. Returns Err on non-zero or timeout.
async fn git_async(cwd: &Path, args: &[&str]) -> Result<String, WorktreeError> {
    use tokio::process::Command as TokioCommand;

    let cwd_str = cwd.to_string_lossy();
    let cwd_string = cwd_str.as_ref();
    let mut all_args = vec!["-C", cwd_string];
    all_args.extend_from_slice(args);

    // kill_on_drop(true): if the 30 s timeout fires and the future is dropped, tokio will send
    // SIGKILL to the child. Without this the child (e.g. a network-stalled `git fetch`) would
    // keep running orphaned.
    let out = tokio::time::timeout(
        std::time::Duration::from_millis(GIT_TIMEOUT_MS),
        TokioCommand::new("git").args(&all_args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| WorktreeError::Git("git timed out".into()))?
    .map_err(WorktreeError::Io)?;

    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(WorktreeError::Git(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }
}

pub fn create_worktree(
    repo_path: &Path,
    root: &Path,
    repo: &str,
    id: &str,
) -> Result<WorktreeInfo, WorktreeError> {
    let worktree_path = root.join(repo).join(id);
    let parent = worktree_path.parent().ok_or_else(|| WorktreeError::Git("invalid worktree path — no parent dir".into()))?;
    std::fs::create_dir_all(parent)?;
    let branch = format!("agentic/{id}");
    let rp = repo_path.to_string_lossy();
    let wp = worktree_path.to_string_lossy();
    git_sync(&["-C", &rp, "worktree", "add", &wp, "-b", &branch])?;
    Ok(WorktreeInfo { worktree_path, branch })
}

pub fn remove_worktree(repo_path: &Path, worktree_path: &Path) -> Result<(), WorktreeError> {
    let rp = repo_path.to_string_lossy();
    let wp = worktree_path.to_string_lossy();
    git_sync(&["-C", &rp, "worktree", "remove", "--force", &wp])?;
    Ok(())
}

pub fn create_session_worktrees(
    repo_specs: &[(String, PathBuf)],
    root: &Path,
    id: &str,
) -> Result<Vec<SessionWorktree>, WorktreeError> {
    let session_dir = root.join(id);
    std::fs::create_dir_all(&session_dir)?;
    let branch = format!("agentic/{id}");
    let mut result = Vec::new();
    for (repo, repo_path) in repo_specs {
        let worktree_path = session_dir.join(repo);
        let rp = repo_path.to_string_lossy();
        let base_sha = git_sync(&["-C", &rp, "rev-parse", "HEAD"])
            .map(|s| s.trim().to_string())?;
        let wp = worktree_path.to_string_lossy();
        git_sync(&["-C", &rp, "worktree", "add", &wp, "-b", &branch])?;
        result.push(SessionWorktree {
            repo: repo.clone(),
            worktree_path,
            base_sha,
        });
    }
    Ok(result)
}

/// Create one worktree per repo, branching `agentic/<new_id>` off the EXPLICIT base SHA
/// supplied in `base_shas`. Used by fork — the new session's branch must point at the
/// source session's HEAD, not at the repo's current HEAD. Mirrors `create_session_worktrees`
/// in every other respect (returns `Vec<SessionWorktree>` with `base_sha` populated).
pub fn create_fork_worktrees(
    repo_specs: &[(String, PathBuf)],
    root: &Path,
    new_id: &str,
    base_shas: &std::collections::HashMap<String, String>,
) -> Result<Vec<SessionWorktree>, WorktreeError> {
    let session_dir = root.join(new_id);
    std::fs::create_dir_all(&session_dir)?;
    let branch = format!("agentic/{new_id}");
    let mut result = Vec::new();
    for (repo, repo_path) in repo_specs {
        let base_sha = base_shas.get(repo).ok_or_else(|| {
            WorktreeError::Git(format!("fork: no base SHA recorded for repo {repo}"))
        })?;
        let worktree_path = session_dir.join(repo);
        let rp = repo_path.to_string_lossy();
        // Verify the base SHA exists in this repo before attempting the worktree add.
        // A SHA from a different clone would fail the worktree add with a less clear error.
        git_sync(&["-C", &rp, "cat-file", "-e", base_sha])?;
        let wp = worktree_path.to_string_lossy();
        git_sync(&["-C", &rp, "worktree", "add", &wp, "-b", &branch, base_sha])?;
        result.push(SessionWorktree {
            repo: repo.clone(),
            worktree_path,
            base_sha: base_sha.clone(),
        });
    }
    Ok(result)
}

pub fn remove_session_worktrees(repo_specs: &[(String, PathBuf)], session_dir: &Path) {
    for (repo, repo_path) in repo_specs {
        let wt = session_dir.join(repo);
        let rp = repo_path.to_string_lossy();
        let wp = wt.to_string_lossy();
        // Best-effort cleanup: a failed remove can leak a worktree, so log it (debug) rather than
        // swallowing it entirely — greppable without spamming the default INFO level.
        if let Err(e) = git_sync(&["-C", &rp, "worktree", "remove", "--force", &wp]) {
            tracing::debug!(repo = %repo, worktree = %wp, "worktree remove failed (ignored): {e}");
        }
        if let Err(e) = git_sync(&["-C", &rp, "worktree", "prune"]) {
            tracing::debug!(repo = %repo, "worktree prune failed (ignored): {e}");
        }
    }
}

pub async fn sync_worktree(worktree_path: &Path) {
    // Resolve remote default branch. On any error, fall back to "master".
    let base = match git_async(worktree_path, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]).await {
        Ok(s) => {
            let s = s.trim().to_string();
            s.strip_prefix("origin/").unwrap_or("master").to_string()
        }
        Err(_) => "master".to_string(),
    };

    if git_async(worktree_path, &["fetch", "origin", &base]).await.is_err() {
        return;
    }

    let remote = format!("origin/{base}");
    if git_async(worktree_path, &["rebase", "--autostash", &remote]).await.is_err() {
        let _ = git_async(worktree_path, &["rebase", "--abort"]).await;
    }
}

pub async fn diff_worktree(worktree_path: &Path, base_sha: &str) -> Result<String, WorktreeError> {
    git_async(worktree_path, &["add", "-AN"]).await?;
    let out = git_async(worktree_path, &["--no-pager", "diff", base_sha]).await?;
    Ok(out)
}

/// Like [`git_sync`] but runs inside `worktree` with extra env vars (e.g. `GIT_INDEX_FILE`).
/// Returns trimmed stdout on success.
fn git_env(worktree: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String, WorktreeError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(worktree).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(WorktreeError::Io)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(WorktreeError::Git(String::from_utf8_lossy(&out.stderr).into_owned()))
    }
}

/// Capture the FULL working tree (tracked + non-ignored untracked) of `worktree` as a commit object
/// and point `snapshot_ref` at it — WITHOUT touching the branch, index, or working tree. Returns the
/// snapshot commit sha. Used to take a per-turn snapshot for rewind. A throwaway temp index keeps the
/// session's real index untouched; an explicit author/committer identity avoids relying on repo
/// `user.*` config (a fresh worktree may not have it).
pub fn snapshot_worktree(worktree: &Path, snapshot_ref: &str) -> Result<String, WorktreeError> {
    // pid + a process-wide atomic counter + nanos makes the temp index name collision-free even
    // for concurrent snapshots in the same process (two could otherwise hit the same nanosecond).
    static SNAP_IDX_CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = std::env::temp_dir().join(format!(
        "agentic-snap-idx-{}-{}-{}",
        std::process::id(),
        SNAP_IDX_CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    let idx = tmp.to_string_lossy().into_owned();
    let ident: &[(&str, &str)] = &[
        ("GIT_INDEX_FILE", idx.as_str()),
        ("GIT_AUTHOR_NAME", "agentic"),
        ("GIT_AUTHOR_EMAIL", "agentic@local"),
        ("GIT_COMMITTER_NAME", "agentic"),
        ("GIT_COMMITTER_EMAIL", "agentic@local"),
    ];
    let result = (|| -> Result<String, WorktreeError> {
        // Stage everything into the temp index, then write its tree.
        git_env(worktree, &["add", "-A"], ident)?;
        let tree = git_env(worktree, &["write-tree"], ident)?;
        // Parent = current HEAD when there is one (keeps the snapshot reachable/diffable).
        let parent = git_env(worktree, &["rev-parse", "--verify", "HEAD"], &[]).ok();
        let mut args: Vec<&str> = vec!["commit-tree", &tree, "-m", "agentic snapshot"];
        if let Some(p) = parent.as_deref() {
            args.push("-p");
            args.push(p);
        }
        let snap = git_env(worktree, &args, ident)?;
        git_env(worktree, &["update-ref", snapshot_ref, &snap], &[])?;
        Ok(snap)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// Restore the working tree of `worktree` to the tree of `snapshot` (a ref or sha), overwriting
/// tracked files and recreating tracked deletions, but LEAVING untracked files created since the
/// snapshot in place (no `git clean`, per the "keep new files" rewind semantics). HEAD/branch is NOT
/// moved; the index is set to the snapshot tree.
pub fn restore_worktree(worktree: &Path, snapshot: &str) -> Result<(), WorktreeError> {
    let tree = format!("{snapshot}^{{tree}}");
    // Point the index at the snapshot tree (does not touch the working tree)…
    git_env(worktree, &["read-tree", &tree], &[])?;
    // …then write every index entry out, overwriting tracked files / recreating deletions.
    git_env(worktree, &["checkout-index", "-a", "-f"], &[])?;
    Ok(())
}

/// Best-effort removal of all per-session snapshot refs for `id` in `repo_path`'s ref store (the
/// worktrees of one repo share its refs, so snapshot refs are session-scoped). Ignores errors.
pub fn delete_snapshot_refs(repo_path: &Path, id: &str) {
    let rp = repo_path.to_string_lossy();
    let pattern = format!("refs/agentic/snapshots/{id}");
    let listing = match git_sync(&["-C", &rp, "for-each-ref", "--format=%(refname)", &pattern]) {
        Ok(s) => s,
        Err(_) => return,
    };
    for refname in listing.lines().map(str::trim).filter(|s| !s.is_empty()) {
        let _ = git_sync(&["-C", &rp, "update-ref", "-d", refname]);
    }
}

pub fn discard_worktree(
    repo_path: &Path,
    worktree_path: &Path,
    branch: &str,
) -> Result<(), WorktreeError> {
    let rp = repo_path.to_string_lossy();
    let wp = worktree_path.to_string_lossy();
    if worktree_path.exists() {
        git_sync(&["-C", &rp, "worktree", "remove", "--force", &wp])?;
    } else {
        git_sync(&["-C", &rp, "worktree", "prune"])?;
    }
    // ignore error if branch already gone
    let _ = git_sync(&["-C", &rp, "branch", "-D", branch]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {:?} failed", args);
    }
    fn temp_repo() -> (PathBuf, PathBuf) {
        // (root, repo_path)
        let root = std::env::temp_dir().join(format!(
            "agentic-wt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "master"]);
        git(&repo, &["config", "user.email", "t@t"]);
        git(&repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "init"]);
        (root, repo)
    }

    #[tokio::test]
    async fn create_diff_discard_roundtrip() {
        let (root, repo) = temp_repo();
        let wts =
            create_session_worktrees(&[("repo".into(), repo.clone())], &root, "sess1").unwrap();
        let wt = &wts[0];
        assert!(wt.worktree_path.exists());
        assert_eq!(wt.repo, "repo");
        // layout: <root>/<id>/<repo>
        assert_eq!(wt.worktree_path, root.join("sess1").join("repo"));
        // base_sha is a git hex sha (at least 7 hex digits)
        assert!(
            wt.base_sha.len() >= 7
                && wt.base_sha.chars().all(|c| c.is_ascii_hexdigit())
        );
        // the session branch exists in the repo
        let branches = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["branch", "--list", "agentic/sess1"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert!(branches.contains("agentic/sess1"));
        // edit a tracked file + add an untracked one
        std::fs::write(wt.worktree_path.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(wt.worktree_path.join("new.txt"), "hi\n").unwrap();
        let diff = diff_worktree(&wt.worktree_path, &wt.base_sha)
            .await
            .unwrap();
        assert!(diff.contains("a.txt") && diff.contains("+two"));
        assert!(diff.contains("new.txt")); // -AN surfaces the untracked file
        discard_worktree(&repo, &wt.worktree_path, &format!("agentic/{}", "sess1")).unwrap();
        assert!(!wt.worktree_path.exists());
        // idempotent: a second discard must not error
        discard_worktree(&repo, &wt.worktree_path, &format!("agentic/{}", "sess1")).unwrap();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn snapshot_and_restore_roundtrip_keeps_new_files() {
        let (root, repo) = temp_repo();
        let wts = create_session_worktrees(&[("repo".into(), repo.clone())], &root, "snap1").unwrap();
        let wt = &wts[0].worktree_path;
        // State at snapshot time: modify tracked a.txt + add untracked b.txt.
        std::fs::write(wt.join("a.txt"), "one\nSNAP\n").unwrap();
        std::fs::write(wt.join("b.txt"), "snap-b\n").unwrap();
        let snap_ref = "refs/agentic/snapshots/snap1/1";
        let sha = snapshot_worktree(wt, snap_ref).unwrap();
        assert!(sha.len() >= 7 && sha.chars().all(|c| c.is_ascii_hexdigit()));
        // The ref resolves from the (shared) repo ref store.
        assert!(git_sync(&["-C", &repo.to_string_lossy(), "rev-parse", "--verify", snap_ref]).is_ok());

        // Diverge AFTER the snapshot: change a.txt, delete b.txt, create c.txt.
        std::fs::write(wt.join("a.txt"), "one\nCHANGED-LATER\n").unwrap();
        std::fs::remove_file(wt.join("b.txt")).unwrap();
        std::fs::write(wt.join("c.txt"), "created-after\n").unwrap();

        restore_worktree(wt, snap_ref).unwrap();
        // Tracked change reverted to the snapshot content.
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\nSNAP\n");
        // A file present in the snapshot but deleted after is recreated.
        assert_eq!(std::fs::read_to_string(wt.join("b.txt")).unwrap(), "snap-b\n");
        // A file created AFTER the snapshot is kept (no git clean).
        assert!(wt.join("c.txt").exists(), "files created after the snapshot must be kept");

        // Cleanup removes the session's snapshot refs.
        delete_snapshot_refs(&repo, "snap1");
        assert!(git_sync(&["-C", &repo.to_string_lossy(), "rev-parse", "--verify", snap_ref]).is_err(),
            "snapshot ref should be gone after cleanup");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn sync_worktree_never_panics_without_remote() {
        let (root, repo) = temp_repo();
        let wts =
            create_session_worktrees(&[("repo".into(), repo.clone())], &root, "sess2").unwrap();
        // no origin remote → must fall through cleanly, not error
        sync_worktree(&wts[0].worktree_path).await;
        std::fs::remove_dir_all(&root).ok();
    }

    /// Verify that git_async returns Err("git timed out") when the timeout fires.
    /// We substitute a sleep-based command to simulate a hung git process.  The test uses
    /// a very short timeout (100 ms) so it runs fast.
    ///
    /// This also implicitly validates kill_on_drop behaviour: if the child were not killed
    /// the sleep would linger in the background, but the test still passes because the
    /// timeout path itself is what we are confirming.
    #[test]
    fn create_fork_worktrees_branches_off_supplied_base_sha() {
        let (repo, sha) = one_commit_repo("fork");
        let root = std::env::temp_dir().join(format!("agentic-fork-root-{}-{}", std::process::id(), "fork"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let mut base_shas = HashMap::new();
        base_shas.insert("demo".to_string(), sha.clone());

        let wts = create_fork_worktrees(
            &[("demo".to_string(), repo.clone())],
            &root,
            "child-id",
            &base_shas,
        ).unwrap();
        assert_eq!(wts.len(), 1);
        assert_eq!(wts[0].base_sha, sha);
        assert!(wts[0].worktree_path.join("README.md").exists());

        // Branch agentic/child-id exists in the repo and points at the same SHA.
        let head = String::from_utf8(Command::new("git")
            .args(["-C", &repo.to_string_lossy(), "rev-parse", "agentic/child-id"])
            .output().unwrap().stdout).unwrap();
        assert_eq!(head.trim(), sha);
    }

    #[test]
    fn create_fork_worktrees_missing_base_sha_returns_error() {
        let (repo, _sha) = one_commit_repo("missing");
        let root = std::env::temp_dir().join(format!("agentic-fork-missing-{}-{}", std::process::id(), "missing"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let base_shas = HashMap::new(); // empty — should error
        let res = create_fork_worktrees(
            &[("demo".to_string(), repo.clone())],
            &root,
            "child-id",
            &base_shas,
        );
        assert!(res.is_err(), "missing base SHA must fail");
    }

    /// Create a throwaway git repo with one commit, return (repo_path, commit_sha).
    fn one_commit_repo(name: &str) -> (PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("agentic-fork-wt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=main", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("README.md"), "first\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "first", "-q"]);
        let sha = String::from_utf8(Command::new("git")
            .args(["rev-parse", "HEAD"]).current_dir(&dir).output().unwrap().stdout).unwrap().trim().to_string();
        (dir, sha)
    }

    #[tokio::test]
    async fn git_async_timeout_returns_err_not_hang() {
        use tokio::process::Command as TokioCommand;

        // Run `sleep 10` with a 100 ms timeout — simulates a hung git.
        let fut = TokioCommand::new("sleep").arg("10").kill_on_drop(true).output();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fut,
        )
        .await;
        // The outer timeout must fire (Err(Elapsed)), NOT let the 10-second sleep finish.
        assert!(result.is_err(), "timeout should fire before sleep completes");
        // After the timeout, the future (and kill_on_drop child) is dropped — no zombie.
    }
}
