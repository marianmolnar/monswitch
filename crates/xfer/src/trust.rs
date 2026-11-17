//! Deskflow's pairing: this machine's certificate and the fingerprints it
//! trusts.
//!
//! Deskflow keeps, in its `tls` folder:
//! - `deskflow.pem` – private key + self-signed certificate of this machine;
//! - `trusted-servers` (client) / `trusted-clients` (server) – one
//!   fingerprint per line, `v2:sha256:<hex>` (SHA-256 of the DER certificate).
//!
//! The transfer channel accepts a peer whose certificate fingerprint is in
//! either list (or in `[xfer].trusted`), so pairing once in Deskflow is
//! enough for both directions.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const CERT_FILE: &str = "deskflow.pem";
pub const TRUST_FILES: [&str; 2] = ["trusted-servers", "trusted-clients"];

/// SHA-256 of a DER certificate as lowercase hex (Deskflow's v2 fingerprint).
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `aa:bb:…`, `AABB…` or `v2:sha256:aabb…` → 64 lowercase hex digits.
/// `None` for anything else (including Deskflow's old SHA-1 lines).
pub fn normalize(fp: &str) -> Option<String> {
    let fp = fp.trim();
    let fp = fp.strip_prefix("v2:").unwrap_or(fp);
    let fp = fp.strip_prefix("sha256:").unwrap_or(fp);
    let hex: String = fp
        .chars()
        .filter(|c| *c != ':')
        .collect::<String>()
        .to_ascii_lowercase();
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit())).then_some(hex)
}

/// Fingerprints from a Deskflow trust file.
pub fn parse_trust_file(text: &str) -> Vec<String> {
    text.lines().filter_map(normalize).collect()
}

/// `aa:bb:cc…` for showing to the user (Deskflow displays it this way).
pub fn pretty(fp: &str) -> String {
    fp.as_bytes()
        .chunks(2)
        .map(|c| String::from_utf8_lossy(c).to_ascii_uppercase())
        .collect::<Vec<_>>()
        .join(":")
}

/// Folders Deskflow may use for its TLS files, most specific first.
fn candidates(settings_file: Option<&Path>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(s) = settings_file {
        if let Ok(text) = fs::read_to_string(s) {
            if let Some(cert) = security_certificate(&text) {
                if let Some(parent) = Path::new(&cert).parent() {
                    v.push(parent.to_path_buf());
                }
            }
        }
        if let Some(dir) = s.parent() {
            v.push(dir.join("tls"));
        }
    }
    if cfg!(windows) {
        for var in ["ProgramData", "APPDATA", "LOCALAPPDATA"] {
            if let Some(p) = std::env::var_os(var) {
                v.push(PathBuf::from(p).join("Deskflow").join("tls"));
            }
        }
    } else if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for sub in [
            "Library/Deskflow",
            "Library/Application Support/Deskflow",
            "Library/Preferences/Deskflow",
            ".config/Deskflow",
            ".local/share/Deskflow",
        ] {
            v.push(home.join(sub).join("tls"));
        }
    }
    v
}

/// `[security] certificate=` from a Deskflow settings file.
fn security_certificate(text: &str) -> Option<String> {
    let mut in_security = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_security = line.eq_ignore_ascii_case("[security]");
        } else if in_security {
            if let Some(v) = line.strip_prefix("certificate=") {
                let v = v.trim().trim_matches('"');
                return (!v.is_empty()).then(|| v.to_string());
            }
        }
    }
    None
}

/// The TLS folder: `configured` if set, otherwise the first candidate that
/// holds `deskflow.pem`.
pub fn find_tls_dir(configured: &str, settings_file: Option<&Path>) -> Option<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        return Some(PathBuf::from(configured));
    }
    candidates(settings_file)
        .into_iter()
        .find(|d| d.join(CERT_FILE).is_file())
}

/// Who may connect: fingerprints from Deskflow's trust files (re-read on
/// every check, so pairing done later in Deskflow works without a restart)
/// plus `[xfer].trusted`.
#[derive(Debug, Clone)]
pub struct Trust {
    pub dir: PathBuf,
    pub extra: Vec<String>,
}

impl Trust {
    pub fn new(dir: PathBuf, extra: &[String]) -> Self {
        let extra = extra
            .iter()
            .filter_map(|f| {
                let n = normalize(f);
                if n.is_none() {
                    log::warn!("xfer: ignoring invalid fingerprint in [xfer].trusted: {f}");
                }
                n
            })
            .collect();
        Self { dir, extra }
    }

    pub fn fingerprints(&self) -> Vec<String> {
        let mut v = self.extra.clone();
        for f in TRUST_FILES {
            if let Ok(text) = fs::read_to_string(self.dir.join(f)) {
                v.extend(parse_trust_file(&text));
            }
        }
        v
    }

    pub fn allows(&self, cert_der: &[u8]) -> bool {
        let fp = fingerprint(cert_der);
        let ok = self.fingerprints().contains(&fp);
        if !ok {
            log::warn!(
                "xfer: rejected peer with untrusted certificate {}",
                pretty(&fp)
            );
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn normalizes_deskflow_formats() {
        assert_eq!(normalize(&format!("v2:sha256:{FP}")).as_deref(), Some(FP));
        assert_eq!(normalize(&pretty(FP)).as_deref(), Some(FP));
        assert_eq!(normalize(&FP.to_uppercase()).as_deref(), Some(FP));
        // legacy SHA-1 line
        assert_eq!(
            normalize("AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01"),
            None
        );
        assert_eq!(normalize(""), None);
    }

    #[test]
    fn trust_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("trusted-clients"),
            format!("v2:sha256:{FP}\n\ngarbage\n"),
        )
        .unwrap();
        let t = Trust::new(tmp.path().into(), &["nope".into()]);
        assert_eq!(t.fingerprints(), vec![FP.to_string()]);
        assert!(!t.allows(b"some other cert"));
    }

    #[test]
    fn sha256_fingerprint() {
        assert_eq!(
            fingerprint(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn certificate_from_settings() {
        let s = "[core]\ncomputerName=x\n[security]\ncertificate=C:/Deskflow/tls/deskflow.pem\n";
        assert_eq!(
            security_certificate(s).as_deref(),
            Some("C:/Deskflow/tls/deskflow.pem")
        );
        assert_eq!(security_certificate("[core]\ncertificate=x\n"), None);
    }
}
