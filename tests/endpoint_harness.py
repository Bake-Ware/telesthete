"""Shared pieces for the Endpoint tests (not a test module).

`Net` connects peer Bands to a path-mode Endpoint in memory: the Endpoint's
send callback hands each frame to the Link that owns the path, and a Link
feeds what its Band sends into `Endpoint.receive` on its path. A Link can be
moved to a new path (a reconnect, a new NAT mapping), and it records every
frame its Band sent, for the replay tests.
"""

import asyncio
import time
from collections import defaultdict

from telesthete import Band, Endpoint

LO = "127.0.0.1"
_epoch = int(time.time() * 1000)


def next_epoch():
    """A session epoch larger than every earlier one (SPEC §4.3)."""
    global _epoch
    _epoch += 1
    return _epoch


async def until(predicate, timeout=5.0):
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


def current_path(ep, band):
    """The key's current path (SPEC §10.2 rule 4); test-only peek."""
    return ep._keys[band.band_id].view.path


class Net:
    def __init__(self):
        self.ep = None
        self.links = {}      # path -> Link
        self.sent = []       # (path, frame) the Endpoint sent

    def send(self, path, frame):
        """The Endpoint's send callback."""
        self.sent.append((path, frame))
        link = self.links.get(path)
        if link is not None:
            asyncio.get_running_loop().call_soon(link.deliver, frame)

    def endpoint(self, session_epoch=None, **kw):
        self.ep = Endpoint(hostname="hub", send=self.send,
                           session_epoch=session_epoch or next_epoch(), **kw)
        return self.ep

    def link(self, path):
        link = Link(self, path)
        self.links[path] = link
        return link

    def move(self, link, path):
        del self.links[link.path]
        link.path = path
        self.links[path] = link


class Link:
    """A peer Band's transport into the Endpoint over one path."""

    EP = ("hub", 1)   # where the peer Band sees the Endpoint

    def __init__(self, net, path):
        self.net, self.path = net, path
        self.local_address = ("link", 0)
        self._handlers = defaultdict(list)
        self.sent = []

    def start(self):
        pass

    async def run(self):
        pass

    async def stop(self):
        pass

    def register_handler(self, channel_type, handler):
        self._handlers[channel_type].append(handler)

    def send(self, destination, frame):
        self.sent.append(frame)
        self.net.ep.receive(self.path, frame)

    def deliver(self, frame):
        for handler in list(self._handlers.get(frame[16], [])):
            handler(self.EP, frame)


class Peer:
    """A peer Band that opens channel 0 to the Endpoint and reopens it after
    every reset. `on_link` puts it on an in-memory path, `on_udp` on its own
    UDP socket."""

    def __init__(self, band, target):
        self.band, self.target = band, target
        self.msgs, self.resets, self.streamed = [], [], []
        self.link = None

    @classmethod
    def on_link(cls, net, psk, path, epoch=None):
        link = net.link(path)
        band = Band(psk, hostname="peer", transport=link,
                    session_epoch=epoch or next_epoch(), reconnect=True)
        peer = cls(band, Link.EP)
        peer.link = link
        return peer

    @classmethod
    def on_udp(cls, psk, ep_addr, port=0, epoch=None):
        band = Band(psk, hostname="peer", bind_address=LO, bind_port=port,
                    session_epoch=epoch or next_epoch(), reconnect=True)
        return cls(band, ep_addr)

    async def start(self, open_channel=True):
        await self.band.start()
        self.band.stream(1).on_receive(lambda d, p, t: self.streamed.append(d))
        self.ch = self.band.channel(0, self.target)
        self.ch.on_message(self.msgs.append)
        self.ch.on_reset(self._reset)
        self.band.connect_peer(*self.target)
        if open_channel:
            await self.ch.open()
        return self

    @property
    def up(self):
        return self.ch.state == "ESTABLISHED"

    def _reset(self, reason):
        self.resets.append(reason)
        asyncio.get_running_loop().create_task(self._reopen())

    async def _reopen(self):
        if self.ch.state == "CLOSED":
            await self.ch.open()


class Served:
    """The application's side of one key: accepts channel 0 and answers each
    message with "ack:" + message; records Stream data."""

    def __init__(self, ep, psk):
        self.band = ep.add_key(psk)
        self.got, self.resets, self.streamed = [], [], []
        self.ch = None
        self.band.on_channel(self._accept)
        self.band.stream(1).on_receive(lambda d, p, t: self.streamed.append(d))

    def _accept(self, ch):
        self.ch = ch
        ch.on_message(self._echo)
        ch.on_reset(self.resets.append)

    def _echo(self, message):
        self.got.append(message)
        self.ch.send_message(b"ack:" + message)
