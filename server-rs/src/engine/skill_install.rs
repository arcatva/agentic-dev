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

/// The default store source, seeded when no sources file exists yet.
pub const DEFAULT_SOURCE: &str = "anthropics/skills";
/// Store sources live in `<config_base>/skill-sources.json` as `{"sources": ["owner/repo", …]}`.
const SOURCES_FILE: &str = "skill-sources.json";

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
    /// The store source (as configured) this entry came from — for display/grouping.
    #[serde(rename = "sourceRepo")]
    pub source_repo: String,
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
    /// Tree mode 100755 — helper scripts must stay runnable after install.
    pub executable: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InstallPlan {
    pub name: String,
    pub files: Vec<PlannedFile>,
}

fn valid_gh_segment(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Parse a user-supplied skill source into candidate [GithubRef]s. Accepted forms:
/// - `https://github.com/{owner}/{repo}` (repo root, default branch)
/// - `https://github.com/{owner}/{repo}/tree/{branch}/{path...}`
/// - shorthand `{owner}/{repo}[/{path...}]` (default branch)
/// URL forms return MULTIPLE candidates when the branch/path split is ambiguous (branch
/// names may contain '/'); other forms return exactly one.
pub fn parse_github_source(src: &str) -> Result<Vec<GithubRef>, String> {
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
    let make = |branch: String, path_parts: &[&str]| -> Result<GithubRef, String> {
        let path = path_parts.join("/");
        if !path.is_empty() && !safe_rel_path(path.trim_matches('/')) {
            return Err("path contains unsupported characters".into());
        }
        Ok(GithubRef { owner: owner.clone(), repo: repo.clone(), branch, path: path.trim_matches('/').to_string() })
    };
    match parts.get(2) {
        None => Ok(vec![make("HEAD".into(), &[])?]),
        // URL form: /tree/{branch}/{path...} (also tolerate /blob/… pointing at a dir).
        // A branch NAME may itself contain '/' (e.g. feature/foo) and the URL gives no
        // delimiter — return every split candidate, longest-branch first; the caller resolves
        // each against GitHub until one exists. Longest-first so `feature/foo` wins over a
        // coincidental `feature` branch with a `foo/...` path.
        Some(&"tree") | Some(&"blob") if from_url => {
            let segs: &[&str] = parts.get(3..).unwrap_or(&[]);
            if segs.is_empty() || !segs.iter().all(|s| valid_gh_segment(s)) {
                return Err("URL is missing the branch after /tree/".into());
            }
            let mut out = Vec::with_capacity(segs.len());
            for split in (1..=segs.len()).rev() {
                out.push(make(segs[..split].join("/"), &segs[split..])?);
            }
            Ok(out)
        }
        // Shorthand: everything after owner/repo is the path, default branch.
        Some(_) if !from_url => Ok(vec![make("HEAD".into(), &parts[2..])?]),
        Some(_) => Err("unsupported github.com URL — use the /tree/<branch>/<path> form".into()),
    }
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
        files.push(PlannedFile {
            repo_path: p.to_string(),
            rel_path: rel.to_string(),
            executable: e.get("mode").and_then(|m| m.as_str()) == Some("100755"),
        });
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

/// Ensure a downloaded SKILL.md's frontmatter `name:` matches the DIRECTORY name it installs
/// under. `list_skills` uses the frontmatter value as the component id while delete/toggle
/// resolve `<skills_dir>/<name>` — a mismatching (or invalid) frontmatter name would create a
/// component the API can list but never manage. A missing name line is fine (listing falls
/// back to the directory name); a differing one is REWRITTEN to the directory name.
fn normalize_skill_md(bytes: &[u8], dir_name: &str) -> Vec<u8> {
    let Ok(text) = std::str::from_utf8(bytes) else { return bytes.to_vec() };
    let Some(fm) = text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")) else {
        return bytes.to_vec();
    };
    let fm_len = fm.0.len();
    let mut changed = false;
    let mut out_fm = String::with_capacity(fm_len);
    let mut name_seen = false;
    for line in fm.0.lines() {
        if let Some(v) = line.strip_prefix("name:") {
            // Mirror list_skills: only the FIRST name line counts.
            if !name_seen {
                name_seen = true;
                if v.trim() != dir_name {
                    out_fm.push_str(&format!("name: {dir_name}"));
                    out_fm.push('\n');
                    changed = true;
                    continue;
                }
            }
        }
        out_fm.push_str(line);
        out_fm.push('\n');
    }
    if !changed {
        return bytes.to_vec();
    }
    let body = &text[4 + fm_len..]; // everything from "\n---" onwards
    format!("---\n{out_fm}{}", body.strip_prefix('\n').unwrap_or(body)).into_bytes()
}

/// Write downloaded skill files under `<skills_dir>/<name>/…`, atomically: everything goes
/// into a temp sibling dir first, then one rename publishes the skill. Refuses if the skill
/// already exists — unless `replace` (update): then the old dir is swapped out and restored
/// on failure. Pure filesystem — no network.
/// `files` = (skill-relative path, bytes, executable).
pub fn write_skill_files(skills_dir: &Path, name: &str, files: &[(String, Vec<u8>, bool)], replace: bool) -> io::Result<()> {
    if !valid_name(name) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("invalid skill name '{name}'")));
    }
    // Serialize the publish phase across concurrent installs/updates: without this, an update
    // racing another update (or a delete) of the SAME name could park the other call's freshly
    // published dir. Installs are rare and fast — one process-wide lock is fine.
    static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let target = skills_dir.join(name);
    if target.exists() && !replace {
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
        for (rel, bytes, executable) in files {
            if !safe_rel_path(rel) {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!("unsafe path '{rel}'")));
            }
            let dest = tmp.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // The SKILL.md frontmatter name must match the install directory or the skill
            // becomes unmanageable through the API (listed by frontmatter name, deleted by dir).
            if rel == "SKILL.md" {
                std::fs::write(&dest, normalize_skill_md(bytes, name))?;
            } else {
                std::fs::write(&dest, bytes)?;
            }
            // Preserve helper-script executability (tree mode 100755) — a skill's scripts
            // would otherwise fail with permission errors when the agent runs them.
            #[cfg(unix)]
            if *executable {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
            }
            #[cfg(not(unix))]
            let _ = executable;
        }
        if replace && target.exists() {
            // Update: park the old version, publish the new one, then drop the old. If the
            // publish rename fails the old version is restored — never left half-updated.
            let old = skills_dir.join(format!(".old-{}-{}-{}", name, std::process::id(), nonce));
            std::fs::rename(&target, &old)?;
            match std::fs::rename(&tmp, &target) {
                Ok(()) => {
                    let _ = std::fs::remove_dir_all(&old);
                    Ok(())
                }
                Err(e) => {
                    let _ = std::fs::rename(&old, &target); // restore
                    Err(e)
                }
            }
        } else {
            std::fs::rename(&tmp, &target)
        }
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp); // best-effort cleanup; the skill dir never appeared
    }
    result
}

