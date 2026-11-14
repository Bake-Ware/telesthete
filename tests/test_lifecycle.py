"""Link lifecycle (SPEC §4.11 reconnection, §6.7 Channel lifecycle) over a mock
transport: one test per condition of spec/terminating-endpoints.md §7.1, each
with `reconnect` on (recovers) and off (behaves as before).

The mock network replaces the Band's UDPTransport, delivers each datagram on
the next loop iteration, and can drop traffic to model loss and partitions.
"""

import asyncio
import json
import os
import sys
import time
from collections import defaultdict

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

import telesthete.band as band_mod
from telesthete import Band
from telesthete.protocol.channel import (
    Channel, pack_frame, unpack_frame, FLAG_SYN, FLAG_ACK, FLAG_FIN, FLAG_RST,
)
from telesthete.protocol.crypto import BandCrypto
from telesthete.protocol.framing import pack_packet, unpack_packet, ChannelType
from telesthete.protocol.fragment import Fragmenter
from telesthete.protocol.sequence import SequenceSource

PSK = "lifecycle-test-psk"
A = ("10.0.0.1", 1)
B = ("10.0.0.2", 2)
C = ("10.0.0.3", 3)


# ----------------------------------------------------------------- mock net

class MockNet:
    """In-memory datagram network keyed by (host, port)."""

    def __init__(self):
        self.nodes = {}
        self.partitioned = False
        self.log = []  # (time, src, dst, packet) of every datagram sent

    def transport(self, bind_address, bind_port):
        return MockTransport(self, (bind_address, bind_port))

    def send(self, src, dst, packet):
        self.log.append((time.monotonic(), src, dst, packet))
        if self.partitioned:
            return
        asyncio.get_running_loop().call_soon(self._deliver, src, dst, packet)

    def _deliver(self, src, dst, packet):
        node = self.nodes.get(dst)
        if node is not None:
            node.receive(src, packet)

    def sent(self, src, dst, channel_type=None, since=0.0):
        return [p for (t, s, d, p) in self.log
                if s == src and d == dst and t >= since
                and (channel_type is None or p[16] == channel_type)]


class MockTransport:
    """The transport interface the Band uses (start/run/stop,
    register_handler, send, local_address)."""

    def __init__(self, net, addr):
        self.net = net
        self.addr = addr
        self.local_address = None
        self.handlers = defaultdict(list)

    def start(self):
        self.local_address = self.addr
        self.net.nodes[self.addr] = self

    async def run(self):
        await asyncio.Event().wait()

    async def stop(self):
        if self.net.nodes.get(self.addr) is self:
            del self.net.nodes[self.addr]

    def register_handler(self, channel_type, handler):
        self.handlers[channel_type].append(handler)

    def send(self, dest, packet):
        self.net.send(self.addr, dest, packet)

    def receive(self, src, packet):
        if len(packet) < 17:
            return
        for handler in self.handlers.get(packet[16], []):
            handler(src, packet)


@pytest.fixture
def net(monkeypatch):
    n = MockNet()
    monkeypatch.setattr(band_mod, "UDPTransport", n.transport)
    return n


@pytest.fixture
def bands():
    """Collects Bands to stop at the end of a test."""
    made = []
    yield made
    for b in made:
        for c in b.channels.values():
            if c._retransmit_task is not None:
                c._retransmit_task.cancel()
        for t in b._tasks:
            t.cancel()


def make(bands, addr, epoch, reconnect=False, name=None):
    kw = {"reconnect": True} if reconnect else {}
    b = Band(PSK, hostname=name or f"h{addr[1]}", bind_address=addr[0],
             bind_port=addr[1], session_epoch=epoch, **kw)
    bands.append(b)
    return b


async def crash(band):
    """Stop a Band the way a killed process does: no GOODBYE."""
    band._running = False
    for c in band.channels.values():
        if c._retransmit_task is not None:
            c._retransmit_task.cancel()
    for t in band._tasks:
        t.cancel()
    await band.transport.stop()


