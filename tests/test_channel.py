"""Phase 6 — reliable Channel (SPEC §6): handshake, reliability, flow control,
fragmentation, and the nonce-safety property that killed the old channel.py
(a per-object outer counter starting at 0 = guaranteed AEAD nonce reuse).
"""

import asyncio
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.protocol import channel as chmod
from telesthete.protocol.channel import (
    Channel, pack_frame, unpack_frame,
    FLAG_SYN, FLAG_FIN, FLAG_ACK,
)
from telesthete.protocol.crypto import BandCrypto
from telesthete.protocol.framing import pack_packet, unpack_packet, ChannelType
from telesthete.protocol.sequence import SequenceSource

VECTORS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vectors.json")

CHANNEL_ID = 9
A_ADDR = ("a", 1)
B_ADDR = ("b", 2)


class MockTransport:
    def __init__(self):
        self.sent = []

    def send(self, dest, packet):
        self.sent.append((dest, packet))


class Pair:
    """Two Channels cross-wired through MockTransports (manual delivery, so
    tests control loss/reordering). Distinct random-start-style sources per
    side, as in the real Band (one shared source PER SENDER, §3.3)."""

    def __init__(self):
        self.crypto = BandCrypto("channel-test-psk")
        self.ta = MockTransport()
        self.tb = MockTransport()
        self.a = Channel(self.crypto.band_id, CHANNEL_ID, B_ADDR,
                         transport=self.ta, crypto=self.crypto,
                         seq_source=SequenceSource(start=1_000))
        self.b = Channel(self.crypto.band_id, CHANNEL_ID, A_ADDR,
                         transport=self.tb, crypto=self.crypto,
                         seq_source=SequenceSource(start=2_000))

    def deliver(self):
        """Shuttle every queued packet both ways until quiescent."""
        while self.ta.sent or self.tb.sent:
            while self.ta.sent:
                _, pkt = self.ta.sent.pop(0)
                self.b.handle_packet(A_ADDR, pkt)
            while self.tb.sent:
                _, pkt = self.tb.sent.pop(0)
                self.a.handle_packet(B_ADDR, pkt)

    async def establish(self):
        await self.a.open()
        self.deliver()
        assert self.a._state == "ESTABLISHED"
        assert self.b._state == "ESTABLISHED"

    def cleanup(self):
        for ch in (self.a, self.b):
            if ch._retransmit_task is not None:
                ch._retransmit_task.cancel()


@pytest.fixture
def pair():
    p = Pair()
    yield p
    p.cleanup()


def decode(crypto, packet_bytes):
    """-> (outer_seq, (flags, ack_num, window, seq, data)) of a wire packet."""
    p = unpack_packet(packet_bytes)
    aad = bytes([ChannelType.CHANNEL, CHANNEL_ID >> 8, CHANNEL_ID & 0xFF])
    return p.sequence, unpack_frame(crypto.decrypt(p.sequence, p.ciphertext, aad))


# ------------------------------------------------------------------ vectors

def test_frame_pack_matches_spec_literals():
    # §6.1 layout, pinned byte-for-byte (the Rust side asserts the same).
    assert pack_frame(1, 0, 32, 0, b"").hex() == (
        "0100000000000000000020" + "0000000000000000")
    assert pack_frame(4, 1, 32, 5, b"hello").hex() == (
        "0400000000000000010020" + "0000000000000005" + "68656c6c6f")


def test_conformance_vectors_channel_frame():
    import json
    with open(VECTORS) as f:
        v = json.load(f)
    cases = v["channel_frame"]
    assert len(cases) == 2
    for case in cases:
        packed = pack_frame(case["flags"], case["ack_num"], case["window"],
                            case["seq"], bytes.fromhex(case["data_hex"]))
        assert packed.hex() == case["packed_hex"], "channel frame bytes diverged"
        # And the parse round-trips.
        flags, ack, win, seq, data = unpack_frame(packed)
        assert (flags, ack, win, seq, data.hex()) == (
            case["flags"], case["ack_num"], case["window"], case["seq"],
            case["data_hex"])


def test_reserved_flag_bits_rejected_by_parser():
    # §6.2: bits 4-7 reserved, must be 0; receiver drops such frames.
    assert unpack_frame(pack_frame(0x14, 0, 32, 0, b"")) is None
    assert unpack_frame(b"\x01" * 18) is None  # short frame


# ---------------------------------------------------------------- handshake

