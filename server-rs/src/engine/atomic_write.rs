use std::io::Write;
use std::path::Path;

/// Write a file atomically + durably: write to `<path>.tmp`, fsync it, rename over the target,
/// then best-effort fsync the parent dir so the rename survives power loss.
pub fn write_file_atomic(path: &Path, content: &str) -> std::io::Result<()> {
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
}