async def until(predicate, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        await asyncio.sleep(0.01)
    return predicate()


def is_hello(packet):
    """True if `packet` is a HELLO (the only frame type under the base key
    that a band sends before it has a session)."""
    if packet[16] != ChannelType.CONTROL:
        return False
    p = unpack_packet(packet)
    try:
        body = BandCrypto(PSK).decrypt(p.sequence, p.ciphertext,
                                       bytes([ChannelType.CONTROL, 0, 0]))
    except Exception:
        return False
    return json.loads(body)["type"] == 1


async def connected(x, x_addr, y, y_addr):
    """x connects to y; wait for both sides to hold a session."""
    x.connect_peer(*y_addr)
    assert await until(lambda: y_addr in x.peers and x_addr in y.peers
                       and x.peers[y_addr].session_epoch >= 0, 2.0)


# ---------------------------------------------- condition 1 (decision 3)

async def test_condition1_channel_id_stays_bound_to_first_peer(net, bands):
    # §7.1 condition 1 is left as is in Python (decision 3, §6.7 (a) allows
    # it): an id is used with one peer at a time; asking for the same id with
    # another peer returns the channel bound to the first.
    a = make(bands, A, 1000)
    first = a.channel(5, B)
    again = a.channel(5, C)
    assert again is first
    assert again.peer_addr == B


# -------------------------------------------------- condition 2: accepting

async def test_condition2_accept_hook_creates_channel_for_unsolicited_syn(net, bands):
    # §6.7 (b): with Band.on_channel registered, a SYN for an id the receiver
    # has no Channel for creates it, hands it over, then processes the SYN.
    # Independent of `reconnect`.
    a = make(bands, A, 1000)
    b = make(bands, B, 2000)
    accepted = []
    got = []

    def accept(ch):
        accepted.append(ch)
        ch.on_message(got.append)

    b.on_channel(accept)
    await a.start(); await b.start()
    await connected(a, A, b, B)

    ch = a.channel(7, B)
    await ch.open()
    ch.send_message(b"hello")
    assert await until(lambda: got == [b"hello"], 2.0)
    assert len(accepted) == 1
    assert accepted[0] is b.channels[7]
    assert accepted[0].peer_addr == A
    assert accepted[0].state == "ESTABLISHED"
    assert ch.state == "ESTABLISHED"


@pytest.mark.parametrize("reconnect", [False, True])
async def test_condition2_without_hook_unsolicited_syn_is_dropped(net, bands, reconnect):
    # No accept hook: the SYN is dropped as today, and never answered with
    # RST (§6.7 (f): a SYN that beats the application's channel() call must
    # still succeed on its retransmission).
    a = make(bands, A, 1000, reconnect)
    b = make(bands, B, 2000, reconnect)
    await a.start(); await b.start()
    await connected(a, A, b, B)
    ch = a.channel(7, B)
    ch.rto = 0.05
    await ch.open()
    await asyncio.sleep(0.4)  # several retransmitted SYNs
    assert ch._state == "SYN_SENT"
    assert 7 not in b.channels
    assert net.sent(B, A, ChannelType.CHANNEL) == []

    # The application now creates the channel: the next retransmission wins.
    got = []
    b.channel(7, A).on_message(got.append)
    ch.send_message(b"late")
    assert await until(lambda: got == [b"late"], 2.0)


# --------------------------------------------- condition 3: lost HELLO

@pytest.mark.parametrize("reconnect", [False, True])
async def test_condition3_lost_hello(net, bands, reconnect):
    # The first HELLO goes to a receiver that is not bound yet. With
    # reconnection (§4.11 step 2) it is resent until answered; without, the
    # band never gets a peer (as on main).
    a = make(bands, A, 1000, reconnect)
    b = make(bands, B, 2000, reconnect)
    await a.start()
    a.connect_peer(*B)
    await asyncio.sleep(0.2)
    await b.start()
    ok = await until(lambda: B in a.peers and A in b.peers, 2.0)
    if reconnect:
        assert ok
    else:
        assert not ok
        assert a.get_peers() == [] and b.get_peers() == []


async def test_hello_retry_schedule_then_stops_when_answered(net, bands):
    # §4.11 step 2: resent after 0.5 s, 1 s, 2 s, 4 s, then every
    # KEEPALIVE_INTERVAL; each resend a fresh sequence, same epoch.
    a = make(bands, A, 1000, reconnect=True)
    b = make(bands, B, 2000, reconnect=True)
    await a.start()
    t0 = time.monotonic()
    a.connect_peer(*B)
    await asyncio.sleep(4.0)
    stamps = [t - t0 for (t, s, d, p) in net.log
              if s == A and d == B and is_hello(p)]
    assert len(stamps) == 4, stamps  # 0, 0.5, 1.5, 3.5
    gaps = [b_ - a_ for a_, b_ in zip(stamps, stamps[1:])]
    for got, want in zip(gaps, [0.5, 1.0, 2.0]):
        assert abs(got - want) < 0.2, gaps
    hellos = [unpack_packet(p) for (t, s, d, p) in net.log
              if s == A and d == B and is_hello(p)]
    assert len({h.sequence for h in hellos}) == len(hellos)
    for h in hellos:
        body = json.loads(BandCrypto(PSK).decrypt(
            h.sequence, h.ciphertext, bytes([ChannelType.CONTROL, 0, 0])))
        assert body["payload"]["session"] == 1000

    # Answered: no more HELLOs.
    await b.start()
    assert await until(lambda: B in a.peers, 6.0)
    mark = time.monotonic()
    await asyncio.sleep(1.5)
    assert [p for p in net.sent(A, B, since=mark) if is_hello(p)] == []


async def test_target_lost_after_silence_is_hello_again(net, bands, monkeypatch):
    # §4.11 step 3: nothing heard from a target for PEER_TIMEOUT -> evicted
    # (§4.5) and step 2 resumes. PEER_TIMEOUT shortened for the test.
    monkeypatch.setattr(band_mod, "_PEER_TIMEOUT", 0.6)
    a = make(bands, A, 1000, reconnect=True)
    b = make(bands, B, 2000)
    await a.start(); await b.start()
    await connected(a, A, b, B)

    net.partitioned = True
    mark = time.monotonic()
    assert await until(lambda: B not in a.peers, 2.0)
    assert await until(lambda: any(is_hello(p) for p in net.sent(A, B, since=mark)), 1.0)
    net.partitioned = False
    assert await until(lambda: B in a.peers, 3.0)


async def test_target_sending_only_unauthenticated_frames_is_lost(net, bands, monkeypatch):
    # §4.11 step 3 counts authenticated arrivals only. Frames from the
    # target's address that fail authentication (for example a restarted
    # peer's, under an epoch we do not know) do not keep it alive.
    monkeypatch.setattr(band_mod, "_PEER_TIMEOUT", 0.6)
    a = make(bands, A, 1000, reconnect=True)
    b = make(bands, B, 2000)
    await a.start(); await b.start()
    await connected(a, A, b, B)
    net.partitioned = True
    junk = pack_packet(a.band_id, ChannelType.CHANNEL, 0, 1, os.urandom(40))
    mark = time.monotonic()
    while B in a.peers and time.monotonic() - mark < 2.0:
        net._deliver(B, A, junk)
        await asyncio.sleep(0.05)
    assert B not in a.peers
    assert any(is_hello(p) for p in net.sent(A, B, since=mark))


# ------------------------------------- condition 4: restarted peer reopens

@pytest.mark.parametrize("reconnect", [False, True])
async def test_condition4_restarted_peer_reopens_channel(net, bands, reconnect):
    # B opens channel 0 to A, restarts (new process: same address, newer
    # epoch) and opens channel 0 again. With §4.11 step 4 + §6.7 (c)/(d) A
    # resets its channel and accepts the new SYN; without, A keeps the old
    # state and B stays in SYN_SENT (as on main).
    a = make(bands, A, 1000, reconnect)
    b = make(bands, B, 2000, reconnect)
    got, resets = [], []
    a_ch = a.channel(0, B)
    a_ch.on_message(got.append)
    if reconnect:
        a_ch.on_reset(resets.append)
    await a.start(); await b.start()
    await connected(b, B, a, A)
    b_ch = b.channel(0, A)
    await b_ch.open()
    b_ch.send_message(b"one")
    assert await until(lambda: got == [b"one"], 2.0)

    await b.stop()  # GOODBYE
    b2 = make(bands, B, 3000, reconnect)
    await b2.start()
    b2.connect_peer(*A)
    b2_ch = b2.channel(0, A)
    await b2_ch.open()
    b2_ch.send_message(b"two")
    if reconnect:
        assert await until(lambda: got == [b"one", b"two"], 3.0)
        assert resets and resets[0] == "peer_left"
        assert b2_ch.state == "ESTABLISHED"
    else:
        await asyncio.sleep(1.5)
        assert got == [b"one"]
        assert b2_ch._state == "SYN_SENT"


async def test_condition4_crashed_peer_new_session_resets_only_its_channels(net, bands):
    # No GOODBYE: the newer epoch in B's HELLO resets every Channel A has
    # with B (§4.11 step 4) with reason peer_restarted, and no other.
    a = make(bands, A, 1000, reconnect=True)
    b = make(bands, B, 2000, reconnect=True)
    c = make(bands, C, 5000, reconnect=True)
    got, resets, c_resets = [], [], []
    a.on_channel(lambda ch: ch.on_message(got.append))
    await a.start(); await b.start(); await c.start()
    await connected(b, B, a, A)
    await connected(c, C, a, A)
    b_ch = b.channel(0, A)
    await b_ch.open()
    c_ch = c.channel(1, A)
    await c_ch.open()
    assert await until(lambda: 0 in a.channels and 1 in a.channels
                       and a.channels[1].state == "ESTABLISHED", 2.0)
    a.channels[0].on_reset(resets.append)
    a.channels[1].on_reset(c_resets.append)
    b_ch.send_message(b"one")
    assert await until(lambda: got == [b"one"], 2.0)

    await crash(b)
    b2 = make(bands, B, 3000, reconnect=True)
    await b2.start()
    b2.connect_peer(*A)
    assert await until(lambda: resets == ["peer_restarted"], 2.0)
    assert a.channels[0].state == "CLOSED"
    b2_ch = b2.channel(0, A)
    await b2_ch.open()
    b2_ch.send_message(b"two")
    assert await until(lambda: got == [b"one", b"two"], 3.0)
    assert c_resets == []
    assert a.channels[1].state == "ESTABLISHED"


# ------------------------------------ condition 5: silent peer, give-up

@pytest.mark.parametrize("reconnect", [False, True])
async def test_condition5_give_up_resets_and_channel_reopens(net, bands, reconnect):
    # A loses B mid-message and gives up. With §6.7 (g) the channel is reset
    # (reopenable, inner seqs 0); after the outage A reopens the same object
    # and B, still ESTABLISHED, takes the SYN as a new incarnation (§6.7 (e)).
    # Without: the channel cannot be brought back (as on main).
    a = make(bands, A, 1000, reconnect)
    b = make(bands, B, 2000, reconnect)
    got, resets, b_resets = [], [], []
    b_ch = b.channel(0, A)
    b_ch.on_message(got.append)
    if reconnect:
        b_ch.on_reset(b_resets.append)
    await a.start(); await b.start()
    await connected(a, A, b, B)
    a_ch = a.channel(0, B)
    if reconnect:
        a_ch.on_reset(resets.append)
    await a_ch.open()
    a_ch.send_message(b"one")
    assert await until(lambda: got == [b"one"], 2.0)

    net.partitioned = True
    a_ch.rto = 0.02
    a_ch.send_message(b"lost")
    assert await until(lambda: a_ch._state == "CLOSED", 2.0)
    net.partitioned = False
    assert a.channel(0, B) is a_ch

    if reconnect:
        assert resets == ["peer_unresponsive"]
        assert a_ch._snd_next == 0 and a_ch._rcv_next == 0
        a_ch.rto = 0.5
        await a_ch.open()
        a_ch.send_message(b"two")
        assert await until(lambda: got == [b"one", b"two"], 3.0)
        assert b_resets == ["peer_reset"]
        assert a_ch.state == "ESTABLISHED" and b_ch.state == "ESTABLISHED"
    else:
        await a_ch.open()
        a_ch.send_message(b"two")
        await asyncio.sleep(1.0)
        assert got == [b"one"]


async def test_condition5_responder_gave_up_rst_brings_initiator_back(net, bands):
    # §7.3 "short outage": the responder gave up; the initiator's next data
    # draws an RST (§6.7 (f)), it resets and reopens.
    a = make(bands, A, 1000, reconnect=True)
    b = make(bands, B, 2000, reconnect=True)
    got, a_resets, b_resets = [], [], []
    b_ch = b.channel(0, A)
    b_ch.on_reset(b_resets.append)
    await a.start(); await b.start()
    await connected(a, A, b, B)
    a_ch = a.channel(0, B)
    a_ch.on_message(got.append)

    def reopen(reason):
        a_resets.append(reason)
        asyncio.get_running_loop().create_task(a_ch.open())

    a_ch.on_reset(reopen)
    await a_ch.open()
    assert await until(lambda: b_ch.state == "ESTABLISHED", 2.0)

    net.partitioned = True
    b_ch.rto = 0.02
    b_ch.send_message(b"lost")
    assert await until(lambda: b_resets == ["peer_unresponsive"], 2.0)
    net.partitioned = False

    a_ch.send_message(b"from-a")  # B holds the channel CLOSED -> RST
    assert await until(lambda: a_resets == ["peer_reset"], 2.0)
    assert await until(lambda: a_ch.state == "ESTABLISHED"
                       and b_ch.state == "ESTABLISHED", 2.0)
    b_ch.rto = 0.5
    b_ch.send_message(b"again")
    assert await until(lambda: got == [b"again"], 2.0)


# --------------------------------------------------------------- §6.7 (f)

def decode_channel(crypto, packet):
    p = unpack_packet(packet)
    aad = bytes([ChannelType.CHANNEL, p.channel_id >> 8, p.channel_id & 0xFF])
    return unpack_frame(crypto.decrypt(p.sequence, p.ciphertext, aad))


@pytest.mark.parametrize("reconnect", [False, True])
async def test_rst_answers_data_for_unknown_channel(net, bands, reconnect):
    # B holds no channel 9. Authenticated data on it draws one RST with
    # reconnect on (flags 0x08, ack 0, window 0, seq 0, no data); nothing
    # with it off (as on main).
    a = make(bands, A, 1000)
    b = make(bands, B, 2000, reconnect)
    b.channel(1, A)  # registers the CHANNEL handler, as on main
    await a.start(); await b.start()
    await connected(a, A, b, B)
    ch = a.channel(9, B)
    ch._state = "ESTABLISHED"  # A believes the channel is up (B lost it)
    ch.send_message(b"x")
    await asyncio.sleep(0.1)
    replies = net.sent(B, A, ChannelType.CHANNEL)
    if reconnect:
        assert len(replies) == 1
        assert decode_channel(b.send_crypto(A), replies[0]) == (FLAG_RST, 0, 0, 0, b"")
        assert ch._state == "CLOSED"  # A tears down on RST, as today
    else:
        assert replies == []
        assert ch._state == "ESTABLISHED"
    ch._retransmit_task.cancel()


# ------------------------------------------------ Channel-level §6.7 rules

CH = 9
A_ADDR = ("a", 1)
B_ADDR = ("b", 2)
AAD = bytes([ChannelType.CHANNEL, CH >> 8, CH & 0xFF])


class Wire:
    def __init__(self):
        self.sent = []

    def send(self, dest, packet):
        self.sent.append(packet)


class Pair:
    """Two Channels joined by hand, with the §6.7 behaviour switched on as a
    Band with reconnect=True does."""

    def __init__(self, lifecycle_a=True, lifecycle_b=True):
        self.crypto = BandCrypto("channel-lifecycle-psk")
        self.wa, self.wb = Wire(), Wire()
        self.a = Channel(self.crypto.band_id, CH, B_ADDR, transport=self.wa,
                         crypto=self.crypto, seq_source=SequenceSource(start=1000))
        self.b = Channel(self.crypto.band_id, CH, A_ADDR, transport=self.wb,
                         crypto=self.crypto, seq_source=SequenceSource(start=2000))
        self.a._lifecycle = lifecycle_a
        self.b._lifecycle = lifecycle_b

    def deliver(self):
        while self.wa.sent or self.wb.sent:
            while self.wa.sent:
                self.b.handle_packet(A_ADDR, self.wa.sent.pop(0))
            while self.wb.sent:
                self.a.handle_packet(B_ADDR, self.wb.sent.pop(0))

    def inject(self, to, flags, ack, window, seq, data=b"", source=None):
        """Deliver a hand-made authenticated frame to channel `to`."""
        source = source or SequenceSource(start=900_000)
        outer = source.next()
        ct = self.crypto.encrypt(outer, pack_frame(flags, ack, window, seq, data), AAD)
        pkt = pack_packet(self.crypto.band_id, ChannelType.CHANNEL, CH, outer, ct)
        to.handle_packet(B_ADDR if to is self.a else A_ADDR, pkt)
        return outer

    def cleanup(self):
        for ch in (self.a, self.b):
            if ch._retransmit_task is not None:
                ch._retransmit_task.cancel()


@pytest.fixture
def pair():
    p = Pair()
    yield p
    p.cleanup()


def env(message):
    return Fragmenter().split(message)[0]


async def test_reset_clears_state_keeps_watermark_and_tells_application(pair):
    # §6.7 (c): CLOSED, inner seqs 0, queued/unacked/out-of-order/partial data
    # gone, retransmission stopped, outer watermark untouched, app told.
    resets = []
    pair.a.on_reset(resets.append)
    await pair.a.open()
    pair.deliver()
    pair.a.rto = 0.02
    pair.a.send_message(os.urandom(3000))  # 3 chunks in flight
    pair.wa.sent.clear()                   # lost
    outer = pair.inject(pair.a, FLAG_RST, 0, 0, 0)
    assert resets == ["peer_reset"]
    assert pair.a.state == "CLOSED"
    assert (pair.a._snd_next, pair.a._rcv_next) == (0, 0)
    assert not pair.a._send_buffer and not pair.a._send_queue
    assert not pair.a._recv_buffer
    assert pair.a._recv_watermark[B_ADDR] == outer
    await asyncio.sleep(0.1)
    assert pair.wa.sent == [], "a reset channel must not retransmit"
    with pytest.raises(AttributeError):
        pair.a.state = "ESTABLISHED"  # read-only


async def test_rst_never_for_syn_pure_ack_or_rst(pair):
    # §6.7 (f): a CLOSED channel answers data/FIN with one RST, and never a
    # SYN, a pure ACK or an RST. RST consumes no seq, is not retransmitted.
    src = SequenceSource(start=900_000)
    pair.inject(pair.b, FLAG_ACK, 0, 32, 0, source=src)
    pair.inject(pair.b, FLAG_RST, 0, 0, 0, source=src)
    assert pair.wb.sent == []
    pair.inject(pair.b, FLAG_ACK, 1, 32, 3, env(b"stale"), source=src)
    pair.inject(pair.b, FLAG_FIN | FLAG_ACK, 1, 32, 4, source=src)
    assert len(pair.wb.sent) == 2
    for pkt in pair.wb.sent:
        assert decode_channel(pair.crypto, pkt) == (FLAG_RST, 0, 0, 0, b"")
    assert pair.b._snd_next == 0 and not pair.b._send_buffer
    pair.wb.sent.clear()
    pair.inject(pair.b, FLAG_SYN, 0, 32, 0, source=src)
    flags = decode_channel(pair.crypto, pair.wb.sent[0])[0]
    assert flags == FLAG_SYN | FLAG_ACK


async def test_retransmitted_syn_before_ack_is_not_a_new_incarnation(pair):
    # §6.7 (e): before the peer has acknowledged our SYN+ACK, a SYN is a
    # retransmission and is answered as before (re-ACK), not a reset.
    resets = []
    pair.b.on_reset(resets.append)
    await pair.a.open()
    syn = pair.wa.sent.pop(0)
    pair.b.handle_packet(A_ADDR, syn)
    assert pair.b.state == "ESTABLISHED"
    pair.wb.sent.clear()                  # our SYN+ACK lost
    pair.inject(pair.b, FLAG_SYN, 0, 32, 0)  # the initiator's retransmission
    assert resets == []
    assert pair.b.state == "ESTABLISHED"
    flags, ack, _, _, _ = decode_channel(pair.crypto, pair.wb.sent[-1])
    assert flags == FLAG_ACK and ack == 1


async def test_new_incarnation_syn_after_ack_resets_and_establishes(pair):
    # §6.7 (e): after the peer acknowledged our SYN+ACK, a SYN without ACK
    # means it reopened: reset, then handle the SYN as in CLOSED.
    resets = []
    pair.b.on_reset(resets.append)
    got = []
    pair.b.on_message(got.append)
    await pair.a.open()
    pair.deliver()
    pair.a.send_message(b"one")
    pair.deliver()
    old = pair.a
    old._retransmit_task.cancel()
    a2 = Channel(pair.crypto.band_id, CH, B_ADDR, transport=pair.wa,
                 crypto=pair.crypto, seq_source=SequenceSource(start=5000))
    a2._lifecycle = True
    pair.a = a2
    await a2.open()
    pair.deliver()
    a2.send_message(b"two")
    pair.deliver()
    assert resets == ["peer_reset"]
    assert got == [b"one", b"two"]
    assert a2.state == "ESTABLISHED" and pair.b.state == "ESTABLISHED"


async def test_closed_channel_takes_syn_seq_as_start_with_reconnect_off():
    # §6.7 (d), second half, always on: a CLOSED channel that receives a SYN
    # expects seq + 1 next, whatever its old inner state. On main a channel
    # closed by FIN (rcv_next > 0) took a reopened SYN at seq 0 for a
    # duplicate and never established.
    p = Pair(lifecycle_a=True, lifecycle_b=False)
    try:
        got = []
        p.b.on_message(got.append)
        await p.a.open()
        p.deliver()
        p.a.send_message(b"one")
        p.deliver()
        closing = asyncio.ensure_future(p.a.close())
        await asyncio.sleep(0)
        p.deliver()
        await closing
        assert p.b._state == "CLOSED" and p.b._rcv_next > 0

        await p.a.open()  # reopened: SYN at inner seq 0 (§6.7 (d))
        assert decode_channel(p.crypto, p.wa.sent[0])[3] == 0
        p.deliver()
        assert p.b._state == "ESTABLISHED"
        assert p.a.state == "ESTABLISHED"
        p.a.send_message(b"two")
        p.deliver()
        assert got == [b"one", b"two"]
    finally:
        p.cleanup()


async def test_reconnect_off_reopen_keeps_main_sequence_behaviour():
    # With reconnect off, open() after a close keeps today's inner sequence
    # (no reset), exactly as on main.
    p = Pair(lifecycle_a=False, lifecycle_b=False)
    try:
        await p.a.open()
        p.deliver()
        closing = asyncio.ensure_future(p.a.close())
        await asyncio.sleep(0)
        p.deliver()
        await closing
        before = p.a._snd_next
        assert before > 0
        await p.a.open()
        assert decode_channel(p.crypto, p.wa.sent[0])[3] == before
    finally:
        p.cleanup()
