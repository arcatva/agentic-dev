use std::path::Path;

pub const USAGE_ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

/// Shared HTTP client — holds a connection pool; creating one per call wastes sockets.
static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

#[derive(thiserror::Error, Debug)]
pub enum UsageError {
    #[error("no oauth token in credentials")]
    NoToken,
    #[error("usage endpoint {0}")]
    Status(u16),
    #[error("{0}")]
    Other(String),
}

pub async fn fetch_usage(
    base: &Path,
    fetch_fn: Option<&(dyn Fn(&str, &str) -> Result<serde_json::Value, u16> + Send + Sync)>,
) -> Result<serde_json::Value, UsageError> {
    let text = std::fs::read_to_string(base.join(".credentials.json"))
        .map_err(|e| UsageError::Other(e.to_string()))?;
    let creds: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| UsageError::Other(e.to_string()))?;
    let token = creds
        .get("claudeAiOauth")
        .and_then(|c| c.get("accessToken"))
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .ok_or(UsageError::NoToken)?;
    if let Some(f) = fetch_fn {
        return f(USAGE_ENDPOINT, token).map_err(UsageError::Status);
    }
    let res = HTTP_CLIENT
        .get(USAGE_ENDPOINT)
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("content-type", "application/json")
        .send()
        .await
        .map_err(|e| UsageError::Other(e.to_string()))?;
    if !res.status().is_success() {
        return Err(UsageError::Status(res.status().as_u16()));
    }
    res.json::<serde_json::Value>()
        .await
        .map_err(|e| UsageError::Other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base_with(creds: serde_json::Value) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agentic-usage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(".credentials.json"), creds.to_string()).unwrap();
        d
    }

    #[tokio::test]
    async fn sends_bearer_and_returns_parsed() {
        let base = base_with(serde_json::json!({"claudeAiOauth":{"accessToken":"tok-123"}}));
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(String::new()));
        let s = seen.clone();
        let f = move |_url: &str, tok: &str| -> Result<serde_json::Value, u16> {
            *s.lock().unwrap() = tok.to_string();
            Ok(serde_json::json!({"seven_day":{"utilization":49,"resets_at":"y"}}))
        };
        let u = fetch_usage(&base, Some(&f)).await.unwrap();
        assert_eq!(*seen.lock().unwrap(), "tok-123");
        assert_eq!(u["seven_day"]["utilization"], 49);
    }

    #[tokio::test]
    async fn errors_when_token_missing() {
        let base = base_with(serde_json::json!({"claudeAiOauth":{}}));
        let f =
            |_u: &str, _t: &str| -> Result<serde_json::Value, u16> { Ok(serde_json::json!({})) };
        let e = fetch_usage(&base, Some(&f)).await.unwrap_err();
        assert!(matches!(e, UsageError::NoToken));
    }

    #[tokio::test]
    async fn errors_on_non_ok() {
        let base = base_with(serde_json::json!({"claudeAiOauth":{"accessToken":"t"}}));
        let f = |_u: &str, _t: &str| -> Result<serde_json::Value, u16> { Err(401) };
        let e = fetch_usage(&base, Some(&f)).await.unwrap_err();
        assert!(matches!(e, UsageError::Status(401)));
    }
}
