"""
Control channel

Handles peer discovery, keepalive, focus changes, and metacontrol (settings sync)
"""

import json
import time
import logging
from typing import Callable, Optional, Dict, Any
from enum import IntEnum

from .framing import pack_control_message, unpack_packet, ChannelType

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

    def __init__(self, band_id: bytes, crypto, transport):
        """
        Initialize control channel

        Args:
            band_id: Band identifier
            crypto: BandCrypto instance
            transport: UDPTransport instance
        """
        self.band_id = band_id
        self.crypto = crypto
        self.transport = transport

        # Sequence number
        self._send_sequence = 0

        # Peer destinations
        self._destinations = []

        # Callbacks for different message types
        self._handlers: Dict[int, list] = {}

        # Keepalive tracking
        self._last_keepalive_sent = 0
        self._keepalive_interval = 5.0  # seconds

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

    def send_message(self, msg_type: ControlMessageType, payload: Dict[str, Any], dest: Optional[tuple] = None):
        """
        Send a control message

        Args:
            msg_type: Message type
            payload: Payload dictionary (will be JSON-encoded)
            dest: Specific destination, or None to broadcast to all peers
        """

        # Increment sequence
        sequence = self._send_sequence
        self._send_sequence += 1

        # Build message
        message = {
            "type": int(msg_type),
            "timestamp": int(time.time() * 1000),
            "payload": payload
        }

        # Encode as JSON
        message_bytes = json.dumps(message).encode('utf-8')

        # AAD for AEAD
        aad = bytes([ChannelType.CONTROL, 0, 0])

        # Encrypt
        ciphertext = self.crypto.encrypt(sequence, message_bytes, aad)

        # Pack
        packet = pack_control_message(self.band_id, sequence, ciphertext)

        # Send
        destinations = [dest] if dest else self._destinations
        for d in destinations:
            self.transport.send(d, packet)

    def send_hello(self, hostname: str, dest: tuple):
        """
        Send HELLO to a peer

        Args:
            hostname: This machine's hostname
            dest: Peer address
        """
        self.send_message(ControlMessageType.HELLO, {"hostname": hostname}, dest)

    def send_hello_ack(self, hostname: str, dest: tuple):
        """Send HELLO_ACK to a peer"""
        self.send_message(ControlMessageType.HELLO_ACK, {"hostname": hostname}, dest)

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

            # Build AAD
            aad = bytes([ChannelType.CONTROL, 0, 0])

            # Decrypt
            message_bytes = self.crypto.decrypt(packet.sequence, packet.ciphertext, aad)

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
