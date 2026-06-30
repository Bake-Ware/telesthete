"""
Control channel

Handles peer discovery, keepalive, focus changes, and metacontrol (settings sync)
"""

import json
import time
import logging
from typing import Callable, Optional, Dict, Any
from collections import defaultdict
from enum import IntEnum

from .framing import pack_control_message, unpack_packet, ChannelType
from .crypto import BASELINE_CIPHER

logger = logging.getLogger(__name__)


class ControlMessageType(IntEnum):
    """Control message types"""
    HELLO = 0x01           # Peer introduction
    HELLO_ACK = 0x02       # Hello acknowledgment
    KEEPALIVE = 0x03       # Heartbeat
    FOCUS_CHANGE = 0x04    # Focus state change
    METACONTROL = 0x05     # Settings/config sync (monitor layout, etc)
    GOODBYE = 0x06         # Peer disconnect


class ControlChannel:
    """
    Control channel for Band management

    Always uses channel_type=0, channel_id=0
    Always reliable (we'll add retransmission later if needed)
    """

    def __init__(self, band_id: bytes, crypto=None, transport=None, crypto_for=None):
        """
        Initialize control channel

        Args:
            band_id: Band identifier
            crypto: a single BandCrypto (single-cipher / tests)
            transport: UDPTransport instance
            crypto_for: optional resolver(peer_addr, cipher_id=None) -> BandCrypto
                for per-peer negotiated ciphers (SPEC §3.5)
        """
        self.band_id = band_id
        self.transport = transport
        if crypto_for is not None:
            self._crypto_for = crypto_for
        else:
            self._crypto_for = lambda addr, cipher_id=None: crypto
        self.crypto = crypto

        # Sequence number
        self._send_sequence = 0

        # Peer destinations
        self._destinations = []

        # Callbacks for different message types
        self._handlers: Dict[int, list] = {}

        # Keepalive tracking
        self._last_keepalive_sent = 0
        self._keepalive_interval = 5.0  # seconds

        # Replay protection: highest accepted sequence per peer (SPEC §3.3).
        # Control is monotonic per sender, so a watermark is exact.
        self._recv_watermark: Dict[tuple, int] = defaultdict(lambda: -1)

    def add_destination(self, peer_addr: tuple):
        """Add peer destination"""
        if peer_addr not in self._destinations:
            self._destinations.append(peer_addr)
            logger.debug(f"Control: Added destination {peer_addr}")

    def remove_destination(self, peer_addr: tuple):
        """Remove peer destination"""
        if peer_addr in self._destinations:
            self._destinations.remove(peer_addr)
            logger.debug(f"Control: Removed destination {peer_addr}")

    def register_handler(self, msg_type: ControlMessageType, handler: Callable):
        """
        Register handler for a control message type

        Args:
            msg_type: Message type to handle
            handler: Callback(peer_addr, payload_dict)
        """
        if msg_type not in self._handlers:
            self._handlers[msg_type] = []
        self._handlers[msg_type].append(handler)

    def send_message(self, msg_type: ControlMessageType, payload: Dict[str, Any], dest: Optional[tuple] = None, baseline: bool = False):
        """
        Send a control message

        Args:
            msg_type: Message type
            payload: Payload dictionary (will be JSON-encoded)
            dest: Specific destination, or None to broadcast to all peers
        """

        # Increment sequence (shared across destinations)
        sequence = self._send_sequence
        self._send_sequence += 1

        message = {
            "type": int(msg_type),
            "timestamp": int(time.time() * 1000),
            "payload": payload
        }
        message_bytes = json.dumps(message).encode('utf-8')
        aad = bytes([ChannelType.CONTROL, 0, 0])

        # HELLO/HELLO_ACK bootstrap on the baseline suite (SPEC §3.5); all
        # other messages use each peer's negotiated suite. Encrypt per
        # destination because peers may have negotiated different ciphers.
        cipher_id = BASELINE_CIPHER if baseline else None
        destinations = [dest] if dest else list(self._destinations)
        for d in destinations:
            crypto = self._crypto_for(d, cipher_id)
            ciphertext = crypto.encrypt(sequence, message_bytes, aad)
            packet = pack_control_message(self.band_id, sequence, ciphertext)
            self.transport.send(d, packet)

    def send_hello(self, hostname: str, dest: tuple,
                   capabilities=None, ciphers=None):
        """Send HELLO with mandatory capabilities + ordered ciphers (SPEC §4.3).
        Always on the baseline suite."""
        self.send_message(ControlMessageType.HELLO, {
            "hostname": hostname,
            "capabilities": capabilities or [],
            "ciphers": ciphers or [BASELINE_CIPHER],
        }, dest, baseline=True)

    def send_hello_ack(self, hostname: str, dest: tuple,
                       capabilities=None, ciphers=None, cipher=None):
        """Send HELLO_ACK committing the negotiated `cipher` (SPEC §4.4)."""
        self.send_message(ControlMessageType.HELLO_ACK, {
            "hostname": hostname,
            "capabilities": capabilities or [],
            "ciphers": ciphers or [BASELINE_CIPHER],
            "cipher": cipher or BASELINE_CIPHER,
        }, dest, baseline=True)

    def send_keepalive(self):
        """Send keepalive to all peers"""
        now = time.time()
        if now - self._last_keepalive_sent >= self._keepalive_interval:
            self.send_message(ControlMessageType.KEEPALIVE, {})
            self._last_keepalive_sent = now

    def send_focus_change(self, focused_peer: Optional[str]):
        """
        Send focus change notification

        Args:
            focused_peer: Hostname of peer now receiving focus, or None for local
        """
        self.send_message(ControlMessageType.FOCUS_CHANGE, {"focused_peer": focused_peer})

    def send_metacontrol(self, settings: Dict[str, Any]):
        """
        Send settings/config update

        Args:
            settings: Settings dictionary (e.g., monitor layout)
        """
        self.send_message(ControlMessageType.METACONTROL, settings)

    def send_goodbye(self):
        """Send goodbye before disconnect"""
        self.send_message(ControlMessageType.GOODBYE, {})

    def _decrypt_control(self, peer_addr: tuple, sequence: int, ciphertext: bytes, aad: bytes) -> bytes:
        """Decrypt with the peer's negotiated suite, falling back to baseline
        (HELLO/HELLO_ACK always use baseline). Raises if both fail."""
        negotiated = self._crypto_for(peer_addr)
        try:
            return negotiated.decrypt(sequence, ciphertext, aad)
        except Exception:
            baseline = self._crypto_for(peer_addr, BASELINE_CIPHER)
            if baseline is negotiated:
                raise
            return baseline.decrypt(sequence, ciphertext, aad)

    def handle_packet(self, peer_addr: tuple, packet_bytes: bytes):
        """
        Handle received control packet

        Args:
            peer_addr: Sender address
            packet_bytes: Raw packet bytes
        """

        try:
            # Unpack
            packet = unpack_packet(packet_bytes)

            # Verify channel type
            if packet.channel_type != ChannelType.CONTROL:
                logger.warning(f"Wrong channel type: {packet.channel_type}")
                return

            # Replay protection (SPEC §3.3): reject seq <= high-water mark.
            if packet.sequence <= self._recv_watermark[peer_addr]:
                logger.debug(
                    f"Dropping replayed/stale control packet: seq={packet.sequence}, "
                    f"watermark={self._recv_watermark[peer_addr]}")
                return

            # Build AAD
            aad = bytes([ChannelType.CONTROL, 0, 0])

            # Decrypt with negotiated suite, baseline fallback (SPEC §3.5).
            # Raises on auth failure -> watermark not advanced, so a forged
            # high-seq packet cannot wedge the mark.
            message_bytes = self._decrypt_control(peer_addr, packet.sequence, packet.ciphertext, aad)

            # Authenticated: advance the watermark.
            self._recv_watermark[peer_addr] = packet.sequence

            # Decode JSON
            message = json.loads(message_bytes.decode('utf-8'))

            msg_type = ControlMessageType(message["type"])
            payload = message["payload"]

            logger.debug(f"Control message from {peer_addr}: type={msg_type.name}")

            # Call handlers
            handlers = self._handlers.get(msg_type, [])
            for handler in handlers:
                try:
                    handler(peer_addr, payload)
                except Exception as e:
                    logger.error(f"Control handler error: {e}")

        except Exception as e:
            logger.error(f"Error handling control packet: {e}")


