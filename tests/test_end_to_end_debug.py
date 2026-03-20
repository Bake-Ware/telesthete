"""
End-to-end test with debug logging
"""

import sys
import os
import asyncio
import logging

sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))

from telesthete import Band

# Enable debug logging for everything
logging.basicConfig(level=logging.DEBUG, format='%(name)-30s: %(message)s')


async def test_debug():
    """Test with full debugging"""
    print("Debug Test")
    print("=" * 60)

    band1 = Band(psk="debug-psk", hostname="m1", bind_port=30001)
    band2 = Band(psk="debug-psk", hostname="m2", bind_port=30002)

    await band1.start()
    await band2.start()

    # Connect
    band1.connect_peer("127.0.0.1", 30002)
    await asyncio.sleep(1.0)

    print("\n" + "=" * 60)
    print("PEERS CONNECTED")
    print("=" * 60)

    # Open streams
    s1 = band1.stream(stream_id=1, priority=0)
    s2 = band2.stream(stream_id=1, priority=0)

    received = []

    def recv(data, peer, ts):
        print(f"\n>>> RECEIVED: {data} from {peer} <<<\n")
        received.append(data)

    s1.on_receive(recv)
    s2.on_receive(recv)

    print("\n" + "=" * 60)
    print("SENDING MESSAGE")
    print("=" * 60 + "\n")

    s1.send(b"TEST")

    await asyncio.sleep(1.0)

    print(f"\nReceived {len(received)} messages")

    await band1.stop()
    await band2.stop()


if __name__ == "__main__":
    asyncio.run(test_debug())
