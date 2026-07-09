use axum::{extract::{ConnectInfo, Extension, State, Json}, http::StatusCode, response::{IntoResponse, Response}};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use subtle::ConstantTimeEq;
use crate::api::state::AppState;
use crate::api::auth::issue_token;
use crate::util::now_ms;

#[derive(Deserialize, Default)]
pub struct LoginBody { pub password: Option<String> }

pub async fn login(
    State(st): State<AppState>,
    connect: Option<Extension<ConnectInfo<SocketAddr>>>,
    raw: axum::body::Bytes,
) -> Response {
    // Lenient body parse (same as every other write handler) so an empty / missing-content-type
    // body falls through to an empty password → 401, rather than a 415/422 from the Json extractor
    // (empty body yields a default-empty struct rather than a parse error).
    let body: LoginBody = crate::api::sessions::parse_body_lenient(&raw);
    let ip = connect.map(|ext| ext.0.ip()).unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
    let now = now_ms() as u64;
    {
        let t = st.throttle.lock();
        if !t.check(ip, now) {
            return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error":"too many attempts"}))).into_response();
        }
    }
    let given = body.password.unwrap_or_default();
    // Hash both sides before comparing — constant-time comparison leaks neither
    // timing nor password length information.
    let a = Sha256::digest(given.as_bytes());
    let b = Sha256::digest(st.config.password.as_bytes());
    let ok: bool = a.ct_eq(&b).into();
    if !ok {
        st.throttle.lock().record_fail(ip, now);
        return (StatusCode::UNAUTHORIZED, Json(json!({"error":"bad password"}))).into_response();
    }
    st.throttle.lock().record_success(ip);
    let token = issue_token(&st.config.auth_secret, 30 * 24 * 3600, now / 1000);
    (StatusCode::OK, Json(json!({"token": token}))).into_response()
}
