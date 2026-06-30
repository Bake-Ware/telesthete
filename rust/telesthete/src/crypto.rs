//! PSK-derived band id + key + AEAD encrypt/decrypt. See SPEC.md §3.
//!
//! ```text
//! band_id = SHA256(PSK)[:16]
//! key     = HKDF-SHA256(salt = "telesthete-v1", ikm = PSK,
//!                       info = "encryption-" + cipher_id)[:32]
//! ```
//!
//! AEAD baseline: ChaCha20-Poly1305 (IETF, RFC 8439). Nonce = 12 bytes, the
//! low 8 of which are the big-endian sequence number; the upper 4 are zero.
//! AAD = 3 bytes: `[channel_type, channel_id_hi, channel_id_lo]`, passed as
//! real AEAD associated data. Must stay byte-identical to the Python
//! reference — see tests/vectors.json.

use aes_gcm::Aes256Gcm;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key as ChaChaKey, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const BAND_ID_LEN: usize = 16;
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// The mandatory AEAD suite every conformant peer implements. SPEC §3.2.
pub const BASELINE_CIPHER: &str = "chacha20-poly1305";

/// 16-byte cleartext routing identifier. Same PSK on both peers => same id.
pub type BandId = [u8; BAND_ID_LEN];

/// 32-byte AEAD key derived from PSK via HKDF.
pub type Key = [u8; KEY_LEN];

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("encrypt failed (XChaCha20-Poly1305)")]
    Encrypt,
    #[error("decrypt failed (auth tag mismatch or ciphertext truncated)")]
    Decrypt,
}

/// `band_id = SHA256(psk)[:16]`. Cleartext on the wire — used by relays for
/// routing without ever seeing the key.
pub fn derive_band_id(psk: &[u8]) -> BandId {
    let mut hasher = Sha256::new();
    hasher.update(psk);
    let out = hasher.finalize();
    let mut id = [0u8; BAND_ID_LEN];
    id.copy_from_slice(&out[..BAND_ID_LEN]);
    id
}

/// `key = HKDF-SHA256(salt = "telesthete-v1", ikm = psk,
/// info = "encryption-" + cipher_id)[:32]`. SPEC §3.1. Binding the key to the
/// suite gives ChaCha and AES distinct keys on the same band.
pub fn derive_key_for(psk: &[u8], cipher_id: &str) -> Key {
    let hk = Hkdf::<Sha256>::new(Some(b"telesthete-v1"), psk);
    let mut info = b"encryption-".to_vec();
    info.extend_from_slice(cipher_id.as_bytes());
    let mut okm = [0u8; KEY_LEN];
    hk.expand(&info, &mut okm)
        .expect("HKDF expand of 32 bytes from SHA-256 cannot fail");
    okm
}

/// Baseline (`chacha20-poly1305`) key. SPEC §3.1.
pub fn derive_key(psk: &[u8]) -> Key {
    derive_key_for(psk, BASELINE_CIPHER)
}

/// Build the 12-byte nonce: 4 zero bytes, then sequence as 8 BE bytes. SPEC §3.2.
fn nonce_bytes(seq: u64) -> [u8; NONCE_LEN] {
    let mut n = [0u8; NONCE_LEN];
    n[4..].copy_from_slice(&seq.to_be_bytes());
    n
}

/// AEAD suites (SPEC §3.2). `ChaCha20Poly1305` is the mandatory baseline;
/// `Aes256Gcm` is optional. Both share the 12-byte nonce, 3-byte AAD, 16-byte tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    ChaCha20Poly1305,
    Aes256Gcm,
}

impl Suite {
    /// Map a `cipher_id` to a suite (`None` for unknown ids).
    pub fn from_id(cipher_id: &str) -> Option<Suite> {
        match cipher_id {
            "chacha20-poly1305" => Some(Suite::ChaCha20Poly1305),
            "aes256-gcm" => Some(Suite::Aes256Gcm),
            _ => None,
        }
    }
    /// The `cipher_id` string used on the wire and in key derivation.
    pub fn id(self) -> &'static str {
        match self {
            Suite::ChaCha20Poly1305 => "chacha20-poly1305",
            Suite::Aes256Gcm => "aes256-gcm",
        }
    }
}

/// 3-byte AAD: `[channel_type, channel_id_hi, channel_id_lo]`. SPEC §3.2.
pub fn build_aad(channel_type: u8, channel_id: u16) -> [u8; 3] {
    [channel_type, (channel_id >> 8) as u8, channel_id as u8]
}

/// Pick the negotiated AEAD suite per SPEC §3.5: the first entry in the
/// *initiator's* ordered preference list that the *responder* also supports.
/// Falls back to the mandatory baseline (both MUST support it), so it never
/// fails. Mirrors `telesthete.protocol.crypto.select_cipher`.
pub fn select_cipher(initiator_prefs: &[String], responder_supported: &[String]) -> String {
    for cid in initiator_prefs {
        if responder_supported.iter().any(|s| s == cid) {
            return cid.clone();
        }
    }
    BASELINE_CIPHER.to_string()
}

/// Encrypt under an explicit suite. `key` MUST be derived with the matching
/// `cipher_id` (`derive_key_for`). Returns ciphertext || 16-byte tag.
pub fn encrypt_suite(
    suite: Suite,
    key: &Key,
    seq: u64,
    aad: &[u8; 3],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let n = nonce_bytes(seq);
    match suite {
        Suite::ChaCha20Poly1305 => ChaCha20Poly1305::new(ChaChaKey::from_slice(key))
            .encrypt(Nonce::from_slice(&n), Payload { msg: plaintext, aad })
            .map_err(|_| CryptoError::Encrypt),
        Suite::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::Encrypt)?;
            cipher
                .encrypt(aes_gcm::Nonce::from_slice(&n), Payload { msg: plaintext, aad })
                .map_err(|_| CryptoError::Encrypt)
        }
    }
}