async def test_three_way_handshake(pair):
    # §6.3: SYN(seq=0) -> SYN+ACK(seq=0, ack=1) -> pure ACK(ack=1).
    await pair.a.open()
    assert pair.a._state == "SYN_SENT"
    _, (flags, ack, _, seq, data) = decode(pair.crypto, pair.ta.sent[0][1])
    assert flags == FLAG_SYN and seq == 0 and ack == 0 and data == b""

    _, pkt = pair.ta.sent.pop(0)
    pair.b.handle_packet(A_ADDR, pkt)
    assert pair.b._state == "ESTABLISHED"  # §6.5: CLOSED -> ESTABLISHED on SYN
    _, (flags, ack, _, seq, data) = decode(pair.crypto, pair.tb.sent[0][1])
    assert flags == (FLAG_SYN | FLAG_ACK) and seq == 0 and ack == 1

    _, pkt = pair.tb.sent.pop(0)
    pair.a.handle_packet(B_ADDR, pkt)
    assert pair.a._state == "ESTABLISHED"
    assert not pair.a._send_buffer, "SYN+ACK must cumulatively ack our SYN"
    _, (flags, ack, _, seq, data) = decode(pair.crypto, pair.ta.sent[0][1])
    # Pure ACK carries the next unconsumed seq WITHOUT consuming it (§6.1).
    assert flags == FLAG_ACK and data == b"" and seq == 1 and ack == 1
    assert pair.a._snd_next == 1

    _, pkt = pair.ta.sent.pop(0)
    pair.b.handle_packet(A_ADDR, pkt)
    assert not pair.b._send_buffer, "final ACK must clear the SYN+ACK"
    assert pair.b._snd_next == 1


# --------------------------------------------------------------- reliability

async def test_in_order_delivery(pair):
    await pair.establish()
    got = []
    pair.b.on_receive(got.append)
    pair.a.send(b"one")
    pair.a.send(b"two")
    pair.deliver()
    assert got == [b"one", b"two"]
    assert await pair.b.recv(timeout=0.1) == b"one"
    assert await pair.b.recv(timeout=0.1) == b"two"
    assert not pair.a._send_buffer, "acks must clear the send buffer"


async def test_out_of_order_arrival_is_reordered(pair):
    # §6.4: frames buffered and reordered by INNER seq. Legitimate inner
    # reordering arrives with INCREASING outer sequences (a §3.3 watermark
    # drops decreasing ones): e.g. originals lost, retransmissions later.
    await pair.establish()
    got = []
    pair.b.on_receive(got.append)
    pair.a._snd_next = 4  # inner 1..3 "consumed"; emit newest-first
    pair.a._transmit(FLAG_ACK, 3, b"three")
    pair.a._transmit(FLAG_ACK, 2, b"two")
    pair.a._transmit(FLAG_ACK, 1, b"one")

    acks = []
    for _, pkt in list(pair.ta.sent):
        pair.b.handle_packet(A_ADDR, pkt)
        acks.append(decode(pair.crypto, pair.tb.sent[-1][1])[1][1])
    assert got == [b"one", b"two", b"three"], "must deliver in inner-seq order"
    # Dup-ACKs restate the expected seq until the gap fills; then cumulative.
    assert acks == [1, 1, 4]
    assert pair.b._rcv_next == 4
    assert not pair.b._recv_buffer


async def test_duplicate_frame_dedup_and_reack(pair):
    await pair.establish()
    got = []
    pair.b.on_receive(got.append)
    pair.a.send(b"x")
    pair.deliver()
    assert got == [b"x"]

    # Spurious retransmit: same inner seq under a FRESH outer sequence (§6.1).
    pair.a._transmit(FLAG_ACK, 1, b"x")
    _, pkt = pair.ta.sent.pop(0)
    pair.b.handle_packet(A_ADDR, pkt)
    assert got == [b"x"], "duplicate inner seq must not be redelivered"
    # Receiver re-ACKs so a sender whose ACK was lost stops retransmitting.
    _, (flags, ack, _, _, data) = decode(pair.crypto, pair.tb.sent[-1][1])
    assert flags == FLAG_ACK and data == b"" and ack == 2


async def test_replayed_packet_dropped_by_outer_watermark(pair):
    # §3.3: an exact replay (same outer sequence) is rejected before any
    # channel state is touched.
    await pair.establish()
    got = []
    pair.b.on_receive(got.append)
    pair.a.send(b"x")
    _, pkt = pair.ta.sent.pop(0)
    pair.b.handle_packet(A_ADDR, pkt)
    tb_frames = len(pair.tb.sent)
    pair.b.handle_packet(A_ADDR, pkt)  # replay
    assert got == [b"x"]
    assert len(pair.tb.sent) == tb_frames, "replay must not even be re-acked"


