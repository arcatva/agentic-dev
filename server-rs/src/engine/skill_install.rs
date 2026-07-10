//! Install skills from external GitHub sources ("skill store").
//!
//! Two halves, split for testability:
//! - PURE planning/writing: [parse_github_source] → [plan_from_tree] → [write_skill_files]
//!   (unit-tested, no network).
//! - Thin async network layer: [fetch_catalog] / [install_from_source] using reqwest against
//!   the GitHub trees API + raw.githubusercontent.com (public repos, unauthenticated).
//!
//! The curated catalog source is the official `anthropics/skills` repository; arbitrary
//! sources are accepted as `owner/repo[/path]` or a full `https://github.com/...` URL.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Curated catalog repo (the "store" front page): every directory containing a SKILL.md.
pub const CATALOG_OWNER: &str = "anthropics";
pub const CATALOG_REPO: &str = "skills";

/// Caps — a skill is a text bundle, not a software distribution.
const MAX_FILES: usize = 40;
const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024;
const CATALOG_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
/// GitHub rejects requests without a User-Agent.
const USER_AGENT: &str = "agentic-dev-server";

#[derive(serde::Serialize, Clone, Debug, PartialEq)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    /// Ready-to-install source reference ("owner/repo/path") — POST it back to install.
    pub source: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GithubRef {
    pub owner: String,
    pub repo: String,
    /// Branch name, or "HEAD" for the repo's default branch (works for both the trees API
    /// and raw.githubusercontent.com).
    pub branch: String,
    /// Directory inside the repo ("" = repo root is the skill).
    pub path: String,
}

/// One file to download: repo-relative source path → skill-relative destination path.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedFile {
    pub repo_path: String,
    pub rel_path: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstallPlan {
    pub name: String,
    pub files: Vec<PlannedFile>,
}

fn valid_gh_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Parse a user-supplied skill source into a [GithubRef]. Accepted forms:
/// - `https://github.com/{owner}/{repo}` (repo root, default branch)
/// - `https://github.com/{owner}/{repo}/tree/{branch}/{path...}`
/// - shorthand `{owner}/{repo}[/{path...}]` (default branch)
pub fn parse_github_source(src: &str) -> Result<GithubRef, String> {
    let src = src.trim().trim_end_matches('/');
    if src.is_empty() {
        return Err("source is required".into());
    }
    let rest = src
        .strip_prefix("https://github.com/")
        .or_else(|| src.strip_prefix("http://github.com/"))
        .or_else(|| src.strip_prefix("github.com/"));
    let (parts, from_url): (Vec<&str>, bool) = match rest {
        Some(r) => (r.split('/').collect(), true),
        None => {
            if src.contains("://") {
                return Err("only github.com sources are supported".into());
            }
            (src.split('/').collect(), false)
        }
    };
    if parts.len() < 2 || !valid_gh_segment(parts[0]) || !valid_gh_segment(parts[1]) {
        return Err("source must be owner/repo[/path] or a github.com URL".into());
    }
    let (owner, repo) = (parts[0].to_string(), parts[1].to_string());
    let (branch, path_parts): (String, &[&str]) = match parts.get(2) {
        None => ("HEAD".into(), &[]),
        // URL form: /tree/{branch}/{path...} (also tolerate /blob/… pointing at a dir).
        Some(&"tree") | Some(&"blob") if from_url => {
            let Some(branch) = parts.get(3).filter(|b| valid_gh_segment(b)) else {
                return Err("URL is missing the branch after /tree/".into());
            };
            (branch.to_string(), parts.get(4..).unwrap_or(&[]))
        }
        // Shorthand: everything after owner/repo is the path, default branch.
        Some(_) if !from_url => ("HEAD".into(), &parts[2..]),
        Some(_) => return Err("unsupported github.com URL — use the /tree/<branch>/<path> form".into()),
    };
    let path = path_parts.join("/");
    if !path.is_empty() && !safe_rel_path(path.trim_matches('/')) {
        return Err("path contains unsupported characters".into());
    }
    Ok(GithubRef { owner, repo, branch, path: path.trim_matches('/').to_string() })
}

