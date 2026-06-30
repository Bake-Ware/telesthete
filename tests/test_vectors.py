"""Cross-impl conformance: the Python reference must reproduce tests/vectors.json
byte-for-byte. The Rust crate checks the same file
(crypto::tests::conformance_vectors_match_python). If the two ever diverge,
one of these fails — which is exactly the regression that shipped silently
before v1.2 (Python XSalsa20 vs spec/Rust ChaCha20-Poly1305).

Run: python -m pytest tests/test_vectors.py   (or: python tests/test_vectors.py)
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from telesthete.protocol import crypto

VECTORS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vectors.json")


def load():
    with open(VECTORS) as f:
        return json.load(f)


def test_band_id():
    v = load()
    assert crypto.derive_band_id(v["psk"]).hex() == v["band_id_hex"]


def test_baseline_key():
    v = load()
    cid = "chacha20-poly1305"
    assert crypto.derive_encryption_key(v["psk"], cid).hex() == v["keys"][cid]


def test_aead_vectors():
    v = load()
    for case in v["aead"]:
        bc = crypto.BandCrypto(v["psk"], case["cipher"])
        aad = bytes.fromhex(case["aad_hex"])
        assert crypto.build_aad(case["channel_type"], case["channel_id"]) == aad
        pt = bytes.fromhex(case["plaintext_hex"])
        ct = bc.encrypt(case["seq"], pt, aad)
        assert ct.hex() == case["ciphertext_hex"], f"ciphertext diverged at seq={case['seq']}"
        assert bc.decrypt(case["seq"], ct, aad) == pt, f"roundtrip seq={case['seq']}"


if __name__ == "__main__":
    test_band_id()
    test_baseline_key()
    test_aead_vectors()
    print("conformance vectors: PASS (Python matches tests/vectors.json)")
