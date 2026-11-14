"""A path-mode Endpoint behind a minimal WebSocket and UDP server built on
the seam (spec/terminating-endpoints.md §5.3, §6.5; SPEC §9.3, §10.2).

`SeamServer` is the test server: one UDP socket and one WebSocket route,
/band, both feeding one Endpoint through `receive`, `close_path` and the
`send` callback. It is test code, not part of the package. Clients are
unmodified: `Band(..., transport=WebSocketTransport(url), reconnect=True)`
and a `Band` on its own UDP socket.

T10 and the WebSocket half of T12 of
spec/terminating-endpoint-conformance.md are here.
"""

import asyncio
import os
import sys
from urllib.parse import urlsplit

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))
sys.path.insert(0, os.path.dirname(__file__))

pytest.importorskip("websockets")
from websockets.asyncio.client import connect  # noqa: E402
from websockets.asyncio.server import serve  # noqa: E402
from websockets.exceptions import InvalidStatus  # noqa: E402

from telesthete import Band, Endpoint  # noqa: E402
from telesthete.protocol.channel import ResetReason  # noqa: E402
from telesthete.protocol.framing import MIN_PACKET_SIZE  # noqa: E402
from telesthete.transport.websocket import WebSocketTransport  # noqa: E402

from endpoint_harness import LO, Peer, Served, next_epoch, until  # noqa: E402

pytestmark = pytest.mark.asyncio


class _Udp(asyncio.DatagramProtocol):
    def __init__(self, ep):
        self.ep = ep

    def datagram_received(self, data, addr):
        self.ep.receive(("udp", addr), data)


class SeamServer:
    """A terminating endpoint: UDP and a WebSocket route on /band, one
    Endpoint behind both. Paths are ("udp", addr) and ("ws", id(conn))."""

    def __init__(self, psks, udp_port=0, ws_port=0, idle=60.0):
        self.psks = psks
        self.udp_port, self.ws_port = udp_port, ws_port
        self.idle = idle
        self.queues = {}
        self.closed = False

    async def start(self):
        self.ep = Endpoint(hostname="hub", send=self._send,
                           session_epoch=next_epoch())
        self.served = {psk: Served(self.ep, psk) for psk in self.psks}
        await self.ep.start()
        loop = asyncio.get_running_loop()
        self.udp, _ = await loop.create_datagram_endpoint(
            lambda: _Udp(self.ep), local_addr=(LO, self.udp_port))
        self.udp_port = self.udp.get_extra_info("sockname")[1]
        self.ws = await serve(self._route, LO, self.ws_port,
                              process_request=self._only_band,
                              compression=None, ping_interval=None)
        self.ws_port = self.ws.sockets[0].getsockname()[1]
        return self

    @property
    def url(self):
        return f"ws://{LO}:{self.ws_port}/band"

    def _send(self, path, frame):
        """The Endpoint's send callback: must not block, must not raise."""
        if self.closed:
            return
        kind, where = path
        if kind == "udp":
            self.udp.sendto(frame, where)
        else:
            queue = self.queues.get(where)
            if queue is not None and not queue.full():
                queue.put_nowait(frame)

    def _only_band(self, connection, request):
        if urlsplit(request.path).path != "/band":
            return connection.respond(404, "Not Found\n")
        return None

    async def _route(self, ws):
        path = ("ws", id(ws))
        queue = self.queues[id(ws)] = asyncio.Queue(256)

        async def pump():
            while True:
                await ws.send(await queue.get())   # one binary message per frame

        writer = asyncio.create_task(pump())
        try:
            while True:
                try:
                    message = await asyncio.wait_for(ws.recv(), self.idle)
                except asyncio.TimeoutError:
                    await ws.close()              # silent for `idle` seconds
                    return
                if isinstance(message, bytes) and len(message) >= MIN_PACKET_SIZE:
                    self.ep.receive(path, message)
        except Exception:
            pass
        finally:
            self.ep.close_path(path)
            writer.cancel()
            del self.queues[id(ws)]

    async def kill(self):
        """Gone at once, like a killed process: no GOODBYE leaves."""
        self.closed = True
        self.ws.close()
        await self.ws.wait_closed()
        self.udp.close()
        await self.ep.stop()