/// Parse `description:` out of a SKILL.md frontmatter (same forgiving rules as
/// [crate::engine::skills::list_skills]: first match wins, missing → empty). Surrounding
/// quotes are stripped — YAML-quoted descriptions (e.g. every openclaw skill) would
/// otherwise display with literal quote marks in the store.
fn frontmatter_description(text: &str) -> String {
    let Some(fm) = text.strip_prefix("---\n").and_then(|rest| rest.split_once("\n---")) else {
        return String::new();
    };
    for line in fm.0.lines() {
        if let Some(v) = line.strip_prefix("description:") {
            let v = v.trim();
            let unquoted = v
                .strip_prefix('"').and_then(|s| s.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(v);
            return unquoted.to_string();
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

/// Install a skill from a user-supplied source. `update` replaces an existing install
/// (atomically, restoring the old version if the swap fails). Returns the skill's name.
pub async fn install_from_source(skills_dir: &Path, source: &str, update: bool) -> Result<String, String> {
    let candidates = parse_github_source(source)?;
    let client = http_client()?;
    // Resolve the first branch/path candidate whose ref actually exists (URL branch names may
    // contain '/'), then pin the whole install to that resolved commit: tree scan and raw
    // downloads must see the same snapshot (see [resolve_commit_sha]).
    let mut gh = None;
    let mut last_err = String::new();
    for mut cand in candidates {
        match resolve_commit_sha(&client, &cand).await {
            Ok(sha) => {
                cand.branch = sha;
                gh = Some(cand);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some(gh) = gh else { return Err(last_err) };
    let tree = fetch_tree(&client, &gh).await?;
    let plan = plan_from_tree(&tree, &gh)?;
    // Refuse early (before any downloads) if the name is taken and this isn't an update.
    if !update && skills_dir.join(&plan.name).exists() {
        return Err(format!("skill '{}' already exists", plan.name));
    }
    let mut files = Vec::with_capacity(plan.files.len());
    for f in &plan.files {
        let bytes = fetch_raw(&client, &raw_url(&gh, &f.repo_path), MAX_FILE_BYTES).await?;
        files.push((f.rel_path.clone(), bytes, f.executable));
    }
    write_skill_files(skills_dir, &plan.name, &files, update).map_err(|e| e.to_string())?;
    Ok(plan.name)
}

// ── Store sources (skill-sources.json) ──────────────────────────────────────

/// Serializes source-file writes within this process (same trade-off as global_settings).
static SOURCES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Read the configured store sources (best-effort READ path). Missing/corrupt file → the
/// seeded default; writes go through [read_sources_for_write], which refuses to clobber a
/// corrupt file (mirrors settings.local.json handling).
pub fn read_sources(config_base: &Path) -> Vec<String> {
    match read_sources_for_write(config_base) {
        Ok(Some(sources)) => sources,
        _ => vec![DEFAULT_SOURCE.to_string()],
    }
}

/// Read for a WRITE: `Ok(None)` = missing (treat as the seeded default), `Err` = corrupt —
/// a mutation must NOT proceed, or the user's stored list would be silently replaced.
fn read_sources_for_write(config_base: &Path) -> io::Result<Option<Vec<String>>> {
    let path = config_base.join(SOURCES_FILE);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v.get("sources").and_then(|s| s.as_array()).map(|a| {
                a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect::<Vec<_>>()
            }))
            .map(Some)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "skill-sources.json is corrupt")),
    }
}

fn write_sources(config_base: &Path, sources: &[String]) -> io::Result<()> {
    let content = serde_json::to_string_pretty(&serde_json::json!({ "sources": sources }))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::create_dir_all(config_base)?;
    crate::engine::atomic_write::write_file_atomic(&config_base.join(SOURCES_FILE), &content)
}

/// Canonical identity of a source: the FIRST parse candidate. Dedupes spelling variants
/// ("owner/repo", "owner/repo/", "https://github.com/owner/repo") that scan the same repo.
fn source_key(src: &str) -> Option<GithubRef> {
    parse_github_source(src).ok()?.into_iter().next()
}

/// Drop a source's cached scan so mutations take effect immediately (and removed sources
/// don't linger in memory).
fn invalidate_source_cache(source: &str) {
    if let Some(m) = CATALOG_CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(source);
    }
}

/// Add a store source (validated by [parse_github_source]; deduplicated by canonical
/// identity, not raw spelling). Returns the new list.
pub fn add_source(config_base: &Path, source: &str) -> Result<Vec<String>, String> {
    let source = source.trim().trim_end_matches('/');
    let key = source_key(source).ok_or_else(|| "invalid source".to_string())?;
    let _guard = SOURCES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sources = read_sources_for_write(config_base)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| vec![DEFAULT_SOURCE.to_string()]);
    if !sources.iter().any(|s| source_key(s).as_ref() == Some(&key)) {
        sources.push(source.to_string());
        write_sources(config_base, &sources).map_err(|e| e.to_string())?;
        invalidate_source_cache(source);
    }
    Ok(sources)
}

/// Remove a store source. Returns (new list, found). Removing the last source is allowed —
/// the store is simply empty then (the default is only seeded while NO file exists).
pub fn remove_source(config_base: &Path, source: &str) -> Result<(Vec<String>, bool), String> {
    let source = source.trim().trim_end_matches('/');
    let _guard = SOURCES_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut sources = read_sources_for_write(config_base)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| vec![DEFAULT_SOURCE.to_string()]);
    let before = sources.len();
    sources.retain(|s| s != source);
    let found = sources.len() != before;
    if found {
        write_sources(config_base, &sources).map_err(|e| e.to_string())?;
        invalidate_source_cache(source);
    }
    Ok((sources, found))
}