async def test_rto_retransmission_fresh_outer_sequence(pair):
    # §6.4: after RTO the frame is re-sent — actually re-sent on the wire —
    # re-encrypted under a NEW outer sequence, same (flags, inner seq, data).
    await pair.establish()
    pair.a.rto = 0.05
    pair.a.send(b"lost")
    outer1, (flags1, _, _, seq1, data1) = decode(pair.crypto, pair.ta.sent[0][1])
    pair.ta.sent.clear()  # drop it: never delivered

    await asyncio.sleep(0.15)
    assert pair.ta.sent, "unacked frame must be retransmitted after RTO"
    outer2, (flags2, _, _, seq2, data2) = decode(pair.crypto, pair.ta.sent[0][1])
    assert outer2 != outer1, "retransmission must NOT reuse the outer sequence (nonce)"
    assert outer2 > outer1
    assert (flags2, seq2, data2) == (flags1, seq1, data1) == (FLAG_ACK, 1, b"lost")

    got = []
    pair.b.on_receive(got.append)
    pair.deliver()
    assert got == [b"lost"]
    assert not pair.a._send_buffer


async def test_cumulative_ack_clears_send_buffer(pair):
    # §6.1: ack_num acknowledges every frame with seq < ack_num; the LAST ack
    # alone must clear the whole buffer.
    await pair.establish()
    for payload in (b"1", b"2", b"3"):
        pair.a.send(payload)
    for _, pkt in list(pair.ta.sent):
        pair.b.handle_packet(A_ADDR, pkt)
    pair.ta.sent.clear()
    assert len(pair.a._send_buffer) == 3

    last_ack = pair.tb.sent[-1][1]
    pair.tb.sent.clear()
    pair.a.handle_packet(B_ADDR, last_ack)
    assert not pair.a._send_buffer, "one cumulative ack must clear all three"


async def test_window_full_queues_then_drains(pair):
    # §6.4 flow control: frames beyond min(32, peer window) are QUEUED, not
    # dropped (the old code silently dropped — data loss), then drained as
    # acks arrive.
    await pair.establish()
    pair.a._remote_window = 2  # peer advertised a tiny window
    got = []
    pair.b.on_receive(got.append)
    chunks = [bytes([0x30 + i]) for i in range(5)]
    for c in chunks:
        pair.a.send(c)
    assert len(pair.ta.sent) == 2, "only the window's worth may be in flight"
    assert len(pair.a._send_queue) == 3
    assert len(pair.a._send_buffer) == 2

    pair.deliver()  # acks re-advertise window=32 and drain the queue
    assert got == chunks, "queued frames must all arrive, in order"
    assert not pair.a._send_queue and not pair.a._send_buffer


async def test_send_respects_advertised_window_of_32(pair):
    # §6.4: default sliding window is 32 frames in flight.
    await pair.establish()
    for i in range(40):
        pair.a.send(bytes([i]))
    assert len(pair.ta.sent) == 32
    assert len(pair.a._send_queue) == 8


# -------------------------------------------------------------------- close

async def test_fin_close_initiated_by_opener(pair):
    # §6.3: FIN -> FIN+ACK -> ACK; §6.5 both ends CLOSED.
    await pair.establish()
    close_task = asyncio.create_task(pair.a.close())
    await asyncio.sleep(0)  # let close() transmit FIN before shuttling
    _, (flags, _, _, seq, _) = decode(pair.crypto, pair.ta.sent[0][1])
    assert flags == (FLAG_FIN | FLAG_ACK) and seq == 1  # FIN consumes a seq
    pair.deliver()
    await asyncio.wait_for(close_task, timeout=1.0)
    assert pair.a._state == "CLOSED"
    assert pair.b._state == "CLOSED"
    assert not pair.a._send_buffer and not pair.b._send_buffer


async def test_fin_close_initiated_by_responder(pair):
    await pair.establish()
    close_task = asyncio.create_task(pair.b.close())
    await asyncio.sleep(0)
    pair.deliver()
    await asyncio.wait_for(close_task, timeout=1.0)
    assert pair.a._state == "CLOSED"
    assert pair.b._state == "CLOSED"
    with pytest.raises(RuntimeError):
        pair.a.send(b"nope")


