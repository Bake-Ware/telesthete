"""Band-level Board (§7) + Drop (§8) over real loopback UDP."""

import asyncio
import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.band import Band


async def _handshake_pair(psk):
    a = Band(psk=psk, hostname="alice", bind_address="127.0.0.1", bind_port=0)
    b = Band(psk=psk, hostname="bob", bind_address="127.0.0.1", bind_port=0)
    await a.start()
    await b.start()
    a.connect_peer("127.0.0.1", b.transport.local_address[1])
    await asyncio.sleep(0.3)  # HELLO/HELLO_ACK exchange
    assert a.peers and b.peers, "handshake must register both peers"
    return a, b


async def test_band_board_replicates_over_udp():
    a, b = await _handshake_pair("band-board-e2e")
    try:
        board_a = a.board(5)
        board_b = b.board(5)
        board_a.set("cursor", [1, 2])
        await asyncio.sleep(0.3)
        assert board_b.get("cursor") == [1, 2]

        # Anti-entropy repairs a receiver that missed the SET (fresh board).
        board_a.set("late", "joiner-misses-this")
        await asyncio.sleep(0.2)
        b.boards[5]._entries.pop("late", None)  # simulate loss
        board_a.send_digest()
        await asyncio.sleep(0.4)
        assert board_b.get("late") == "joiner-misses-this"
    finally:
        await a.stop()
        await b.stop()


async def test_band_drop_transfers_over_udp():
    a, b = await _handshake_pair("band-drop-e2e")
    try:
        data = os.urandom(3000)  # 3 chunks
        sender = a.drop_sender(9, "blob.bin", data)
        receiver = b.drop_receiver(9)
        done = []
        receiver.on_complete(lambda d, ok: done.append((d, ok)))

        b_addr = ("127.0.0.1", b.transport.local_address[1])
        sender.offer(b_addr)
        await asyncio.sleep(0.5)
        assert done and done[0][1] is True
        assert done[0][0] == data
        assert sender.completed and list(sender.completed.values()) == [True]
        assert "drop-v1" in a.capabilities  # advertised once the Drop opened (§12.5)
    finally:
        await a.stop()
        await b.stop()