/// Is every component of `rel` a plain, safe path segment? Beyond traversal characters this
/// also rejects URL metacharacters (`?`, `#`, `%`) — these paths are interpolated into
/// raw.githubusercontent.com URLs, where they would change query/fragment/encoding semantics.
fn safe_rel_path(rel: &str) -> bool {
    !rel.is_empty()
        && rel.split('/').all(|seg| {
            !seg.is_empty()
                && seg != "."
                && seg != ".."
                && !seg.contains(['\\', '\0', '?', '#', '%'])
        })
}

/// Build an install plan from a GitHub `git/trees?recursive=1` response: all blob files under
/// [GithubRef::path], which must contain a SKILL.md at its root. Pure — fixture-testable.
pub fn plan_from_tree(tree: &serde_json::Value, gh: &GithubRef) -> Result<InstallPlan, String> {
    let entries = tree
        .get("tree")
        .and_then(|t| t.as_array())
        .ok_or("unexpected GitHub tree response")?;
    // A truncated listing could silently drop companion files of the skill — never plan from it.
    if tree.get("truncated").and_then(|t| t.as_bool()) == Some(true) {
        return Err("repository too large to scan completely — install from a smaller repository".into());
    }

    let prefix = if gh.path.is_empty() { String::new() } else { format!("{}/", gh.path) };
    let name = if gh.path.is_empty() {
        gh.repo.clone()
    } else {
        gh.path.rsplit('/').next().unwrap_or(&gh.path).to_string()
    };

    let mut files = Vec::new();
    let mut total: u64 = 0;
    let mut has_skill_md = false;
    for e in entries {
        if e.get("type").and_then(|t| t.as_str()) != Some("blob") {
            continue;
        }
        let Some(p) = e.get("path").and_then(|p| p.as_str()) else { continue };
        let Some(rel) = p.strip_prefix(&prefix) else { continue };
        if !prefix.is_empty() && rel == p {
            continue; // no common prefix (strip_prefix returned original) — defensive
        }
        // Symlinks (mode 120000) could point anywhere on disk — never install them.
        if e.get("mode").and_then(|m| m.as_str()) == Some("120000") {
            return Err(format!("refusing to install: '{p}' is a symlink"));
        }
        if !safe_rel_path(rel) {
            return Err(format!("refusing to install: unsafe path '{p}'"));
        }
        let size = e.get("size").and_then(|s| s.as_u64()).unwrap_or(0);
        if size > MAX_FILE_BYTES {
            return Err(format!("file '{p}' exceeds the {MAX_FILE_BYTES}-byte per-file limit"));
        }
        total += size;
        if rel == "SKILL.md" {
            has_skill_md = true;
        }
        files.push(PlannedFile { repo_path: p.to_string(), rel_path: rel.to_string() });
    }
    if !has_skill_md {
        return Err(format!(
            "no SKILL.md found at '{}' — the source must point at a skill directory",
            if gh.path.is_empty() { "the repository root" } else { &gh.path },
        ));
    }
    if files.len() > MAX_FILES {
        return Err(format!("skill has {} files — more than the {MAX_FILES}-file limit", files.len()));
    }
    if total > MAX_TOTAL_BYTES {
        return Err(format!("skill is {total} bytes — more than the {MAX_TOTAL_BYTES}-byte limit"));
    }
    if !valid_name(&name) {
        return Err(format!("'{name}' is not a valid skill name"));
    }
    Ok(InstallPlan { name, files })
}

