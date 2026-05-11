//! PSK-derived band id + key + AEAD encrypt/decrypt. See SPEC.md §3.
//!
//! ```text
//! band_id = SHA256(PSK)[:16]
//! key     = HKDF-SHA256(salt = "telesthete-v1", ikm = PSK, info = "encryption")[:32]
//! ```
//!
//! AEAD: XChaCha20-Poly1305. Nonce = 24 bytes, the low 8 of which are the
//! big-endian sequence number; the upper 16 bytes are zero. AAD = 3 bytes:
//! `[channel_type, channel_id_hi, channel_id_lo]`.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key as ChaChaKey, XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const BAND_ID_LEN: usize = 16;
pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;

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

/// `key = HKDF-SHA256(salt = "telesthete-v1", ikm = psk, info = "encryption")[:32]`.
pub fn derive_key(psk: &[u8]) -> Key {
    let hk = Hkdf::<Sha256>::new(Some(b"telesthete-v1"), psk);
    let mut okm = [0u8; KEY_LEN];
    hk.expand(b"encryption", &mut okm)
        .expect("HKDF expand of 32 bytes from SHA-256 cannot fail");
    okm
}

/// Build the 24-byte XChaCha20 nonce: 16 zero bytes, then sequence as 8 BE bytes.
fn nonce_from_seq(seq: u64) -> XNonce {
    let mut n = [0u8; NONCE_LEN];
    n[16..].copy_from_slice(&seq.to_be_bytes());
    XNonce::clone_from_slice(&n)
}

/// 3-byte AAD: `[channel_type, channel_id_hi, channel_id_lo]`. SPEC §3.2.
pub fn build_aad(channel_type: u8, channel_id: u16) -> [u8; 3] {
    [channel_type, (channel_id >> 8) as u8, channel_id as u8]
}

/// Encrypt `plaintext` with the given key + sequence + AAD.
/// Returns ciphertext + 16-byte Poly1305 tag concatenated.
pub fn encrypt(
    key: &Key,
    seq: u64,
    aad: &[u8; 3],
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(ChaChaKey::from_slice(key));
    let nonce = nonce_from_seq(seq);
    cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Encrypt)
}

/// Decrypt + authenticate. `ciphertext` is body || 16-byte tag.
pub fn decrypt(
    key: &Key,
    seq: u64,
    aad: &[u8; 3],
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    let cipher = XChaCha20Poly1305::new(ChaChaKey::from_slice(key));
    let nonce = nonce_from_seq(seq);
    cipher
        .decrypt(
            &nonce,
            Payload {
                msg: ciphertext,
                aad,
            },
        )
        .map_err(|_| CryptoError::Decrypt)
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
}
