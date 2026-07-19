"""§7 Board — LWW replicated map: merge rule, digest anti-entropy, snapshot sync."""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.protocol.crypto import BandCrypto
from telesthete.protocol.board import Board, BoardMessageType


class MockTransport:
    def __init__(self):
        self.sent = []

    def send(self, dest, packet):
        self.sent.append((dest, packet))


def _pair(psk="board-psk"):
    crypto = BandCrypto(psk)
    ta, tb = MockTransport(), MockTransport()
    a = Board(crypto.band_id, 3, "alice", crypto=crypto, transport=ta)
    b = Board(crypto.band_id, 3, "bob", crypto=crypto, transport=tb)
    a.add_destination(("b", 1))
    b.add_destination(("a", 1))
    return a, b, ta, tb


def _deliver(from_mt, to_board, sender_addr):
    for _, pkt in from_mt.sent:
        to_board.handle_packet(sender_addr, pkt)
    from_mt.sent.clear()


def test_set_replicates_and_get():
    a, b, ta, _ = _pair()
    a.set("cursor", {"x": 4, "y": 2})
    _deliver(ta, b, ("alice", 1))
    assert b.get("cursor") == {"x": 4, "y": 2}
    assert b.items() == {"cursor": {"x": 4, "y": 2}}


def test_lww_merge_higher_lamport_wins():
    a, b, ta, tb = _pair()
    a.set("k", "from-alice")
    _deliver(ta, b, ("alice", 1))
    b.set("k", "from-bob")  # b merged lamport=1, so this is lamport=2
    _deliver(tb, a, ("bob", 1))
    assert a.get("k") == "from-bob"
    assert b.get("k") == "from-bob"


def test_equal_lamport_actor_tiebreak():
    # Concurrent writes with equal clocks: higher actor string wins everywhere.
    a, b, ta, tb = _pair()
    a.set("k", "alice-val")  # (1, "alice")
    b.set("k", "bob-val")    # (1, "bob") -> "bob" > "alice"
    _deliver(ta, b, ("alice", 1))
    _deliver(tb, a, ("bob", 1))
    assert a.get("k") == "bob-val"
    assert b.get("k") == "bob-val"


def test_delete_tombstone_propagates():
    a, b, ta, _ = _pair()
    a.set("k", 1)
    a.delete("k")
    _deliver(ta, b, ("alice", 1))
    assert b.get("k") is None
    assert "k" not in b.items()
    # Tombstone must beat the older live entry even if replayed out of order.
    assert b._entries["k"].deleted


def test_stale_merge_is_ignored():
    a, _, _, _ = _pair()
    a.set("k", "new")  # lamport 1 by alice... bump to make room
    a.set("k", "newer")  # lamport 2
    changed = a.merge_entry({"key": "k", "value": "old", "ts": [1, "zzz"]})
    assert not changed
    assert a.get("k") == "newer"


def test_digest_equal_iff_converged():
    a, b, ta, tb = _pair()
    a.set("x", 1)
    a.set("y", 2)
    assert a.digest() != b.digest()
    _deliver(ta, b, ("alice", 1))
    assert a.digest() == b.digest()
    b.set("z", 3)
    assert a.digest() != b.digest()
    _deliver(tb, a, ("bob", 1))
    assert a.digest() == b.digest()


def test_digest_mismatch_triggers_sync_req_then_snapshot_converges():
    a, b, ta, tb = _pair()
    a.set("only-on-a", "v")
    ta.sent.clear()  # b never saw the SET (lossy network)

    # Anti-entropy round (§7.4): a probes, b answers SYNC_REQ, a snapshots.
    a.send_digest(dest=("b", 1))
    _deliver(ta, b, ("alice", 1))          # DIGEST -> b sends SYNC_REQ
    assert len(tb.sent) == 1
    _deliver(tb, a, ("bob", 1))            # SYNC_REQ -> a sends SNAPSHOT
    assert len(ta.sent) >= 1
    _deliver(ta, b, ("alice", 1))          # SNAPSHOT chunks -> b merges
    assert b.get("only-on-a") == "v"
    assert a.digest() == b.digest()


def test_matching_digest_stays_quiet():
    a, b, ta, tb = _pair()
    a.set("k", 1)
    _deliver(ta, b, ("alice", 1))
    a.send_digest(dest=("b", 1))
    _deliver(ta, b, ("alice", 1))
    assert tb.sent == [], "matching digest must not trigger a sync"


def test_large_snapshot_fragments_and_reassembles():
    a, b, ta, tb = _pair()
    for i in range(50):
        a.set(f"key-{i}", "v" * 100)  # ~5KB of entries > one chunk
    ta.sent.clear()
    a.send_snapshot(("b", 1))
    assert len(ta.sent) > 1, "large snapshot must fragment (§6.6 envelope)"
    _deliver(ta, b, ("alice", 1))
    assert b.items() == a.items()


def test_replayed_set_is_dropped_by_watermark():
    a, b, ta, _ = _pair()
    changes = []
    b.on_change(lambda k, v, d: changes.append((k, v, d)))
    a.set("k", 1)
    pkt = ta.sent[-1][1]
    b.handle_packet(("alice", 1), pkt)
    b.handle_packet(("alice", 1), pkt)  # replay
    assert len(changes) == 1, "replayed SET must be dropped (SPEC §3.3)"


def test_out_of_range_lamport_rejected_keeps_digest_working():
    # A hostile SET with a >2^64 (or negative) Lamport clock must be rejected,
    # not stored — else digest()'s lamport.to_bytes(8) raises forever (§7.4).
    a, _, _, _ = _pair()
    a.set("k", "ok")
    assert a.merge_entry({"key": "bad", "value": "x",
                          "ts": [1 << 64, "attacker"]}) is False
    assert a.merge_entry({"key": "neg", "value": "x",
                          "ts": [-1, "attacker"]}) is False
    assert "bad" not in a._entries and "neg" not in a._entries
    a.digest()  # must not raise


def test_lamport_advances_past_merged_clock():
    a, b, ta, tb = _pair()
    for _ in range(5):
        a.set("k", "spin")  # a's clock at 5
    _deliver(ta, b, ("alice", 1))
    b.set("k", "bob-wins")  # must be lamport 6, not 1
    assert b._entries["k"].lamport == 6
    _deliver(tb, a, ("bob", 1))
    assert a.get("k") == "bob-wins"
