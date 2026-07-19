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


def test_channel_is_not_wired_into_band():
    # Regression guard (Phase 6 deferral): the reliable Channel still has an
    # unfixed per-object counter, so it MUST NOT be on the live path or it would
    # silently reintroduce nonce reuse. If a future change wires it up, this
    # fails until Channel draws from the shared SequenceSource.
    from telesthete.band import Band
    from telesthete.protocol.framing import ChannelType
    b = Band(psk="channel-guard-psk", bind_port=0)
    registered = set(b.transport._handlers.keys())
    assert ChannelType.CHANNEL not in registered, "Channel must not be wired (nonce-unsafe until Phase 6)"
    assert ChannelType.CONTROL in registered
    assert ChannelType.STREAM in registered
