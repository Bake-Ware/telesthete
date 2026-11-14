"""Link lifecycle over real loopback UDP (spec/terminating-endpoints.md §7.5):
restarts of either side, 20 % datagram loss including HELLOs, and partitions
shorter and longer than PEER_TIMEOUT.

A *hub* accepts channel 0 with Band.on_channel and echoes each message; a
*client* opens channel 0, reopens it whenever it is reset, and sends. Both
run with reconnect=True. Every socket binds port 0, except that a restarted
Band rebinds the port its predecessor had.
"""

import asyncio
import os
import random
import sys
import time

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete import Band

PSK = "lifecycle-live-psk"
LO = "127.0.0.1"
_epoch = int(time.time() * 1000)


def next_epoch():
    global _epoch
    _epoch += 1
    return _epoch


async def until(predicate, timeout):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        await asyncio.sleep(0.02)
    return predicate()


async def kill(band):
    """Stop a Band the way a killed process does: no GOODBYE."""
    band._running = False
    for c in band.channels.values():
        if c._retransmit_task is not None:
            c._retransmit_task.cancel()
    for t in band._tasks:
        t.cancel()
    await asyncio.gather(*band._tasks, return_exceptions=True)
    await band.transport.stop()


class Hub:
    """Accepts channel 0 and echoes every message back with an "ack:" prefix."""

    def __init__(self, port=0, remember=None):
        self.band = Band(PSK, hostname="hub", bind_address=LO, bind_port=port,
                         session_epoch=next_epoch(), reconnect=True)
        self.received = []
        self.resets = []
        self.remember = remember
        self.band.on_channel(self._accept)

    def _accept(self, ch):
        def echo(message):
            self.received.append(message)
            ch.send_message(b"ack:" + message)
        ch.on_message(echo)
        ch.on_reset(self.resets.append)

    @property
    def port(self):
        return self.band.transport.local_address[1]

    async def start(self):
        await self.band.start()
        if self.remember is not None:
            # §10.2: an endpoint may send HELLO to addresses it remembers.
            self.band.connect_peer(*self.remember)
        return self


class Client:
    """Opens channel 0 to the hub, reopens it after every reset."""

    def __init__(self, hub_addr, port=0):
        self.band = Band(PSK, hostname="client", bind_address=LO,
                         bind_port=port, session_epoch=next_epoch(),
                         reconnect=True)
        self.hub_addr = hub_addr
        self.acks = []
        self.resets = []

    @property
    def port(self):
        return self.band.transport.local_address[1]

    @property
    def up(self):
        return self.ch.state == "ESTABLISHED"

    async def start(self):
        await self.band.start()
        self.ch = self.band.channel(0, self.hub_addr)
        self.ch.on_message(self.acks.append)
        self.ch.on_reset(self._reset)
        self.band.connect_peer(*self.hub_addr)
        await self.ch.open()
        return self

    def _reset(self, reason):
        self.resets.append(reason)
        asyncio.get_running_loop().create_task(self._reopen())

    async def _reopen(self):
        if self.ch.state == "CLOSED":
            await self.ch.open()

    def send(self, message):
        """Send if the channel is up; return whether it was sent."""
        if not self.up:
            return False
        self.ch.send_message(message)
        return True


class Proxy:
    """Joins two Bands over loopback through two sockets: one faces `a`, one
    faces `b`. Drops a fraction of datagrams, the first `drop_first` from
    each side, or everything while partitioned."""

    def __init__(self, loss=0.0, drop_first=0, seed=1):
        self.loss = loss
        self.rng = random.Random(seed)
        self.partitioned = False
        self.first = {"a": drop_first, "b": drop_first}
        self.dropped = 0

    async def start(self, a_port, b_port):
        loop = asyncio.get_running_loop()
        self.a_port, self.b_port = a_port, b_port
        self.ta, _ = await loop.create_datagram_endpoint(
            lambda: _Side(self, "a"), local_addr=(LO, 0))
        self.tb, _ = await loop.create_datagram_endpoint(
            lambda: _Side(self, "b"), local_addr=(LO, 0))
        return self

    @property
    def for_a(self):
        """The address `a` uses to reach `b`."""
        return (LO, self.ta.get_extra_info("sockname")[1])

    @property
    def for_b(self):
        return (LO, self.tb.get_extra_info("sockname")[1])

    def forward(self, side, data):
        if self.partitioned or self.first[side] > 0 or self.rng.random() < self.loss:
            self.first[side] = max(0, self.first[side] - 1)
            self.dropped += 1
            return
        if side == "a":
            self.tb.sendto(data, (LO, self.b_port))
        else:
            self.ta.sendto(data, (LO, self.a_port))

    def close(self):
        self.ta.close()
        self.tb.close()


