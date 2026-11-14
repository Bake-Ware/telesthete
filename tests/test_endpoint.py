"""Endpoint: many keys on one set of transports, terminating mode
(SPEC §3.6, §10.2; spec/terminating-endpoints.md §4, §5).

One test per conformance item T1 to T15 of
spec/terminating-endpoint-conformance.md, named for the item (T10, and the
WebSocket half of T12, are in test_endpoint_websocket.py), then the tests of
spec/terminating-endpoints.md §4.7 and §5.8: 50 keys, rekey, keys added and
removed under load, restarts of either side, and the replay probe as a test of
T15 and of the stated non-claim.

Path mode runs over the in-memory paths of endpoint_harness; UDP mode over
loopback, every socket on a port the OS picks.
"""

import asyncio
import logging
import os
import socket
import sys
import time

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
sys.path.insert(0, os.path.dirname(__file__))

from telesthete import Band, Endpoint  # noqa: E402
from telesthete.endpoint import RECEIVE_QUEUE_FRAMES  # noqa: E402
from telesthete.protocol import route  # noqa: E402
from telesthete.protocol.channel import ResetReason  # noqa: E402
from telesthete.protocol.crypto import (  # noqa: E402
    BandCrypto, derive_band_id, derive_encryption_key)
from telesthete.protocol.framing import ChannelType, pack_packet  # noqa: E402

from endpoint_harness import (  # noqa: E402
    LO, Link, Net, Peer, Served, current_path, kill, next_epoch, until)

pytestmark = pytest.mark.asyncio


@pytest.fixture
def decrypts(monkeypatch):
    """Every BandCrypto.decrypt call, as the band_id of the key used."""
    calls = []
    original = BandCrypto.decrypt

    def counting(self, *args, **kwargs):
        calls.append(self.band_id)
        return original(self, *args, **kwargs)

    monkeypatch.setattr(BandCrypto, "decrypt", counting)
    return calls


async def path_mode(*psks):
    """A started path-mode Endpoint serving `psks`, one peer per key on path
    ("p", i), each with channel 0 established."""
    net = Net()
    ep = net.endpoint()
    await ep.start()
    served = [Served(ep, psk) for psk in psks]
    peers = [await Peer.on_link(net, psk, ("p", i)).start()
             for i, psk in enumerate(psks)]
    assert await until(lambda: all(p.up for p in peers))
    return net, ep, served, peers


async def shutdown(ep, *peers):
    for p in peers:
        if p.band._running:
            await p.band.stop()
    await ep.stop()


async def udp_endpoint(*psks, port=0, epoch=None):
    ep = Endpoint(hostname="hub", bind_address=LO, bind_port=port,
                  session_epoch=epoch or next_epoch())
    served = [Served(ep, psk) for psk in psks]
    await ep.start()
    return ep, served


def ep_addr(ep):
    return ep._local_address


def forged(band_id, channel_type, channel_id=0):
    """A frame bearing `band_id` that authenticates under no key."""
    return pack_packet(band_id, channel_type, channel_id,
                       int.from_bytes(os.urandom(7), "big"), os.urandom(48))


# ------------------------------------------------------------------- T1

async def test_t1_short_and_unknown_band_id_dropped_without_decrypt(decrypts):
    net = Net()
    ep = net.endpoint()
    await ep.start()
    Served(ep, "k1")
    held = derive_band_id("k1")
    try:
        ep.receive(("x", 1), b"\x00" * 10)
        ep.receive(("x", 1), held + b"\x00" * 26)            # 42 bytes
        ep.receive(("x", 1), forged(derive_band_id("not-held"),
                                    ChannelType.CONTROL))
        await asyncio.sleep(0.2)
        assert ep.dropped_unknown == 3
        assert decrypts == []
        assert net.sent == []
    finally:
        await ep.stop()


async def test_t1_udp_mode(decrypts):
    ep, _ = await udp_endpoint("k1")
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.sendto(b"\x00" * 10, ep_addr(ep))
        s.sendto(forged(derive_band_id("not-held"), ChannelType.STREAM, 1),
                 ep_addr(ep))
        assert await until(lambda: ep.dropped_unknown == 2)
        await asyncio.sleep(0.1)
        assert decrypts == []
    finally:
        s.close()
        await ep.stop()


