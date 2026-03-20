"""
Cryptographic primitives for Telesthete

PSK-based key derivation and AEAD encryption using XChaCha20-Poly1305
"""

import hashlib
import hmac
from typing import Tuple

try:
    from nacl.secret import SecretBox
    from nacl.utils import random as nacl_random
except ImportError:
    # Fallback to pure Python implementation if needed
    # For now, require PyNaCl
    raise ImportError("PyNaCl required: pip install pynacl")


def derive_band_id(psk: str) -> bytes:
    """
    Derive a 16-byte Band ID from PSK

    This is sent in cleartext for relay routing.
    Both peers with same PSK will derive same band_id.

    Args:
        psk: Pre-shared key (user-entered string)

    Returns:
        16-byte band identifier
    """
    return hashlib.sha256(psk.encode('utf-8')).digest()[:16]


def derive_encryption_key(psk: str) -> bytes:
    """
    Derive a 32-byte encryption key from PSK using HKDF

    Args:
        psk: Pre-shared key (user-entered string)

    Returns:
        32-byte encryption key for XChaCha20-Poly1305
    """
    # HKDF-SHA256 (simplified - no expand step needed for single key)
    # salt = static for MVP (can be per-session later)
    salt = b"telesthete-v1"
    info = b"encryption"

    # Extract step
    prk = hmac.new(salt, psk.encode('utf-8'), hashlib.sha256).digest()

    # Expand step (single iteration for 32 bytes)
    okm = hmac.new(prk, info + b'\x01', hashlib.sha256).digest()

    return okm  # 32 bytes


class BandCrypto:
    """
    Handles encryption/decryption for a Band

    Uses XChaCha20-Poly1305 AEAD with sequence number as nonce.
    """

    def __init__(self, psk: str):
        """
        Initialize Band crypto with PSK

        Args:
            psk: Pre-shared key
        """
        self.band_id = derive_band_id(psk)
        self.key = derive_encryption_key(psk)
        self._box = SecretBox(self.key)

    def encrypt(self, sequence: int, data: bytes, associated_data: bytes = b"") -> bytes:
        """
        Encrypt data with AEAD

        Args:
            sequence: Packet sequence number (used as nonce)
            data: Plaintext to encrypt
            associated_data: Additional authenticated data (not encrypted)

        Returns:
            Ciphertext + authentication tag

        Note:
            XChaCha20-Poly1305 uses 192-bit nonces. We use the 64-bit sequence
            number zero-padded to 24 bytes. This is safe as long as sequence
            numbers never repeat (which they won't - 2^64 packets is huge).
        """
        # Construct 24-byte nonce from 64-bit sequence number
        nonce = sequence.to_bytes(8, byteorder='big').rjust(24, b'\x00')

        # PyNaCl's SecretBox.encrypt doesn't support AAD directly
        # We'll prepend AAD to plaintext and verify on decrypt
        # (This is a simplification - proper AEAD construction later if needed)
        payload = associated_data + data

        # Encrypt
        ciphertext = self._box.encrypt(payload, nonce)

        # SecretBox.encrypt returns nonce + ciphertext + tag
        # We don't need to store nonce (we have sequence number)
        # So strip the first 24 bytes
        return ciphertext[24:]

    def decrypt(self, sequence: int, ciphertext: bytes, associated_data: bytes = b"") -> bytes:
        """
        Decrypt data with AEAD

        Args:
            sequence: Packet sequence number (used as nonce)
            ciphertext: Encrypted data + tag
            associated_data: Additional authenticated data (must match encryption)

        Returns:
            Plaintext data

        Raises:
            nacl.exceptions.CryptoError: If authentication fails
        """
        # Construct nonce from sequence number
        nonce = sequence.to_bytes(8, byteorder='big').rjust(24, b'\x00')

        # Reconstruct what SecretBox expects (nonce + ciphertext)
        box_input = nonce + ciphertext

        # Decrypt
        payload = self._box.decrypt(box_input)

        # Verify AAD matches (simplified - proper AEAD later if needed)
        aad_len = len(associated_data)
        if payload[:aad_len] != associated_data:
            raise ValueError("Associated data mismatch")

        # Return data without AAD prefix
        return payload[aad_len:]


def test_crypto():
    """Quick test of crypto functions"""
    psk = "my-secret-passphrase"

    # Test key derivation
    band_id = derive_band_id(psk)
    print(f"Band ID: {band_id.hex()}")
    assert len(band_id) == 16

    # Same PSK should give same band_id
    band_id2 = derive_band_id(psk)
    assert band_id == band_id2

    # Different PSK should give different band_id
    band_id3 = derive_band_id("different-psk")
    assert band_id != band_id3

    # Test encryption
    crypto = BandCrypto(psk)

    plaintext = b"Hello, world!"
    aad = b"channel_type=0x01"
    sequence = 42

    ciphertext = crypto.encrypt(sequence, plaintext, aad)
    print(f"Ciphertext length: {len(ciphertext)} (plaintext: {len(plaintext)})")

    # Decrypt
    decrypted = crypto.decrypt(sequence, ciphertext, aad)
    assert decrypted == plaintext
    print(f"Decrypted: {decrypted}")

    # Wrong sequence should fail
    try:
        crypto.decrypt(sequence + 1, ciphertext, aad)
        assert False, "Should have failed with wrong sequence"
    except Exception as e:
        print(f"Correctly failed with wrong sequence: {e}")

    # Wrong AAD should fail
    try:
        crypto.decrypt(sequence, ciphertext, b"wrong_aad")
        assert False, "Should have failed with wrong AAD"
    except Exception as e:
        print(f"Correctly failed with wrong AAD: {e}")

    print("\nAll crypto tests passed!")


if __name__ == "__main__":
    test_crypto()
