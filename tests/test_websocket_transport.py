"""WebSocket client transport (SPEC §9.3), against an in-process server.

Checks the client's side of §9.3 without the relay: one frame per binary
message, HELLO as the connection's first frame and again on each new
connection, text and short binaries ignored, frames attributed to the
server's address, no subprotocol or extension offered, pings answered,
frames sent while disconnected dropped, reconnection with backoff and
jitter. `Band(transport=)` and the optional extra are checked here too.
"""

import asyncio
import json
import os
import subprocess
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

pytest.importorskip("websockets")
from websockets.asyncio.server import serve  # noqa: E402

from telesthete import Band  # noqa: E402
from telesthete.protocol.framing import (  # noqa: E402
    ChannelType, MIN_PACKET_SIZE, unpack_packet)
from telesthete.protocol.control import ControlMessageType  # noqa: E402
from telesthete.transport import websocket as wsmod  # noqa: E402
from telesthete.transport.udp import UDPTransport  # noqa: E402
from telesthete.transport.websocket import WebSocketTransport  # noqa: E402

PSK = "ws-transport-test"


class RecordingServer:
    """Accepts connections on /band and records each one's messages and
    request headers. `script(ws)` optionally runs after the first message."""

    def __init__(self, script=None):
        self.script = script
        self.conns = []          # one list of received messages per connection
        self.headers = []
        self.server = None
        self.port = None
        self._new_message = asyncio.Event()

    async def handler(self, ws):
        msgs = []
        self.conns.append(msgs)
        self.headers.append(ws.request.headers)
        try:
            async for m in ws:
                msgs.append(m)
                self._new_message.set()
                if self.script is not None and len(msgs) == 1:
                    await self.script(ws)
        except Exception:
            pass

    async def __aenter__(self):
        self.server = await serve(self.handler, "127.0.0.1", 0)
        self.port = self.server.sockets[0].getsockname()[1]
        return self

    async def __aexit__(self, *exc):
        self.server.close()
        await self.server.wait_closed()

    @property
    def url(self):
        return f"ws://127.0.0.1:{self.port}/band"

    async def wait_for(self, pred, timeout=5.0):
        async def loop():
            while not pred():
                self._new_message.clear()
                await self._new_message.wait()
        await asyncio.wait_for(loop(), timeout)


def is_hello(band, frame):
    """True if `frame` is a HELLO under the band's base key (§4.3)."""
    packet = unpack_packet(frame)
    if packet.channel_type != ChannelType.CONTROL:
        return False
    plaintext = band.base_crypto().decrypt(
        packet.sequence, packet.ciphertext, bytes([ChannelType.CONTROL, 0, 0]))
    return json.loads(plaintext)["type"] == ControlMessageType.HELLO


def test_peer_address_from_url():
    assert WebSocketTransport("wss://hub.example.com/band").peer_address == (
        "hub.example.com", 443)
    assert WebSocketTransport("ws://hub.example.com/band").peer_address == (
        "hub.example.com", 80)
    assert WebSocketTransport("wss://[::1]:8443/band?x=1").peer_address == (
        "::1", 8443)
    with pytest.raises(ValueError):
        WebSocketTransport("http://hub.example.com/band")


def test_band_without_transport_builds_udp():
    band = Band(psk=PSK, bind_port=0)
    assert isinstance(band.transport, UDPTransport)
    assert not hasattr(band.transport, "on_connected")


@pytest.mark.asyncio
async def test_hello_first_one_frame_per_binary_message():
    async with RecordingServer() as srv:
        ws = WebSocketTransport(srv.url)
        band = Band(psk=PSK, hostname="ws-client", transport=ws)
        assert band.transport is ws
        stream = band.stream(stream_id=1)
        stream.add_destination(ws.peer_address)
        # Sent before the connection exists: dropped, not queued.
        stream.send(b"too early")
        await band.start()
        try:
            await srv.wait_for(lambda: srv.conns and srv.conns[0])
            first = srv.conns[0][0]
            assert isinstance(first, bytes)
            assert is_hello(band, first), "first frame on the connection is HELLO"

            stream.send(b"x" * 500)
            await srv.wait_for(lambda: len(srv.conns[0]) >= 2)
            assert all(isinstance(m, bytes) for m in srv.conns[0])
            streams = [m for m in srv.conns[0]
                       if unpack_packet(m).channel_type == ChannelType.STREAM]
            # One binary message is exactly one frame, no length prefix, and
            # the frame sent while disconnected never arrives.
            assert len(streams) == 1
            assert unpack_packet(streams[0]).band_id == band.band_id
            assert len(streams[0]) > 500
        finally:
            await band.stop()