// ── Aggregated catalog ──────────────────────────────────────────────────────

/// Per-source catalog cache: source string → (fetched-at, entries).
static CATALOG_CACHE: std::sync::Mutex<Option<std::collections::HashMap<String, (Instant, Vec<CatalogEntry>)>>> =
    std::sync::Mutex::new(None);

/// The aggregated skill catalog across every configured store source. One broken/unreachable
/// source degrades to an entry in `errors` instead of failing the whole store. Per-source
/// results are cached for [CATALOG_TTL]; `refresh` bypasses the cache.
pub async fn fetch_catalog(config_base: &Path, refresh: bool) -> (Vec<CatalogEntry>, Vec<String>) {
    let sources = read_sources(config_base);
    let mut entries: Vec<CatalogEntry> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return (entries, vec![e]),
    };
    for src in sources {
        if !refresh {
            let cache = CATALOG_CACHE.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, cached)) = cache.as_ref().and_then(|m| m.get(&src)) {
                if at.elapsed() < CATALOG_TTL {
                    entries.extend(cached.iter().cloned());
                    continue;
                }
            }
        }
        match scan_source(&client, &src).await {
            Ok(found) => {
                CATALOG_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert_with(Default::default)
                    .insert(src.clone(), (Instant::now(), found.clone()));
                entries.extend(found);
            }
            Err(e) => errors.push(format!("{src}: {e}")),
        }
    }
    entries.sort_by(|a, b| (a.name.as_str(), a.source_repo.as_str()).cmp(&(b.name.as_str(), b.source_repo.as_str())));
    (entries, errors)
}

