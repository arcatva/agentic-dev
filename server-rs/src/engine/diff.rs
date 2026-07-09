//! Engine: structured-diff read methods (commit-history graph + changed files).
//! Read-only — works for running and terminal sessions.

use super::{Engine, EngineError};

impl Engine {
    /// Per-repo commit-history graph. Read-only (works for running and terminal sessions).
    pub async fn commit_graph(&self, id: &str) -> Result<serde_json::Value, EngineError> {
        let s = self.get(id).await.ok_or_else(|| EngineError::NotFound(id.to_string()))?;
        let wt_root = s.worktree_path.clone().ok_or(EngineError::NoWorktree)?;
        // Run the per-repo git reads concurrently, then re-assemble in repo order so the
        // response stays deterministic.
        let mut handles = Vec::with_capacity(s.repos.len());
        for repo in &s.repos {
            let wt = std::path::Path::new(&wt_root).join(repo);
            let base = s.base_shas.get(repo).and_then(|o| o.clone());
            let repo = repo.clone();
            handles.push(tokio::spawn(async move {
                let g = crate::engine::structured_diff::commit_graph_for_repo(&wt, base.as_deref()).await;
                serde_json::json!({ "repo": repo, "commits": g.commits, "uncommitted": g.uncommitted })
            }));
        }
        let mut out = Vec::with_capacity(handles.len());
        for h in handles {
            out.push(h.await.map_err(|e| EngineError::Internal(format!("commit_graph task: {e}")))?);
        }
        Ok(serde_json::Value::Array(out))
    }

    /// Changed-file list for one commit (or the working tree) in a repo. Validates repo ∈ s.repos and sha.
    pub async fn commit_files(&self, id: &str, repo: &str, sha: &str) -> Result<Vec<crate::engine::structured_diff::CommitFile>, EngineError> {
        let s = self.get(id).await.ok_or_else(|| EngineError::NotFound(id.to_string()))?;
        if !s.repos.iter().any(|r| r == repo) { return Err(EngineError::BadInput(format!("unknown repo: {repo}"))); }
        let valid_sha = sha == "working"
            || (sha.len() >= 4 && sha.len() <= 40 && sha.chars().all(|c| c.is_ascii_hexdigit()));
        if !valid_sha { return Err(EngineError::BadInput(format!("bad sha: {sha}"))); }
        let wt_root = s.worktree_path.clone().ok_or(EngineError::NoWorktree)?;
        let wt = std::path::Path::new(&wt_root).join(repo);
        Ok(crate::engine::structured_diff::commit_files_for_repo(&wt, sha).await)
    }

    /// Line-level diff for one file in a commit (or the working tree). Validates repo ∈ s.repos,
    /// the sha shape, and that `path` is a safe relative path (no leading `/`, no `..` segment) —
    /// the path is passed to `git -- <path>` and joined to the worktree for untracked reads.
    pub async fn commit_diff(&self, id: &str, repo: &str, sha: &str, path: &str)
        -> Result<crate::engine::structured_diff::FileDiff, EngineError>
    {
        let s = self.get(id).await.ok_or_else(|| EngineError::NotFound(id.to_string()))?;
        if !s.repos.iter().any(|r| r == repo) { return Err(EngineError::BadInput(format!("unknown repo: {repo}"))); }
        let valid_sha = sha == "working"
            || (sha.len() >= 4 && sha.len() <= 40 && sha.chars().all(|c| c.is_ascii_hexdigit()));
        if !valid_sha { return Err(EngineError::BadInput(format!("bad sha: {sha}"))); }
        if path.is_empty() { return Err(EngineError::BadInput("path required".to_string())); }
        if path.starts_with('/') || path.split('/').any(|seg| seg == "..") {
            return Err(EngineError::BadInput(format!("bad path: {path}")));
        }
        let wt_root = s.worktree_path.clone().ok_or(EngineError::NoWorktree)?;
        let wt = std::path::Path::new(&wt_root).join(repo);
        Ok(crate::engine::structured_diff::commit_diff_for_repo(&wt, sha, path).await)
    }
}
