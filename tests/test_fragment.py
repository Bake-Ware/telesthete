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
    assert r.pending == 2, "an evicted message's late chunk is not re-buffered"
    assert r.feed(_chunk(b, 1, 2, b"B")) == b"bbbbB"
    assert r.feed(_chunk(c, 1, 2, b"C")) == b"ccccC"
    assert r.pending == 0 and r.buffered_bytes == 0


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


# -- evicted-message memory (zombie partials) --------------------------------

def test_evicted_message_chunks_are_dropped_not_rebuffered():
    # Without memory of evicted ids, a's late chunks would start a fresh
    # partial that can never complete, and evict b in turn.
    r = fragment.Reassembler(max_buffered_bytes=9)  # room for b, c + one tail byte
    a, b, c = b"\x20" * 16, b"\x21" * 16, b"\x26" * 16
    assert r.feed(_chunk(a, 0, 3, b"aaaa")) is None
    assert r.feed(_chunk(b, 0, 2, b"bbbb")) is None
    assert r.feed(_chunk(c, 0, 2, b"cccc")) is None  # evicts a (oldest)
    assert r.pending == 2 and r.buffered_bytes == 8
    assert r.feed(_chunk(a, 1, 3, b"aaaa")) is None
    assert r.feed(_chunk(a, 2, 3, b"aaaa")) is None
    assert r.pending == 2 and r.buffered_bytes == 8, "zombie must not displace b or c"
    assert r.feed(_chunk(b, 1, 2, b"B")) == b"bbbbB"
    assert r.feed(_chunk(c, 1, 2, b"C")) == b"ccccC"


def test_message_too_big_for_budget_is_remembered():
    r = fragment.Reassembler(max_buffered_bytes=6)
    a = b"\x27" * 16
    assert r.feed(_chunk(a, 0, 3, b"1234")) is None
    assert r.feed(_chunk(a, 1, 3, b"5678")) is None  # can't fit: dropped
    assert r.feed(_chunk(a, 2, 3, b"9")) is None
    assert r.pending == 0 and r.buffered_bytes == 0, "its tail is not re-buffered"


def test_buffer_limit_eviction_is_remembered_too():
    r = fragment.Reassembler(buffer_limit=1)
    a, b = b"\x22" * 16, b"\x23" * 16
    r.feed(_chunk(a, 0, 2, b"a"))
    r.feed(_chunk(b, 0, 2, b"b"))       # evicts a
    assert r.feed(_chunk(a, 1, 2, b"A")) is None
    assert r.feed(_chunk(b, 1, 2, b"B")) == b"bB", "a's tail must not evict b"


def test_evicted_memory_expires_with_timeout():
    import time as _t
    r = fragment.Reassembler(timeout=0.05, buffer_limit=1)
    a, b = b"\x24" * 16, b"\x25" * 16
    r.feed(_chunk(a, 0, 2, b"a"))
    r.feed(_chunk(b, 0, 2, b"b"))       # evicts a
    _t.sleep(0.08)
    # Long after: a's id is forgotten (and b timed out), so a new message
    # that happens to reuse it is accepted normally.
    r.feed(_chunk(a, 0, 2, b"x"))
    assert r.feed(_chunk(a, 1, 2, b"y")) == b"xy"


def test_evicted_memory_is_bounded():
    r = fragment.Reassembler(buffer_limit=1)
    for i in range(fragment.DEFAULT_EVICTED_MEMORY + 50):
        r.feed(_chunk(i.to_bytes(16, "big"), 0, 2, b"z"))
    assert len(r._evicted) <= fragment.DEFAULT_EVICTED_MEMORY


def test_three_interleaved_30mb_messages_under_64mib_two_complete():
    # 3 x 30 MB interleaved chunk-by-chunk under a 64 MiB budget: one must be
    # sacrificed, and with evicted-id memory the other two complete. (Before:
    # the sacrificed message's later chunks kept restarting it and evicting
    # the others, so nothing completed.)
    size = 30 * 1000 * 1000
    cs = fragment.MAX_CHUNK_PAYLOAD
    total = -(-size // cs)
    r = fragment.Reassembler(max_fragments=fragment.WIRE_MAX_FRAGMENTS,
                             max_buffered_bytes=64 * 1024 * 1024)
    msgs = {bytes([0x30 + k]) * 16: bytes([0x41 + k]) * size for k in range(3)}
    done = {}
    for seq in range(total):
        for fid, payload in msgs.items():
            out = r.feed(fragment.pack_chunk(fid, seq, total,
                                             payload[seq * cs:(seq + 1) * cs]))
            if out is not None:
                done[fid] = out
    assert len(done) == 2
    for fid, out in done.items():
        assert out == msgs[fid]
    assert r.buffered_bytes <= 64 * 1024 * 1024
    assert r.pending == 0


# -- library Channel/Board reassembly limits ---------------------------------

def test_fragmenter_reassembler_accepts_what_the_fragmenter_emits():
    big = fragment.Fragmenter(chunk_size=2000)
    r = big.reassembler()
    payload = bytes(range(256)) * 20  # 5120 B -> 2000 B chunks
    assert _reassemble_with(r, big.split(payload)) == payload
    # The plain default would drop those 2000 B chunks.
    assert _reassemble_with(fragment.Reassembler(), big.split(payload)) is None
    # More chunks than the receiver-side default cap (16384) is fine too.
    tiny = fragment.Fragmenter(chunk_size=1)
    payload = b"t" * (fragment.DEFAULT_MAX_FRAGMENTS + 10)
    assert _reassemble_with(tiny.reassembler(), tiny.split(payload)) == payload
    # Memory stays bounded, and overrides pass through.
    assert r._max_bytes >= fragment.LIBRARY_MAX_BUFFERED_BYTES
    assert big.reassembler(max_buffered_bytes=10)._max_bytes == 10


def test_library_channel_and_board_use_matched_reassemblers():
    from telesthete.protocol.channel import Channel
    from telesthete.protocol.crypto import BandCrypto
    crypto = BandCrypto("frag-limits")
    ch = Channel(crypto.band_id, 1, ("127.0.0.1", 9), transport=None, crypto=crypto)
    assert ch._reassembler._max_chunk == ch._fragmenter.chunk_size
    assert ch._reassembler._max_fragments == fragment.WIRE_MAX_FRAGMENTS
    assert ch._reassembler._max_bytes is not None


if __name__ == "__main__":
    test_round_trips()
    test_rejects_garbage_and_dupes()
    test_conformance_vectors()
    print("fragmentation: PASS (round-trip + Python matches tests/vectors.json)")
