"""§8 Drop — receiver-driven chunked resumable transfer."""

import hashlib
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.protocol.crypto import BandCrypto
from telesthete.protocol.drop import (
    DropSender, DropReceiver, CHUNK_SIZE, MAX_RANGE_CHUNKS,
)


class MockTransport:
    def __init__(self):
        self.sent = []

    def send(self, dest, packet):
        self.sent.append((dest, packet))


def _pair(data: bytes, have=None, psk="drop-psk"):
    crypto = BandCrypto(psk)
    ts, tr = MockTransport(), MockTransport()
    sender = DropSender(crypto.band_id, 7, "blob.bin", data,
                        crypto=crypto, transport=ts)
    receiver = DropReceiver(crypto.band_id, 7, have=have,
                            crypto=crypto, transport=tr)
    return sender, receiver, ts, tr


def _pump(sender, receiver, ts, tr, rounds=100):
    """Deliver frames both ways until both queues drain."""
    for _ in range(rounds):
        if not ts.sent and not tr.sent:
            return
        for _, pkt in ts.sent:
            receiver.handle_packet(("sender", 1), pkt)
        ts.sent.clear()
        for _, pkt in tr.sent:
            sender.handle_packet(("receiver", 1), pkt)
        tr.sent.clear()


def test_small_file_transfers_and_verifies():
    data = b"hello drop"
    sender, receiver, ts, tr = _pair(data)
    done = []
    receiver.on_complete(lambda d, ok: done.append((d, ok)))
    sender.offer(("receiver", 1))
    _pump(sender, receiver, ts, tr)
    assert done == [(data, True)]
    assert receiver.verified is True
    assert sender.completed[("receiver", 1)] is True


def test_multi_window_transfer():
    # > 64 chunks forces several REQUEST windows (§8.2 one-outstanding-window).
    data = os.urandom(CHUNK_SIZE * (MAX_RANGE_CHUNKS * 2 + 3) + 17)
    sender, receiver, ts, tr = _pair(data)
    sender.offer(("receiver", 1))
    _pump(sender, receiver, ts, tr)
    assert receiver.verified is True
    got = b"".join(receiver.have[i] for i in range(sender.total_chunks))[:len(data)]
    assert hashlib.sha256(got).digest() == hashlib.sha256(data).digest()


def test_resume_requests_only_missing_chunks():
    data = os.urandom(CHUNK_SIZE * 10)
    chunks = {i: data[i * CHUNK_SIZE:(i + 1) * CHUNK_SIZE] for i in range(10)}
    have = {i: chunks[i] for i in (0, 1, 2, 5, 9)}  # persisted from a prior run
    sender, receiver, ts, tr = _pair(data, have=have)
    sender.offer(("receiver", 1))
    _pump(sender, receiver, ts, tr)
    assert receiver.verified is True
    assert receiver.missing_ranges() == []


def test_lost_chunks_rerequested_on_tick():
    data = os.urandom(CHUNK_SIZE * 4)
    sender, receiver, ts, tr = _pair(data)
    sender.offer(("receiver", 1))
    # Deliver OFFER, then LOSE the sender's chunk replies entirely.
    for _, pkt in ts.sent:
        receiver.handle_packet(("sender", 1), pkt)
    ts.sent.clear()
    for _, pkt in tr.sent:
        sender.handle_packet(("receiver", 1), pkt)
    tr.sent.clear()
    ts.sent.clear()  # chunks lost in flight
    assert receiver.verified is None

    # tick() past the timeout re-requests; this time let everything through.
    receiver.tick(now=1e9)
    _pump(sender, receiver, ts, tr)
    assert receiver.verified is True


def test_corrupted_transfer_reports_mismatch():
    data = os.urandom(CHUNK_SIZE * 2)
    sender, receiver, ts, tr = _pair(data)
    verdicts = []
    sender.on_complete(lambda addr, ok: verdicts.append(ok))
    sender.offer(("receiver", 1))
    # Deliver offer + request, then corrupt the sender's data before serving.
    for _, pkt in ts.sent:
        receiver.handle_packet(("sender", 1), pkt)
    ts.sent.clear()
    sender.data = os.urandom(len(data))  # sender's bytes changed mid-flight
    _pump(sender, receiver, ts, tr)
    assert receiver.verified is False
    assert verdicts == [False]


def test_hostile_request_is_clamped():
    data = os.urandom(CHUNK_SIZE * 3)
    sender, receiver, ts, tr = _pair(data)
    sender.offer(("receiver", 1))
    for _, pkt in ts.sent:
        receiver.handle_packet(("sender", 1), pkt)
    ts.sent.clear()
    tr.sent.clear()
    # Forge an absurd range through the real encrypt path.
    receiver._send_json(("sender", 1), 0x02, {"ranges": [[0, 10_000_000]]})
    for _, pkt in tr.sent:
        sender.handle_packet(("receiver", 1), pkt)
    # Served at most total_chunks frames, not 10M.
    assert len(ts.sent) <= sender.total_chunks


