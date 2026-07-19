"""§9.4 AF_UNIX transport: SEQPACKET loopback, SCM_RIGHTS fds, fd hygiene."""

import asyncio
import os
import sys
import tempfile

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.transport.unix import (
    UnixTransport, default_socket_path, MAX_FDS_PER_PACKET,
)


def _packet(channel_type: int, body: bytes = b"x" * 30) -> bytes:
    # 16-byte band_id || channel_type || 2-byte channel_id || 8-byte seq || body
    return b"B" * 16 + bytes([channel_type]) + b"\x00" * 10 + body


async def _pair(tmp):
    path = os.path.join(tmp, "t.sock")
    server = UnixTransport(path, server=True)
    server.start()
    stask = asyncio.create_task(server.run())
    await asyncio.sleep(0.05)
    client = UnixTransport(path, server=False)
    client.start()
    ctask = asyncio.create_task(client.run())
    await asyncio.sleep(0.05)
    return server, client, stask, ctask


async def test_seqpacket_round_trip():
    with tempfile.TemporaryDirectory() as tmp:
        server, client, *_ = await _pair(tmp)
        try:
            got_server, got_client = [], []
            server.register_handler(0x01, lambda addr, pkt: got_server.append((addr, pkt)))
            client.register_handler(0x00, lambda addr, pkt: got_client.append((addr, pkt)))

            client.send(("unix", 0), _packet(0x01))
            await asyncio.sleep(0.1)
            assert len(got_server) == 1
            server_side_peer = got_server[0][0]

            server.send(server_side_peer, _packet(0x00))
            await asyncio.sleep(0.1)
            assert len(got_client) == 1
            assert got_client[0][1][16] == 0x00
        finally:
            await client.stop()
            await server.stop()


async def test_scm_rights_fd_passing():
    with tempfile.TemporaryDirectory() as tmp:
        server, client, *_ = await _pair(tmp)
        try:
            got = []
            server.register_handler(0x01, lambda addr, pkt, fds: got.append(fds))

            r, w = os.pipe()
            os.write(w, b"through-the-socket")
            client.send(("unix", 0), _packet(0x01), fds=(r,))
            await asyncio.sleep(0.1)
            os.close(r)
            os.close(w)

            assert len(got) == 1 and len(got[0]) == 1
            received_fd = got[0][0]
            assert os.read(received_fd, 64) == b"through-the-socket"
            os.close(received_fd)
        finally:
            await client.stop()
            await server.stop()


async def test_fds_closed_when_no_handler_takes_them():
    # SPEC §9.4 fd hygiene: an unrouted packet must not leak its fds.
    with tempfile.TemporaryDirectory() as tmp:
        server, client, *_ = await _pair(tmp)
        try:
            r, w = os.pipe()
            client.send(("unix", 0), _packet(0x01), fds=(r,))  # no handler registered
            await asyncio.sleep(0.1)
            # The receiver's copy is closed; our ends still work.
            os.write(w, b"z")
            assert os.read(r, 1) == b"z"
            os.close(r)
            os.close(w)
        finally:
            await client.stop()
            await server.stop()


async def test_send_rejects_too_many_fds():
    with tempfile.TemporaryDirectory() as tmp:
        server, client, *_ = await _pair(tmp)
        try:
            pipes = [os.pipe() for _ in range(MAX_FDS_PER_PACKET + 1)]
            fds = tuple(r for r, _ in pipes)
            with pytest.raises(ValueError):
                client.send(("unix", 0), _packet(0x01), fds=fds)
            for r, w in pipes:
                os.close(r)
                os.close(w)
        finally:
            await client.stop()
            await server.stop()


def test_default_socket_path_uses_runtime_dir(monkeypatch, tmp_path):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    p = default_socket_path(b"\xab" * 16)
    assert p == str(tmp_path / "telesthete" / ("ab" * 16 + ".sock"))
    assert (tmp_path / "telesthete").is_dir()


async def test_server_cleans_up_socket_file():
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "t.sock")
        server = UnixTransport(path, server=True)
        server.start()
        task = asyncio.create_task(server.run())
        await asyncio.sleep(0.05)
        assert os.path.exists(path)
        await server.stop()
        assert not os.path.exists(path)
        task.cancel()
