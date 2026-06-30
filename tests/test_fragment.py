"""Channel fragmentation (SPEC §6.6): round-trip + cross-impl vectors.
The Rust crate checks the same `fragment` section of tests/vectors.json
(wire::fragment::tests::conformance_vectors_match_python).
Run: python tests/test_fragment.py
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from telesthete.protocol import fragment

VECTORS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vectors.json")


def _reassemble(chunks):
    r = fragment.Reassembler()
    out = None
    for c in chunks:
        o = r.feed(c)
        if o is not None:
            out = o
    return out


def test_round_trips():
    fid = b"\x07" * 16
    for payload, cs in [(b"hello", fragment.MAX_CHUNK_PAYLOAD),
                        (b"", fragment.MAX_CHUNK_PAYLOAD),
                        (bytes((i % 256) for i in range(fragment.MAX_CHUNK_PAYLOAD * 2 + 5)),
                         fragment.MAX_CHUNK_PAYLOAD),
                        (b"\x01\x02\x03\x04", 2)]:
        chunks = fragment.fragment(payload, cs, fragment_id=fid)
        assert _reassemble(chunks) == payload


def test_rejects_garbage_and_dupes():
    r = fragment.Reassembler()
    assert r.feed(b"short") is None
    chunks = fragment.fragment(b"\x01\x02\x03\x04", 2, fragment_id=b"\x01" * 16)
    assert r.feed(chunks[0]) is None
    assert r.feed(chunks[0]) is None  # duplicate
    assert r.feed(chunks[1]) == b"\x01\x02\x03\x04"


def test_conformance_vectors():
    with open(VECTORS) as f:
        v = json.load(f)
    for case in v.get("fragment", []):
        fid = bytes.fromhex(case["fragment_id_hex"])
        payload = bytes.fromhex(case["payload_hex"])
        chunks = fragment.fragment(payload, case["chunk_size"], fragment_id=fid)
        assert [c.hex() for c in chunks] == case["chunks_hex"], "fragment bytes diverged"
        assert _reassemble(chunks) == payload


if __name__ == "__main__":
    test_round_trips()
    test_rejects_garbage_and_dupes()
    test_conformance_vectors()
    print("fragmentation: PASS (round-trip + Python matches tests/vectors.json)")
