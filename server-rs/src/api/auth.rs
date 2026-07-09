//! Token auth: stable token format (HMAC-SHA256 over the expiry).

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

fn sign(secret: &str, payload: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC takes a key of any size");
    mac.update(payload.as_bytes());
    URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
}

/// Token = "<expEpochSec>.<base64url(HMAC_SHA256(secret, exp))>" (HMAC-SHA256 over the expiry).
pub fn issue_token(secret: &str, ttl_seconds: u64, now_secs: u64) -> String {
    let exp = now_secs + ttl_seconds;
    let payload = exp.to_string();
    let sig = sign(secret, &payload);
    format!("{payload}.{sig}")
}

pub fn verify_token(secret: &str, token: &str, now_secs: u64) -> bool {
    let Some((payload, sig_b64)) = token.split_once('.') else { return false };
    let Ok(provided) = URL_SAFE_NO_PAD.decode(sig_b64) else { return false };
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) { Ok(m) => m, Err(_) => return false };
    mac.update(payload.as_bytes());
    if mac.verify_slice(&provided).is_err() { return false; } // constant-time
    match payload.parse::<u64>() { Ok(exp) => exp > now_secs, Err(_) => false }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Token minted with secret "test-secret", exp 9999999999 — token-format compatibility gate.
    const COMPAT_TOKEN: &str = "9999999999.Rl7rud9Sqkll6ysQ-xw6BsEtc89CSCfEEvHaCyFxSi4";

    #[test]
    fn verifies_a_cross_format_token() {
        assert!(verify_token("test-secret", COMPAT_TOKEN, 1_000));
    }

    #[test]
    fn round_trips() {
        let t = issue_token("s3cret", 3600, 1_000);
        assert!(verify_token("s3cret", &t, 1_000));
        assert!(verify_token("s3cret", &t, 1_000 + 3599));
    }

    #[test]
    fn tampered_token_signature_rejected() {
        // Flip the last character of the COMPAT_TOKEN signature to produce a tampered token.
        // Ensures the decode-then-verify_slice path catches mutation, not a string compare.
        let mut tampered = COMPAT_TOKEN.to_string();
        let last = tampered.pop().unwrap();
        let replacement = if last == 'A' { 'B' } else { 'A' };
        tampered.push(replacement);
        assert!(!verify_token("test-secret", &tampered, 1_000), "tampered signature must be rejected");
    }

    #[test]
    fn rejects_expired_tampered_and_wrong_secret() {
        let t = issue_token("s3cret", 3600, 1_000);
        assert!(!verify_token("s3cret", &t, 1_000 + 3601)); // expired
        assert!(!verify_token("wrong", &t, 1_000));          // wrong secret
        assert!(!verify_token("s3cret", "9999999999.bm90YXNpZw", 1_000)); // bad sig
        assert!(!verify_token("s3cret", "nodot", 1_000));    // malformed
        assert!(!verify_token("s3cret", "notanumber.sig", 1_000)); // non-numeric exp
    }
}