# ------------------------------------------------------------------- T2

async def test_t2_no_answer_to_a_frame_that_fails_authentication():
    net = Net()
    ep = net.endpoint()
    await ep.start()
    s = Served(ep, "k1")
    held = s.band.band_id
    attempts = [forged(held, ct) for ct in (
        ChannelType.CONTROL, ChannelType.STREAM, ChannelType.CHANNEL,
        ChannelType.BOARD, ChannelType.DROP, ChannelType.ADDRESSED)]
    attempts.append(forged(held, ChannelType.CHANNEL, 7))   # unknown channel
    # A real HELLO under another key, re-headed with the held band_id.
    other = Peer.on_link(net, "other-key", ("o", 0))
    await other.band.start()
    other.band.connect_peer(*Link.EP)
    hello = other.link.sent[0]
    attempts.append(held + hello[16:])
    try:
        for frame in attempts:
            ep.receive(("evil", 1), frame)
        await asyncio.sleep(0.5)
        assert net.sent == []                 # no session: nothing at all
        p = await Peer.on_link(net, "k1", ("p", 0)).start()
        assert await until(lambda: p.up)
        before = len(net.sent)
        for frame in attempts:
            ep.receive(("evil", 1), frame)
        await asyncio.sleep(0.5)
        assert [path for path, _ in net.sent[before:]
                if path == ("evil", 1)] == []
        assert current_path(ep, s.band) == ("p", 0)
    finally:
        await other.band.stop()
        await shutdown(ep, p)


# ------------------------------------------------------------------- T3

async def test_t3_keys_are_isolated():
    net, ep, (a, b), (pa, pb) = await path_mode("key-a", "key-b")
    try:
        pa.ch.send_message(b"from a")
        pb.ch.send_message(b"from b")
        assert await until(lambda: pa.msgs and pb.msgs)
        assert a.got == [b"from a"] and b.got == [b"from b"]
        assert pa.msgs == [b"ack:from a"] and pb.msgs == [b"ack:from b"]
        # Key A's frames arriving on key B's path are handled under key A.
        net.links.pop(("p", 1))
        net.move(pa.link, ("p", 1))
        pa.ch.send_message(b"a on b's path")
        assert await until(lambda: len(pa.msgs) == 2)
        assert a.got[-1] == b"a on b's path" and b.got == [b"from b"]
        assert current_path(ep, a.band) == ("p", 1)
        assert current_path(ep, b.band) == ("p", 1)  # unchanged: never moved
    finally:
        await shutdown(ep, pa, pb)


# ------------------------------------------------------------------- T4

async def test_t4_remove_key_mid_message_then_nothing_decrypted(decrypts):
    net, ep, (a, b), (pa, pb) = await path_mode("key-a", "key-b")
    bid = a.band.band_id
    deliver = ep._deliver
    seen = []

    async def remove_after_ten(path, frame):
        if frame[:16] == bid and frame[16] == ChannelType.CHANNEL:
            seen.append(frame)
            if len(seen) == 10:
                assert ep.remove_key(bid) is True
        await deliver(path, frame)

    ep._deliver = remove_after_ten
    try:
        pa.ch.send_message(os.urandom(300_000))    # ~300 chunks
        assert await until(lambda: len(seen) >= 10)
        mark = len(decrypts)
        dropped = ep.dropped_unknown
        await asyncio.sleep(1.5)                   # peer retransmits
        assert a.got == []
        assert bid not in decrypts[mark:]
        assert ep.dropped_unknown > dropped
        assert ep.band(bid) is None and ep.band_ids() == [b.band.band_id]
        # Every other key undisturbed.
        pb.ch.send_message(b"still here")
        assert await until(lambda: pb.msgs == [b"ack:still here"])
        assert b.resets == [] and pb.resets == []
    finally:
        await shutdown(ep, pa, pb)


# ------------------------------------------------------------------- T5

async def test_t5_answers_follow_the_latest_accepted_frame():
    net, ep, (s,), (p,) = await path_mode("k1")
    try:
        net.move(p.link, ("p", 9))
        p.band.stream(1).send(b"moved")
        assert await until(lambda: s.streamed == [b"moved"])
        assert current_path(ep, s.band) == ("p", 9)
        mark = len(net.sent)
        s.band.stream(1).send(b"answer")
        assert await until(lambda: p.streamed == [b"answer"])
        assert {path for path, _ in net.sent[mark:]} == {("p", 9)}
    finally:
        await shutdown(ep, p)


