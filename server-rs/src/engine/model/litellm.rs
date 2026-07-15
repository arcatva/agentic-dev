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
/// Pure config renderer: the YAML for the openai providers + the `(env_var, key)` pairs to inject.
/// The subscription provider gets the codex `extra_headers` and its rotating bearer via
/// `effective_key` (the OAuth access token from the 0600 store). Split out so it is unit-testable
/// without touching the global providers file or the config path.
fn render_config(
    providers: &[crate::engine::providers::Provider],
) -> (String, Vec<(String, String)>) {
    let mut yaml = String::from("model_list:\n");
    let mut envs: Vec<(String, String)> = Vec::new();
    for p in providers {
        if !matches!(p.protocol, Protocol::Openai) {
            continue;
        }
        let key = p.effective_key();
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
        // The ChatGPT subscription backend needs the codex headers alongside the rotating bearer.
        // ponytail: the codex endpoint speaks the Responses API (/responses, forced streaming),
        // which differs from the default /chat/completions shape litellm translates to; the config
        // carries the right base_url, bearer, and headers — the exact litellm route for the codex
        // backend is the one integration point needing a live check (untestable here). Upgrade to a
        // custom litellm provider/route if the default shape mismatches.
        if p.is_subscription() {
            let account_id = crate::engine::model::openai_oauth::load()
                .map(|t| t.account_id)
                .unwrap_or_default();
            yaml.push_str(&format!(
                "      extra_headers:\n        ChatGPT-Account-Id: {id}\n        originator: {orig}\n        OpenAI-Beta: {beta}\n",
                id = yaml_q(&account_id),
                orig = yaml_q("codex_cli_rs"),
                beta = yaml_q("responses=experimental"),
            ));
        }
        envs.push((var, key));
    }
    (yaml, envs)
}

fn build_config() -> Vec<(String, String)> {
    let reg = ProviderRegistry::load();
    let (yaml, envs) = render_config(&reg.providers);
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
    fn render_config_subscription_has_codex_base_and_headers() {
        use crate::engine::model::openai_oauth;
        use crate::engine::providers::Provider;
        let _g = openai_oauth::TEST_STORE_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("s.json");
        *openai_oauth::STORE_FILE_OVERRIDE.lock() = Some(f.clone());
        openai_oauth::save(&openai_oauth::StoredToken {
            access_token: "tok".into(),
            account_id: "acc-9".into(),
            ..Default::default()
        })
        .unwrap();
        let sub = Provider {
            name: openai_oauth::PROVIDER_NAME.into(),
            base_url: openai_oauth::CODEX_BASE_URL.into(),
            api_key: String::new(),
            api_key_env: None,
            model: "gpt-5".into(),
            protocol: Protocol::Openai,
            capability: 0.9,
            description: None,
            priority: 0.5,
            cost: 0.9,
            router: false,
            enabled: true,
        };
        let (yaml, envs) = render_config(&[sub]);
        assert!(yaml.contains("api_base: \"https://chatgpt.com/backend-api/codex\""));
        assert!(yaml.contains("ChatGPT-Account-Id: \"acc-9\""));
        assert!(yaml.contains("originator: \"codex_cli_rs\""));
        assert!(yaml.contains("OpenAI-Beta: \"responses=experimental\""));
        assert_eq!(envs.len(), 1, "rotating bearer injected via env");
        assert_eq!(envs[0].1, "tok");
        // A disconnected subscription (empty store) is skipped entirely.
        openai_oauth::clear();
        let sub2 = Provider {
            name: openai_oauth::PROVIDER_NAME.into(),
            base_url: openai_oauth::CODEX_BASE_URL.into(),
            api_key: String::new(),
            api_key_env: None,
            model: "gpt-5".into(),
            protocol: Protocol::Openai,
            capability: 0.9,
            description: None,
            priority: 0.5,
            cost: 0.9,
            router: false,
            enabled: true,
        };
        let (_, envs2) = render_config(&[sub2]);
        assert!(envs2.is_empty(), "no token → provider omitted");
        *openai_oauth::STORE_FILE_OVERRIDE.lock() = None;
    }
}