def test_control():
    """Test control channel"""
    print("Testing Control Channel")
    print("=" * 60)

    import sys
    import os
    sys.path.insert(0, os.path.join(os.path.dirname(__file__), '../..'))

    from telesthete.protocol.crypto import BandCrypto

    # Mock transport
    class MockTransport:
        def __init__(self):
            self.sent_packets = []

        def send(self, dest, packet):
            self.sent_packets.append((dest, packet))

    # Setup
    psk = "test-control-psk"
    crypto = BandCrypto(psk)
    transport = MockTransport()

    # Create control channel
    control = ControlChannel(crypto.band_id, crypto, transport)
    control.add_destination(("127.0.0.1", 9999))

    # Track received messages
    received_hellos = []
    received_keepalives = []

    def on_hello(peer_addr, payload):
        print(f"HELLO from {peer_addr}: {payload}")
        received_hellos.append((peer_addr, payload))

    def on_keepalive(peer_addr, payload):
        print(f"KEEPALIVE from {peer_addr}")
        received_keepalives.append(peer_addr)

    control.register_handler(ControlMessageType.HELLO, on_hello)
    control.register_handler(ControlMessageType.KEEPALIVE, on_keepalive)

    # Send messages
    print("\nSending messages...")
    control.send_hello("test-machine", ("127.0.0.1", 9999))
    control.send_keepalive()

    print(f"Sent {len(transport.sent_packets)} packets")

    # Simulate receiving
    print("\nSimulating receive...")
    for dest, packet_bytes in transport.sent_packets:
        control.handle_packet(("127.0.0.1", 8888), packet_bytes)

    # Check results
    assert len(received_hellos) == 1
    assert received_hellos[0][1]["hostname"] == "test-machine"
    assert len(received_keepalives) == 1

    print("\nControl channel test passed")


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    test_control()
