//! TLS identity for the hub's secure transports (SPEC §9.3 WSS, §9.6
//! WebTransport).
//!
//! WebTransport's browser `serverCertificateHashes` path (§9.6) constrains a
//! self-signed cert to **ECDSA P-256** with validity **≤ 14 days**, and needs
//! the **SHA-256 of the DER cert** published so the browser can pin it. This
//! module generates exactly that, or loads an operator-supplied cert/key from
//! PEM. The same identity serves WSS.

use std::net::IpAddr;

use rcgen::string::Ia5String;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};

/// The browser hash-pinning ceiling for self-signed WebTransport certs (§9.6).
pub const MAX_SELF_SIGNED_DAYS: i64 = 14;

/// A usable TLS identity plus the SHA-256 of its DER cert (for
/// `serverCertificateHashes`).
#[derive(Clone)]
pub struct HubCert {
    /// DER-encoded X.509 certificate.
    pub cert_der: Vec<u8>,
    /// PEM certificate (for tooling / operators).
    pub cert_pem: String,
    /// PKCS#8 DER private key.
    pub key_der: Vec<u8>,
    /// PKCS#8 PEM private key.
    pub key_pem: String,
    /// SHA-256 of `cert_der` — the `serverCertificateHashes` value (§9.6).
    pub sha256: [u8; 32],
}

impl HubCert {
    /// SHA-256 as lowercase colon-separated hex (e.g. `e3:4e:...`).
    pub fn sha256_hex(&self) -> String {
        let mut s = String::with_capacity(32 * 3);
        for (i, b) in self.sha256.iter().enumerate() {
            if i > 0 {
                s.push(':');
            }
            s.push_str(&format!("{b:02x}"));
        }
        s
    }
}

/// Errors from identity construction.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    #[error("self-signed validity must be <= {MAX_SELF_SIGNED_DAYS} days (browser serverCertificateHashes limit), got {0}")]
    ValidityTooLong(i64),
    #[error("validity days must be positive, got {0}")]
    ValidityNonPositive(i64),
    #[error("certificate generation failed: {0}")]
    Rcgen(#[from] rcgen::Error),
    #[error("could not parse PEM: {0}")]
    Parse(&'static str),
}

/// Generate a self-signed **ECDSA P-256** identity valid for `valid_days`
/// (≤ 14, §9.6), covering the given DNS names and IP-address SANs.
///
/// `sans` entries that parse as an IP become IP SANs; the rest become DNS SANs.
pub fn self_signed(sans: &[&str], valid_days: i64) -> Result<HubCert, TlsError> {
    if valid_days <= 0 {
        return Err(TlsError::ValidityNonPositive(valid_days));
    }
    if valid_days > MAX_SELF_SIGNED_DAYS {
        return Err(TlsError::ValidityTooLong(valid_days));
    }

    // Explicit ECDSA P-256 (also rcgen's default, but be unambiguous).
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;

    let mut params = CertificateParams::default();
    params.subject_alt_names = sans
        .iter()
        .map(|s| match s.parse::<IpAddr>() {
            Ok(ip) => Ok(SanType::IpAddress(ip)),
            Err(_) => Ia5String::try_from(*s).map(SanType::DnsName),
        })
        .collect::<Result<Vec<_>, _>>()?;

    // The browser serverCertificateHashes rule caps the *total* validity span at
    // 14 days (1,209,600 s). Anchor not_after to not_before so the skew slack at
    // the front never pushes the span over the ceiling.
    let now = OffsetDateTime::now_utc();
    let not_before = now - Duration::hours(1); // clock-skew slack
    params.not_before = not_before;
    params.not_after = not_before + Duration::days(valid_days) - Duration::minutes(1);

    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "telesthete-hub");
    params.distinguished_name = dn;

    let cert = params.self_signed(&key_pair)?;
    let cert_der = cert.der().to_vec();
    let sha256: [u8; 32] = Sha256::digest(&cert_der).into();

    Ok(HubCert {
        cert_pem: cert.pem(),
        key_der: key_pair.serialize_der(),
        key_pem: key_pair.serialize_pem(),
        cert_der,
        sha256,
    })
}

