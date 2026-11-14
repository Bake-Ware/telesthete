"""WebSocket client against the built reference relay (SPEC §9.3, §10).

A Band over WebSocketTransport and a Band over UDP join one band through
`telesthete-hub` (rust/telesthitium) and exchange a 100 KB Channel message
and a Stream frame both ways. The relay is then killed and restarted on the
same ports: the WebSocket Band reconnects, sends HELLO, and both exchanges
work again. Last, the same over native WSS with a self-signed certificate
trusted through an explicit ssl.SSLContext.

The relay is built with cargo; the tests skip when cargo is unavailable.
"""

import asyncio
import datetime
import ipaddress
import os
import socket
import ssl
import subprocess
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

pytest.importorskip("websockets")

from telesthete import Band  # noqa: E402
from telesthete.transport.websocket import WebSocketTransport  # noqa: E402

RUST_DIR = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "rust"))
PSK = "ws-relay-test"
BIG = bytes(range(256)) * 400   # 102,400 bytes: many §6.6 chunks


def _build_relay():
    if not os.path.isdir(RUST_DIR):
        pytest.skip("rust workspace not present")
    try:
        subprocess.run(
            ["cargo", "build", "--release", "-p", "telesthitium",
             "--bin", "telesthete-hub"],
            cwd=RUST_DIR, check=True, capture_output=True, text=True,
        )
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        pytest.skip(f"cargo build unavailable: {e}")
    exe = os.path.join(RUST_DIR, "target", "release", "telesthete-hub")
    assert os.path.exists(exe), exe
    return exe


def _free_port(kind):
    with socket.socket(socket.AF_INET, kind) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Relay:
    """telesthete-hub on free localhost ports, restartable on the same ones."""

    def __init__(self, exe, tls=None):
        self.exe = exe
        self.udp_port = _free_port(socket.SOCK_DGRAM)
        self.ws_port = _free_port(socket.SOCK_STREAM)
        self.env = dict(os.environ,
                        HUB_BIND=f"127.0.0.1:{self.udp_port}",
                        HUB_WS_BIND=f"127.0.0.1:{self.ws_port}",
                        RUST_LOG="warn")
        if tls is not None:
            cert, key = tls
            self.env.update(HUB_WS_TLS="1", HUB_TLS_CERT=cert, HUB_TLS_KEY=key)
        self.proc = None

    async def start(self):
        self.proc = subprocess.Popen([self.exe], env=self.env,
                                     stdout=subprocess.DEVNULL,
                                     stderr=subprocess.DEVNULL)
        deadline = asyncio.get_running_loop().time() + 10
        while True:
            try:
                _, w = await asyncio.open_connection("127.0.0.1", self.ws_port)
                w.close()
                break
            except OSError:
                if asyncio.get_running_loop().time() > deadline:
                    raise RuntimeError("relay did not start")
                await asyncio.sleep(0.05)
        await asyncio.sleep(0.2)   # the UDP task binds alongside the WS one

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.proc = None


class Side:
    """One Band with channel 0 and stream 1 to its single peer address."""

    def __init__(self, band, peer):
        self.band, self.peer = band, peer
        self.stream_rx = asyncio.Queue()
        self.stream = band.stream(stream_id=1)
        self.stream.on_receive(lambda data, addr, ts: self.stream_rx.put_nowait(data))
        self.channel = band.channel(0, peer)


async def _until(pred, timeout, what, every=None):
    loop = asyncio.get_running_loop()
    deadline = loop.time() + timeout
    while not pred():
        if loop.time() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        if every is not None:
            every()
        await asyncio.sleep(0.25)


async def _linked(ws_side, udp_side, timeout=20):
    """Stream frames flow both ways through the relay. The UDP Band re-sends
    HELLO meanwhile: the relay delivers to a UDP source only after its second
    packet (CONFORMANCE D4), and forgets it when restarted."""
    got = {"ws": False, "udp": False}

    def probe():
        udp_side.band.connect_peer(*udp_side.peer)
        if ws_side.band.peers:
            ws_side.stream.send(b"probe")
        if udp_side.band.peers:
            udp_side.stream.send(b"probe")
        for name, side in (("ws", ws_side), ("udp", udp_side)):
            while not side.stream_rx.empty():
                side.stream_rx.get_nowait()
                got[name] = True

    await _until(lambda: all(got.values()), timeout, "stream both ways", probe)