async def test_t5_udp_mode_answers_from_the_address_the_peer_sent_to():
    ep, (s,) = await udp_endpoint("k1")
    p = await Peer.on_udp("k1", ep_addr(ep)).start()
    try:
        assert await until(lambda: p.up)
        p.ch.send_message(b"over udp")
        assert await until(lambda: p.msgs == [b"ack:over udp"])
        # The peer keys peers by source address: one peer, the one it sent to.
        assert [q.address for q in p.band.get_peers()] == [ep_addr(ep)]
        assert current_path(ep, s.band) == (LO, p.band.transport.local_address[1])
    finally:
        await p.band.stop()
        await ep.stop()


# ------------------------------------------------------------------- T6

async def test_t6_unauthenticated_replayed_or_older_epoch_frames_do_not_move_the_path():
    net, ep, (s,), (p,) = await path_mode("k1")
    old_hello = p.link.sent[0]
    try:
        # Fails authentication.
        tampered = bytearray(p.link.sent[-1])
        tampered[-1] ^= 1
        ep.receive(("evil", 1), bytes(tampered))
        # A replay of frames already accepted.
        for frame in list(p.link.sent):
            ep.receive(("evil", 2), frame)
        await asyncio.sleep(0.3)
        assert current_path(ep, s.band) == ("p", 0)
        # An older-epoch HELLO, after the peer restarted to a newer epoch.
        await kill(p.band)
        net.links.pop(("p", 0))
        p2 = await Peer.on_link(net, "k1", ("p", 1)).start()
        assert await until(lambda: p2.up and current_path(ep, s.band) == ("p", 1))
        ep.receive(("evil", 3), old_hello)
        await asyncio.sleep(0.3)
        assert current_path(ep, s.band) == ("p", 1)
        p2.ch.send_message(b"newer session")
        assert await until(lambda: p2.msgs == [b"ack:newer session"])
    finally:
        await shutdown(ep, p2)


# ------------------------------------------------------------------- T7

async def test_t7_new_path_keeps_session_and_channels():
    net, ep, (s,), (p,) = await path_mode("k1")
    try:
        p.ch.send_message(b"one")
        assert await until(lambda: p.msgs == [b"ack:one"])
        mark = len(net.sent)
        net.move(p.link, ("p", 1))            # a reconnect on a new path
        p.ch.send_message(b"two")
        assert await until(lambda: p.msgs == [b"ack:one", b"ack:two"])
        # The answer to the first frame on the new path went out on it too.
        assert {path for path, _ in net.sent[mark:]} == {("p", 1)}
        assert p.resets == [] and s.resets == []
        assert s.ch is not None and s.ch.state == "ESTABLISHED"
    finally:
        await shutdown(ep, p)


class Rebinder:
    """A UDP NAT between a peer and the Endpoint. The peer sends to `front`;
    frames reach the Endpoint from an upstream socket, which `rebind`
    replaces, so the Endpoint sees a new source port."""

    def __init__(self, ep_address):
        self.ep_address = ep_address
        self.peer_address = None

    async def start(self):
        loop = asyncio.get_running_loop()
        self.front, _ = await loop.create_datagram_endpoint(
            lambda: _Recv(self._from_peer), local_addr=(LO, 0))
        await self.rebind()
        return self

    async def rebind(self):
        loop = asyncio.get_running_loop()
        old = getattr(self, "up", None)
        self.up, _ = await loop.create_datagram_endpoint(
            lambda: _Recv(self._from_ep), local_addr=(LO, 0))
        if old is not None:
            old.close()

    @property
    def address(self):
        return self.front.get_extra_info("sockname")[:2]

    def _from_peer(self, data, addr):
        self.peer_address = addr
        self.up.sendto(data, self.ep_address)

    def _from_ep(self, data, addr):
        self.front.sendto(data, self.peer_address)

    def close(self):
        self.front.close()
        self.up.close()


