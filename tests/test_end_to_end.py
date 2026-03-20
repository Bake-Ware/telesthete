"""
End-to-end integration test

Two Bands communicate over UDP on localhost
"""

import sys
import os
import asyncio
import logging

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))

from telesthete import Band

logging.basicConfig(level=logging.INFO, format='%(name)s: %(message)s')


async def test_two_bands():
    """
    Test two bands exchanging messages
    """
    print("End-to-End Test: Two Bands")
    print("=" * 60)

    # Create two bands
    band1 = Band(psk="test-e2e-psk", hostname="alice", bind_port=20001)
    band2 = Band(psk="test-e2e-psk", hostname="bob", bind_port=20002)

    # Start both
    await band1.start()
    await band2.start()

    print(f"Alice on {band1.transport.local_address}")
    print(f"Bob on {band2.transport.local_address}")

    # Connect Alice to Bob
    band1.connect_peer("127.0.0.1", 20002)

    # Wait for connection to establish
    print("\nWaiting for connection...")
    await asyncio.sleep(1.0)

    print(f"Alice sees peers: {[p.hostname for p in band1.get_peers()]}")
    print(f"Bob sees peers: {[p.hostname for p in band2.get_peers()]}")

    # Open streams
    stream_alice = band1.stream(stream_id=100, priority=0)
    stream_bob = band2.stream(stream_id=100, priority=0)

    # Track received messages
    alice_received = []
    bob_received = []

    def alice_recv(data, peer, ts):
        print(f"Alice received: {data} from {peer}")
        alice_received.append(data)

    def bob_recv(data, peer, ts):
        print(f"Bob received: {data} from {peer}")
        bob_received.append(data)

    stream_alice.on_receive(alice_recv)
    stream_bob.on_receive(bob_recv)

    # Send messages
    print("\nSending messages...")
    stream_alice.send(b"Hello Bob!")
    stream_bob.send(b"Hello Alice!")

    # Wait for messages to be received
    await asyncio.sleep(0.5)

    print(f"\nAlice received {len(alice_received)} messages")
    print(f"Bob received {len(bob_received)} messages")

    # Verify
    success = True
    if len(bob_received) == 1 and bob_received[0] == b"Hello Bob!":
        print("[PASS] Bob received Alice's message")
    else:
        print("[FAIL] Bob did not receive Alice's message")
        success = False

    if len(alice_received) == 1 and alice_received[0] == b"Hello Alice!":
        print("[PASS] Alice received Bob's message")
    else:
        print("[FAIL] Alice did not receive Bob's message")
        success = False

    # Cleanup
    await band1.stop()
    await band2.stop()

    print(f"\n{'Test PASSED' if success else 'Test FAILED'}")
    return success


if __name__ == "__main__":
    success = asyncio.run(test_two_bands())
    sys.exit(0 if success else 1)
