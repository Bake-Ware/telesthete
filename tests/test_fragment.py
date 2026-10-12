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


# -- hardening (SPEC §6.6 memory bounds) -------------------------------------

def _chunk(fid: bytes, seq: int, total: int, data: bytes) -> bytes:
    return fragment.pack_chunk(fid, seq, total, data)


def test_default_constructor_is_compatible():
    # Existing callers construct with no args or the two historical positionals.
    fragment.Reassembler()
    fragment.Reassembler(30.0, 256)
    fragment.Reassembler(timeout=5.0, buffer_limit=8)


def test_rejects_oversize_chunk_in_multi_chunk_message():
    r = fragment.Reassembler()
    fid = b"\x02" * 16
    big = b"x" * (fragment.MAX_CHUNK_PAYLOAD + 1)
    assert r.feed(_chunk(fid, 0, 2, big)) is None
    assert r.pending == 0 and r.buffered_bytes == 0, "oversize chunk must not be buffered"
    # Exactly MAX_CHUNK_PAYLOAD is fine.
    ok = b"y" * fragment.MAX_CHUNK_PAYLOAD
    assert r.feed(_chunk(fid, 0, 2, ok)) is None
    assert r.feed(_chunk(fid, 1, 2, b"z")) == ok + b"z"


def test_single_chunk_message_is_exempt_from_chunk_cap():
    # The ESP32 firmware always sends total=1 with the whole payload; a long
    # info.ping echo exceeds MAX_CHUNK_PAYLOAD. Never buffered, so harmless.
    r = fragment.Reassembler(max_buffered_bytes=10)
    big = b"q" * 1400
    assert r.feed(_chunk(b"\x03" * 16, 0, 1, big)) == big
    assert r.pending == 0 and r.buffered_bytes == 0


def test_max_chunk_payload_is_configurable():
    r = fragment.Reassembler(max_chunk_payload=4)
    fid = b"\x04" * 16
    assert r.feed(_chunk(fid, 0, 2, b"12345")) is None
    assert r.feed(_chunk(fid, 0, 2, b"1234")) is None
    assert r.feed(_chunk(fid, 1, 2, b"5")) == b"12345"


def test_rejects_declared_total_above_max_fragments():
    assert fragment.DEFAULT_MAX_FRAGMENTS == 16384
    r = fragment.Reassembler()
    assert r.feed(_chunk(b"\x05" * 16, 0, fragment.DEFAULT_MAX_FRAGMENTS + 1, b"a")) is None
    assert r.pending == 0
    r = fragment.Reassembler(max_fragments=3)
    fid = b"\x06" * 16
    assert r.feed(_chunk(fid, 0, 4, b"a")) is None and r.pending == 0
    chunks = fragment.fragment(b"abc", 1, fragment_id=fid)  # exactly 3
    assert _reassemble_with(r, chunks) == b"abc"


def _reassemble_with(r, chunks):
    out = None
    for c in chunks:
        o = r.feed(c)
        if o is not None:
            out = o
    return out


def test_byte_budget_evicts_oldest_partial():
    r = fragment.Reassembler(max_buffered_bytes=10)
    a, b, c = b"\x0a" * 16, b"\x0b" * 16, b"\x0c" * 16
    assert r.feed(_chunk(a, 0, 2, b"aaaa")) is None
    assert r.feed(_chunk(b, 0, 2, b"bbbb")) is None
    assert r.buffered_bytes == 8
    # 8 + 4 > 10: the oldest other partial (a) goes, c is buffered.
    assert r.feed(_chunk(c, 0, 2, b"cccc")) is None
    assert r.pending == 2 and r.buffered_bytes == 8
    assert r.feed(_chunk(a, 1, 2, b"A")) is None, "a was evicted; its tail can't complete it"
    assert r.feed(_chunk(b, 1, 2, b"B")) == b"bbbbB"
    assert r.feed(_chunk(c, 1, 2, b"C")) == b"ccccC"
    # Only a's orphan tail (1 B) remains buffered, until it times out.
    assert r.pending == 1 and r.buffered_bytes == 1


def test_byte_budget_drops_message_that_cannot_fit():
    r = fragment.Reassembler(max_buffered_bytes=6)
    fid = b"\x0d" * 16
    assert r.feed(_chunk(fid, 0, 3, b"1234")) is None
    # Its own next chunk would need 8 bytes with nothing else to evict.
    assert r.feed(_chunk(fid, 1, 3, b"5678")) is None
    assert r.pending == 0 and r.buffered_bytes == 0
    # A single chunk bigger than the whole budget is refused without evicting.
    keep = b"\x0e" * 16
    assert r.feed(_chunk(keep, 0, 2, b"kk")) is None
    assert r.feed(_chunk(b"\x0f" * 16, 0, 2, b"x" * 7)) is None
    assert r.pending == 1 and r.buffered_bytes == 2


def test_byte_accounting_survives_reset_gc_and_completion():
    r = fragment.Reassembler(timeout=0.0, max_buffered_bytes=100)
    fid = b"\x10" * 16
    r.feed(_chunk(fid, 0, 3, b"abc"))
    # total mismatch resets the buffer (and its bytes)
    r.feed(_chunk(fid, 0, 2, b"de"))
    assert r.buffered_bytes == 2
    import time as _t
    _t.sleep(0.01)
    r.feed(_chunk(b"\x11" * 16, 0, 2, b"zz"))  # GC drops the stale one first
    assert r.pending == 1 and r.buffered_bytes == 2
    r = fragment.Reassembler(max_buffered_bytes=100)
    chunks = fragment.fragment(b"hello world", 3, fragment_id=fid)
    assert _reassemble_with(r, chunks) == b"hello world"
    assert r.pending == 0 and r.buffered_bytes == 0


def test_invalid_budget_args_rejected():
    import pytest
    with pytest.raises(ValueError):
        fragment.Reassembler(max_fragments=0)
    with pytest.raises(ValueError):
        fragment.Reassembler(max_buffered_bytes=-1)
    with pytest.raises(ValueError):
        fragment.Reassembler(max_chunk_payload=0)


if __name__ == "__main__":
    test_round_trips()
    test_rejects_garbage_and_dupes()
    test_conformance_vectors()
    print("fragmentation: PASS (round-trip + Python matches tests/vectors.json)")