async def _exchange(ws_side, udp_side, tag):
    # Stream frame both ways.
    for side in (ws_side, udp_side):
        while not side.stream_rx.empty():
            side.stream_rx.get_nowait()
    ws_frame, udp_frame = f"{tag} from ws".encode(), f"{tag} from udp".encode()

    async def first(side, want):
        while True:
            data = await side.stream_rx.get()
            if data == want:
                return
    for _ in range(3):   # a Stream is lossy; resend until seen
        ws_side.stream.send(ws_frame)
        udp_side.stream.send(udp_frame)
        try:
            await asyncio.wait_for(asyncio.gather(first(udp_side, ws_frame),
                                                  first(ws_side, udp_frame)), 2)
            break
        except asyncio.TimeoutError:
            continue
    else:
        raise AssertionError("stream frame did not cross both ways")

    # 100 KB Channel message both ways.
    ws_msg, udp_msg = tag.encode() + b"W" + BIG, tag.encode() + b"U" + BIG
    ws_side.channel.send_message(ws_msg)
    udp_side.channel.send_message(udp_msg)
    assert await udp_side.channel.recv_message(timeout=15) == ws_msg
    assert await ws_side.channel.recv_message(timeout=15) == udp_msg


async def _setup(relay, url, ssl_ctx=None):
    ws = WebSocketTransport(url, ssl=ssl_ctx)
    ws_band = Band(psk=PSK, hostname="ws-peer", transport=ws)
    udp_band = Band(psk=PSK, hostname="udp-peer", bind_address="127.0.0.1",
                    bind_port=0)
    ws_side = Side(ws_band, ws.peer_address)
    udp_side = Side(udp_band, ("127.0.0.1", relay.udp_port))
    await ws_band.start()
    await udp_band.start()
    ws_band.connect_peer(*ws.peer_address)   # as section 6.2's example does
    await _linked(ws_side, udp_side)
    await ws_side.channel.open()
    await _until(lambda: ws_side.channel._state == "ESTABLISHED"
                 and udp_side.channel._state == "ESTABLISHED",
                 10, "channel established")
    return ws, ws_side, udp_side


@pytest.mark.asyncio
async def test_ws_and_udp_peer_exchange_through_relay():
    relay = Relay(_build_relay())
    await relay.start()
    ws_side = udp_side = None
    try:
        ws, ws_side, udp_side = await _setup(
            relay, f"ws://127.0.0.1:{relay.ws_port}/band")
        await _exchange(ws_side, udp_side, "one")
    finally:
        for side in (ws_side, udp_side):
            if side is not None:
                await side.band.stop()
        relay.stop()


@pytest.mark.asyncio
async def test_ws_client_reconnects_after_relay_restart():
    relay = Relay(_build_relay())
    await relay.start()
    ws_side = udp_side = None
    try:
        ws, ws_side, udp_side = await _setup(
            relay, f"ws://127.0.0.1:{relay.ws_port}/band")
        await _exchange(ws_side, udp_side, "before")
        first_local = ws.local_address

        relay.stop()
        await _until(lambda: ws.local_address is None, 5, "ws to notice the close")
        await relay.start()
        await _until(lambda: ws.local_address is not None, 10, "ws to reconnect")
        assert ws.local_address != first_local, "a new connection"

        await _linked(ws_side, udp_side)
        await _exchange(ws_side, udp_side, "after")
    finally:
        for side in (ws_side, udp_side):
            if side is not None:
                await side.band.stop()
        relay.stop()


def _self_signed(tmp_path):
    x509 = pytest.importorskip("cryptography.x509")
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "127.0.0.1")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (x509.CertificateBuilder()
            .subject_name(name).issuer_name(name)
            .public_key(key.public_key())
            .serial_number(x509.random_serial_number())
            .not_valid_before(now - datetime.timedelta(minutes=5))
            .not_valid_after(now + datetime.timedelta(days=1))
            .add_extension(x509.SubjectAlternativeName(
                [x509.IPAddress(ipaddress.ip_address("127.0.0.1"))]), critical=False)
            .add_extension(x509.BasicConstraints(ca=True, path_length=None),
                           critical=True)
            .sign(key, hashes.SHA256()))
    cert_path, key_path = tmp_path / "cert.pem", tmp_path / "key.pem"
    cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(key.private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8,
        serialization.NoEncryption()))
    return str(cert_path), str(key_path)


@pytest.mark.asyncio
async def test_wss_with_self_signed_certificate(tmp_path):
    cert, key = _self_signed(tmp_path)
    relay = Relay(_build_relay(), tls=(cert, key))
    await relay.start()
    ws_side = udp_side = None
    try:
        url = f"wss://127.0.0.1:{relay.ws_port}/band"

        # Without the certificate trusted, the client cannot connect.
        untrusted = WebSocketTransport(url)
        untrusted_band = Band(psk=PSK, transport=untrusted)
        await untrusted_band.start()
        await asyncio.sleep(1.0)
        assert untrusted.local_address is None
        await untrusted_band.stop()

        ctx = ssl.create_default_context(cafile=cert)
        ws, ws_side, udp_side = await _setup(relay, url, ssl_ctx=ctx)
        await _exchange(ws_side, udp_side, "wss")
    finally:
        for side in (ws_side, udp_side):
            if side is not None:
                await side.band.stop()
        relay.stop()
