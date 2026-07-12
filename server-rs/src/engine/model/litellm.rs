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
    let (yaml, envs) = build_model_yaml(&reg);
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

/// Pure builder: turn the openai-protocol providers into the litellm `model_list` YAML plus the
/// `(env_var, key)` pairs the proxy process needs. Separated from file IO so it is unit testable.
fn build_model_yaml(reg: &ProviderRegistry) -> (String, Vec<(String, String)>) {
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
        // A ChatGPT-subscription (OAuth) provider talks to the Codex backend, which requires three
        // non-standard headers (the account id comes from the JWT stored with the token). The bearer
        // itself is still the rotating access token injected via env above; only these extra headers
        // are static. ponytail: assumes litellm can speak the Codex `responses` shape at this
        // api_base; if not, the upgrade is a thin Anthropic→Codex shim for this one provider.
        if let Some(tok) = crate::engine::oauth::subscription_token(&p.name) {
            yaml.push_str(&format!(
                "      extra_headers:\n        ChatGPT-Account-Id: {acct}\n        originator: {orig}\n        OpenAI-Beta: {beta}\n",
                acct = yaml_q(&tok.account_id),
                orig = yaml_q(crate::engine::oauth::ORIGINATOR),
                beta = yaml_q(crate::engine::oauth::OPENAI_BETA),
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

    #[test]
    fn oauth_provider_yaml_has_codex_headers_and_env_bearer() {
        use crate::engine::oauth;
        // Serialize with the other tests that swap the global token-store override.
        let _serial = oauth::TEST_STORE_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Point the token store at a temp file and give the "gpt" provider a subscription token.
        let dir = std::env::temp_dir().join(format!("litellm-oauth-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        *oauth::TOKENS_FILE_OVERRIDE.lock() = Some(dir.join("oauth-tokens.json"));
        oauth::put_token(
            "gpt",
            oauth::SubscriptionToken {
                access_token: "live-bearer".into(),
                refresh_token: "r".into(),
                account_id: "acct_777".into(),
                account_email: None,
                expires_at: i64::MAX,
                needs_reauth: false,
            },
        )
        .unwrap();

        let reg = ProviderRegistry {
            providers: vec![crate::engine::providers::Provider {
                name: "gpt".into(),
                base_url: oauth::CODEX_BASE_URL.into(),
                api_key: String::new(), // resolved from the token store
                api_key_env: None,
                model: "gpt-5-codex".into(),
                protocol: Protocol::Openai,
                capability: 0.9,
                description: None,
                priority: 0.5,
                cost: 0.6,
                router: false,
                enabled: true,
            }],
        };
        let (yaml, envs) = build_model_yaml(&reg);
        *oauth::TOKENS_FILE_OVERRIDE.lock() = None;
        let _ = std::fs::remove_dir_all(&dir);

        assert!(yaml.contains("api_base: \"https://chatgpt.com/backend-api/codex\""));
        assert!(yaml.contains("ChatGPT-Account-Id: \"acct_777\""));
        assert!(yaml.contains("originator: \"codex_cli_rs\""));
        assert!(yaml.contains("OpenAI-Beta: \"responses=experimental\""));
        // Bearer goes in the process env, never the yaml file.
        assert!(!yaml.contains("live-bearer"));
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].1, "live-bearer");
    }
}
