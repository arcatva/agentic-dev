//! TLS mode selection + self-signed cert management for the listener.
//!
//! HTTPS is **on by default**. Selection (see [`crate::api::config`]):
//!
//! - `AGENTIC_TLS=off` → plain HTTP ([`TlsMode::Disabled`]).
//! - `AGENTIC_TLS_CERT` + `AGENTIC_TLS_KEY` set → serve that operator-supplied PEM pair
//!   ([`TlsMode::Byo`]).
//! - otherwise → generate + persist a self-signed cert under `AGENTIC_TLS_DIR`
//!   ([`TlsMode::SelfSigned`]). The cert lists the host's IPs (plus `AGENTIC_TLS_SAN`) as SANs so
//!   it also validates for browsers/curl connecting by IP; clients (the Android app) establish
//!   trust by pinning it on first use.
//!
//! `from_config` is pure and unit-tested; [`ensure_self_signed`] does the filesystem + rcgen work.

use crate::api::config::Config;
use std::io;
use std::path::{Path, PathBuf};

/// How the listener should terminate (or not terminate) TLS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlsMode {
    /// Plain HTTP (`AGENTIC_TLS=off`).
    Disabled,
    /// Terminate TLS with an operator-supplied PEM cert chain + key ("bring your own").
    Byo { cert: PathBuf, key: PathBuf },
    /// Terminate TLS with a self-signed cert generated + persisted under `dir`.
    SelfSigned {
        dir: PathBuf,
        extra_sans: Vec<String>,
        regen: bool,
    },
}

impl TlsMode {
    /// Decide the TLS mode from config. `AGENTIC_TLS=off` wins; otherwise a complete BYO pair
    /// (both cert + key) is used; otherwise the self-signed path.
    pub fn from_config(c: &Config) -> TlsMode {
        if !c.tls_enabled {
            return TlsMode::Disabled;
        }
        match (c.tls_cert.as_ref(), c.tls_key.as_ref()) {
            (Some(cert), Some(key)) => TlsMode::Byo {
                cert: cert.clone(),
                key: key.clone(),
            },
            _ => TlsMode::SelfSigned {
                dir: c.tls_dir.clone(),
                extra_sans: c.tls_extra_sans.clone(),
                regen: c.tls_regen,
            },
        }
    }

    /// True when this mode terminates TLS (Byo or SelfSigned).
    pub fn is_tls(&self) -> bool {
        !matches!(self, TlsMode::Disabled)
    }

    /// URL scheme this mode serves.
    pub fn scheme(&self) -> &'static str {
        if self.is_tls() {
            "https"
        } else {
            "http"
        }
    }

    /// True when TLS is enabled but exactly one of cert/key is set — a misconfiguration that
    /// silently falls back to the self-signed cert. Lets the caller warn.
    pub fn byo_half_configured(c: &Config) -> bool {
        c.tls_enabled && c.tls_cert.is_some() != c.tls_key.is_some()
    }
}

/// Ensure a self-signed cert+key exist under `dir`, returning `(cert_path, key_path)`.
/// Reuses the existing pair unless `regen` is set — stability matters because clients pin the cert.
///
/// Regenerating produces a NEW fingerprint, which invalidates every client that pinned the old
/// cert, so we only do it deliberately (regen) or when the on-disk pair is incomplete — and we warn
/// loudly in the latter case. Writes go through temp files + rename so a crash never leaves a
/// half-written file. (Two instances sharing one `dir` is out of scope — the sqlite store already
/// assumes a single instance per data dir.)
pub fn ensure_self_signed(
    dir: &Path,
    extra_sans: &[String],
    regen: bool,
) -> io::Result<(PathBuf, PathBuf)> {
    let cert_path = dir.join("cert.pem");
    let key_path = dir.join("key.pem");
    let (cert_there, key_there) = (cert_path.exists(), key_path.exists());
    if !regen && cert_there && key_there {
        return Ok((cert_path, key_path));
    }
    if !regen && (cert_there || key_there) {
        tracing::warn!(
            target: "tls",
            "incomplete self-signed TLS material in {} (cert.pem={cert_there}, key.pem={key_there}) — \
             regenerating; clients that pinned the previous cert must trust the new one",
            dir.display()
        );
    }
    std::fs::create_dir_all(dir)?;
    let sans = build_sans(extra_sans);
    let (cert_pem, key_pem) = generate_self_signed(&sans)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("rcgen: {e}")))?;
    // Key first (tight perms), then the public cert. Each write is atomic (temp file + rename).
    write_atomic(&key_path, key_pem.as_bytes(), /* private */ true)?;
    write_atomic(&cert_path, cert_pem.as_bytes(), /* private */ false)?;
    Ok((cert_path, key_path))
}