async def test_fin_queues_behind_pending_data(pair):
    # FIN must consume the LAST inner seq: data queued before close() still
    # arrives (close cannot reorder ahead of window-blocked frames).
    await pair.establish()
    pair.a._remote_window = 1
    got = []
    pair.b.on_receive(got.append)
    pair.a.send(b"first")
    pair.a.send(b"second")  # window-blocked: queued
    close_task = asyncio.create_task(pair.a.close())
    await asyncio.sleep(0)
    pair.deliver()
    await asyncio.wait_for(close_task, timeout=1.0)
    assert got == [b"first", b"second"]
    assert pair.a._state == "CLOSED" and pair.b._state == "CLOSED"


# ------------------------------------------------------------- fragmentation

async def test_send_message_5000_bytes_round_trips(pair):
    # §6.6: 5000 B -> 5 enveloped chunks (<=1003 B data each), reassembled.
    await pair.establish()
    messages = []
    pair.b.on_message(messages.append)
    payload = os.urandom(5000)
    pair.a.send_message(payload)
    assert len(pair.ta.sent) == 5
    pair.deliver()
    assert messages == [payload]
    assert not pair.a._send_buffer


async def test_small_message_still_uses_envelope(pair):
    # §6.6: even a <=1-chunk message is enveloped so the parser is stateless.
    await pair.establish()
    pair.a.send_message(b"small")
    _, (_, _, _, _, data) = decode(pair.crypto, pair.ta.sent[0][1])
    assert len(data) == 21 + 5  # FRAG_HEADER_LEN + payload
    pair.deliver()
    assert await pair.b.recv_message(timeout=0.1) == b"small"


# ------------------------------------------------------- drops / key gating

async def test_reserved_flag_bits_dropped_on_receive(pair):
    await pair.establish()
    outer = pair.a._seq_source.next()
    aad = bytes([ChannelType.CHANNEL, CHANNEL_ID >> 8, CHANNEL_ID & 0xFF])
    payload = pack_frame(0x10 | FLAG_ACK, 0, 32, 1, b"evil")
    ct = pair.crypto.encrypt(outer, payload, aad)
    pkt = pack_packet(pair.crypto.band_id, ChannelType.CHANNEL, CHANNEL_ID, outer, ct)
    before = pair.b._rcv_next
    pair.b.handle_packet(A_ADDR, pkt)
    assert pair.b._rcv_next == before
    assert not pair.b._recv_buffer and not pair.b._recv_queue


async def test_no_session_key_drops_packet(pair):
    # recv_crypto -> None means the peer's HELLO hasn't been seen: the packet
    # cannot be authenticated yet and MUST be dropped, not buffered.
    keyless = Channel(pair.crypto.band_id, CHANNEL_ID, A_ADDR,
                      transport=MockTransport(),
                      recv_crypto=lambda addr: None,
                      send_crypto=lambda addr: pair.crypto,
                      seq_source=SequenceSource(start=3_000))
    await pair.a.open()
    _, pkt = pair.ta.sent.pop(0)
    keyless.handle_packet(A_ADDR, pkt)
    assert keyless._state == "CLOSED"
    assert keyless.transport.sent == []


# ------------------------------------------------------------- nonce safety

async def test_band_control_stream_channel_share_outer_sequences():
    # THE property this phase exists for: every packet one Band emits —
    # Control, Stream, and now Channel — draws its outer sequence (= AEAD
    # nonce, §3.3) from ONE shared source, so no two packets ever collide.
    # The old channel.py's per-object counter starting at 0 breaks this.
    from telesthete.band import Band
    from telesthete.protocol.control import ControlMessageType

    band = Band(psk="nonce-prop-psk", bind_port=0)
    sent = []
    band.transport.send = lambda dest, pkt: sent.append(pkt)
    dest = ("d", 1)

    band.control.add_destination(dest)
    band.control.send_message(ControlMessageType.KEEPALIVE, {}, dest=dest)

    stream = band.stream(1)
    stream.add_destination(dest)
    for _ in range(5):
        stream.send(b"s")

    ch = band.channel(2, dest)
    try:
        await ch.open()                  # SYN
        ch._state = "ESTABLISHED"        # bypass handshake: send-side property
        for _ in range(5):
            ch.send(b"c")
        ch.send_message(os.urandom(2500))  # + 3 fragmented frames

        assert ch._seq_source is band.seq_source
        seqs = [unpack_packet(p).sequence for p in sent]
        assert len(seqs) == 1 + 5 + 1 + 5 + 3
        assert len(set(seqs)) == len(seqs), \
            "one Band must never repeat an outer sequence (AEAD nonce)"
    finally:
        if ch._retransmit_task is not None:
            ch._retransmit_task.cancel()
