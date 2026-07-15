//! Backend-managed LiteLLM proxy sidecar — makes `openai`-protocol providers usable as delegate
//! workers.
//!
//! A delegate worker is a headless `claude` (the Agent SDK); it only speaks the Anthropic
//! `/v1/messages` API to whatever `ANTHROPIC_BASE_URL` points at. OpenAI exposes a different API, so
//! for an `openai` provider we point the worker at THIS local proxy, which translates Anthropic ↔
//! OpenAI Chat Completions (verified end-to-end against a real backend).
//!
//! The proxy is spawned and SUPERVISED by the backend: started at boot when there are openai
//! providers, restarted on crash, and reloaded when the provider set changes. The real OpenAI keys
//! are passed to the proxy process via env (`os.environ/...` references in the generated config),
//! NOT written to the config file. The proxy binds to localhost only.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crate::engine::providers::{Protocol, ProviderRegistry};

/// Localhost port the proxy listens on (override with `AGENTIC_LITELLM_PORT`).
pub fn port() -> u16 {
    std::env::var("AGENTIC_LITELLM_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4111)
}

/// The base URL a worker's `ANTHROPIC_BASE_URL` points at for an openai provider.
pub fn proxy_base_url() -> String {
    format!("http://127.0.0.1:{}", port())
}

/// Dummy auth token for the worker→proxy hop. The proxy is open on localhost; the SDK only needs a
/// non-empty `ANTHROPIC_AUTH_TOKEN`.
pub const PROXY_TOKEN: &str = "sk-agentic-litellm-local";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

fn binary() -> PathBuf {
    std::env::var("AGENTIC_LITELLM_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            home()
                .join(".agentic-dev")
                .join("litellm-venv")
                .join("bin")
                .join("litellm")
        })
}

fn config_path() -> PathBuf {
    home().join(".agentic-dev").join("litellm-config.yaml")
}

/// True when the litellm binary is present, i.e. openai providers can actually run.
pub fn available() -> bool {
    binary().exists()
}

/// Double-quote + escape a value for a YAML scalar (base_url / model names can't be trusted bare).
fn yaml_q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Generate the litellm config from the openai-protocol providers. Returns the `(env_var, key)` pairs
/// to inject into the proxy process — the keys go in the ENV (referenced via `os.environ/...`), never
/// in the file. Empty result = no usable openai providers.
fn build_config() -> Vec<(String, String)> {
    let reg = ProviderRegistry::load();
    let (yaml, envs) = render_config(&reg);
    if let Some(parent) = config_path().parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::error!("[litellm] create config dir {:?} failed: {e}", parent);
        }
    }
    if let Err(e) = std::fs::write(config_path(), &yaml) {
        tracing::error!("[litellm] write config {:?} failed: {e}", config_path());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(config_path(), std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("[litellm] chmod 600 config failed: {e}");
        }
    }
    envs
}

/// Pure YAML+env renderer (no file I/O) so the config shape is unit-testable. Returns the config
/// text and the `(env_var, key)` pairs whose keys are injected into the proxy env, never the file.
fn render_config(reg: &ProviderRegistry) -> (String, Vec<(String, String)>) {
    let mut yaml = String::from("model_list:\n");
    let mut envs: Vec<(String, String)> = Vec::new();
    for p in reg.providers.iter() {
        if !matches!(p.protocol, Protocol::Openai) {
            continue;
        }
        let key = p.resolved_key();
        if key.is_empty() {
            continue;
        }
        let var = format!("AGENTIC_LITELLM_KEY_{}", envs.len());
        // model_name == the provider's model id (what the worker spawns with). litellm calls
        // `openai/<model>` at `api_base`, i.e. POST <api_base>/chat/completions.
        yaml.push_str(&format!(
            "  - model_name: {name}\n    litellm_params:\n      model: {model}\n      api_base: {base}\n      api_key: os.environ/{var}\n",
            name = yaml_q(&p.model),
            model = yaml_q(&format!("openai/{}", p.model)),
            base = yaml_q(&p.base_url),
            var = var,
        ));
        // ChatGPT subscription (OAuth) providers need the Codex/Responses headers on every request.
        // The rotating bearer still flows through `key` above (resolved_key reads the live token);
        // only these constant/account headers are added here.
        if p.is_oauth_subscription() {
            yaml.push_str("      extra_headers:\n");
            let account = crate::engine::oauth_store::current_account_id();
            if !account.is_empty() {
                yaml.push_str(&format!(
                    "        {}: {}\n",
                    crate::engine::oauth_store::ACCOUNT_HEADER,
                    yaml_q(&account)
                ));
            }
            yaml.push_str(&format!(
                "        originator: {}\n        OpenAI-Beta: {}\n",
                yaml_q(crate::engine::oauth_store::ORIGINATOR),
                yaml_q(crate::engine::oauth_store::OPENAI_BETA),
            ));
        }
        envs.push((var, key));
    }
    (yaml, envs)
}

static RELOAD: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();

/// Ask the supervisor to regenerate the config + restart the proxy (call after a provider CRUD).
pub fn request_reload() {
    if let Some(n) = RELOAD.get() {
        n.notify_one();
    }
}