/// Scan ONE store source: every directory under it containing a SKILL.md (or the source path
/// itself, when it points directly at a single skill), descriptions from each frontmatter.
async fn scan_source(client: &reqwest::Client, src: &str) -> Result<Vec<CatalogEntry>, String> {
    // Resolve the first branch/path candidate that exists, then pin to its commit so the
    // listing and the description fetches read one snapshot.
    let mut gh = None;
    let mut last_err = String::from("unresolvable source");
    for mut cand in parse_github_source(src)? {
        match resolve_commit_sha(client, &cand).await {
            Ok(sha) => {
                cand.branch = sha;
                gh = Some(cand);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let Some(gh) = gh else { return Err(last_err) };
    let tree = fetch_tree(client, &gh).await?;
    let tree_entries = tree.get("tree").and_then(|t| t.as_array()).ok_or("unexpected GitHub tree response")?;
    let prefix = if gh.path.is_empty() { String::new() } else { format!("{}/", gh.path) };
    let mut dirs: Vec<String> = tree_entries
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("blob"))
        .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
        .filter_map(|p| {
            let rel = p.strip_prefix(&prefix)?;
            // SKILL.md at the source path itself → the source IS one skill. This also covers
            // a repo whose ROOT is the skill (empty prefix, rel == "SKILL.md" → dir == "").
            if rel == "SKILL.md" {
                return Some(gh.path.clone());
            }
            rel.strip_suffix("/SKILL.md").map(|d| format!("{prefix}{d}"))
        })
        .collect();
    dirs.sort();
    dirs.dedup();
    // Pull each SKILL.md's description (bounded concurrency).
    let mut out = Vec::with_capacity(dirs.len());
    for chunk in dirs.chunks(8) {
        let fetches = chunk.iter().map(|dir| {
            let client = &client;
            let gh = &gh;
            let src = src;
            async move {
                let md_path = if dir.is_empty() { "SKILL.md".to_string() } else { format!("{dir}/SKILL.md") };
                let text = fetch_raw(client, &raw_url(gh, &md_path), 64 * 1024)
                    .await
                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                    .unwrap_or_default();
                let name = if dir.is_empty() {
                    gh.repo.clone()
                } else {
                    dir.rsplit('/').next().unwrap_or(dir).to_string()
                };
                CatalogEntry {
                    name,
                    description: frontmatter_description(&text),
                    source: if dir.is_empty() {
                        format!("{}/{}", gh.owner, gh.repo)
                    } else {
                        format!("{}/{}/{}", gh.owner, gh.repo, dir)
                    },
                    source_repo: src.to_string(),
                }
            }
        });
        out.extend(futures_util::future::join_all(fetches).await);
    }
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
            vec![GithubRef { owner: "anthropics".into(), repo: "skills".into(), branch: "HEAD".into(), path: "document-skills/xlsx".into() }],
        );
        // URL: ambiguous branch/path splits, longest branch candidate first.
        assert_eq!(
            parse_github_source("https://github.com/anthropics/skills/tree/main/artifacts-builder").unwrap(),
            vec![
                GithubRef { owner: "anthropics".into(), repo: "skills".into(), branch: "main/artifacts-builder".into(), path: "".into() },
                GithubRef { owner: "anthropics".into(), repo: "skills".into(), branch: "main".into(), path: "artifacts-builder".into() },
            ],
        );
        // Slashed branch (feature/foo): the right split is among the candidates.
        let cands = parse_github_source("https://github.com/o/r/tree/feature/foo/my-skill").unwrap();
        assert!(cands.contains(&GithubRef { owner: "o".into(), repo: "r".into(), branch: "feature/foo".into(), path: "my-skill".into() }));
        assert!(cands.contains(&GithubRef { owner: "o".into(), repo: "r".into(), branch: "feature".into(), path: "foo/my-skill".into() }));
        assert_eq!(
            parse_github_source("https://github.com/owner/repo").unwrap(),
            vec![GithubRef { owner: "owner".into(), repo: "repo".into(), branch: "HEAD".into(), path: "".into() }],
        );
        assert!(parse_github_source("").is_err());
        assert!(parse_github_source("https://gitlab.com/x/y").is_err());
        assert!(parse_github_source("owner").is_err());
        assert!(parse_github_source("owner/repo/../etc").is_err());
        assert!(parse_github_source("bad owner/repo").is_err());
        assert!(parse_github_source("owner/repo/pa?th").is_err());
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
            ("SKILL.md".to_string(), b"---\nname: x\n---\nbody".to_vec(), false),
            ("refs/a.md".to_string(), b"ref".to_vec(), false),
        ];
        write_skill_files(&dir, "x", &files, false).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("x/refs/a.md")).unwrap(), "ref");
        // Existing skill → AlreadyExists; nothing overwritten.
        let err = write_skill_files(&dir, "x", &files, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        // Traversal in the name and in a rel path → refused.
        assert!(write_skill_files(&dir, "..", &files, false).is_err());
        let bad = vec![("../escape.md".to_string(), b"x".to_vec(), false)];
        let err = write_skill_files(&dir, "y", &bad, false).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(!dir.join("y").exists(), "failed install must not leave a skill dir");
        assert!(!dir.parent().unwrap().join("escape.md").exists());
    }

    #[test]
    fn write_rewrites_mismatching_frontmatter_name_and_sets_exec_bit() {
        let dir = tmp();
        let files = vec![
            // Frontmatter name differs from the install dir — must be rewritten, or the
            // component lists under a name the delete/toggle routes can't resolve.
            ("SKILL.md".to_string(), b"---\nname: other-name\ndescription: d\n---\nbody\n".to_vec(), false),
            ("scripts/run.sh".to_string(), b"#!/bin/sh\n".to_vec(), true),
        ];
        write_skill_files(&dir, "my-skill", &files, false).unwrap();
        let md = std::fs::read_to_string(dir.join("my-skill/SKILL.md")).unwrap();
        assert!(md.contains("name: my-skill"), "frontmatter name must be rewritten: {md}");
        assert!(md.contains("description: d"), "other frontmatter lines must survive");
        assert!(md.contains("body"), "body must survive");
        // list_skills resolves the component under the DIRECTORY name.
        let listed = crate::engine::skills::list_skills(&dir);
        assert!(listed.iter().any(|s| s.name == "my-skill"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("my-skill/scripts/run.sh")).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "helper script must stay executable");
        }
    }

    #[test]
    fn replace_swaps_existing_skill_and_missing_target_still_works() {
        let dir = tmp();
        let v1 = vec![("SKILL.md".to_string(), b"---\nname: x\n---\nv1".to_vec(), false)];
        write_skill_files(&dir, "x", &v1, false).unwrap();
        // Update: replace=true swaps in the new version.
        let v2 = vec![
            ("SKILL.md".to_string(), b"---\nname: x\n---\nv2".to_vec(), false),
            ("refs/new.md".to_string(), b"n".to_vec(), false),
        ];
        write_skill_files(&dir, "x", &v2, true).unwrap();
        assert!(std::fs::read_to_string(dir.join("x/SKILL.md")).unwrap().contains("v2"));
        assert!(dir.join("x/refs/new.md").exists());
        // No leftover parked .old-* dirs.
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".old-")));
        // replace=true with no existing target behaves like a fresh install.
        write_skill_files(&dir, "fresh", &v1, true).unwrap();
        assert!(dir.join("fresh/SKILL.md").exists());
    }

    #[test]
    fn sources_crud_roundtrip() {
        let dir = tmp();
        // No file → seeded default.
        assert_eq!(read_sources(&dir), vec![DEFAULT_SOURCE.to_string()]);
        // Add: validates syntax, dedupes, persists.
        let s = add_source(&dir, "owner/repo/skills").unwrap();
        assert_eq!(s, vec![DEFAULT_SOURCE.to_string(), "owner/repo/skills".to_string()]);
        assert_eq!(add_source(&dir, "owner/repo/skills").unwrap().len(), 2, "dedupe");
        assert!(add_source(&dir, "not a source").is_err());
        assert!(add_source(&dir, "owner/repo/pa?th").is_err());
        // Remove: found flag; unknown → false.
        let (s, found) = remove_source(&dir, "owner/repo/skills").unwrap();
        assert!(found);
        assert_eq!(s, vec![DEFAULT_SOURCE.to_string()]);
        assert!(!remove_source(&dir, "never-added/repo").unwrap().1);
        // Removing the default works too — an empty store is allowed once a file exists.
        let (s, found) = remove_source(&dir, DEFAULT_SOURCE).unwrap();
        assert!(found);
        assert!(s.is_empty());
        assert_eq!(read_sources(&dir), Vec::<String>::new());
    }

    #[test]
    fn normalize_skill_md_keeps_matching_or_missing_names_verbatim() {
        let matching = b"---\nname: x\n---\nbody".to_vec();
        assert_eq!(normalize_skill_md(&matching, "x"), matching);
        let no_name = b"---\ndescription: d\n---\nbody".to_vec();
        assert_eq!(normalize_skill_md(&no_name, "x"), no_name);
        let no_fm = b"just markdown".to_vec();
        assert_eq!(normalize_skill_md(&no_fm, "x"), no_fm);
    }

    #[test]
    fn frontmatter_description_parses() {
        assert_eq!(
            frontmatter_description("---\nname: x\ndescription: does things\n---\nbody"),
            "does things",
        );
        // YAML-quoted values (openclaw-style) display without the quote marks.
        assert_eq!(
            frontmatter_description("---\ndescription: \"Current weather, with quotes\"\n---\n"),
            "Current weather, with quotes",
        );
        assert_eq!(frontmatter_description("---\ndescription: 'single'\n---\n"), "single");
        assert_eq!(frontmatter_description("no frontmatter"), "");
    }
}