class _Recv(asyncio.DatagramProtocol):
    def __init__(self, callback):
        self.callback = callback

    def datagram_received(self, data, addr):
        self.callback(data, addr)


async def test_t7_udp_source_port_change_keeps_session_and_channels():
    ep, (s,) = await udp_endpoint("k1")
    nat = await Rebinder(ep_addr(ep)).start()
    p = await Peer.on_udp("k1", nat.address).start()
    try:
        assert await until(lambda: p.up)
        p.ch.send_message(b"one")
        assert await until(lambda: p.msgs == [b"ack:one"])
        first = current_path(ep, s.band)
        await nat.rebind()
        p.ch.send_message(b"two")
        assert await until(lambda: p.msgs == [b"ack:one", b"ack:two"])
        assert current_path(ep, s.band) != first
        assert p.resets == [] and s.resets == []
    finally:
        await p.band.stop()
        await ep.stop()
        nat.close()


# ------------------------------------------------------------------- T8

async def test_t8_close_path_ends_the_path_until_the_next_accepted_frame():
    net, ep, (s,), (p,) = await path_mode("k1")
    try:
        ep.close_path(("p", 0))
        assert current_path(ep, s.band) is None
        mark = len(net.sent)
        s.band.stream(1).send(b"nowhere to go")
        await asyncio.sleep(0.2)
        assert net.sent[mark:] == []
        net.move(p.link, ("p", 1))
        p.ch.send_message(b"back")
        assert await until(lambda: p.msgs == [b"ack:back"])
        assert current_path(ep, s.band) == ("p", 1)
    finally:
        await shutdown(ep, p)


async def test_close_path_drops_frames_still_queued_from_that_path():
    net, ep, (s,), (p,) = await path_mode("k1")
    try:
        p.band.stream(1).send(b"queued before the close")   # queued, not yet handled
        ep.close_path(("p", 0))
        await asyncio.sleep(0.3)
        assert s.streamed == []
        assert current_path(ep, s.band) is None
    finally:
        await shutdown(ep, p)


# ------------------------------------------------------------------- T9

class PumpNet(Net):
    """A send callback as an application writes one: a bounded queue and a
    writer per path, dropping when full. The `blocked` path's writer never
    gets its frames out."""

    def __init__(self, blocked):
        super().__init__()
        self.blocked = blocked
        self.queues, self.tasks = {}, []
        self.dropped = 0

    def send(self, path, frame):
        queue = self.queues.get(path)
        if queue is None:
            queue = self.queues[path] = asyncio.Queue(16)
            self.tasks.append(asyncio.get_running_loop().create_task(
                self._pump(path, queue)))
        try:
            queue.put_nowait(frame)
        except asyncio.QueueFull:
            self.dropped += 1

    async def _pump(self, path, queue):
        while True:
            frame = await queue.get()
            if path == self.blocked:
                await asyncio.Event().wait()
            Net.send(self, path, frame)


async def test_t9_a_blocked_path_does_not_delay_the_others():
    net = PumpNet(blocked=("p", 0))
    ep = net.endpoint()
    await ep.start()
    a, b = Served(ep, "key-a"), Served(ep, "key-b")
    pa = await Peer.on_link(net, "key-a", ("p", 0)).start()
    pb = await Peer.on_link(net, "key-b", ("p", 1)).start()
    try:
        assert await until(lambda: pb.up)
        assert await until(lambda: current_path(ep, a.band) == ("p", 0))
        for i in range(1000):
            a.band.stream(1).send(b"flood %d" % i)
        t0 = time.monotonic()
        for i in range(20):
            pb.ch.send_message(b"m%d" % i)
        assert await until(lambda: len(pb.msgs) == 20, timeout=2.0)
        assert time.monotonic() - t0 < 1.0
        assert net.dropped > 0 and pa.streamed == []
    finally:
        for t in net.tasks:
            t.cancel()
        await shutdown(ep, pa, pb)


# ------------------------------------------------------------------- T11

async def test_t11_route_frames_never_reach_the_application():
    net, ep, (s,), (p,) = await path_mode("k1")
    got = []
    s.band.transport.register_handler(ChannelType.ROUTE,
                                      lambda addr, frame: got.append(frame))
    try:
        frame = route.pack_route_declaration(p.band.base_crypto(),
                                             p.band.seq_source.next(), 7)
        p.link.send(Link.EP, frame)
        await asyncio.sleep(0.3)
        assert got == []
    finally:
        await shutdown(ep, p)


