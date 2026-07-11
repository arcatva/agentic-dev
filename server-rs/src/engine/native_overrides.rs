//! Per-family routing overrides for the native Claude models. The model LIST stays dynamic
//! (Anthropic Models API discovery); only the routing metrics per family are persisted here.
//! Mirrors the providers-file CRUD pattern: atomic write, 0600, FILE_LOCK,
//! corrupt-file-errors-rather-than-wiping, and a data-race-free test-override static.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One family's override. Full values (the edit form writes all four); "reset to default" removes
/// the whole row. An empty `description` falls back to the generated per-model description at
/// routing time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NativeOverride {
    pub capability: f32,
    pub priority: f32,
    pub cost: f32,
    #[serde(default)]
    pub description: String,
}

/// family -> override. `BTreeMap` for deterministic on-disk ordering.
pub type OverrideMap = BTreeMap<String, NativeOverride>;

/// Serializes read-modify-write across concurrent API requests (axum multi-threaded executor).
static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Test-only override for [native_overrides_file_path]. A plain static (not `env::set_var`, which
/// races concurrent getenv from other test threads — UB in glibc). Always `None` in production.
pub static NATIVE_OVERRIDES_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> =
    parking_lot::Mutex::new(None);

/// The overrides JSON file: the test override if set, else `AGENTIC_NATIVE_OVERRIDES_FILE`, else
/// `~/.agentic-dev/native-overrides.json`.
pub fn native_overrides_file_path() -> PathBuf {
    if let Some(p) = NATIVE_OVERRIDES_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_NATIVE_OVERRIDES_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".agentic-dev").join("native-overrides.json")
}

/// Read the map from `path`. `Ok(empty)` when missing; `Err` when present but unreadable or invalid
/// JSON — callers MUST NOT then overwrite it (that would wipe valid data).
pub fn load_map_from(path: &Path) -> std::io::Result<OverrideMap> {
    if !path.exists() {
        return Ok(OverrideMap::new());
    }
    let text = std::fs::read_to_string(path)?;
    serde_json::from_str(&text).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Write the map to `path` atomically (temp + rename); mode 0600 on unix.
pub fn save_map_to(path: &Path, map: &OverrideMap) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(map).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Insert or replace one family's override in the file at `path`.
pub fn upsert_at(path: &Path, family: &str, ov: NativeOverride) -> std::io::Result<()> {
    let _guard = FILE_LOCK.lock();
    let mut map = load_map_from(path)?;
    map.insert(family.to_string(), ov);
    save_map_to(path, &map)
}

/// Remove one family's override. Returns true if one was present.
pub fn remove_at(path: &Path, family: &str) -> std::io::Result<bool> {
    let _guard = FILE_LOCK.lock();
    let mut map = load_map_from(path)?;
    let removed = map.remove(family).is_some();
    if removed {
        save_map_to(path, &map)?;
    }
    Ok(removed)
}

// Convenience wrappers on the configured file.
pub fn load_map() -> OverrideMap {
    match load_map_from(&native_overrides_file_path()) {
        Ok(m) => m,
        Err(e) => {
            // Mirror ProviderRegistry::load: a corrupt/unreadable file must not be silent — the
            // router would otherwise run on defaults with no hint why the overrides "vanished".
            tracing::warn!(
                target: "native_overrides",
                "native overrides file unreadable ({e}); routing with family defaults"
            );
            OverrideMap::default()
        }
    }
}
pub fn upsert(family: &str, ov: NativeOverride) -> std::io::Result<()> {
    upsert_at(&native_overrides_file_path(), family, ov)
}
pub fn remove(family: &str) -> std::io::Result<bool> {
    remove_at(&native_overrides_file_path(), family)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(c: f32, p: f32, k: f32, d: &str) -> NativeOverride {
        NativeOverride { capability: c, priority: p, cost: k, description: d.into() }
    }

    #[test]
    fn crud_roundtrip_on_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("native-overrides.json");
        assert!(load_map_from(&f).unwrap().is_empty());
        upsert_at(&f, "opus", ov(0.9, 0.8, 0.9, "hard only")).unwrap();
        upsert_at(&f, "haiku", ov(0.6, 0.2, 0.3, "")).unwrap();
        let m = load_map_from(&f).unwrap();
        assert_eq!(m.len(), 2);
        assert!((m["opus"].priority - 0.8).abs() < f32::EPSILON);
        // replace by same family key
        upsert_at(&f, "opus", ov(0.97, 0.5, 0.9, "x")).unwrap();
        assert!((load_map_from(&f).unwrap()["opus"].priority - 0.5).abs() < f32::EPSILON);
        // remove (idempotent second call → false)
        assert!(remove_at(&f, "opus").unwrap());
        assert!(!remove_at(&f, "opus").unwrap());
        assert_eq!(load_map_from(&f).unwrap().len(), 1);
    }

    #[test]
    fn corrupt_file_errors_instead_of_wiping() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("native-overrides.json");
        upsert_at(&f, "opus", ov(0.9, 0.8, 0.9, "")).unwrap();
        std::fs::write(&f, "{ this is not json").unwrap();
        assert!(load_map_from(&f).is_err());
        assert!(remove_at(&f, "opus").is_err());
        assert!(upsert_at(&f, "sonnet", ov(0.8, 0.5, 0.5, "")).is_err());
        // the corrupt file is left intact, NOT wiped
        assert!(std::fs::read_to_string(&f).unwrap().contains("not json"));
    }
}