@pytest.mark.asyncio
async def test_no_subprotocol_or_extension_offered():
    async with RecordingServer() as srv:
        ws = WebSocketTransport(srv.url)
        band = Band(psk=PSK, transport=ws)
        await band.start()
        try:
            await srv.wait_for(lambda: srv.conns and srv.conns[0])
            headers = srv.headers[0]
            assert "Sec-WebSocket-Protocol" not in headers
            assert "Sec-WebSocket-Extensions" not in headers
        finally:
            await band.stop()


@pytest.mark.asyncio
async def test_text_and_short_binary_ignored_frames_attributed_to_server():
    seen = []
    frame = bytes(16) + bytes([ChannelType.STREAM]) + bytes(MIN_PACKET_SIZE - 17)

    async def script(conn):
        await conn.send("text message")
        await conn.send(b"short")
        await conn.send(frame[:-1])   # one byte under MIN_PACKET_SIZE
        await conn.send(frame)

    async with RecordingServer(script) as srv:
        ws = WebSocketTransport(srv.url)
        got = asyncio.Event()

        def handler(peer_addr, packet_bytes):
            seen.append((peer_addr, packet_bytes))
            got.set()

        ws.register_handler(ChannelType.STREAM, handler)
        ws.on_connected = lambda: ws.send(None, frame)  # the client sends first
        ws.start()
        task = asyncio.create_task(ws.run())
        try:
            await asyncio.wait_for(got.wait(), 5)
            await asyncio.sleep(0.2)
            assert seen == [(ws.peer_address, frame)]
            assert ws.peer_address == ("127.0.0.1", srv.port)
        finally:
            await ws.stop()
            await asyncio.gather(task, return_exceptions=True)


@pytest.mark.asyncio
async def test_answers_pings():
    pong = asyncio.get_running_loop().create_future()

    async def script(conn):
        waiter = await conn.ping()
        try:
            await asyncio.wait_for(waiter, 3)
            pong.set_result(True)
        except Exception as e:
            pong.set_exception(e)

    async with RecordingServer(script) as srv:
        band = Band(psk=PSK, transport=WebSocketTransport(srv.url))
        await band.start()
        try:
            assert await asyncio.wait_for(pong, 5)
        finally:
            await band.stop()


@pytest.mark.asyncio
async def test_reconnects_and_sends_hello_on_each_connection():
    async def script(conn):
        await conn.close()   # the server drops the client after its first frame

    async with RecordingServer(script) as srv:
        ws = WebSocketTransport(srv.url, reconnect_min=0.05, reconnect_max=0.2)
        band = Band(psk=PSK, transport=ws)
        await band.start()
        try:
            await srv.wait_for(lambda: len(srv.conns) >= 3 and srv.conns[2])
            for msgs in srv.conns[:3]:
                assert msgs and is_hello(band, msgs[0])
        finally:
            await band.stop()


@pytest.mark.asyncio
async def test_backoff_doubles_with_jitter(monkeypatch):
    attempts = []
    loop = asyncio.get_running_loop()

    def failing_connect(*args, **kwargs):
        attempts.append(loop.time())
        raise OSError("connection refused")

    monkeypatch.setattr(wsmod, "connect", failing_connect)
    ws = WebSocketTransport("ws://127.0.0.1:1/band",
                            reconnect_min=0.05, reconnect_max=0.2)
    ws.start()
    task = asyncio.create_task(ws.run())
    try:
        while len(attempts) < 6:
            await asyncio.sleep(0.02)
    finally:
        await ws.stop()
        await asyncio.gather(task, return_exceptions=True)

    gaps = [b - a for a, b in zip(attempts, attempts[1:])]
    expected = [0.05, 0.1, 0.2, 0.2, 0.2]
    for gap, exp in zip(gaps, expected):
        assert exp * 0.8 - 0.01 <= gap <= exp * 1.2 + 0.05, (gaps, expected)


def test_base_package_works_without_websockets():
    # The extra is optional (decision 4): without it the package imports,
    # a UDP Band runs, and only the WebSocket module says what to install.
    code = (
        "import sys, asyncio\n"
        "sys.modules['websockets'] = None\n"
        "import telesthete\n"
        "from telesthete import Band\n"
        "async def main():\n"
        "    b = Band(psk='x', bind_address='127.0.0.1', bind_port=0)\n"
        "    await b.start(); await b.stop()\n"
        "asyncio.run(main())\n"
        "try:\n"
        "    import telesthete.transport.websocket\n"
        "except ImportError as e:\n"
        "    assert 'telesthete[ws]' in str(e), e\n"
        "    print('ok')\n"
    )
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
    out = subprocess.run([sys.executable, "-c", code], cwd=root,
                         capture_output=True, text=True, timeout=30)
    assert out.returncode == 0, out.stderr
    assert out.stdout.strip() == "ok", (out.stdout, out.stderr)