/// Spawn the supervisor task once. No-op when the litellm binary is absent.
pub fn start_supervisor() {
    if !available() {
        tracing::info!(
            "[litellm] binary not found at {:?}; openai providers disabled",
            binary()
        );
        return;
    }
    let notify = Arc::new(tokio::sync::Notify::new());
    if RELOAD.set(notify.clone()).is_err() {
        return; // already started
    }
    tokio::spawn(async move {
        loop {
            let envs = build_config();
            if envs.is_empty() {
                // No openai providers → don't run the proxy; wait for a reload signal.
                notify.notified().await;
                continue;
            }
            tracing::info!(
                "[litellm] starting proxy on :{} with {} openai model(s)",
                port(),
                envs.len()
            );
            // Send the proxy's stdout+stderr to a log file so startup/crash failures (bad port, venv
            // issue, …) are diagnosable instead of vanishing.
            let log_path = home().join(".agentic-dev").join("litellm.log");
            let (out, err) = match std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
            {
                Ok(f) => match f.try_clone() {
                    Ok(f2) => (std::process::Stdio::from(f), std::process::Stdio::from(f2)),
                    Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
                },
                Err(e) => {
                    tracing::warn!("[litellm] cannot open log {:?}: {e}", log_path);
                    (std::process::Stdio::null(), std::process::Stdio::null())
                }
            };
            let mut cmd = tokio::process::Command::new(binary());
            cmd.arg("--config")
                .arg(config_path())
                .arg("--host")
                .arg("127.0.0.1")
                .arg("--port")
                .arg(port().to_string())
                .stdout(out)
                .stderr(err)
                .kill_on_drop(true);
            for (k, v) in &envs {
                cmd.env(k, v);
            }
            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    tracing::error!("[litellm] spawn failed: {e}; retrying in 10s");
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    continue;
                }
            };
            // Supervise WITHOUT polling: wake on the child exiting OR a reload signal, then the outer
            // loop regenerates the config and respawns.
            tokio::select! {
                _ = notify.notified() => {
                    let _ = child.kill().await; // async kill + reap
                }
                status = child.wait() => {
                    tracing::warn!("[litellm] proxy exited ({status:?}); restarting in 2s");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_q_escapes() {
        assert_eq!(yaml_q("https://x/v1"), "\"https://x/v1\"");
        assert_eq!(yaml_q(r#"a"b\c"#), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn proxy_base_url_uses_port() {
        assert!(proxy_base_url().starts_with("http://127.0.0.1:"));
    }

    use crate::engine::providers::{Protocol, Provider, ProviderRegistry};

    fn openai_provider(name: &str, key_env: Option<&str>, api_key: &str) -> Provider {
        Provider {
            name: name.into(),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            api_key: api_key.into(),
            api_key_env: key_env.map(str::to_string),
            model: "gpt-5".into(),
            protocol: Protocol::Openai,
            capability: 0.7,
            description: None,
            priority: 0.5,
            cost: 0.4,
            router: false,
            enabled: true,
        }
    }

    #[test]
    fn oauth_provider_emits_codex_headers() {
        use crate::engine::oauth_store;
        let _guard = oauth_store::test_util::isolate();
        oauth_store::save(&oauth_store::ChatGptTokens {
            access_token: "sk-live".into(),
            refresh_token: "rt".into(),
            account_id: "acct_42".into(),
            expires_at: 9_999_999_999_000,
            ..Default::default()
        })
        .unwrap();

        // OAuth subscription provider → rotating bearer via env + Codex headers.
        let oauth = openai_provider("chatgpt", Some(oauth_store::SENTINEL), "");
        // Ordinary BYOK openai provider → no extra headers.
        let byok = openai_provider("plainoai", None, "sk-static");
        let reg = ProviderRegistry {
            providers: vec![oauth, byok],
        };
        let (yaml, envs) = render_config(&reg);

        // Codex headers present for the oauth provider.
        assert!(yaml.contains("extra_headers:"), "yaml: {yaml}");
        assert!(yaml.contains("ChatGPT-Account-Id: \"acct_42\""));
        assert!(yaml.contains("originator: \"codex_cli_rs\""));
        assert!(yaml.contains("OpenAI-Beta: \"responses=experimental\""));
        // The live token is injected via env, never written into the yaml.
        assert!(!yaml.contains("sk-live"));
        assert!(envs.iter().any(|(_, v)| v == "sk-live"));
        assert!(envs.iter().any(|(_, v)| v == "sk-static"));
        // The BYOK provider block carries no ChatGPT headers.
        assert_eq!(yaml.matches("extra_headers:").count(), 1);
    }

    #[test]
    fn oauth_provider_without_login_is_skipped() {
        use crate::engine::oauth_store;
        let _guard = oauth_store::test_util::isolate();
        // No token saved → resolved_key empty → provider omitted from the proxy config entirely.
        let reg = ProviderRegistry {
            providers: vec![openai_provider("chatgpt", Some(oauth_store::SENTINEL), "")],
        };
        let (yaml, envs) = render_config(&reg);
        assert!(envs.is_empty());
        assert!(!yaml.contains("chatgpt") && !yaml.contains("extra_headers"));
    }
}