class WsPeer(Peer):
    """A WebSocket client Band as an application writes one (§6.2)."""

    @classmethod
    def on_ws(cls, psk, url):
        ws = WebSocketTransport(url)
        band = Band(psk, hostname="ws-peer", transport=ws,
                    session_epoch=next_epoch(), reconnect=True)
        return cls(band, ws.peer_address)


async def test_ws_and_udp_peers_on_two_keys_control_and_100kb_both_ways():
    server = await SeamServer(["key-ws", "key-udp"]).start()
    wsp = await WsPeer.on_ws("key-ws", server.url).start()
    udp = await Peer.on_udp("key-udp", (LO, server.udp_port)).start()
    big = os.urandom(100_000)
    try:
        assert await until(lambda: wsp.up and udp.up)
        # Control: each side knows the other as a peer (HELLO / HELLO_ACK).
        for psk, p in (("key-ws", wsp), ("key-udp", udp)):
            s = server.served[psk]
            assert [q.hostname for q in s.band.get_peers()] == [p.band.hostname]
            assert [q.hostname for q in p.band.get_peers()] == ["hub"]
        wsp.ch.send_message(big)
        udp.ch.send_message(big[::-1])
        assert await until(lambda: wsp.msgs and udp.msgs, timeout=10.0)
        assert server.served["key-ws"].got == [big]
        assert server.served["key-udp"].got == [big[::-1]]
        assert wsp.msgs == [b"ack:" + big] and udp.msgs == [b"ack:" + big[::-1]]
    finally:
        await wsp.band.stop()
        await udp.band.stop()
        await server.kill()


async def test_t12_ws_peer_recovers_after_endpoint_restart():
    server = await SeamServer(["key-ws"]).start()
    wsp = await WsPeer.on_ws("key-ws", server.url).start()
    try:
        assert await until(lambda: wsp.up)
        wsp.ch.send_message(b"before")
        assert await until(lambda: wsp.msgs == [b"ack:before"])
        await server.kill()
        server = await SeamServer(["key-ws"], udp_port=server.udp_port,
                                  ws_port=server.ws_port).start()
        # The client reconnects, sends HELLO, sees the newer epoch, resets
        # and reopens; the new process accepts the channel.
        assert await until(lambda: ResetReason.PEER_RESTARTED in wsp.resets
                           and wsp.up, timeout=10.0)
        wsp.ch.send_message(b"after")
        assert await until(lambda: wsp.msgs[-1:] == [b"ack:after"])
        assert server.served["key-ws"].got == [b"after"]
    finally:
        await wsp.band.stop()
        await server.kill()


async def test_t10_websocket_server_built_on_the_seam():
    server = await SeamServer(["key-ws"], idle=1.0).start()
    try:
        # Other paths: 404.
        with pytest.raises(InvalidStatus) as err:
            async with connect(f"ws://{LO}:{server.ws_port}/other"):
                pass
        assert err.value.response.status_code == 404
        # /band with a query string, no subprotocol offered or selected.
        async with connect(server.url + "?x=1", compression=None,
                           ping_interval=None) as ws:
            assert ws.subprotocol is None
            assert "Sec-WebSocket-Protocol" not in ws.response.headers
            # Nothing is sent before the connection's first frame.
            with pytest.raises(asyncio.TimeoutError):
                await asyncio.wait_for(ws.recv(), 0.5)
            # Text and short binaries are ignored; the connection stays open.
            await ws.send("text")
            await ws.send(b"\x00" * 5)
            with pytest.raises(asyncio.TimeoutError):
                await asyncio.wait_for(ws.recv(), 0.3)
            # Pings are answered.
            pong = await ws.ping()
            await asyncio.wait_for(pong, 1.0)
            # Closed once silent for the idle timeout (60 s in a real
            # server; 1 s here).
            await asyncio.wait_for(ws.wait_closed(), 3.0)
        assert server.ep.dropped_unknown == 0   # nothing reached the Endpoint
    finally:
        await server.kill()