class _Side(asyncio.DatagramProtocol):
    def __init__(self, proxy, side):
        self.proxy, self.side = proxy, side

    def datagram_received(self, data, addr):
        self.proxy.forward(self.side, data)


async def stop_all(*bands):
    for b in bands:
        if b._running:
            await b.stop()


# ------------------------------------------------------------------ restart

async def test_restart_each_side_20_times_while_messages_flow():
    hub = await Hub().start()
    hub_addr = (LO, hub.port)
    client = await Client(hub_addr).start()
    client_addr = (LO, client.port)
    state = {"client": client, "n": 0, "run": True}
    clients = [client]

    async def flow():
        while state["run"]:
            if state["client"].send(b"m%d" % state["n"]):
                state["n"] += 1
            await asyncio.sleep(0.02)

    flowing = asyncio.create_task(flow())

    async def round_trip():
        # A message sent after the restart comes back from the hub.
        c = state["client"]
        mark = state["n"]
        return await until(lambda: any(int(a[5:]) >= mark for a in c.acks
                                       if a.startswith(b"ack:m")), 5.0)

    try:
        assert await round_trip()
        for i in range(20):
            # Client side: graceful stop on even rounds, killed on odd ones.
            c = state["client"]
            await (c.band.stop() if i % 2 == 0 else kill(c.band))
            state["client"] = await Client(hub_addr, port=client_addr[1]).start()
            clients.append(state["client"])
            assert await round_trip(), f"client restart {i}"

            # Hub side, the same; the new hub remembers the client's address.
            await (hub.band.stop() if i % 2 == 0 else kill(hub.band))
            hub = await Hub(port=hub_addr[1], remember=client_addr).start()
            assert await round_trip(), f"hub restart {i}"
        # Hub restarts reached the client as GOODBYE (graceful) and as a
        # newer session epoch (killed).
        seen = {r for c in clients for r in c.resets}
        assert {"peer_left", "peer_restarted"} <= seen, seen
    finally:
        state["run"] = False
        flowing.cancel()
        await stop_all(state["client"].band, hub.band)


# --------------------------------------------------------------------- loss

async def test_twenty_percent_loss_including_hellos():
    # 20 % of datagrams dropped each way, and the first datagram from each
    # side (the client's first HELLO among them) dropped for certain. Every
    # message arrives once, in order, with no reset.
    hub = Hub()
    await hub.band.start()
    proxy = Proxy(loss=0.2, drop_first=1, seed=7)
    client = Client(None)
    await client.band.start()
    await proxy.start(a_port=client.port, b_port=hub.port)
    client.hub_addr = proxy.for_a
    try:
        await client.start()
        assert await until(lambda: client.up, 15.0)
        sent = [os.urandom(1500) for _ in range(40)]
        for m in sent:
            client.ch.send_message(m)
        assert await until(lambda: len(client.acks) == len(sent), 60.0)
        assert hub.received == sent
        assert client.acks == [b"ack:" + m for m in sent]
        assert client.resets == [] and hub.resets == []
        assert proxy.dropped > 0
    finally:
        await stop_all(client.band, hub.band)
        proxy.close()


# ---------------------------------------------------------------- partition

@pytest.mark.parametrize("seconds", [3, 20])
async def test_partition(seconds):
    # Shorter than give-up and PEER_TIMEOUT (3 s): retransmission covers it,
    # nothing is reset, nothing is lost. Longer (20 s): channels give up and
    # reset, peers are evicted, HELLO resumes; after it heals the client is
    # back and messages flow again.
    hub = Hub()
    await hub.band.start()
    proxy = Proxy()
    client = Client(None)
    await client.band.start()
    await proxy.start(a_port=client.port, b_port=hub.port)
    client.hub_addr = proxy.for_a
    state = {"n": 0, "run": True}
    sent = []

    async def flow():
        while state["run"]:
            m = b"m%d" % state["n"]
            if client.send(m):
                sent.append(m)
                state["n"] += 1
            await asyncio.sleep(0.1)

    try:
        await client.start()
        assert await until(lambda: client.up, 5.0)
        flowing = asyncio.create_task(flow())
        await asyncio.sleep(1.0)
        proxy.partitioned = True
        await asyncio.sleep(seconds)
        proxy.partitioned = False
        mark = state["n"]
        assert await until(lambda: any(int(a[5:]) >= mark for a in client.acks), 15.0)
        if seconds < 5:
            assert client.resets == [] and hub.resets == []
            await asyncio.sleep(1.0)
            state["run"] = False
            assert await until(lambda: len(hub.received) == len(sent), 5.0)
            assert hub.received == sent
        else:
            assert "peer_unresponsive" in client.resets
    finally:
        state["run"] = False
        await stop_all(client.band, hub.band)
        proxy.close()
