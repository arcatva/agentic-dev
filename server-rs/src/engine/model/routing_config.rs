//! Global routing config for the delegate router: a single `tradeoff` knob (0.0..1.0,
//! `0` = cheapest, `1` = strongest) that sets the cost⇄quality weighting of the joint score
//! in `engine::router::select_model`. Persisted as a tiny JSON file, mirroring the
//! providers / native-overrides CRUD pattern: atomic temp+rename write, 0600 on unix, a
//! `FILE_LOCK`, and a data-race-free test-override static (never `env::set_var`).
//!
//! Unlike the providers/native-overrides files, a missing OR unreadable file here yields the
//! DEFAULT (0.5) rather than an error — a single scalar has nothing to "wipe", and routing must
//! keep working even if the file is corrupt. `tradeoff` is clamped to `[0,1]` on load and save.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Default tradeoff when no file is present (or it is unreadable): balanced.
pub const DEFAULT_TRADEOFF: f32 = 0.5;

fn default_tradeoff() -> f32 {
    DEFAULT_TRADEOFF
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Cost⇄quality knob, 0.0 (cheapest) .. 1.0 (strongest). Defaults 0.5.
    #[serde(default = "default_tradeoff")]
    pub tradeoff: f32,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            tradeoff: DEFAULT_TRADEOFF,
        }
    }
}

impl RoutingConfig {
    /// Clamp `tradeoff` into `[0,1]` (defensive against a hand-edited file or a bad client).
    fn clamped(mut self) -> Self {
        self.tradeoff = if self.tradeoff.is_nan() {
            DEFAULT_TRADEOFF
        } else {
            self.tradeoff.clamp(0.0, 1.0)
        };
        self
    }
}

/// Serializes read-modify-write across concurrent API requests (axum multi-threaded executor).
static FILE_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Test-only override for [routing_config_file_path]. A plain static (not `env::set_var`, which
/// races concurrent getenv from other test threads — UB in glibc). Always `None` in production.
pub static ROUTING_FILE_OVERRIDE: parking_lot::Mutex<Option<PathBuf>> =
    parking_lot::Mutex::new(None);

/// The routing config file: the test override if set, else `AGENTIC_ROUTING_FILE`, else
/// `~/.agentic-dev/routing.json`.
pub fn routing_config_file_path() -> PathBuf {
    if let Some(p) = ROUTING_FILE_OVERRIDE.lock().clone() {
        return p;
    }
    if let Ok(p) = std::env::var("AGENTIC_ROUTING_FILE") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".agentic-dev")
        .join("routing.json")
}

/// Read the config from `path`. Missing OR unreadable/invalid → clamped default (never errors:
/// routing must keep working). Takes the path directly so tests need no global env var.
pub fn load_from(path: &Path) -> RoutingConfig {
    let cfg = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<RoutingConfig>(&text).ok())
        .unwrap_or_default();
    cfg.clamped()
}

/// Write the config to `path` atomically (temp + rename); mode 0600 on unix. `tradeoff` is
/// clamped before writing so a bad value never lands on disk.
pub fn save_to(path: &Path, cfg: &RoutingConfig) -> std::io::Result<()> {
    let _guard = FILE_LOCK.lock();
    let cfg = cfg.clamped();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_string_pretty(&cfg).map_err(std::io::Error::other)?;
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

// Convenience wrappers on the configured file.
pub fn load() -> RoutingConfig {
    load_from(&routing_config_file_path())
}
pub fn save(cfg: &RoutingConfig) -> std::io::Result<()> {
    save_to(&routing_config_file_path(), cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tradeoff_defaults_clamps_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("routing.json");
        // missing → default
        assert_eq!(load_from(&f).tradeoff, DEFAULT_TRADEOFF);
        // out-of-range saved value is clamped both on write and read
        save_to(&f, &RoutingConfig { tradeoff: 1.7 }).unwrap();
        assert!((load_from(&f).tradeoff - 1.0).abs() < f32::EPSILON);
        save_to(&f, &RoutingConfig { tradeoff: -0.4 }).unwrap();
        assert!((load_from(&f).tradeoff - 0.0).abs() < f32::EPSILON);
        // in-range round-trips
        save_to(&f, &RoutingConfig { tradeoff: 0.2 }).unwrap();
        assert!((load_from(&f).tradeoff - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn missing_field_and_corrupt_file_fall_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("routing.json");
        // empty object → serde default fills tradeoff
        std::fs::write(&f, "{}").unwrap();
        assert_eq!(load_from(&f).tradeoff, DEFAULT_TRADEOFF);
        // corrupt JSON → default (never panics / errors)
        std::fs::write(&f, "{ not json").unwrap();
        assert_eq!(load_from(&f).tradeoff, DEFAULT_TRADEOFF);
        // NaN on disk → default
        std::fs::write(&f, r#"{"tradeoff": null}"#).unwrap();
        assert_eq!(load_from(&f).tradeoff, DEFAULT_TRADEOFF);
    }
}
