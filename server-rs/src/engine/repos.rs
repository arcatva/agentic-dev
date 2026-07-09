use std::path::{Path, PathBuf};

/// List all direct-child directories of src_root that contain a `.git` dir, sorted alphabetically.
pub fn list_repos(src_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(src_root) else { return vec![]; };
    let mut out: Vec<String> = entries.flatten()
        .filter(|e| e.path().is_dir() && e.path().join(".git").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn default_gh_list(org: &str) -> Option<String> {
    let o = std::process::Command::new("gh")
        .args(["repo", "list", org, "--json", "name", "-L", "200"]).output().ok()?;
    if !o.status.success() { return None; }
    Some(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// Run `gh repo list <org> --json name -L 200`, parse the result and return sorted names.
/// `gh_fn` is an injectable seam (tests pass a closure); None → real `gh` subprocess.
pub fn list_remote_repos(git_org: &str, gh_fn: Option<&dyn Fn(&str) -> Option<String>>) -> Vec<String> {
    let raw = match gh_fn { Some(f) => f(git_org), None => default_gh_list(git_org) };
    let Some(raw) = raw else { return vec![]; };
    let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&raw) else { return vec![]; };
    let mut names: Vec<String> = arr.iter()
        .filter_map(|r| r.get("name").and_then(|n| n.as_str()).map(String::from)).collect();
    names.sort();
    names
}

/// Return the local path of `repo` under src_root, cloning from GitHub first if absent.
/// `clone_fn(url, dest)` is injectable (tests pass a closure that errors to simulate "no network").
/// Returns Err if the repo is absent AND the clone fails.
pub fn ensure_local(
    repo: &str,
    src_root: &Path,
    git_org: &str,
    clone_fn: &dyn Fn(&str, &str) -> std::io::Result<()>,
) -> std::io::Result<PathBuf> {
    let dest = src_root.join(repo);
    if dest.join(".git").exists() {
        return Ok(dest);
    }
    let url = format!("https://github.com/{git_org}/{repo}.git");
    clone_fn(&url, &dest.to_string_lossy())?;
    Ok(dest)
}

/// Production clone via `git clone <url> <dest>`. Used when EngineConfig.clone_fn is None.
pub fn default_clone(url: &str, dest: &str) -> std::io::Result<()> {
    let status = std::process::Command::new("git").args(["clone", url, dest]).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("git clone failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-repos-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn list_repos_returns_sorted_git_dirs_only() {
        let src = tmp();
        std::fs::create_dir_all(src.join("zeta").join(".git")).unwrap();
        std::fs::create_dir_all(src.join("alpha").join(".git")).unwrap();
        std::fs::create_dir_all(src.join("notrepo")).unwrap(); // no .git
        assert_eq!(list_repos(&src), vec!["alpha".to_string(), "zeta".to_string()]);
        assert!(list_repos(&src.join("missing")).is_empty());
    }

    #[test]
    fn list_remote_repos_parses_and_sorts_and_is_lenient() {
        let gh = |_org: &str| -> Option<String> { Some(r#"[{"name":"b"},{"name":"a"}]"#.to_string()) };
        assert_eq!(list_remote_repos("org", Some(&gh)), vec!["a".to_string(), "b".to_string()]);
        let bad = |_o: &str| -> Option<String> { None };
        assert!(list_remote_repos("org", Some(&bad)).is_empty());
    }

    #[test]
    fn returns_existing_local_repo_without_cloning() {
        let src = tmp();
        std::fs::create_dir_all(src.join("demo").join(".git")).unwrap();
        let never = |_: &str, _: &str| -> std::io::Result<()> { panic!("must not clone") };
        let p = ensure_local("demo", &src, "arcatva", &never).unwrap();
        assert_eq!(p, src.join("demo"));
    }

    #[test]
    fn clones_when_absent_using_the_org_url() {
        let src = tmp();
        let seen = std::cell::RefCell::new(String::new());
        let clone = |url: &str, dest: &str| -> std::io::Result<()> {
            *seen.borrow_mut() = url.to_string();
            std::fs::create_dir_all(std::path::Path::new(dest).join(".git")).unwrap();
            Ok(())
        };
        let p = ensure_local("newrepo", &src, "arcatva", &clone).unwrap();
        assert_eq!(p, src.join("newrepo"));
        assert_eq!(*seen.borrow(), "https://github.com/arcatva/newrepo.git");
    }

    #[test]
    fn propagates_clone_failure_for_an_unresolvable_repo() {
        let src = tmp();
        let boom = |_: &str, _: &str| -> std::io::Result<()> {
            Err(std::io::Error::other(
                "clone disabled in tests",
            ))
        };
        assert!(ensure_local("nope", &src, "arcatva", &boom).is_err());
    }
}