# ------------------------------------------------------------------- T12

async def restart_udp_endpoint(connect):
    ep, (s,) = await udp_endpoint("k1")
    port = ep_addr(ep)[1]
    p = await Peer.on_udp("k1", ep_addr(ep)).start()
    assert await until(lambda: p.up)
    p.ch.send_message(b"before")
    assert await until(lambda: p.msgs == [b"ack:before"])
    # Killed: no GOODBYE reaches the peer.
    ep._udp.close()
    ep._udp = None
    await ep.stop()
    ep2, (s2,) = await udp_endpoint("k1", port=port)
    if connect:
        ep2.connect(s2.band.band_id, p.band.transport.local_address[:2])
    return ep2, s2, p


async def test_t12_udp_peer_recovers_at_once_after_connect():
    ep, s, p = await restart_udp_endpoint(connect=True)
    try:
        t0 = time.monotonic()
        assert await until(lambda: ResetReason.PEER_RESTARTED in p.resets and p.up)
        assert time.monotonic() - t0 < 3.0
        p.ch.send_message(b"after")
        assert await until(lambda: p.msgs[-1:] == [b"ack:after"])
        assert s.got == [b"after"]
    finally:
        await p.band.stop()
        await ep.stop()


async def test_t12_udp_peer_recovers_within_peer_timeout():
    ep, s, p = await restart_udp_endpoint(connect=False)
    try:
        # The peer declares the endpoint lost after PEER_TIMEOUT (15 s) and
        # sends HELLO again (SPEC §4.11 step 3).
        assert await until(lambda: ResetReason.PEER_RESTARTED in p.resets and p.up,
                           timeout=25.0)
        p.ch.send_message(b"after")
        assert await until(lambda: p.msgs[-1:] == [b"ack:after"])
    finally:
        await p.band.stop()
        await ep.stop()


# ------------------------------------------------------------------- T13

async def test_t13_no_keys_or_plaintext_in_logs(caplog):
    caplog.set_level(logging.DEBUG)
    psk = "psk-SECRET-KEY-4f2a"
    plaintext = b"PLAINTEXT-MARKER-91c3"
    net, ep, (s,), (p,) = await path_mode(psk)
    try:
        p.ch.send_message(plaintext)
        p.band.stream(1).send(plaintext)
        assert await until(lambda: s.got and s.streamed and p.msgs)
        ep.remove_key(s.band.band_id)
    finally:
        await shutdown(ep, p)
    secrets = [psk, plaintext.decode(),
               derive_encryption_key(psk).hex(),
               derive_encryption_key(psk, "chacha20-poly1305").hex()]
    assert caplog.records
    for record in caplog.records:
        text = record.getMessage()
        for secret in secrets:
            assert secret not in text, record


# ------------------------------------------------------------------- T14

async def test_t14_stop_returns_promptly_with_peers_connected():
    psks = [f"key-{i}" for i in range(20)]
    net, ep, served, peers = await path_mode(*psks)
    t0 = time.monotonic()
    await ep.stop()
    assert time.monotonic() - t0 < 1.0
    assert all(not s.band._running for s in served)
    for p in peers:
        await p.band.stop()


# ------------------------------------------------------------------- T15

async def record_session(net, ep, s, p):
    """One Channel message and one Stream message, recorded on the peer's
    side as an on-path attacker would capture them."""
    p.ch.send_message(b"CALL once")
    p.band.stream(1).send(b"STREAM once")
    assert await until(lambda: s.got and s.streamed)
    return list(p.link.sent)


async def replay(ep, recording, path):
    for frame in recording:
        ep.receive(path, frame)
        await asyncio.sleep(0.01)
    await asyncio.sleep(0.5)