/// Same rules as the API layer's `valid_component_name` — an installed skill MUST be
/// manageable by the delete/toggle routes afterwards (a looser rule here would create
/// undeletable, untogglable skills), plus the single-path-component guard.
fn valid_name(name: &str) -> bool {
    use std::path::Component;
    let mut components = Path::new(name).components();
    let single = matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    single
        && !name.is_empty()
        && name != "agentic"
        && !name.starts_with('-')
        && !name.chars().all(|c| c == '.')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// Write downloaded skill files under `<skills_dir>/<name>/…`, atomically: everything goes
/// into a temp sibling dir first, then one rename publishes the skill. Refuses if the skill
/// already exists. Pure filesystem — no network.
pub fn write_skill_files(skills_dir: &Path, name: &str, files: &[(String, Vec<u8>)]) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("invalid skill name '{name}'")));
    }
    let target = skills_dir.join(name);
    if target.exists() {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("skill '{name}' already exists")));
    }
    // Unique per call (pid + atomic counter) so concurrent installs of the same skill name
    // can't scribble into each other's temp dir; the loser of the final rename gets ENOTEMPTY
    // or AlreadyExists rather than corrupting the winner's files.
    static NONCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = skills_dir.join(format!(".install-{}-{}-{}", name, std::process::id(), nonce));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    let result = (|| -> io::Result<()> {
        for (rel, bytes) in files {
            if !safe_rel_path(rel) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("unsafe path '{rel}'")));
            }
            let dest = tmp.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, bytes)?;
        }
        std::fs::rename(&tmp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp); // best-effort cleanup; the skill dir never appeared
    }
    result
}

/// Parse `name:`/`description:` out of a SKILL.md frontmatter (same forgiving rules as
/// [crate::engine::skills::list_skills]: first match wins, missing → fallback).
fn frontmatter_description(text: &str) -> String {
    let Some(fm) = text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")) else {
        return String::new();
    };
    for line in fm.0.lines() {
        if let Some(v) = line.strip_prefix("description:") {
            return v.trim().to_string();
        }
    }
    String::new()
}

// ── Network layer ───────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("http client: {e}"))
}

async fn fetch_tree(client: &reqwest::Client, gh: &GithubRef) -> Result<serde_json::Value, String> {
    let url = format!(
        "https://api.github.com/repos/{}/{}/git/trees/{}?recursive=1",
        gh.owner, gh.repo, gh.branch,
    );
    let resp = client.get(&url).send().await.map_err(|e| format!("GitHub unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("{}/{} (branch {}) not found on GitHub", gh.owner, gh.repo, gh.branch));
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub tree API returned {}", resp.status()));
    }
    resp.json().await.map_err(|e| format!("GitHub tree response: {e}"))
}

fn raw_url(gh: &GithubRef, repo_path: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{}/{}/{}",
        gh.owner, gh.repo, gh.branch, repo_path,
    )
}