def test_out_of_range_preseed_does_not_crash_finish():
    # Pre-seeded chunks that this OFFER doesn't cover (out-of-range index or
    # wrong length) must be pruned, not carried into _finish (KeyError/corrupt).
    data = os.urandom(CHUNK_SIZE * 2)
    good = {0: data[:CHUNK_SIZE]}
    poison = {0: data[:CHUNK_SIZE], 99: b"nonsense", 1: b"wrong-length"}
    sender, receiver, ts, tr = _pair(data, have=poison)
    sender.offer(("receiver", 1))
    _pump(sender, receiver, ts, tr)
    assert receiver.verified is True
    assert 99 not in receiver.have  # out-of-range pruned


def test_malformed_offer_rejected():
    data = os.urandom(CHUNK_SIZE * 3)
    sender, receiver, ts, tr = _pair(data)
    # Forge an OFFER whose total_chunks lies about the size.
    receiver._send_raw = lambda *a, **k: None  # silence
    receiver.handle_packet(("sender", 1),
                           _offer_packet(sender, total_chunks=10_000_000))
    assert receiver.offer is None, "malformed OFFER must be rejected"


def _offer_packet(sender, total_chunks):
    import json as _json
    from telesthete.protocol.framing import pack_packet, ChannelType
    from telesthete.protocol.drop import DropFrameType
    body = bytes([DropFrameType.OFFER]) + _json.dumps({
        "name": "x", "size": len(sender.data), "chunk_size": 1024,
        "total_chunks": total_chunks, "sha256": sender.sha256}).encode()
    seq = sender._seq_source.next()
    aad = bytes([ChannelType.DROP, sender.drop_id >> 8, sender.drop_id & 0xFF])
    ct = sender._send_crypto(("receiver", 1)).encrypt(seq, body, aad)
    return pack_packet(sender.band_id, ChannelType.DROP, sender.drop_id, seq, ct)


def test_empty_file():
    sender, receiver, ts, tr = _pair(b"")
    sender.offer(("receiver", 1))
    _pump(sender, receiver, ts, tr)
    assert receiver.verified is True


def test_next_window_matches_missing_ranges():
    # The incremental window search must pick exactly what the full scan
    # would: the first missing run, capped at MAX_RANGE_CHUNKS.
    import random
    rng = random.Random(3)
    data = os.urandom(CHUNK_SIZE * 300)
    for _ in range(50):
        _, receiver, _, _ = _pair(data)
        receiver.offer = {"size": len(data), "total_chunks": 300}
        held = set(rng.sample(range(300), rng.randrange(300)))
        receiver.have = {i: b"" for i in held}
        expected = receiver.missing_ranges()[:1]
        got = receiver._next_missing_range()
        assert ([got] if got else []) == expected
        # Filling chunks only moves the search forward; it stays exact.
        for i in sorted(set(range(300)) - held)[:rng.randrange(1, 40)]:
            receiver.have[i] = b""
        expected = receiver.missing_ranges()[:1]
        got = receiver._next_missing_range()
        assert ([got] if got else []) == expected


class _CountingDict(dict):
    lookups = 0

    def __contains__(self, key):
        _CountingDict.lookups += 1
        return super().__contains__(key)


def test_chunk_handling_is_linear_in_file_size():
    # Each CHUNK used to re-check every index (all(i in have ...)), so a
    # 32 MiB pull cost ~half a billion lookups. Now completion is a count and
    # only the <= 64-chunk outstanding window is checked per chunk.
    data = os.urandom(CHUNK_SIZE * 2048)
    sender, receiver, ts, tr = _pair(data)
    sender.offer(("receiver", 1))
    for _, pkt in ts.sent:
        receiver.handle_packet(("sender", 1), pkt)   # OFFER
    ts.sent.clear()
    receiver.have = _CountingDict(receiver.have)
    _CountingDict.lookups = 0
    _pump(sender, receiver, ts, tr, rounds=1000)
    assert receiver.verified is True
    n = sender.total_chunks
    assert _CountingDict.lookups < n * (MAX_RANGE_CHUNKS + 8), _CountingDict.lookups


def test_resume_with_every_chunk_finishes_on_offer():
    data = os.urandom(CHUNK_SIZE * 5 + 3)
    chunks = {i: data[i * CHUNK_SIZE:(i + 1) * CHUNK_SIZE] for i in range(6)}
    sender, receiver, ts, tr = _pair(data, have=chunks)
    sender.offer(("receiver", 1))
    _pump(sender, receiver, ts, tr)
    assert receiver.verified is True
