use std::path::Path;
use crate::engine::atomic_write::write_file_atomic;

/// Shared HTTP client — holds a connection pool; creating one per call wastes sockets.
static HTTP_CLIENT: std::sync::LazyLock<reqwest::Client> =
    std::sync::LazyLock::new(reqwest::Client::new);

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct DeviceRecord {
    pub token: String,
    #[serde(rename = "registeredAt")]
    pub registered_at: i64,
}

pub fn load_device_token(path: &Path) -> Option<DeviceRecord> {
    let text = std::fs::read_to_string(path).ok()?;
    let rec: DeviceRecord = serde_json::from_str(&text).ok()?;
    if rec.token.is_empty() { None } else { Some(rec) }
}

pub fn save_device_token(path: &Path, token: &str) -> std::io::Result<DeviceRecord> {
    let rec = DeviceRecord {
        token: token.to_string(),
        registered_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as i64,
    };
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    write_file_atomic(path, &serde_json::to_string_pretty(&rec)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?)?;
    Ok(rec)
}

#[derive(Clone, Debug)]
pub struct FcmCreds { pub project_id: String, pub server_key: String }

pub fn load_fcm_creds(get: impl Fn(&str) -> Option<String>) -> Option<FcmCreds> {
    let project_id = get("AGENTIC_FCM_PROJECT_ID").filter(|s| !s.is_empty())?;
    let server_key = get("AGENTIC_FCM_SERVER_KEY").filter(|s| !s.is_empty())?;
    Some(FcmCreds { project_id, server_key })
}

#[derive(Clone, Debug)]
pub struct PushPayload {
    pub session_id: String,
    pub status: String,
    pub is_error: bool,
    pub error_text: Option<String>,
    pub cost_usd: Option<f64>,
    /// Session display title (the auto-retitled `prompt`). Shown as the notification title so the
    /// user can tell WHICH session finished; falls back to "Session {status}" when absent.
    pub title: Option<String>,
}

pub fn fcm_body(payload: &PushPayload, device_token: &str) -> serde_json::Value {
    // Session title: first non-empty line, capped at 80 chars (char-safe for CJK) — prompts can
    // be multi-KB and would otherwise also blow FCM's 4KB data-message limit (the same capped
    // string feeds the data block below). The notification title falls back to "Session {status}"
    // when no usable title exists; the data field stays empty in that case.
    let session_title: Option<String> = payload
        .title
        .as_deref()
        .and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(|l| l.chars().take(80).collect());
    let title = session_title
        .clone()
        .unwrap_or_else(|| format!("Session {}", payload.status));
    let body = if payload.is_error {
        format!("Error: {}", payload.error_text.as_deref().unwrap_or("unknown error"))
    } else {
        match payload.cost_usd {
            Some(c) => format!("Completed (${:.4})", c),
            None => "Completed".to_string(),
        }
    };
    let cost_str = match payload.cost_usd { Some(c) => c.to_string(), None => String::new() };
    serde_json::json!({
        "message": {
            "token": device_token,
            "notification": { "title": title, "body": body },
            "data": {
                "sessionId": payload.session_id,
                "status": payload.status,
                "isError": payload.is_error.to_string(),
                "costUsd": cost_str,
                // The CAPPED title, not the raw prompt — data values count against FCM's 4KB limit.
                "title": session_title.unwrap_or_default(),
            }
        }
    })
}

