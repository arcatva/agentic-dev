use std::io::Write;
use std::path::Path;

/// Write a file atomically + durably: write to `<path>.tmp`, fsync it, rename over the target,
/// then best-effort fsync the parent dir so the rename survives power loss.
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    write_file_atomic_impl(path, content, None)
}

/// Like [write_file_atomic] but also sets the unix file `mode` (e.g. `0o600`) on the temp file
/// BEFORE the rename, so the target is never briefly readable at a wider mode. `mode` is ignored
/// on non-unix. Use for files holding secrets (e.g. OAuth tokens). Same fsync/durability as
/// [write_file_atomic] — do not hand-roll a second atomic writer that drops the fsync.
pub fn write_file_atomic_mode(path: &Path, content: &str, mode: u32) -> std::io::Result<()> {
    write_file_atomic_impl(path, content, Some(mode))
}

fn write_file_atomic_impl(path: &Path, content: &str, mode: Option<u32>) -> std::io::Result<()> {
    let tmp = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?; // fsync the bytes
    }
    // Tighten permissions on the temp file before the rename so the published file is never
    // momentarily world-readable (a chmod-after-rename would leave that window open).
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(m))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path)?;
    // Best-effort directory fsync so the rename is durable. Must never turn success into an error.
    if let Some(parent) = path.parent() {
        if let Ok(d) = std::fs::File::open(parent) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-aw-{}-{}",
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
    fn writes_and_overwrites_atomically() {
        let dir = tmp();
        let p = dir.join("f.json");
        write_file_atomic(&p, "first").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "first");
        write_file_atomic(&p, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "second");
        // no leftover temp file
        assert!(!dir.join("f.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[cfg(unix)]
    fn write_file_atomic_mode_sets_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp();
        let p = dir.join("secret.json");
        write_file_atomic_mode(&p, "token", 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "token");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret file must be owner-only");
        assert!(!dir.join("secret.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