@pytest.mark.parametrize("ending", ["running", "goodbye", "evicted"])
async def test_t15_replay_rejected_while_the_key_is_held(ending):
    net, ep, (s,), (p,) = await path_mode("k1")
    recording = await record_session(net, ep, s, p)
    try:
        if ending == "goodbye":
            await p.band.stop()
            assert await until(lambda: not s.band.peers)
        elif ending == "evicted":
            await kill(p.band)
            # Evicted under §4.5 after PEER_TIMEOUT (15 s) of silence.
            assert await until(lambda: not s.band.peers, timeout=25.0)
        path_before = current_path(ep, s.band)
        for path in (("p", 0), ("evil", 9)):          # same path, any path
            await replay(ep, recording, path)
            assert s.got == [b"CALL once"] and s.streamed == [b"STREAM once"]
            assert current_path(ep, s.band) == path_before
    finally:
        await shutdown(ep, p)


@pytest.mark.parametrize("how", ["restart", "remove-add"])
async def test_nonclaim_endpoint_without_the_key_state_accepts_a_replay(how):
    """The stated non-claim (spec/terminating-endpoint-conformance.md): an
    endpoint that has lost a key's state, by restarting or by remove_key and
    add_key, accepts a replay of a session recorded before, on any path, and
    the path follows the replay. If this test starts failing, the behaviour
    changed: update the non-claim."""
    net, ep, (s,), (p,) = await path_mode("k1")
    recording = await record_session(net, ep, s, p)
    await kill(p.band)
    net.links.pop(("p", 0))        # the dead peer hears nothing more
    if how == "restart":
        await ep.stop()
        ep = net.endpoint()
        await ep.start()
    else:
        ep.remove_key(s.band.band_id)
    s2 = Served(ep, "k1")
    try:
        await replay(ep, recording, ("evil", 9))
        assert s2.got == [b"CALL once"] and s2.streamed == [b"STREAM once"]
        assert current_path(ep, s2.band) == ("evil", 9)
    finally:
        await ep.stop()


async def test_nonclaim_restarted_peer_accepts_a_replay_of_what_the_endpoint_sent():
    """The second half of the non-claim: a peer that restarts accepts a
    replay of what the endpoint sent it before. The Endpoint does nothing
    about it, and cannot (spec/terminating-endpoints.md §7.6)."""
    net, ep, (s,), (p,) = await path_mode("k1")
    await record_session(net, ep, s, p)
    assert await until(lambda: p.msgs == [b"ack:CALL once"])
    recording = [frame for path, frame in net.sent if path == ("p", 0)]
    await kill(p.band)
    net.links.pop(("p", 0))
    elsewhere = Net()                      # the restarted peer reaches no one
    elsewhere.endpoint()
    p2 = await Peer.on_link(elsewhere, "k1", ("q", 0)).start()
    try:
        for frame in recording:
            p2.link.deliver(frame)
            await asyncio.sleep(0.01)
        assert await until(lambda: p2.msgs == [b"ack:CALL once"])
    finally:
        await p2.band.stop()
        await ep.stop()


# ----------------------------------------------------- §4.2 API and rules

async def test_add_key_idempotent_lookup_and_lifecycle_on_by_default():
    net = Net()
    ep = net.endpoint(capabilities=["x-cap"])
    band = ep.add_key("k1")
    assert ep.add_key("k1") is band
    assert band.reconnect is True                           # decision 2
    assert band.session_epoch == ep.session_epoch
    assert "x-cap" in band.capabilities
    assert ep.band(band.band_id) is band and ep.band_ids() == [band.band_id]
    assert ep.band(derive_band_id("other")) is None
    with pytest.raises(KeyError):
        ep.connect(derive_band_id("other"), ("p", 0))
    assert ep.remove_key(band.band_id) is True
    assert ep.remove_key(band.band_id) is False
    assert ep.band_ids() == []


async def test_receive_queue_full_drops_and_counts():
    net = Net()
    ep = net.endpoint()                 # not started: nothing drains
    Served(ep, "k1")
    frame = forged(derive_band_id("k1"), ChannelType.STREAM, 1)
    for _ in range(RECEIVE_QUEUE_FRAMES + 5):
        ep.receive(("p", 0), frame)
    assert ep.dropped_full == 5


# -------------------------------------------- §4.7: many keys, rekey, load

