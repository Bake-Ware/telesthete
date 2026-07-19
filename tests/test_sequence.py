"""Phase 1 — sequence/nonce uniqueness + restart recovery (SPEC §3.3/§4.3).

These would have caught the original cross-channel / cross-sender nonce-reuse
bug and the restart-lockout.
"""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.protocol.sequence import SequenceSource
from telesthete.protocol.crypto import BandCrypto, nonce_from_seq
from telesthete.protocol.control import ControlChannel, ControlMessageType
from telesthete.protocol.stream import Stream
from telesthete.protocol.framing import unpack_packet


class MockTransport:
    def __init__(self):
        self.sent = []

    def send(self, dest, packet):
        self.sent.append((dest, packet))


def test_source_is_monotonic_and_unique():
    src = SequenceSource(start=0)
    seqs = [src.next() for _ in range(1000)]
    assert seqs == list(range(1000))
    assert len({nonce_from_seq(s) for s in seqs}) == 1000


def test_random_init_separates_senders():
    # Two independent senders almost surely start far apart (63-bit space).
    starts = {SequenceSource().peek() for _ in range(50)}
    assert len(starts) == 50
    assert all(s < (1 << 63) for s in starts)


def test_wrap_is_masked_to_64_bits():
    src = SequenceSource(start=(1 << 64) - 1)
    assert src.next() == (1 << 64) - 1
    assert src.next() == 0


def test_control_and_stream_share_one_source_no_reuse():
    # The critical fix: a Control message and Stream frames from one sender draw
    # from ONE source, so no two packets reuse a sequence (hence a nonce).
    crypto = BandCrypto("psk-shared")
    mt = MockTransport()
    src = SequenceSource(start=0)
    ctl = ControlChannel(crypto.band_id, crypto=crypto, transport=mt, seq_source=src)
    st = Stream(crypto.band_id, 7, crypto=crypto, transport=mt, seq_source=src)
    ctl.add_destination(("d", 1))
    st.add_destination(("d", 1))

    ctl.send_message(ControlMessageType.KEEPALIVE, {}, dest=("d", 1))
    st.send(b"a")
    st.send(b"b")

    seqs = [unpack_packet(p).sequence for _, p in mt.sent]
    assert len(seqs) == 3
    assert len(set(seqs)) == 3, "shared source must never reuse a sequence"


def test_receiver_accepts_first_seen_then_strict():
    # A receiver accepts a sender's first (random-start) packet, then requires
    # strictly increasing sequences.
    crypto = BandCrypto("psk-accept-first")
    sender_mt = MockTransport()
    sender = ControlChannel(crypto.band_id, crypto=crypto, transport=sender_mt,
                            seq_source=SequenceSource(start=5_000_000))
    sender.add_destination(("s", 1))
    receiver = ControlChannel(crypto.band_id, crypto=crypto, transport=MockTransport())
    got = []
    receiver.register_handler(ControlMessageType.KEEPALIVE, lambda a, p: got.append(p))

    sender.send_message(ControlMessageType.KEEPALIVE, {}, dest=("s", 1))
    first = sender_mt.sent[-1][1]
    receiver.handle_packet(("peer", 1), first)
    assert len(got) == 1, "first packet at a high random start must be accepted"

    # Replay of the same packet is rejected.
    receiver.handle_packet(("peer", 1), first)
    assert len(got) == 1, "replayed packet must be dropped"


def test_restart_hello_rebases_watermark():
    # A restarted sender (new SequenceSource at a LOWER start, but a NEWER
    # session epoch) must have its HELLO accepted, not locked out.
    crypto = BandCrypto("psk-restart")
    sender_mt = MockTransport()
    sender = ControlChannel(crypto.band_id, crypto=crypto, transport=sender_mt,
                            seq_source=SequenceSource(start=9_000_000))
    sender.add_destination(("s", 1))
    receiver = ControlChannel(crypto.band_id, crypto=crypto, transport=MockTransport())
    hellos = []
    receiver.register_handler(ControlMessageType.HELLO, lambda a, p: hellos.append(p))

    sender.send_hello("h", ("s", 1), session=1)
    receiver.handle_packet(("peer", 1), sender_mt.sent[-1][1])
    assert len(hellos) == 1

    # Advance the receiver's watermark high with normal traffic.
    for _ in range(3):
        sender.send_message(ControlMessageType.KEEPALIVE, {}, dest=("s", 1))
        receiver.handle_packet(("peer", 1), sender_mt.sent[-1][1])

    # "Restart": fresh source at a LOW start, NEWER session epoch.
    sender_mt.sent.clear()
    restarted = ControlChannel(crypto.band_id, crypto=crypto, transport=sender_mt,
                               seq_source=SequenceSource(start=10))
    restarted.add_destination(("s", 1))
    restarted.send_hello("h", ("s", 1), session=2)
    receiver.handle_packet(("peer", 1), sender_mt.sent[-1][1])
    assert len(hellos) == 2, "restarted peer's HELLO (newer epoch) must be accepted"

    # A replayed OLD-epoch HELLO at a low sequence must NOT rebase again.
    stale_mt = MockTransport()
    stale = ControlChannel(crypto.band_id, crypto=crypto, transport=stale_mt,
                           seq_source=SequenceSource(start=1))
    stale.add_destination(("s", 1))
    stale.send_hello("h", ("s", 1), session=1)  # old epoch
    receiver.handle_packet(("peer", 1), stale_mt.sent[-1][1])
    assert len(hellos) == 2, "older-epoch HELLO must not rebase the watermark"


