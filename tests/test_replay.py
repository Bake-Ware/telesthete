"""Replay protection (SPEC §3.3): Control and Stream MUST reject packets whose
sequence is <= the per-peer high-water mark. Run: python tests/test_replay.py
"""

import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from telesthete.protocol.crypto import BandCrypto
from telesthete.protocol.control import ControlChannel, ControlMessageType
from telesthete.protocol.stream import Stream


class MockTransport:
    def __init__(self):
        self.sent = []

    def send(self, dest, packet):
        self.sent.append((dest, packet))


PEER = ("127.0.0.1", 8888)


def test_control_rejects_replay():
    c = BandCrypto("replay-psk")
    tx = MockTransport()
    ctrl = ControlChannel(c.band_id, c, tx)
    ctrl.add_destination(("127.0.0.1", 9999))

    got = []
    ctrl.register_handler(ControlMessageType.METACONTROL, lambda a, p: got.append(p))
    ctrl.send_metacontrol({"k": "v"})
    _, packet = tx.sent[-1]

    ctrl.handle_packet(PEER, packet)  # first delivery
    ctrl.handle_packet(PEER, packet)  # replay -> dropped
    ctrl.handle_packet(PEER, packet)  # replay -> dropped
    assert len(got) == 1, f"replay not dropped: {len(got)} deliveries"


def test_stream_rejects_replay_and_stale():
    c = BandCrypto("replay-psk")
    tx = MockTransport()
    s = Stream(c.band_id, 5, c, tx, priority=0)
    s.add_destination(("127.0.0.1", 9999))

    got = []
    s.on_receive(lambda data, peer, ts: got.append(data))

    s.send(b"one")
    s.send(b"two")
    p1 = tx.sent[0][1]
    p2 = tx.sent[1][1]

    s.handle_packet(PEER, p2)  # seq=1 accepted -> watermark 1
    s.handle_packet(PEER, p1)  # seq=0 stale -> dropped
    s.handle_packet(PEER, p2)  # seq=1 replay -> dropped
    assert got == [b"two"], f"stale/replay not dropped: {got}"


if __name__ == "__main__":
    test_control_rejects_replay()
    test_stream_rejects_replay_and_stale()
    print("replay protection: PASS (control + stream reject <= watermark)")
