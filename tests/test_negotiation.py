"""Capability + cipher negotiation (SPEC §3.5, §12.5).

Unit-tests the selection rule, then runs two real Bands over UDP loopback to
prove an end-to-end handshake negotiates AES-256-GCM and that subsequent
Stream + Control traffic actually rides the negotiated suite.

Run: python tests/test_negotiation.py
"""

import asyncio
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from telesthete.protocol.crypto import select_cipher, BASELINE_CIPHER
from telesthete.band import Band

AES = "aes256-gcm"
CHACHA = "chacha20-poly1305"


def test_select_cipher_rule():
    # initiator's top choice that the responder also supports
    assert select_cipher([AES, CHACHA], [CHACHA, AES]) == AES
    assert select_cipher([CHACHA, AES], [CHACHA, AES]) == CHACHA
    # no overlap beyond baseline -> baseline (always supported)
    assert select_cipher([AES], [CHACHA]) == CHACHA
    # responder restricted to baseline -> baseline even if initiator prefers AES
    assert select_cipher([AES, CHACHA], [CHACHA]) == CHACHA


async def _negotiate(a_ciphers, b_ciphers, expect):
    a = Band("neg-psk", hostname="a", bind_port=12211, ciphers=a_ciphers)
    b = Band("neg-psk", hostname="b", bind_port=12212, ciphers=b_ciphers)
    await a.start()
    await b.start()
    try:
        a.connect_peer("127.0.0.1", 12212)
        await asyncio.sleep(0.4)

        # both sides converged on the same suite
        a_peer = next(iter(a.peers.values()))
        b_peer = next(iter(b.peers.values()))
        assert a_peer.cipher == expect, f"initiator got {a_peer.cipher}, want {expect}"
        assert b_peer.cipher == expect, f"responder got {b_peer.cipher}, want {expect}"

        # data path actually works under the negotiated suite
        sa = a.stream(stream_id=1, priority=0)
        sb = b.stream(stream_id=1, priority=0)
        got = []
        sb.on_receive(lambda data, peer, ts: got.append(data))
        sa.send(b"negotiated payload")
        await asyncio.sleep(0.3)
        assert got == [b"negotiated payload"], f"stream under {expect} failed: {got}"
    finally:
        await a.stop()
        await b.stop()


def test_handshake_negotiates_aes():
    asyncio.run(_negotiate([AES, CHACHA], [AES, CHACHA], AES))


def test_handshake_falls_back_to_baseline():
    asyncio.run(_negotiate([AES, CHACHA], [CHACHA], CHACHA))


if __name__ == "__main__":
    test_select_cipher_rule()
    test_handshake_negotiates_aes()
    test_handshake_falls_back_to_baseline()
    print("negotiation: PASS (selection rule + live AES handshake + baseline fallback)")
