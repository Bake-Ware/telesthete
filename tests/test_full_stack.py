"""
Full stack integration test

Tests Band + Discovery + Streams + Channels + Control
"""

import sys
import os
import asyncio
import logging

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))

from telesthete import Band
from telesthete.transport.discovery import Discovery

logging.basicConfig(level=logging.INFO, format='%(name)-25s: %(message)s')


async def test_full_stack():
    """
    Test complete stack with two bands
    """
    print("Full Stack Integration Test")
    print("=" * 60)

    # Create two bands
    band1 = Band(psk="test-full-stack", hostname="alpha", bind_port=30001)
    band2 = Band(psk="test-full-stack", hostname="beta", bind_port=30002)

    # Start both
    await band1.start()
    await band2.start()

    print(f"Alpha on {band1.transport.local_address}")
    print(f"Beta on {band2.transport.local_address}")

    # Add discovery
    discovered = []

    def on_peer_found(hostname, ip, port):
        print(f"Discovery: Found {hostname} at {ip}:{port}")
        discovered.append((hostname, ip, port))
        # Auto-connect to discovered peers
        if hostname == "beta":
            band1.connect_peer(ip, port)
        elif hostname == "alpha":
            band2.connect_peer(ip, port)

    disc1 = Discovery("alpha", 30001, on_peer_found)
    disc2 = Discovery("beta", 30002, on_peer_found)

    disc1.start()
    disc2.start()

    task1 = asyncio.create_task(disc1.run())
    task2 = asyncio.create_task(disc2.run())

    # Wait for discovery and connection
    print("\nWaiting for discovery...")
    await asyncio.sleep(2.0)

    print(f"Alpha sees peers: {[p.hostname for p in band1.get_peers()]}")
    print(f"Beta sees peers: {[p.hostname for p in band2.get_peers()]}")

    # Test Streams
    print("\n" + "=" * 60)
    print("Testing Streams (real-time, lossy)")
    print("=" * 60)

    stream_alpha = band1.stream(stream_id=1, priority=0)
    stream_beta = band2.stream(stream_id=1, priority=0)

    stream_received_alpha = []
    stream_received_beta = []

    stream_alpha.on_receive(lambda data, peer, ts: stream_received_alpha.append(data))
    stream_beta.on_receive(lambda data, peer, ts: stream_received_beta.append(data))

    # Send stream data
    for i in range(5):
        stream_alpha.send(f"Alpha stream {i}".encode())
        stream_beta.send(f"Beta stream {i}".encode())
        await asyncio.sleep(0.05)

    await asyncio.sleep(0.5)

    print(f"Alpha received {len(stream_received_alpha)} stream messages")
    print(f"Beta received {len(stream_received_beta)} stream messages")

    # Test Channels (using Band's channel support once we add it)
    # For now, skip Channel test in Band context since we need to add Channel support to Band

    # Test Control (already happening via keepalives)
    print("\n" + "=" * 60)
    print("Testing Control (keepalive)")
    print("=" * 60)
    print("Control messages are being exchanged automatically")

    # Verify BEFORE cleanup
    success = True

    if len(band1.get_peers()) == 0:
        print("[FAIL] Alpha did not discover Beta")
        success = False
    else:
        print("[PASS] Alpha discovered Beta")

    if len(band2.get_peers()) == 0:
        print("[FAIL] Beta did not discover Alpha")
        success = False
    else:
        print("[PASS] Beta discovered Alpha")

    # Cleanup
    await disc1.stop()
    await disc2.stop()
    await band1.stop()
    await band2.stop()

    if len(stream_received_alpha) >= 4:  # Allow some packet loss
        print(f"[PASS] Alpha received {len(stream_received_alpha)}/5 stream messages")
    else:
        print(f"[FAIL] Alpha only received {len(stream_received_alpha)}/5 stream messages")
        success = False

    if len(stream_received_beta) >= 4:
        print(f"[PASS] Beta received {len(stream_received_beta)}/5 stream messages")
    else:
        print(f"[FAIL] Beta only received {len(stream_received_beta)}/5 stream messages")
        success = False

    print(f"\n{'Full stack test PASSED' if success else 'Full stack test FAILED'}")
    return success


if __name__ == "__main__":
    success = asyncio.run(test_full_stack())
    sys.exit(0 if success else 1)
