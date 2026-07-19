"""
Cryptographic primitives for Telesthete. See SPEC.md §3.

PSK-based key derivation and AEAD. The wire format is the contract, so this
MUST match the Rust reference byte-for-byte (see tests/vectors.json):

    band_id = SHA256(PSK)[:16]                         # cleartext routing
    key     = HKDF-SHA256(salt="telesthete-v1",        # 32-byte per-cipher key
                          ikm=PSK,
                          info="encryption-" + cipher_id)

AEAD suites (§3.2), drop-in for one another — same 12-byte nonce, 3-byte AAD,
16-byte tag:
    chacha20-poly1305   ChaCha20-Poly1305 (IETF, RFC 8439)   MANDATORY baseline
    aes256-gcm          AES-256-GCM                          OPTIONAL

Nonce = 4 zero bytes || 8-byte big-endian sequence (96 bits).
AAD = [channel_type, channel_id_hi, channel_id_lo] passed as *real* AEAD
associated data — never prepended to the plaintext.
"""

import hashlib
import hmac
import logging
from typing import Tuple

_log = logging.getLogger(__name__)

try:
    from nacl import bindings as _na
except ImportError:
    raise ImportError("PyNaCl required: pip install pynacl")


BASELINE_CIPHER = "chacha20-poly1305"
NONCE_LEN = 12
TAG_LEN = 16
KEY_LEN = 32

SUPPORTED_CIPHERS = ("chacha20-poly1305", "aes256-gcm")


def derive_band_id(psk: str) -> bytes:
    """16-byte cleartext routing id: SHA256(PSK)[:16]. SPEC §3.1."""
    return hashlib.sha256(psk.encode("utf-8")).digest()[:16]


def derive_encryption_key(psk: str, cipher_id: str = BASELINE_CIPHER) -> bytes:
    """32-byte per-cipher AEAD key via HKDF-SHA256. SPEC §3.1.

    info = "encryption-" + cipher_id, so each suite gets a distinct key.
    """
    salt = b"telesthete-v1"
    info = b"encryption-" + cipher_id.encode("utf-8")
    # HKDF-Extract
    prk = hmac.new(salt, psk.encode("utf-8"), hashlib.sha256).digest()
    # HKDF-Expand, single 32-byte block
    okm = hmac.new(prk, info + b"\x01", hashlib.sha256).digest()
    return okm  # 32 bytes


def select_cipher(initiator_prefs, responder_supported) -> str:
    """Pick the negotiated AEAD suite per SPEC §3.5: the first entry in the
    *initiator's* ordered preference list that the *responder* also supports.

    Per §3.5 rule 2 / §12.5, both lists MUST include the mandatory baseline
    (``chacha20-poly1305``). A list that omits it is non-conformant; we normalize
    by treating the baseline as always supported (it is mandatory) so selection
    never fails, and log the violation for the operator.
    """
    if BASELINE_CIPHER not in responder_supported:
        _log.warning("cipher list omits the mandatory baseline (SPEC §12.5); "
                     "treating baseline as supported")
    supported = set(responder_supported)
    supported.add(BASELINE_CIPHER)  # baseline is mandatory (§3.5 rule 2)
    for cid in initiator_prefs:
        if cid in supported:
            return cid
    return BASELINE_CIPHER


def build_aad(channel_type: int, channel_id: int) -> bytes:
    """3-byte AAD: [channel_type, channel_id_hi, channel_id_lo]. SPEC §3.2."""
    return bytes([channel_type & 0xFF,
                  (channel_id >> 8) & 0xFF,
                  channel_id & 0xFF])


def nonce_from_seq(sequence: int) -> bytes:
    """12-byte nonce: 4 zero bytes || 8-byte BE sequence. SPEC §3.2."""
    return b"\x00\x00\x00\x00" + sequence.to_bytes(8, "big")


class BandCrypto:
    """AEAD encrypt/decrypt for a Band under one negotiated suite.

    Args:
        psk: pre-shared key string.
        cipher_id: AEAD suite (default: the mandatory baseline).
    """

    def __init__(self, psk: str, cipher_id: str = BASELINE_CIPHER):
        if cipher_id not in SUPPORTED_CIPHERS:
            raise ValueError(f"unsupported cipher_id: {cipher_id!r}")
        self.cipher_id = cipher_id
        self.band_id = derive_band_id(psk)
        self.key = derive_encryption_key(psk, cipher_id)
        self._aesgcm = None
        if cipher_id == "aes256-gcm":
            # Optional suite — pulled in lazily so the baseline has no extra dep.
            try:
                from cryptography.hazmat.primitives.ciphers.aead import AESGCM
            except ImportError as e:
                raise ImportError(
                    "aes256-gcm requires the 'cryptography' package"
                ) from e
            self._aesgcm = AESGCM(self.key)

    def encrypt(self, sequence: int, data: bytes,
                associated_data: bytes = b"") -> bytes:
        """Encrypt -> ciphertext||tag. `associated_data` is real AEAD AAD."""
        nonce = nonce_from_seq(sequence)
        if self.cipher_id == "chacha20-poly1305":
            return _na.crypto_aead_chacha20poly1305_ietf_encrypt(
                data, associated_data, nonce, self.key)
        return self._aesgcm.encrypt(nonce, data, associated_data or None)

    def decrypt(self, sequence: int, ciphertext: bytes,
                associated_data: bytes = b"") -> bytes:
        """Decrypt + authenticate. Raises on auth failure / wrong nonce/AAD."""
        nonce = nonce_from_seq(sequence)
        if self.cipher_id == "chacha20-poly1305":
            return _na.crypto_aead_chacha20poly1305_ietf_decrypt(
                ciphertext, associated_data, nonce, self.key)
        return self._aesgcm.decrypt(nonce, ciphertext, associated_data or None)