/// Build the SAN list for the self-signed cert: always localhost + loopback, plus every non-loopback
/// local interface IP, plus any operator-supplied `extra_sans`. De-duplicated, order preserved.
pub fn build_sans(extra_sans: &[String]) -> Vec<String> {
    let mut sans = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    match if_addrs::get_if_addrs() {
        Ok(ifaces) => {
            for iface in ifaces {
                let ip = iface.ip();
                if !ip.is_loopback() {
                    sans.push(ip.to_string());
                }
            }
        }
        Err(e) => {
            tracing::warn!(target: "tls", "could not enumerate interface IPs for cert SANs: {e}")
        }
    }
    sans.extend(extra_sans.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    sans.retain(|s| seen.insert(s.clone()));
    sans
}

fn generate_self_signed(sans: &[String]) -> Result<(String, String), rcgen::Error> {
    // CertificateParams::new parses each SAN string as an IP address, else a DNS name.
    let mut params = rcgen::CertificateParams::new(sans.to_vec())?;
    let mut dn = rcgen::DistinguishedName::new();
    dn.push(rcgen::DnType::CommonName, "agentic-dev");
    params.distinguished_name = dn;
    // Long, fixed validity: clients pin this cert (they don't chain-validate it against a CA), so
    // an unusually long lifetime is fine and avoids surprise expiry on a long-running host.
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2100, 1, 1);
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    Ok((cert.pem(), key_pair.serialize_pem()))
}

/// Write `bytes` to `path` atomically (temp sibling + rename). When `private`, the file is chmod
/// 0600 *before* the rename so it is never briefly world-readable.
fn write_atomic(path: &Path, bytes: &[u8], private: bool) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".tmp.{}", std::process::id()));
    let tmp = PathBuf::from(tmp);
    // Clean the temp file on EVERY failure path, not just a failed rename: an early `?` return
    // after a failed set_permissions would otherwise leave a world-readable temp copy of a
    // PRIVATE KEY on disk (flagged in the HTTPS PR review).
    let staged: io::Result<()> = (|| {
        std::fs::write(&tmp, bytes)?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp); // don't leave the temp behind on failure
            Err(e)
        }
    }
}

/// Extract only the `CERTIFICATE` PEM blocks from `pem`, concatenated — dropping any private-key
/// (or other) block. Used by the public cert-download route so a BYO combined cert+key PEM can never
/// leak the key. Returns an empty string when there are no certificate blocks.
pub fn certs_only_pem(pem: &[u8]) -> String {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let text = String::from_utf8_lossy(pem);
    let mut out = String::new();
    let mut rest: &str = &text;
    while let Some(bi) = rest.find(BEGIN) {
        let after = &rest[bi..];
        match after.find(END) {
            Some(ei) => {
                out.push_str(&after[..ei + END.len()]);
                out.push('\n');
                rest = &after[ei + END.len()..];
            }
            None => break, // unterminated block — stop
        }
    }
    out
}

/// SHA-256 fingerprint (uppercase colon-separated hex) of the first certificate in a PEM file.
/// Matches what a client computes over the leaf cert's DER — used for the trust-on-first-use pin.
pub fn cert_fingerprint_sha256(cert_pem_path: &Path) -> io::Result<String> {
    let pem = std::fs::read(cert_pem_path)?;
    let der = first_cert_der(&pem)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no CERTIFICATE block in PEM"))?;
    Ok(sha256_hex_colon(&der))
}

fn first_cert_der(pem: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(pem).ok()?;
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let start = text.find(BEGIN)? + BEGIN.len();
    let stop = text[start..].find(END)? + start;
    let b64: String = text[start..stop]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(b64.as_bytes())
        .ok()
}