async def test_fifty_keys_each_peer_reaches_only_its_own_band_path_mode():
    psks = [f"key-{i}" for i in range(50)]
    net, ep, served, peers = await path_mode(*psks)
    try:
        for i, p in enumerate(peers):
            p.ch.send_message(b"from %d" % i)
        assert await until(lambda: all(p.msgs for p in peers), timeout=10.0)
        for i, (s, p) in enumerate(zip(served, peers)):
            assert s.got == [b"from %d" % i]
            assert p.msgs == [b"ack:from %d" % i]
    finally:
        await shutdown(ep, *peers)


async def test_fifty_keys_each_peer_reaches_only_its_own_band_udp_mode():
    psks = [f"key-{i}" for i in range(50)]
    ep, served = await udp_endpoint(*psks)
    peers = [await Peer.on_udp(psk, ep_addr(ep)).start() for psk in psks]
    try:
        assert await until(lambda: all(p.up for p in peers), timeout=10.0)
        for i, p in enumerate(peers):
            p.ch.send_message(b"from %d" % i)
        assert await until(lambda: all(p.msgs for p in peers), timeout=10.0)
        for i, (s, p) in enumerate(zip(served, peers)):
            assert s.got == [b"from %d" % i]
            assert p.msgs == [b"ack:from %d" % i]
    finally:
        await shutdown(ep, *peers)


async def test_rekey_with_both_keys_live():
    net, ep, (old,), (p_old,) = await path_mode("key-old")
    try:
        new = Served(ep, "key-new")            # §4.4: add the new key first
        p_new = await Peer.on_link(net, "key-new", ("p", 1)).start()
        assert await until(lambda: p_new.up)
        p_old.ch.send_message(b"old")
        p_new.ch.send_message(b"new")
        assert await until(lambda: p_old.msgs and p_new.msgs)
        assert old.got == [b"old"] and new.got == [b"new"]
        ep.remove_key(old.band.band_id)        # then retire the old one
        p_old.ch.send_message(b"old again")
        p_new.ch.send_message(b"new again")
        assert await until(lambda: len(p_new.msgs) == 2)
        await asyncio.sleep(0.3)
        assert old.got == [b"old"]
    finally:
        await shutdown(ep, p_old, p_new)


async def test_keys_added_and_removed_while_traffic_flows():
    psks = [f"key-{i}" for i in range(5)]
    net, ep, served, peers = await path_mode(*psks)
    running = True

    async def traffic(p, i):
        n = 0
        while running:
            p.band.stream(1).send(b"%d:%d" % (i, n))
            n += 1
            await asyncio.sleep(0.005)

    tasks = [asyncio.create_task(traffic(p, i)) for i, p in enumerate(peers)]
    try:
        await asyncio.sleep(0.3)
        added = Served(ep, "key-added")
        p_added = await Peer.on_link(net, "key-added", ("p", 5)).start()
        ep.remove_key(served[2].band.band_id)
        removed_count = len(served[2].streamed)
        marks = [len(s.streamed) for s in served]
        assert await until(lambda: p_added.up)
        p_added.ch.send_message(b"newcomer")
        assert await until(lambda: p_added.msgs == [b"ack:newcomer"])
        await asyncio.sleep(0.3)
        for i, s in enumerate(served):
            if i == 2:
                assert len(s.streamed) == removed_count
            else:
                assert len(s.streamed) > marks[i]
                assert all(d.startswith(b"%d:" % i) for d in s.streamed)
        assert added.got == [b"newcomer"]
    finally:
        running = False
        await asyncio.gather(*tasks)
        await shutdown(ep, *peers, p_added)


# ------------------------------------------------------ restarts (§5.8)

async def test_peer_restart_resets_and_reopens_udp_mode():
    ep, (s,) = await udp_endpoint("k1")
    p = await Peer.on_udp("k1", ep_addr(ep)).start()
    try:
        assert await until(lambda: p.up)
        p.ch.send_message(b"first life")
        assert await until(lambda: p.msgs == [b"ack:first life"])
        port = p.band.transport.local_address[1]
        await kill(p.band)
        p2 = await Peer.on_udp("k1", ep_addr(ep), port=port).start()
        assert await until(lambda: p2.up)
        assert await until(lambda: ResetReason.PEER_RESTARTED in s.resets)
        p2.ch.send_message(b"second life")
        assert await until(lambda: p2.msgs == [b"ack:second life"])
    finally:
        await shutdown(ep, p2)
