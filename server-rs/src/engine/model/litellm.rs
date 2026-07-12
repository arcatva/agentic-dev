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

/// Render the litellm `model_list` YAML + the `(env_var, key)` pairs from a registry. Pure (no disk),
/// so it is unit-testable with an injected registry. Only openai-protocol providers with a resolved
/// key are emitted (an oauth provider's key is its live access token — empty when disconnected).
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
        // ChatGPT-subscription OAuth provider: the api_base is the codex backend. Keeping the litellm
        // provider as `openai/` (not `chatgpt/`) is deliberate — only the "openai" provider takes the
        // Anthropic-messages → Responses-API path (litellm hardcodes that whitelist), which POSTs to
        // <api_base>/responses (the endpoint ChatGPT subscriptions actually accept). These extra_headers
        // supply the codex identity the responses call requires; the rotating bearer rides in `api_key`.
        if p.oauth {
            yaml.push_str("      extra_headers:\n");
            yaml.push_str("        originator: \"codex_cli_rs\"\n");
            yaml.push_str("        OpenAI-Beta: \"responses=experimental\"\n");
            let account = crate::engine::oauth::account_id();
            if !account.is_empty() {
                yaml.push_str(&format!(
                    "        ChatGPT-Account-Id: {}\n",
                    yaml_q(&account)
                ));
            }
        }
        envs.push((var, key));
    }
    (yaml, envs)
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

    fn mk(name: &str, model: &str, proto: Protocol, key: &str, oauth: bool) -> Provider {
        Provider {
            name: name.into(),
            base_url: if oauth { crate::engine::oauth::API_BASE.into() } else { "https://x/v1".into() },
            api_key: key.into(),
            api_key_env: None,
            model: model.into(),
            protocol: proto,
            capability: 0.6,
            description: None,
            priority: 0.5,
            cost: 0.5,
            router: false,
            enabled: true,
            oauth,
        }
    }

    #[test]
    fn render_config_emits_openai_entries_and_skips_keyless() {
        let reg = ProviderRegistry {
            providers: vec![
                mk("anthropic-one", "claude-x", Protocol::Anthropic, "k", false), // skipped: not openai
                mk("openai-one", "deepseek-chat", Protocol::Openai, "sk-1", false),
                mk("openai-nokey", "gpt-nokey", Protocol::Openai, "", false), // skipped: empty key
            ],
        };
        let (yaml, envs) = render_config(&reg);
        assert!(yaml.contains("model_name: \"deepseek-chat\""));
        assert!(yaml.contains("model: \"openai/deepseek-chat\""));
        assert!(!yaml.contains("claude-x"), "anthropic provider must not appear");
        assert!(!yaml.contains("gpt-nokey"), "keyless provider must not appear");
        assert!(!yaml.contains("extra_headers"), "non-oauth provider has no extra_headers");
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].1, "sk-1");
    }

    #[test]
    fn render_config_oauth_provider_emits_codex_responses_headers() {
        use crate::engine::oauth;
        let _guard = oauth::STORE_TEST_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        *oauth::STORE_OVERRIDE.lock() = Some(dir.path().join("openai.json"));
        oauth::save(&oauth::TokenStore {
            access_token: "AT_BEARER".into(),
            refresh_token: "RT".into(),
            account_id: "acct_777".into(),
            ..Default::default()
        })
        .unwrap();

        let reg = ProviderRegistry {
            providers: vec![mk("ChatGPT", "gpt-5", Protocol::Openai, "", true)],
        };
        let (yaml, envs) = render_config(&reg);

        // routed as openai/ (so litellm takes the Responses-API path to <api_base>/responses)
        assert!(yaml.contains("model: \"openai/gpt-5\""));
        assert!(yaml.contains(&format!("api_base: \"{}\"", oauth::API_BASE)));
        // codex identity headers present AND correctly nested: extra_headers is a sibling of
        // api_key (6-space indent under litellm_params), its entries at 8 spaces. Pinning the exact
        // indentation guards the YAML structure (a stray indent would make litellm ignore the headers).
        assert!(yaml.contains(
            "      api_key: os.environ/AGENTIC_LITELLM_KEY_0\n      extra_headers:\n        originator: \"codex_cli_rs\"\n        OpenAI-Beta: \"responses=experimental\"\n        ChatGPT-Account-Id: \"acct_777\"\n"
        ), "extra_headers must nest under litellm_params as a sibling of api_key:\n{yaml}");
        // the rotating bearer is injected via env, never written to the file
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].1, "AT_BEARER");
        assert!(!yaml.contains("AT_BEARER"), "token must not land in the config file");
        assert!(yaml.contains("api_key: os.environ/"));

        *oauth::STORE_OVERRIDE.lock() = None;
    }

    #[test]
    fn render_config_oauth_disconnected_is_skipped() {
        use crate::engine::oauth;
        let _guard = oauth::STORE_TEST_LOCK.lock();
        let dir = tempfile::tempdir().unwrap();
        // point at an empty store dir → no token → resolved_key empty → skipped
        *oauth::STORE_OVERRIDE.lock() = Some(dir.path().join("openai.json"));
        let reg = ProviderRegistry {
            providers: vec![mk("ChatGPT", "gpt-5", Protocol::Openai, "", true)],
        };
        let (yaml, envs) = render_config(&reg);
        assert!(envs.is_empty());
        assert!(!yaml.contains("gpt-5"));
        *oauth::STORE_OVERRIDE.lock() = None;
    }
}