pub async fn send_push(
    payload: &PushPayload,
    device_token: Option<&str>,
    creds: Option<&FcmCreds>,
    send_fn: Option<&(dyn Fn(serde_json::Value) + Send + Sync)>,
) {
    let (Some(token), Some(creds)) = (device_token.filter(|t| !t.is_empty()), creds) else { return; };
    let body = fcm_body(payload, token);
    if let Some(f) = send_fn { f(body); return; }
    // Real FCM HTTP v1 send — failure is swallowed (a push must never block session completion).
    let url = format!("https://fcm.googleapis.com/v1/projects/{}/messages:send", creds.project_id);
    match HTTP_CLIENT.post(&url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", creds.server_key))
        .json(&body).send().await
    {
        Ok(res) if !res.status().is_success() =>
            tracing::warn!("[push] FCM send failed: {}", res.status()),
        Ok(_) => {}
        Err(e) => tracing::warn!("[push] FCM send error (ignored): {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("agentic-push-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn load_returns_none_when_missing() {
        assert!(load_device_token(&tmp().join("device.json")).is_none());
    }

    #[test]
    fn round_trips_and_last_wins() {
        let p = tmp().join("device.json");
        let r = save_device_token(&p, "tok-abc").unwrap();
        assert_eq!(r.token, "tok-abc");
        assert_eq!(load_device_token(&p).unwrap().token, "tok-abc");
        save_device_token(&p, "second").unwrap();
        assert_eq!(load_device_token(&p).unwrap().token, "second");
    }

    #[test]
    fn load_returns_none_for_empty_token_or_garbage() {
        let p = tmp().join("d.json");
        std::fs::write(&p, r#"{"token":"","registeredAt":1}"#).unwrap();
        assert!(load_device_token(&p).is_none());
        std::fs::write(&p, "not json").unwrap();
        assert!(load_device_token(&p).is_none());
    }

    #[test]
    fn creds_require_both_env_vars() {
        assert!(load_fcm_creds(|_| None).is_none());
        assert!(load_fcm_creds(|k| if k == "AGENTIC_FCM_PROJECT_ID" { Some("p".into()) } else { None }).is_none());
        let c = load_fcm_creds(|k| match k {
            "AGENTIC_FCM_PROJECT_ID" => Some("proj".into()),
            "AGENTIC_FCM_SERVER_KEY" => Some("key".into()),
            _ => None,
        }).unwrap();
        assert_eq!(c.project_id, "proj");
        assert_eq!(c.server_key, "key");
    }

    fn payload() -> PushPayload {
        PushPayload { session_id: "s1".into(), status: "done".into(), is_error: false,
            error_text: None, cost_usd: Some(0.0042), title: None }
    }

    #[tokio::test]
    async fn no_op_when_token_or_creds_missing() {
        use std::sync::{Arc, Mutex};
        let calls = Arc::new(Mutex::new(0u32));
        let c = calls.clone();
        let f = move |_b: serde_json::Value| { *c.lock().unwrap() += 1; };
        let creds = FcmCreds { project_id: "p".into(), server_key: "k".into() };
        send_push(&payload(), None, Some(&creds), Some(&f)).await;          // no token
        send_push(&payload(), Some("dev"), None, Some(&f)).await;           // no creds
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn calls_send_fn_with_correct_body() {
        use std::sync::{Arc, Mutex};
        let body = Arc::new(Mutex::new(serde_json::Value::Null));
        let b = body.clone();
        let f = move |v: serde_json::Value| { *b.lock().unwrap() = v; };
        let creds = FcmCreds { project_id: "my-project".into(), server_key: "my-key".into() };
        send_push(&payload(), Some("device-token-xyz"), Some(&creds), Some(&f)).await;
        let v = body.lock().unwrap().clone();
        assert_eq!(v["message"]["token"], "device-token-xyz");
        assert_eq!(v["message"]["data"]["sessionId"], "s1");
        assert_eq!(v["message"]["data"]["status"], "done");
        assert_eq!(v["message"]["data"]["isError"], "false");
        // success body: "Completed ($0.0042)" — 4 decimals
        assert_eq!(v["message"]["notification"]["body"], "Completed ($0.0042)");
        assert_eq!(v["message"]["notification"]["title"], "Session done");
    }

    #[test]
    fn title_prefers_session_title_first_line_capped() {
        // Session title present → notification title is its first line, char-capped at 80.
        let p = PushPayload { title: Some("修复Session选择器UI布局\n第二行不应出现".into()), ..payload() };
        let v = fcm_body(&p, "tok");
        assert_eq!(v["message"]["notification"]["title"], "修复Session选择器UI布局");
        // data.title carries the SAME capped first-line string (FCM 4KB data limit), not the raw prompt.
        assert_eq!(v["message"]["data"]["title"], "修复Session选择器UI布局");
        // 100 CJK chars → capped to 80 CHARS (not bytes — no mid-codepoint split).
        let long: String = std::iter::repeat('测').take(100).collect();
        let p2 = PushPayload { title: Some(long), ..payload() };
        let t = fcm_body(&p2, "tok")["message"]["notification"]["title"].as_str().unwrap().to_string();
        assert_eq!(t.chars().count(), 80);
        // Absent / all-blank title → legacy "Session {status}" fallback, empty data.title.
        let v3 = fcm_body(&payload(), "tok");
        assert_eq!(v3["message"]["notification"]["title"], "Session done");
        assert_eq!(v3["message"]["data"]["title"], "");
        // Blank FIRST line → the first non-empty line is used, not the fallback.
        let p4 = PushPayload { title: Some("   \nreal".into()), ..payload() };
        assert_eq!(fcm_body(&p4, "tok")["message"]["notification"]["title"], "real");
    }

    #[test]
    fn error_body_and_null_cost_serialize_to_strings() {
        let p = PushPayload { session_id: "s2".into(), status: "failed".into(), is_error: true,
            error_text: Some("boom".into()), cost_usd: None, title: None };
        let v = fcm_body(&p, "tok");
        assert_eq!(v["message"]["notification"]["body"], "Error: boom");
        assert_eq!(v["message"]["data"]["isError"], "true");
        assert_eq!(v["message"]["data"]["costUsd"], "");     // null → empty string
        // missing errorText → "unknown error"
        let p2 = PushPayload { error_text: None, ..p.clone() };
        let v2 = fcm_body(&p2, "tok");
        assert_eq!(v2["message"]["notification"]["body"], "Error: unknown error");
    }
}