fn sha256_hex_colon(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    digest
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::load(|_| None)
    }

    #[test]
    fn disabled_when_tls_off() {
        let mut c = cfg();
        c.tls_enabled = false;
        let m = TlsMode::from_config(&c);
        assert_eq!(m, TlsMode::Disabled);
        assert!(!m.is_tls());
        assert_eq!(m.scheme(), "http");
    }

    #[test]
    fn self_signed_by_default() {
        let c = cfg(); // tls_enabled default true, no BYO cert
        let m = TlsMode::from_config(&c);
        assert!(matches!(m, TlsMode::SelfSigned { .. }));
        assert!(m.is_tls());
        assert_eq!(m.scheme(), "https");
        assert!(!TlsMode::byo_half_configured(&c));
    }

    #[test]
    fn byo_when_cert_and_key_set() {
        let mut c = cfg();
        c.tls_cert = Some(PathBuf::from("/c.pem"));
        c.tls_key = Some(PathBuf::from("/k.pem"));
        assert_eq!(
            TlsMode::from_config(&c),
            TlsMode::Byo {
                cert: "/c.pem".into(),
                key: "/k.pem".into()
            }
        );
        assert!(!TlsMode::byo_half_configured(&c));
    }

    #[test]
    fn half_byo_falls_back_to_self_signed_and_is_flagged() {
        let mut c = cfg();
        c.tls_cert = Some(PathBuf::from("/c.pem")); // key missing
        assert!(matches!(
            TlsMode::from_config(&c),
            TlsMode::SelfSigned { .. }
        ));
        assert!(TlsMode::byo_half_configured(&c));
    }

    #[test]
    fn build_sans_includes_loopback_extra_and_dedups() {
        let sans = build_sans(&[
            "10.0.0.5".to_string(),
            "agentic.lan".to_string(),
            "127.0.0.1".to_string(),
        ]);
        assert!(sans.contains(&"localhost".to_string()));
        assert!(sans.contains(&"127.0.0.1".to_string()));
        assert!(sans.contains(&"::1".to_string()));
        assert!(sans.contains(&"10.0.0.5".to_string()));
        assert!(sans.contains(&"agentic.lan".to_string()));
        // "127.0.0.1" appears once despite being both a base entry and an extra.
        assert_eq!(sans.iter().filter(|s| *s == "127.0.0.1").count(), 1);
    }

    #[test]
    fn sha256_hex_colon_known_vector() {
        // SHA-256("") = e3b0c442...  formatted as uppercase colon hex.
        let fp = sha256_hex_colon(b"");
        assert!(fp.starts_with("E3:B0:C4:42:98:FC:1C:14"));
        assert_eq!(fp.split(':').count(), 32); // 32 bytes
    }

    #[test]
    fn ensure_self_signed_generates_reuses_and_regens() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) =
            ensure_self_signed(dir.path(), &["192.168.1.50".to_string()], false).unwrap();
        assert!(cert.exists() && key.exists());
        let fp1 = cert_fingerprint_sha256(&cert).unwrap();
        assert_eq!(fp1.split(':').count(), 32);

        // regen=false reuses the same cert (stable fingerprint — clients rely on the pin).
        let (cert2, _) = ensure_self_signed(dir.path(), &[], false).unwrap();
        assert_eq!(cert, cert2);
        assert_eq!(cert_fingerprint_sha256(&cert2).unwrap(), fp1);

        // regen=true produces a different cert.
        let (cert3, _) = ensure_self_signed(dir.path(), &[], true).unwrap();
        assert_ne!(cert_fingerprint_sha256(&cert3).unwrap(), fp1);
    }

    #[test]
    fn partial_material_regenerates_both() {
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = ensure_self_signed(dir.path(), &[], false).unwrap();
        let fp1 = cert_fingerprint_sha256(&cert).unwrap();
        // Simulate a crash between the two writes: key present, cert missing.
        std::fs::remove_file(&cert).unwrap();
        assert!(!cert.exists() && key.exists());
        // Reuse guard requires BOTH → regenerates a fresh, matching pair (new fingerprint).
        let (cert2, key2) = ensure_self_signed(dir.path(), &[], false).unwrap();
        assert!(cert2.exists() && key2.exists());
        assert_ne!(cert_fingerprint_sha256(&cert2).unwrap(), fp1);
    }

    #[test]
    fn certs_only_pem_drops_private_key() {
        // A combined cert+key PEM (haproxy-style). Only the CERTIFICATE block must survive.
        let combined = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n\
                        -----BEGIN PRIVATE KEY-----\nSECRET\n-----END PRIVATE KEY-----\n";
        let out = certs_only_pem(combined.as_bytes());
        assert!(out.contains("BEGIN CERTIFICATE"));
        assert!(out.contains("AAAA"));
        assert!(!out.contains("PRIVATE KEY"), "private key must be stripped");
        assert!(!out.contains("SECRET"));
        // No certificate blocks → empty (handler turns this into a 404).
        assert!(
            certs_only_pem(b"-----BEGIN PRIVATE KEY-----\nX\n-----END PRIVATE KEY-----\n")
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn self_signed_key_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (_, key) = ensure_self_signed(dir.path(), &[], false).unwrap();
        let mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "self-signed key must be 0600");
    }
}
