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
from .sequence import SequenceSource

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
        crypto=None,
        transport=None,
        priority: int = 128,
        crypto_for=None,
        seq_source=None,
        send_crypto=None,
        recv_crypto=None
    ):
        """
        Initialize a Stream

        Args:
            band_id: Band identifier (16 bytes)
            stream_id: Stream identifier (0-65535)
            crypto: a single BandCrypto (single-cipher / tests)
            transport: UDPTransport instance for sending
            priority: Priority level (0 = highest, 255 = lowest)
            crypto_for: optional resolver(peer_addr, cipher_id=None) -> BandCrypto
                for per-peer negotiated ciphers (SPEC §3.5)
        """
        self.band_id = band_id
        self.stream_id = stream_id
        # Per-session data keys: send uses OUR epoch, recv uses the PEER's
        # (SPEC §3.1/§3.3). Falls back to the legacy single-crypto resolver, or a
        # fixed BandCrypto, for standalone/test use.
        if send_crypto is not None and recv_crypto is not None:
            self._send_crypto = send_crypto
            self._recv_crypto = recv_crypto
        elif crypto_for is not None:
            self._send_crypto = lambda addr: crypto_for(addr)
            self._recv_crypto = lambda addr: crypto_for(addr)
        else:
            self._send_crypto = lambda addr: crypto
            self._recv_crypto = lambda addr: crypto
        self.crypto = crypto
        self.transport = transport
        self.priority = priority

        # Shared per-sender sequence source (SPEC §3.3). One per band, shared with
        # Control/Channel so nonces never collide; a private one if omitted.
        self._seq_source = seq_source if seq_source is not None else SequenceSource()

        # High-water mark for inbound packets (per peer). A missing entry means
        # "not yet seen" -> the first packet is accepted at whatever (random)
        # sequence the sender started from; thereafter strictly increasing.
        self._recv_watermark: Dict[tuple, int] = {}

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

        # Draw one sequence from the shared per-sender source (SPEC §3.3).
        sequence = self._seq_source.next()

        # Build associated data for AEAD
        aad = bytes([
            ChannelType.STREAM,
            self.stream_id >> 8,
            self.stream_id & 0xFF
        ])

        # SPEC §5.1 stream payload = priority (1 byte) || data. (A prior
        # implementation inserted a 4-byte timestamp here, which broke interop
        # with the Rust reference; removed to conform.)
        payload = bytes([self.priority]) + data

        # Encrypt per destination under our session data key for that peer's
        # negotiated suite (SPEC §3.1/§3.5). Sequence is shared across dests.
        for dest in self._destinations:
            crypto = self._send_crypto(dest)
            ciphertext = crypto.encrypt(sequence, payload, aad)
            packet = pack_stream_message(self.band_id, self.stream_id, sequence, ciphertext)
            self.transport.send(dest, packet)

    def reset_peer(self, peer_addr: tuple):
        """Forget a peer's watermark so its next packet is accepted afresh.
        Called when the peer (re)starts a session (SPEC §3.3)."""
        self._recv_watermark.pop(peer_addr, None)

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

            # Check high-water mark (drop stale/replayed packets, SPEC §3.3).
            # First packet from a peer is accepted at its random start; then
            # strictly increasing.
            watermark = self._recv_watermark.get(peer_addr)
            if watermark is not None and packet.sequence <= watermark:
                logger.debug(f"Dropping stale packet: seq={packet.sequence}, watermark={watermark}")
                return

            # Build AAD for decryption
            aad = bytes([
                ChannelType.STREAM,
                self.stream_id >> 8,
                self.stream_id & 0xFF
            ])

            # Decrypt under the sender's session data key (its epoch, learned
            # from its HELLO). `None` -> we have not seen the peer's HELLO yet
            # (or it restarted and we've not re-handshaked), so drop. Raises on
            # auth failure before we advance the watermark, so a forged or
            # old-session packet cannot wedge the mark.
            rc = self._recv_crypto(peer_addr)
            if rc is None:
                logger.debug(f"No session key for {peer_addr} yet; dropping stream packet")
                return
            payload = rc.decrypt(packet.sequence, packet.ciphertext, aad)

            # Authenticated and fresh: advance the watermark.
            self._recv_watermark[peer_addr] = packet.sequence

            # SPEC §5.1: payload = priority (1 byte) || data. The wire carries no
            # timestamp; the receive callback gets a local receive time for
            # compatibility with its (data, peer, timestamp) shape.
            priority = payload[0]  # noqa: F841 — reserved for §5.3 priority handling
            data = payload[1:]
            timestamp = int(time.time() * 1000)

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
