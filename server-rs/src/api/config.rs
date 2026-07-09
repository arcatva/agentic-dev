use std::path::PathBuf;

/// Parse a boolean-ish env value. Accepts 1/true/yes/on (case-insensitive) as true; everything
/// else (including empty) is false. Kept permissive so `AGENTIC_TLS_REGEN=true` and `=1` both work.
fn is_truthy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Parse a boolean-ish "disable" value: 0/false/no/off (case-insensitive) → true (falsy). Empty or
/// anything else → false (i.e. not disabled). Used for `AGENTIC_TLS`, which defaults to ON.
fn is_falsy(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off")
}

/// Split a comma-separated env value into trimmed, non-empty items.
fn split_csv(s: &str) -> Vec<String> {
    s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

#[derive(Clone, Debug)]
pub struct Config {
    pub src_root: PathBuf,
    pub worktrees_root: PathBuf,
    pub log_dir: PathBuf,
    pub db_path: PathBuf,
    pub retitle_enabled: bool, // AGENTIC_RETITLE — only the literal "off" disables (case-insensitive); default true
    pub max_concurrent: Option<u64>, // None = unlimited
    pub git_org: String,
    pub claude_config_base: PathBuf,
    pub port: u16,
    pub host: String,
    pub password: String,
    pub auth_secret: String,
    // TLS / HTTPS. HTTPS is ON by default — the server generates + persists a self-signed cert on
    // first boot and serves it. Clients trust it by pinning (trust-on-first-use).
    //  - Disable entirely (plain HTTP): AGENTIC_TLS=off.
    //  - Bring your own cert: set BOTH tls_cert + tls_key to PEM paths (overrides the self-signed one).
    //  - Otherwise a self-signed cert+key is (re)generated under tls_dir, listing the host's IPs
    //    (plus tls_extra_sans) as SANs so it also validates for browsers/curl connecting by IP.
    pub tls_enabled: bool,           // AGENTIC_TLS       — "off"/"0"/"false"/"no" → plain HTTP; else HTTPS (default)
    pub tls_cert: Option<PathBuf>,   // AGENTIC_TLS_CERT  — PEM cert chain (BYO; needs TLS_KEY)
    pub tls_key: Option<PathBuf>,    // AGENTIC_TLS_KEY   — PEM private key (BYO)
    pub tls_dir: PathBuf,            // AGENTIC_TLS_DIR   — where the self-signed cert+key live (default <data>/tls)
    pub tls_extra_sans: Vec<String>, // AGENTIC_TLS_SAN   — extra SANs (IP or DNS), comma-separated
    pub tls_regen: bool,             // AGENTIC_TLS_REGEN — force-regenerate the self-signed cert on boot
    pub transcript_cache_bytes: usize,
    // Phase 6 additions
    pub skills_dir: PathBuf,
    pub groups_path: PathBuf,
    pub templates_path: PathBuf,
    pub device_token_path: PathBuf,
    pub upload_max_bytes: usize,
    // Watchdog timers + cgroup caps. All opt-in;
    // None when the env var is unset/empty → engine falls back to its built-in defaults.
    pub idle_max_ms: Option<i64>,    // AGENTIC_TURN_IDLE_SEC  (seconds → ms)
    pub wall_max_ms: Option<i64>,    // AGENTIC_TURN_WALL_SEC  (seconds → ms)
    pub idle_ttl_ms: Option<i64>,    // AGENTIC_IDLE_TTL_SEC   (seconds → ms)
    pub memory_max: Option<String>,  // AGENTIC_MEM_MAX   (e.g. "2G")
    pub memory_high: Option<String>, // AGENTIC_MEM_HIGH  (e.g. "1536M")
    pub cpu_quota: Option<String>,   // AGENTIC_CPU_QUOTA (e.g. "200%")
    pub tasks_max: Option<String>,   // AGENTIC_TASKS_MAX (e.g. "512")
}

impl Config {
    pub fn load(get: impl Fn(&str) -> Option<String>) -> Config {
        let home = get("HOME").unwrap_or_else(|| "/root".into());
        let join = |base: &str, sub: &str| PathBuf::from(base).join(sub);
        let src = get("AGENTIC_SRC_ROOT").unwrap_or_else(|| join(&home, "src").to_string_lossy().into_owned());
        let data_dir = get("AGENTIC_DATA_DIR").unwrap_or_else(|| join(&home, ".agentic-dev").to_string_lossy().into_owned());
        Config {
            src_root: PathBuf::from(&src),
            worktrees_root: get("AGENTIC_WORKTREES_ROOT").map(PathBuf::from).unwrap_or_else(|| join(&src, "agentic-worktrees")),
            log_dir: join(&data_dir, "logs"),
            db_path: join(&data_dir, "db.sqlite"),
            retitle_enabled: !matches!(get("AGENTIC_RETITLE").as_deref(), Some(s) if s.eq_ignore_ascii_case("off")),
            max_concurrent: get("AGENTIC_MAX_CONCURRENT").and_then(|v| v.parse::<u64>().ok()),
            git_org: get("AGENTIC_GIT_ORG").unwrap_or_else(|| "arcatva".into()),
            claude_config_base: get("AGENTIC_CLAUDE_CONFIG_BASE").map(PathBuf::from).unwrap_or_else(|| join(&home, ".claude")),
            port: get("AGENTIC_PORT").and_then(|v| v.parse().ok()).unwrap_or(7420),
            host: get("AGENTIC_HOST").unwrap_or_else(|| "0.0.0.0".into()),
            password: get("AGENTIC_PASSWORD").unwrap_or_else(|| "changeme".into()),
            auth_secret: get("AGENTIC_AUTH_SECRET").unwrap_or_else(|| "dev-insecure-secret".into()),
            // TLS: HTTPS on unless explicitly disabled. Empty strings treated as unset.
            tls_enabled: get("AGENTIC_TLS").map(|s| !is_falsy(&s)).unwrap_or(true),
            tls_cert: get("AGENTIC_TLS_CERT").filter(|s| !s.is_empty()).map(PathBuf::from),
            tls_key: get("AGENTIC_TLS_KEY").filter(|s| !s.is_empty()).map(PathBuf::from),
            tls_dir: get("AGENTIC_TLS_DIR").filter(|s| !s.is_empty()).map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&data_dir).join("tls")),
            tls_extra_sans: get("AGENTIC_TLS_SAN").map(|s| split_csv(&s)).unwrap_or_default(),
            tls_regen: get("AGENTIC_TLS_REGEN").map(|s| is_truthy(&s)).unwrap_or(false),
            transcript_cache_bytes: get("AGENTIC_TRANSCRIPT_CACHE_BYTES").and_then(|v| v.parse().ok()).unwrap_or(256 * 1024 * 1024),
            // Phase 6 additions
            skills_dir: get("AGENTIC_SKILLS_DIR").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&home).join(".claude").join("skills")),
            groups_path: get("AGENTIC_GROUPS_PATH").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&data_dir).join("groups.json")),
            templates_path: get("AGENTIC_TEMPLATES_PATH").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&data_dir).join("templates.json")),
            device_token_path: get("AGENTIC_DEVICE_TOKEN_PATH").map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&data_dir).join("device.json")),
            upload_max_bytes: get("AGENTIC_UPLOAD_MAX_BYTES").and_then(|v| v.parse().ok())
                .unwrap_or(64 * 1024 * 1024),
            // Watchdog timers: seconds → ms. Unset/non-numeric → None.
            idle_max_ms: get("AGENTIC_TURN_IDLE_SEC").and_then(|v| v.parse::<i64>().ok()).map(|s| s * 1000),
            wall_max_ms: get("AGENTIC_TURN_WALL_SEC").and_then(|v| v.parse::<i64>().ok()).map(|s| s * 1000),
            idle_ttl_ms: get("AGENTIC_IDLE_TTL_SEC").and_then(|v| v.parse::<i64>().ok()).map(|s| s * 1000),
            // cgroup caps: raw strings; empty → None.
            memory_max: get("AGENTIC_MEM_MAX").filter(|s| !s.is_empty()),
            memory_high: get("AGENTIC_MEM_HIGH").filter(|s| !s.is_empty()),
            cpu_quota: get("AGENTIC_CPU_QUOTA").filter(|s| !s.is_empty()),
            tasks_max: get("AGENTIC_TASKS_MAX").filter(|s| !s.is_empty()),
        }
    }

    /// Convenience constructor for tests: load with no env, then override auth_secret and password.
    pub fn for_test(secret: &str, password: &str) -> Config {
        let mut c = Config::load(|_| None);
        c.auth_secret = secret.into();
        c.password = password.into();
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let m: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| m.get(k).cloned()
    }

    #[test]
    fn defaults_are_correct() {
        let c = Config::load(env_of(&[("HOME", "/home/u")]));
        assert_eq!(c.port, 7420);
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.password, "changeme");
        assert_eq!(c.auth_secret, "dev-insecure-secret");
        assert_eq!(c.max_concurrent, None); // unlimited
        assert_eq!(c.git_org, "arcatva");
        assert_eq!(c.db_path.to_str().unwrap(), "/home/u/.agentic-dev/db.sqlite");
        assert_eq!(c.log_dir.to_str().unwrap(), "/home/u/.agentic-dev/logs");
        assert_eq!(c.src_root.to_str().unwrap(), "/home/u/src");
    }

    #[test]
    fn overrides_apply() {
        let c = Config::load(env_of(&[
            ("HOME", "/home/u"),
            ("AGENTIC_PORT", "9000"),
            ("AGENTIC_MAX_CONCURRENT", "5"),
            ("AGENTIC_AUTH_SECRET", "s3cret"),
            ("AGENTIC_DATA_DIR", "/data"),
        ]));
        assert_eq!(c.port, 9000);
        assert_eq!(c.max_concurrent, Some(5));
        assert_eq!(c.auth_secret, "s3cret");
        assert_eq!(c.db_path.to_str().unwrap(), "/data/db.sqlite");
    }

    #[test]
    fn transcript_cache_bytes_default_and_override() {
        let c = Config::load(env_of(&[("HOME", "/home/u")]));
        assert_eq!(c.transcript_cache_bytes, 256 * 1024 * 1024);
        let c2 = Config::load(env_of(&[("HOME", "/home/u"), ("AGENTIC_TRANSCRIPT_CACHE_BYTES", "1048576")]));
        assert_eq!(c2.transcript_cache_bytes, 1_048_576);
    }

    #[test]
    fn watchdog_and_cgroup_env_parse() {
        // Defaults: all None.
        let c = Config::load(env_of(&[("HOME", "/home/u")]));
        assert_eq!(c.idle_max_ms, None);
        assert_eq!(c.wall_max_ms, None);
        assert_eq!(c.idle_ttl_ms, None);
        assert_eq!(c.memory_max, None);
        assert_eq!(c.cpu_quota, None);
        // Timers convert seconds → ms; cgroup caps pass through verbatim.
        let c2 = Config::load(env_of(&[
            ("HOME", "/home/u"),
            ("AGENTIC_TURN_IDLE_SEC", "120"),
            ("AGENTIC_TURN_WALL_SEC", "7200"),
            ("AGENTIC_IDLE_TTL_SEC", "600"),
            ("AGENTIC_MEM_MAX", "2G"),
            ("AGENTIC_MEM_HIGH", "1536M"),
            ("AGENTIC_CPU_QUOTA", "200%"),
            ("AGENTIC_TASKS_MAX", "512"),
        ]));
        assert_eq!(c2.idle_max_ms, Some(120_000));
        assert_eq!(c2.wall_max_ms, Some(7_200_000));
        assert_eq!(c2.idle_ttl_ms, Some(600_000));
        assert_eq!(c2.memory_max.as_deref(), Some("2G"));
        assert_eq!(c2.memory_high.as_deref(), Some("1536M"));
        assert_eq!(c2.cpu_quota.as_deref(), Some("200%"));
        assert_eq!(c2.tasks_max.as_deref(), Some("512"));
        // Empty string → None (env var present but blank is treated as unset); non-numeric timer → None.
        let c3 = Config::load(env_of(&[
            ("HOME", "/home/u"),
            ("AGENTIC_MEM_MAX", ""),
            ("AGENTIC_TURN_IDLE_SEC", "not_a_number"),
        ]));
        assert_eq!(c3.memory_max, None);
        assert_eq!(c3.idle_max_ms, None);
    }

    #[test]
    fn phase6_path_defaults_and_overrides() {
        let c = Config::load(env_of(&[("HOME", "/home/u")]));
        assert_eq!(c.skills_dir.to_str().unwrap(), "/home/u/.claude/skills");
        assert_eq!(c.groups_path.to_str().unwrap(), "/home/u/.agentic-dev/groups.json");
        assert_eq!(c.templates_path.to_str().unwrap(), "/home/u/.agentic-dev/templates.json");
        assert_eq!(c.device_token_path.to_str().unwrap(), "/home/u/.agentic-dev/device.json");
        assert_eq!(c.upload_max_bytes, 64 * 1024 * 1024);
        let c2 = Config::load(env_of(&[("HOME","/home/u"),("AGENTIC_UPLOAD_MAX_BYTES","10")]));
        assert_eq!(c2.upload_max_bytes, 10);
    }

    #[test]
    fn tls_defaults_on_self_signed() {
        let c = Config::load(env_of(&[("HOME", "/home/u")]));
        assert!(c.tls_enabled, "HTTPS is on by default");
        assert_eq!(c.tls_cert, None); // no BYO cert → self-signed path
        assert_eq!(c.tls_key, None);
        assert!(c.tls_extra_sans.is_empty());
        assert!(!c.tls_regen);
        // Self-signed cert dir defaults under the data dir.
        assert_eq!(c.tls_dir.to_str().unwrap(), "/home/u/.agentic-dev/tls");
    }

    #[test]
    fn tls_can_be_disabled() {
        for v in ["off", "0", "false", "No", "OFF"] {
            let c = Config::load(env_of(&[("HOME", "/home/u"), ("AGENTIC_TLS", v)]));
            assert!(!c.tls_enabled, "AGENTIC_TLS={v} should disable TLS");
        }
        // Anything else (incl. empty / on / 1) keeps HTTPS on.
        for v in ["", "on", "1", "true", "yes"] {
            let c = Config::load(env_of(&[("HOME", "/home/u"), ("AGENTIC_TLS", v)]));
            assert!(c.tls_enabled, "AGENTIC_TLS={v:?} should keep TLS on");
        }
    }

    #[test]
    fn tls_byo_and_sans_env_parse() {
        let c = Config::load(env_of(&[
            ("HOME", "/home/u"),
            ("AGENTIC_TLS_CERT", "/etc/ssl/fullchain.pem"),
            ("AGENTIC_TLS_KEY", "/etc/ssl/privkey.pem"),
            ("AGENTIC_TLS_DIR", "/var/agtls"),
            ("AGENTIC_TLS_SAN", "10.0.0.5, agentic.lan ,"),
            ("AGENTIC_TLS_REGEN", "1"),
        ]));
        assert_eq!(c.tls_cert.as_ref().unwrap().to_str().unwrap(), "/etc/ssl/fullchain.pem");
        assert_eq!(c.tls_key.as_ref().unwrap().to_str().unwrap(), "/etc/ssl/privkey.pem");
        assert_eq!(c.tls_dir.to_str().unwrap(), "/var/agtls");
        assert_eq!(c.tls_extra_sans, vec!["10.0.0.5".to_string(), "agentic.lan".to_string()]);
        assert!(c.tls_regen);

        // Blank cert value is treated as unset.
        let c2 = Config::load(env_of(&[("HOME", "/home/u"), ("AGENTIC_TLS_CERT", "")]));
        assert_eq!(c2.tls_cert, None);
    }

    #[test]
    fn truthy_falsy_parse_common_forms() {
        for s in ["1", "true", "TRUE", "Yes", "on", " on "] {
            assert!(is_truthy(s), "{s:?} should be truthy");
        }
        for s in ["", "0", "false", "off", "no", "maybe"] {
            assert!(!is_truthy(s), "{s:?} should not be truthy");
        }
        for s in ["0", "false", "NO", "off", " off "] {
            assert!(is_falsy(s), "{s:?} should be falsy");
        }
        for s in ["", "1", "true", "on", "yes", "maybe"] {
            assert!(!is_falsy(s), "{s:?} should not be falsy");
        }
    }
}