/// Build an identity from operator-supplied PEM strings (a real CA cert). No
/// validity ceiling — a CA-issued cert is not hash-pinned.
pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<HubCert, TlsError> {
    // Parse the cert PEM to DER via rcgen's pem support path is indirect; parse
    // with rustls-pki-types instead is added when the WSS layer lands. For now
    // the operator path re-encodes through rcgen is unnecessary — WSS/WT accept
    // PEM directly — so we keep the DER for the hash by decoding the PEM here.
    let cert_der = pem_to_der(cert_pem, "CERTIFICATE")
        .ok_or(TlsError::Parse("no CERTIFICATE block"))?;
    let key_der = pem_to_der(key_pem, "PRIVATE KEY")
        .or_else(|| pem_to_der(key_pem, "EC PRIVATE KEY"))
        .or_else(|| pem_to_der(key_pem, "RSA PRIVATE KEY"))
        .ok_or(TlsError::Parse("no PRIVATE KEY block"))?;
    let sha256: [u8; 32] = Sha256::digest(&cert_der).into();
    Ok(HubCert {
        cert_pem: cert_pem.to_string(),
        cert_der,
        key_pem: key_pem.to_string(),
        key_der,
        sha256,
    })
}

/// Minimal PEM base64 body decoder for a given label. Avoids pulling a PEM
/// crate for the one place we need DER out of operator PEM.
fn pem_to_der(pem: &str, label: &str) -> Option<Vec<u8>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = pem.find(&begin)? + begin.len();
    let stop = pem[start..].find(&end)? + start;
    let b64: String = pem[start..stop].split_whitespace().collect();
    base64_decode(&b64)
}

/// Tiny standard-base64 decoder (no external dep for this one use).
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let v = val(c)? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The ANSI X9.62 prime256v1 (P-256) curve OID, DER-encoded:
    // 1.2.840.10045.3.1.7 -> 06 08 2A 86 48 CE 3D 03 01 07
    const P256_OID: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];

    fn contains_subslice(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn self_signed_is_p256() {
        // G7 — the generated cert must be ECDSA P-256.
        let c = self_signed(&["localhost", "127.0.0.1"], 14).unwrap();
        assert!(
            contains_subslice(&c.cert_der, P256_OID),
            "cert must carry the P-256 curve OID"
        );
    }

    #[test]
    fn self_signed_rejects_long_validity() {
        // G7 — >14 days is refused (browser hash-pin ceiling).
        assert!(matches!(
            self_signed(&["localhost"], 15),
            Err(TlsError::ValidityTooLong(15))
        ));
        assert!(matches!(
            self_signed(&["localhost"], 0),
            Err(TlsError::ValidityNonPositive(0))
        ));
    }

    #[test]
    fn exposes_cert_hash() {
        // G7 — SHA-256 of the DER cert is available and internally consistent.
        let c = self_signed(&["localhost"], 7).unwrap();
        assert_eq!(c.sha256.len(), 32);
        let recomputed: [u8; 32] = Sha256::digest(&c.cert_der).into();
        assert_eq!(c.sha256, recomputed);
        assert_eq!(c.sha256_hex().len(), 32 * 3 - 1); // "xx:" * 32 minus last colon
    }

    #[test]
    fn each_call_is_fresh() {
        let a = self_signed(&["localhost"], 7).unwrap();
        let b = self_signed(&["localhost"], 7).unwrap();
        assert_ne!(a.sha256, b.sha256, "fresh key+cert each call");
    }

    #[test]
    fn pem_round_trips_to_der_for_hash() {
        // The operator PEM path recovers the same DER (hence hash) we emit.
        let c = self_signed(&["localhost"], 7).unwrap();
        let loaded = from_pem(&c.cert_pem, &c.key_pem).unwrap();
        assert_eq!(loaded.cert_der, c.cert_der);
        assert_eq!(loaded.sha256, c.sha256);
    }
}