async fn fetch_raw(client: &reqwest::Client, url: &str, cap: u64) -> Result<Vec<u8>, String> {
    let mut resp = client.get(url).send().await.map_err(|e| format!("download failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("download of {url} returned {}", resp.status()));
    }
    // Enforce the cap WHILE streaming (chunk by chunk), not after buffering — a server that
    // lies about (or omits) Content-Length must not balloon this process's memory.
    if resp.content_length().unwrap_or(0) > cap {
        return Err(format!("{url} is larger than the {cap}-byte limit"));
    }
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| format!("download of {url}: {e}"))? {
        if (out.len() + chunk.len()) as u64 > cap {
            return Err(format!("{url} is larger than the {cap}-byte limit"));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// Resolve a branch (or "HEAD") to its current commit SHA, so the tree scan and every raw
/// download read the SAME snapshot — otherwise a push between scan and download could swap
/// file contents (or sneak a symlink past the plan's mode check).
async fn resolve_commit_sha(client: &reqwest::Client, gh: &GithubRef) -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{}/{}/commits/{}", gh.owner, gh.repo, gh.branch);
    let resp = client.get(&url).send().await.map_err(|e| format!("GitHub unreachable: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(format!("{}/{} (branch {}) not found on GitHub", gh.owner, gh.repo, gh.branch));
    }
    if !resp.status().is_success() {
        return Err(format!("GitHub commits API returned {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().await.map_err(|e| format!("GitHub commits response: {e}"))?;
    let sha = v.get("sha").and_then(|s| s.as_str()).ok_or("GitHub commits response missing sha")?;
    if !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("GitHub returned a non-hex commit sha".into());
    }
    Ok(sha.to_string())
}

/// Install a skill from a user-supplied source. Returns the installed skill's name.
pub async fn install_from_source(skills_dir: &Path, source: &str) -> Result<String, String> {
    let mut gh = parse_github_source(source)?;
    let client = http_client()?;
    // Pin the whole install to one commit: tree scan and raw downloads must see the same
    // snapshot (see [resolve_commit_sha]).
    gh.branch = resolve_commit_sha(&client, &gh).await?;
    let tree = fetch_tree(&client, &gh).await?;
    let plan = plan_from_tree(&tree, &gh)?;
    // Refuse early (before any downloads) if the name is taken.
    if skills_dir.join(&plan.name).exists() {
        return Err(format!("skill '{}' already exists", plan.name));
    }
    let mut files = Vec::with_capacity(plan.files.len());
    for f in &plan.files {
        let bytes = fetch_raw(&client, &raw_url(&gh, &f.repo_path), MAX_FILE_BYTES).await?;
        files.push((f.rel_path.clone(), bytes));
    }
    write_skill_files(skills_dir, &plan.name, &files).map_err(|e| e.to_string())?;
    Ok(plan.name)
}

/// In-process catalog cache: (fetched-at, entries).
static CATALOG_CACHE: std::sync::Mutex<Option<(Instant, Vec<CatalogEntry>)>> = std::sync::Mutex::new(None);

/// The curated skill catalog: every directory of `anthropics/skills` that contains a SKILL.md,
/// with the description pulled from each SKILL.md's frontmatter. Cached for [CATALOG_TTL].
pub async fn fetch_catalog() -> Result<Vec<CatalogEntry>, String> {
    if let Some((at, entries)) = CATALOG_CACHE.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        if at.elapsed() < CATALOG_TTL {
            return Ok(entries);
        }
    }
    let gh = GithubRef {
        owner: CATALOG_OWNER.into(),
        repo: CATALOG_REPO.into(),
        branch: "HEAD".into(),
        path: String::new(),
    };
    let client = http_client()?;
    let tree = fetch_tree(&client, &gh).await?;
    let entries = tree.get("tree").and_then(|t| t.as_array()).ok_or("unexpected GitHub tree response")?;
    // Every directory with a SKILL.md is a skill.
    let mut dirs: Vec<String> = entries
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("blob"))
        .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
        .filter_map(|p| p.strip_suffix("/SKILL.md").map(str::to_string))
        .collect();
    dirs.sort();
    // Pull each SKILL.md's description (bounded concurrency).
    let mut out = Vec::with_capacity(dirs.len());
    for chunk in dirs.chunks(8) {
        let fetches = chunk.iter().map(|dir| {
            let client = &client;
            let gh = &gh;
            async move {
                let text = fetch_raw(client, &raw_url(gh, &format!("{dir}/SKILL.md")), 64 * 1024)
                    .await
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                let name = dir.rsplit('/').next().unwrap_or(dir).to_string();
                CatalogEntry {
                    name,
                    description: frontmatter_description(&text),
                    source: format!("{CATALOG_OWNER}/{CATALOG_REPO}/{dir}"),
                }
            }
        });
        out.extend(futures_util::future::join_all(fetches).await);
    }
    *CATALOG_CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), out.clone()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-si-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn parses_shorthand_and_urls() {
        assert_eq!(
            parse_github_source("anthropics/skills/document-skills/xlsx").unwrap(),
            GithubRef { owner: "anthropics".into(), repo: "skills".into(), branch: "HEAD".into(), path: "document-skills/xlsx".into() },
        );
        assert_eq!(
            parse_github_source("https://github.com/anthropics/skills/tree/main/artifacts-builder").unwrap(),
            GithubRef { owner: "anthropics".into(), repo: "skills".into(), branch: "main".into(), path: "artifacts-builder".into() },
        );
        assert_eq!(
            parse_github_source("https://github.com/owner/repo").unwrap(),
            GithubRef { owner: "owner".into(), repo: "repo".into(), branch: "HEAD".into(), path: "".into() },
        );
        assert!(parse_github_source("").is_err());
        assert!(parse_github_source("https://gitlab.com/x/y").is_err());
        assert!(parse_github_source("owner").is_err());
        assert!(parse_github_source("owner/repo/../etc").is_err());
        assert!(parse_github_source("bad owner/repo").is_err());
    }

    fn tree_json(entries: &[(&str, &str, u64, &str)]) -> serde_json::Value {
        serde_json::json!({
            "tree": entries.iter().map(|(path, typ, size, mode)| serde_json::json!({
                "path": path, "type": typ, "size": size, "mode": mode,
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn plan_selects_files_under_path_and_requires_skill_md() {
        let gh = GithubRef { owner: "o".into(), repo: "r".into(), branch: "HEAD".into(), path: "skills/my-skill".into() };
        let tree = tree_json(&[
            ("skills/my-skill", "tree", 0, "040000"),
            ("skills/my-skill/SKILL.md", "blob", 100, "100644"),
            ("skills/my-skill/refs/extra.md", "blob", 200, "100644"),
            ("skills/other/SKILL.md", "blob", 100, "100644"),
            ("README.md", "blob", 50, "100644"),
        ]);
        let plan = plan_from_tree(&tree, &gh).unwrap();
        assert_eq!(plan.name, "my-skill");
        assert_eq!(plan.files.len(), 2);
        assert!(plan.files.iter().any(|f| f.rel_path == "SKILL.md"));
        assert!(plan.files.iter().any(|f| f.rel_path == "refs/extra.md" && f.repo_path == "skills/my-skill/refs/extra.md"));

        // Missing SKILL.md → error.
        let gh2 = GithubRef { path: "skills/other/refs".into(), ..gh.clone() };
        assert!(plan_from_tree(&tree, &gh2).is_err());
    }

    #[test]
    fn plan_rejects_symlinks_and_oversize() {
        let gh = GithubRef { owner: "o".into(), repo: "r".into(), branch: "HEAD".into(), path: "s".into() };
        let link = tree_json(&[
            ("s/SKILL.md", "blob", 10, "100644"),
            ("s/evil", "blob", 10, "120000"),
        ]);
        assert!(plan_from_tree(&link, &gh).unwrap_err().contains("symlink"));

        let big = tree_json(&[("s/SKILL.md", "blob", MAX_FILE_BYTES + 1, "100644")]);
        assert!(plan_from_tree(&big, &gh).unwrap_err().contains("per-file limit"));
    }

    #[test]
    fn write_skill_files_roundtrip_and_guards() {
        let dir = tmp();
        let files = vec![
            ("SKILL.md".to_string(), b"---\nname: x\n---\nbody".to_vec()),
            ("refs/a.md".to_string(), b"ref".to_vec()),
        ];
        write_skill_files(&dir, "x", &files).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("x/refs/a.md")).unwrap(), "ref");
        // Existing skill → AlreadyExists; nothing overwritten.
        let err = write_skill_files(&dir, "x", &files).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // Traversal in the name and in a rel path → refused.
        assert!(write_skill_files(&dir, "..", &files).is_err());
        let bad = vec![("../escape.md".to_string(), b"x".to_vec())];
        let err = write_skill_files(&dir, "y", &bad).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!dir.join("y").exists(), "failed install must not leave a skill dir");
        assert!(!dir.parent().unwrap().join("escape.md").exists());
    }

    #[test]
    fn frontmatter_description_parses() {
        assert_eq!(
            frontmatter_description("---\nname: x\ndescription: does things\n---\nbody"),
            "does things",
        );
        assert_eq!(frontmatter_description("no frontmatter"), "");
    }
}
