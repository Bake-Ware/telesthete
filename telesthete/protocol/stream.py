"""
Stream channel implementation

Real-time, lossy, prioritized datagrams. Fire-and-forget semantics.
No retransmission. Late packets are dropped (high-water mark).
"""

import time
import logging
from typing import Callable, Optional, Dict
from collections import defaultdict

from .framing import pack_stream_message, unpack_packet, ChannelType

logger = logging.getLogger(__name__)


class Stream:
    """
    A lossy, real-time stream channel

    Use for HID events, real-time state, anything where latency matters
    more than reliability.
    """

    def __init__(
        self,
        band_id: bytes,
        stream_id: int,
        crypto,
        transport,
        priority: int = 128
    ):
        """
        Initialize a Stream

        Args:
            band_id: Band identifier (16 bytes)
            stream_id: Stream identifier (0-65535)
            crypto: BandCrypto instance for encryption
            transport: UDPTransport instance for sending
            priority: Priority level (0 = highest, 255 = lowest)
        """
        self.band_id = band_id
        self.stream_id = stream_id
        self.crypto = crypto
        self.transport = transport
        self.priority = priority

        # Sequence number for outbound packets
        self._send_sequence = 0

        # High-water mark for inbound packets (per peer)
        # Maps peer_addr -> highest_sequence_seen
        self._recv_watermark: Dict[tuple, int] = defaultdict(lambda: -1)

        # Receive callback
        self._on_receive: Optional[Callable] = None

        # Peer destinations (where to send)
        self._destinations = []

    def add_destination(self, peer_addr: tuple):
        """
        Add a peer destination for this stream

        Args:
            peer_addr: (host, port) tuple
        """
        if peer_addr not in self._destinations:
            self._destinations.append(peer_addr)
            logger.debug(f"Stream {self.stream_id}: Added destination {peer_addr}")

    def remove_destination(self, peer_addr: tuple):
        """
        Remove a peer destination

        Args:
            peer_addr: (host, port) tuple
        """
        if peer_addr in self._destinations:
            self._destinations.remove(peer_addr)
            logger.debug(f"Stream {self.stream_id}: Removed destination {peer_addr}")

    def send(self, data: bytes):
        """
        Send data on this stream (fire-and-forget)

        Args:
            data: Payload to send
        """

        # Increment sequence
        sequence = self._send_sequence
        self._send_sequence += 1

        # Build associated data for AEAD
        aad = bytes([
            ChannelType.STREAM,
            self.stream_id >> 8,
            self.stream_id & 0xFF
        ])

        # Prepend priority and timestamp to payload
        timestamp = int(time.time() * 1000) & 0xFFFFFFFF  # 32-bit milliseconds
        payload = bytes([self.priority]) + timestamp.to_bytes(4, 'big') + data

        # Encrypt
        ciphertext = self.crypto.encrypt(sequence, payload, aad)

        # Pack
        packet = pack_stream_message(self.band_id, self.stream_id, sequence, ciphertext)

        # Send to all destinations
        for dest in self._destinations:
            self.transport.send(dest, packet)

    def on_receive(self, callback: Callable[[bytes, tuple, int], None]):
        """
        Register callback for received data

        Args:
            callback: Function(data, peer_addr, timestamp)
        """
        self._on_receive = callback

    def handle_packet(self, peer_addr: tuple, packet_bytes: bytes):
        """
        Handle a received packet for this stream

        Called by the Band when a stream packet arrives.

        Args:
            peer_addr: Sender address
            packet_bytes: Raw packet bytes
        """

        try:
            # Unpack
            packet = unpack_packet(packet_bytes)

            # Verify channel type and ID
            if packet.channel_type != ChannelType.STREAM:
                logger.warning(f"Wrong channel type: {packet.channel_type}")
                return

            if packet.channel_id != self.stream_id:
                logger.warning(f"Wrong stream ID: {packet.channel_id}")
                return

            # Check high-water mark (drop stale packets)
            watermark = self._recv_watermark[peer_addr]
            if packet.sequence <= watermark:
                logger.debug(f"Dropping stale packet: seq={packet.sequence}, watermark={watermark}")
                return

            # Update watermark
            self._recv_watermark[peer_addr] = packet.sequence

            # Build AAD for decryption
            aad = bytes([
                ChannelType.STREAM,
                self.stream_id >> 8,
                self.stream_id & 0xFF
            ])

            # Decrypt
            payload = self.crypto.decrypt(packet.sequence, packet.ciphertext, aad)

            # Extract priority and timestamp
            priority = payload[0]
            timestamp = int.from_bytes(payload[1:5], 'big')
            data = payload[5:]

            # Call receive handler
            if self._on_receive:
                self._on_receive(data, peer_addr, timestamp)

        except Exception as e:
            logger.error(f"Error handling stream packet: {e}")


def test_stream():
    """
    Test stream with mock transport and crypto
    """
    print("Testing Stream")
    print("=" * 60)

    import sys
    import os
    sys.path.insert(0, os.path.join(os.path.dirname(__file__), '../..'))

    from telesthete.protocol.crypto import BandCrypto
    from telesthete.protocol.framing import unpack_packet

    # Mock transport
    class MockTransport:
        def __init__(self):
            self.sent_packets = []

        def send(self, dest, packet):
            self.sent_packets.append((dest, packet))

    # Setup
    psk = "test-stream-psk"
    crypto = BandCrypto(psk)
    transport = MockTransport()

    # Create stream
    stream = Stream(crypto.band_id, stream_id=42, crypto=crypto, transport=transport, priority=0)
    stream.add_destination(("127.0.0.1", 9999))

    # Track received data
    received = []

    def on_recv(data, peer_addr, timestamp):
        print(f"Received: {data} from {peer_addr} at {timestamp}")
        received.append((data, peer_addr, timestamp))

    stream.on_receive(on_recv)

    # Send data
    print("\nSending data...")
    stream.send(b"Test message 1")
    stream.send(b"Test message 2")

    print(f"Sent {len(transport.sent_packets)} packets")

    # Simulate receiving the packets
    print("\nSimulating receive...")
    for dest, packet_bytes in transport.sent_packets:
        # Unpack to verify structure
        packet = unpack_packet(packet_bytes)
        print(f"  Packet: seq={packet.sequence}, channel_id={packet.channel_id}")

        # Handle (simulate peer receiving)
        stream.handle_packet(("127.0.0.1", 8888), packet_bytes)

    # Check results
    print(f"\nReceived {len(received)} messages:")
    for data, peer, ts in received:
        print(f"  {data} from {peer}")

    assert len(received) == 2
    assert received[0][0] == b"Test message 1"
    assert received[1][0] == b"Test message 2"

    print("\nStream test passed")


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    test_stream()