def test_select_cipher_normalizes_missing_baseline():
    from telesthete.protocol.crypto import select_cipher, BASELINE_CIPHER
    # Responder omits the mandatory baseline; selection still resolves.
    assert select_cipher([BASELINE_CIPHER], ["aes256-gcm"]) == BASELINE_CIPHER
    assert select_cipher(["aes256-gcm"], ["aes256-gcm"]) == "aes256-gcm"


def test_next_is_thread_safe():
    # A duplicated sequence is a duplicated nonce; concurrent next() must not
    # collide (would fail without the lock).
    import threading
    src = SequenceSource(start=0)
    out = []
    barrier = threading.Barrier(8)

    def worker():
        barrier.wait()
        local = [src.next() for _ in range(2000)]
        out.extend(local)

    threads = [threading.Thread(target=worker) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(out) == 16000
    assert len(set(out)) == 16000, "concurrent next() must never duplicate a sequence"


def test_old_session_packet_fails_under_new_session_key():
    # The cross-session replay closure: a packet encrypted under an old session
    # epoch's key must NOT authenticate under the peer's new-session key or the
    # base key after a restart.
    import pytest
    from telesthete.protocol.crypto import BandCrypto, build_aad
    psk = "replay-window-psk"
    aad = build_aad(1, 0)
    old = BandCrypto(psk, session=1000)          # session 1
    ct = old.encrypt(42, b"old-session secret", aad)
    assert old.decrypt(42, ct, aad) == b"old-session secret"  # sanity

    new = BandCrypto(psk, session=2000)          # peer restarted -> session 2
    base = BandCrypto(psk, session=None)         # base (HELLO) key
    with pytest.raises(Exception):
        new.decrypt(42, ct, aad)
    with pytest.raises(Exception):
        base.decrypt(42, ct, aad)


def test_band_wires_one_shared_source_and_session_keys():
    # Catch broken wiring: Control and every Stream MUST share the Band's one
    # sequence source (else nonces could collide), and Streams MUST use the
    # session-keyed resolvers (else the replay fix is bypassed).
    from telesthete.band import Band
    b = Band(psk="wiring-psk", bind_port=0)
    s1 = b.stream(1)
    s2 = b.stream(2)
    assert b.control._seq_source is b.seq_source
    assert s1._seq_source is b.seq_source is s2._seq_source
    # Streams resolve through the Band's per-session send/recv crypto.
    assert s1._send_crypto == b.send_crypto
    assert s1._recv_crypto == b.recv_crypto
    # A stream's send key is the base band's OWN epoch data key, distinct from
    # the base (HELLO) key.
    send_c = b.send_crypto(("1.2.3.4", 5))
    assert send_c.session == b.session_epoch
    assert send_c.key != b.base_crypto().key


def test_keyframe_req_and_rate_hint_round_trip():
    # §4.9/§4.10 control messages encode + decode both ways.
    crypto = BandCrypto("kf-psk")
    mt = MockTransport()
    sender = ControlChannel(crypto.band_id, crypto=crypto, transport=mt,
                            seq_source=SequenceSource(start=0))
    sender.add_destination(("d", 1))
    receiver = ControlChannel(crypto.band_id, crypto=crypto, transport=MockTransport())
    got = []
    receiver.register_handler(ControlMessageType.KEYFRAME_REQ, lambda a, p: got.append(("kf", p)))
    receiver.register_handler(ControlMessageType.RATE_HINT, lambda a, p: got.append(("rh", p)))

    sender.send_keyframe_req(9, dest=("d", 1))
    receiver.handle_packet(("peer", 1), mt.sent[-1][1])
    sender.send_rate_hint(9, 2_000_000, 0.05, dest=("d", 1))
    receiver.handle_packet(("peer", 1), mt.sent[-1][1])

    assert got[0] == ("kf", {"stream_id": 9})
    assert got[1][0] == "rh"
    assert got[1][1]["stream_id"] == 9 and got[1][1]["target_bps"] == 2_000_000
    assert abs(got[1][1]["loss"] - 0.05) < 1e-6


def test_unknown_control_type_is_ignored():
    # §4.2: an unknown control type must be dropped, not crash or dispatch.
    crypto = BandCrypto("unknown-psk")
    mt = MockTransport()
    sender = ControlChannel(crypto.band_id, crypto=crypto, transport=mt,
                            seq_source=SequenceSource(start=0))
    sender.add_destination(("d", 1))
    receiver = ControlChannel(crypto.band_id, crypto=crypto, transport=MockTransport())
    fired = []
    for t in ControlMessageType:
        receiver.register_handler(t, lambda a, p: fired.append(p))
    sender.send_message(0x42, {"x": 1}, dest=("d", 1))  # undefined type
    receiver.handle_packet(("peer", 1), mt.sent[-1][1])  # must not raise
    assert fired == [], "unknown control type must not be dispatched"


def test_stale_epoch_hello_cannot_ratchet_control_watermark():
    # A replayed pre-restart HELLO (older epoch) whose random outer sequence
    # happens to exceed the live watermark must NOT advance the watermark — that
    # would drop every genuine session-2 control packet (permanent control DoS).
    crypto = BandCrypto("psk-ctrl-dos")
    recv_mt = MockTransport()
    receiver = ControlChannel(crypto.band_id, crypto=crypto, transport=recv_mt)
    keepalives = []
    receiver.register_handler(ControlMessageType.KEEPALIVE,
                              lambda a, p: keepalives.append(p))

    # Session 1 HELLO at a HIGH outer sequence.
    s1 = MockTransport()
    old = ControlChannel(crypto.band_id, crypto=crypto, transport=s1,
                         seq_source=SequenceSource(start=9_000_000))
    old.add_destination(("s", 1))
    old.send_hello("h", ("s", 1), session=1)
    stale_hello = s1.sent[-1][1]
    receiver.handle_packet(("peer", 1), stale_hello)

    # Session 2 HELLO at a LOWER outer sequence (peer restarted, fresh source).
    s2 = MockTransport()
    new = ControlChannel(crypto.band_id, crypto=crypto, transport=s2,
                         seq_source=SequenceSource(start=100))
    new.add_destination(("s", 1))
    new.send_hello("h", ("s", 1), session=2)
    receiver.handle_packet(("peer", 1), s2.sent[-1][1])

    # Replay the stale session-1 HELLO (high seq). It must be dropped by the
    # epoch check, NOT ratchet the watermark up to 9,000,000+.
    receiver.handle_packet(("peer", 1), stale_hello)

    # Live session-2 keepalives (seq ~100+) must still be accepted.
    for _ in range(3):
        new.send_message(ControlMessageType.KEEPALIVE, {}, dest=("s", 1))
        receiver.handle_packet(("peer", 1), s2.sent[-1][1])
    assert len(keepalives) == 3, "stale-epoch HELLO must not wedge the control plane"


def test_stale_epoch_hello_cannot_downgrade_peer():
    # §4.3 monotonicity at the Band layer: a replayed pre-restart HELLO (older
    # epoch) must not roll back peer.session_epoch/cipher — that would swap the
    # peer's data key back to the dead session and wedge its live traffic.
    from telesthete.band import Band
    b = Band(psk="epoch-guard-psk", bind_port=0,
             ciphers=["aes256-gcm", "chacha20-poly1305"])
    acks = []
    b.control.send_hello_ack = lambda *a, **kw: acks.append((a, kw))

    b._on_hello(("p", 1), {"hostname": "peer", "session": 100,
                           "ciphers": ["aes256-gcm", "chacha20-poly1305"]})
    assert b.peers[("p", 1)].session_epoch == 100
    assert b.peers[("p", 1)].cipher == "aes256-gcm"
    assert len(acks) == 1

    # Replayed old-epoch HELLO: ignored entirely (no state change, no ACK).
    b._on_hello(("p", 1), {"hostname": "peer", "session": 50,
                           "ciphers": ["chacha20-poly1305"]})
    assert b.peers[("p", 1)].session_epoch == 100
    assert b.peers[("p", 1)].cipher == "aes256-gcm"
    assert len(acks) == 1, "stale-epoch HELLO must not be acked"

    # Same for HELLO_ACK.
    b._on_hello_ack(("p", 1), {"hostname": "peer", "session": 50,
                               "cipher": "chacha20-poly1305"})
    assert b.peers[("p", 1)].session_epoch == 100
    assert b.peers[("p", 1)].cipher == "aes256-gcm"


def test_channel_is_wired_into_band():
    # Phase 6 (replaces the deferral guard): the reliable Channel now draws
    # outer sequences from the Band's ONE shared SequenceSource — the old
    # per-object counter starting at 0 was guaranteed nonce reuse — so it IS on
    # the live path: Band.channel() registers CHANNEL routing and constructs
    # with the session-keyed resolvers.
    from telesthete.band import Band
    from telesthete.protocol.framing import ChannelType
    b = Band(psk="channel-guard-psk", bind_port=0)
    assert ChannelType.CONTROL in b.transport._handlers
    assert ChannelType.STREAM in b.transport._handlers

    ch = b.channel(7, ("1.2.3.4", 5))
    assert ChannelType.CHANNEL in b.transport._handlers
    assert ch._seq_source is b.seq_source, "Channel must share the band source"
    # Channels resolve through the Band's per-session send/recv crypto.
    assert ch._send_crypto == b.send_crypto
    assert ch._recv_crypto == b.recv_crypto
    # Same object back; handler registration is idempotent.
    assert b.channel(7, ("1.2.3.4", 5)) is ch
    assert len(b.transport._handlers[ChannelType.CHANNEL]) == 1