/// Decrypt + authenticate under an explicit suite. `ciphertext` is body || tag.
pub fn decrypt_suite(
    suite: Suite,
    key: &Key,
    seq: u64,
    aad: &[u8; 3],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let n = nonce_bytes(seq);
    match suite {
        Suite::ChaCha20Poly1305 => ChaCha20Poly1305::new(ChaChaKey::from_slice(key))
            .decrypt(Nonce::from_slice(&n), Payload { msg: ciphertext, aad })
            .map_err(|_| CryptoError::Decrypt),
        Suite::Aes256Gcm => {
            let cipher = Aes256Gcm::new_from_slice(key).map_err(|_| CryptoError::Decrypt)?;
            cipher
                .decrypt(aes_gcm::Nonce::from_slice(&n), Payload { msg: ciphertext, aad })
                .map_err(|_| CryptoError::Decrypt)
        }
    }
}

/// Baseline encrypt (`chacha20-poly1305`). SPEC §3.2.
pub fn encrypt(key: &Key, seq: u64, aad: &[u8; 3], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    encrypt_suite(Suite::ChaCha20Poly1305, key, seq, aad, plaintext)
}

/// Baseline decrypt (`chacha20-poly1305`). SPEC §3.2.
pub fn decrypt(key: &Key, seq: u64, aad: &[u8; 3], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    decrypt_suite(Suite::ChaCha20Poly1305, key, seq, aad, ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_PSK: &[u8] = b"my-secret-key";

    #[test]
    fn band_id_deterministic() {
        let a = derive_band_id(TEST_PSK);
        let b = derive_band_id(TEST_PSK);
        assert_eq!(a, b);
        assert_ne!(a, derive_band_id(b"different-psk"));
    }

    #[test]
    fn key_deterministic_and_different_from_band_id() {
        let key = derive_key(TEST_PSK);
        let id = derive_band_id(TEST_PSK);
        assert_eq!(key, derive_key(TEST_PSK));
        assert_ne!(&key[..BAND_ID_LEN], &id[..]);
    }

    #[test]
    fn round_trip() {
        let key = derive_key(TEST_PSK);
        let aad = build_aad(0x01, 0xBEEF);
        let plaintext = b"hello world".as_slice();
        let ct = encrypt(&key, 1, &aad, plaintext).unwrap();
        assert_eq!(ct.len(), plaintext.len() + TAG_LEN);
        let pt = decrypt(&key, 1, &aad, &ct).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn wrong_seq_fails() {
        let key = derive_key(TEST_PSK);
        let aad = build_aad(0x01, 0);
        let ct = encrypt(&key, 1, &aad, b"abc").unwrap();
        assert!(decrypt(&key, 2, &aad, &ct).is_err());
    }

    #[test]
    fn wrong_aad_fails() {
        let key = derive_key(TEST_PSK);
        let aad = build_aad(0x01, 0);
        let ct = encrypt(&key, 1, &aad, b"abc").unwrap();
        let bad_aad = build_aad(0x02, 0);
        assert!(decrypt(&key, 1, &bad_aad, &ct).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        let key_a = derive_key(b"psk-a");
        let key_b = derive_key(b"psk-b");
        let aad = build_aad(0x01, 0);
        let ct = encrypt(&key_a, 1, &aad, b"abc").unwrap();
        assert!(decrypt(&key_b, 1, &aad, &ct).is_err());
    }

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The whole point of the project: the wire is the spec, so Rust and the
    /// Python reference MUST agree byte-for-byte. These vectors are generated
    /// by the Python impl; if this test fails the two have diverged.
    #[test]
    fn conformance_vectors_match_python() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors.json");
        let raw = std::fs::read_to_string(path).expect("read tests/vectors.json");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();

        let psk = v["psk"].as_str().unwrap().as_bytes();
        assert_eq!(
            to_hex(&derive_band_id(psk)),
            v["band_id_hex"].as_str().unwrap(),
            "band_id diverged"
        );

        // Per-cipher key derivation (both suites).
        for (cipher_id, key_hex) in v["keys"].as_object().unwrap() {
            Suite::from_id(cipher_id).expect("known cipher in vectors");
            let key = derive_key_for(psk, cipher_id);
            assert_eq!(to_hex(&key), key_hex.as_str().unwrap(), "key diverged: {cipher_id}");
        }

        for case in v["aead"].as_array().unwrap() {
            let cipher_id = case["cipher"].as_str().unwrap();
            let suite = Suite::from_id(cipher_id).expect("known cipher");
            let key = derive_key_for(psk, cipher_id);
            let seq = case["seq"].as_u64().unwrap();
            let ct = case["channel_type"].as_u64().unwrap() as u8;
            let cid = case["channel_id"].as_u64().unwrap() as u16;
            let aad = build_aad(ct, cid);
            assert_eq!(to_hex(&aad), case["aad_hex"].as_str().unwrap());
            let pt = from_hex(case["plaintext_hex"].as_str().unwrap());
            let want = case["ciphertext_hex"].as_str().unwrap();
            let got = encrypt_suite(suite, &key, seq, &aad, &pt).unwrap();
            assert_eq!(to_hex(&got), want, "ciphertext diverged: {cipher_id} seq={seq}");
            assert_eq!(
                decrypt_suite(suite, &key, seq, &aad, &got).unwrap(),
                pt,
                "roundtrip {cipher_id} seq={seq}"
            );
        }
    }
}
