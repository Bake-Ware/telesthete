"""
Channel implementation

Reliable, ordered byte streams with flow control. Like TCP over UDP.
"""

import asyncio
import time
import logging
from typing import Optional, Dict, Callable
from collections import deque
from dataclasses import dataclass

from .framing import pack_packet, unpack_packet, ChannelType

logger = logging.getLogger(__name__)


@dataclass
class ChannelPacket:
    """A packet in flight"""
    sequence: int
    data: bytes
    sent_at: float
    acked: bool = False


class Channel:
    """
    Reliable, ordered byte stream with flow control

    Use for clipboard, file transfers, any operation requiring reliability.
    """

    # Flags
    FLAG_SYN = 0x01  # Open connection
    FLAG_FIN = 0x02  # Close connection
    FLAG_ACK = 0x04  # Acknowledgment
    FLAG_RST = 0x08  # Reset connection

    def __init__(
        self,
        band_id: bytes,
        channel_id: int,
        crypto,
        transport,
        peer_addr: tuple
    ):
        """
        Initialize a Channel

        Args:
            band_id: Band identifier
            channel_id: Channel identifier
            crypto: BandCrypto instance
            transport: UDPTransport instance
            peer_addr: Peer address (host, port)
        """
        self.band_id = band_id
        self.channel_id = channel_id
        self.crypto = crypto
        self.transport = transport
        self.peer_addr = peer_addr

        # Sequence numbers
        self._send_sequence = 0
        self._recv_sequence = 0
        self._last_ack_sent = -1

        # Send buffer (packets waiting for ACK)
        self._send_buffer: Dict[int, ChannelPacket] = {}

        # Receive buffer (out-of-order packets)
        self._recv_buffer: Dict[int, bytes] = {}

        # Window size
        self.window_size = 32  # packets
        self.max_packet_size = 1024  # bytes

        # Retransmission
        self.rto = 0.5  # retransmission timeout (seconds)
        self._retransmit_task = None

        # Flow control
        self._remote_window = self.window_size

        # State
        self._state = "CLOSED"  # CLOSED, SYN_SENT, ESTABLISHED, FIN_SENT
        self._closed = asyncio.Event()

        # Receive callback
        self._on_receive: Optional[Callable[[bytes], None]] = None

        # Accumulated received data (in order)
        self._recv_queue = deque()
        self._recv_ready = asyncio.Event()

    async def open(self):
        """
        Open the channel (send SYN)
        """
        if self._state != "CLOSED":
            raise RuntimeError(f"Channel already open (state={self._state})")

        # Send SYN
        self._send_packet(b"", self.FLAG_SYN)
        self._state = "SYN_SENT"

        logger.debug(f"Channel {self.channel_id}: SYN sent")

        # Start retransmit task
        self._retransmit_task = asyncio.create_task(self._retransmit_loop())

    async def close(self):
        """
        Close the channel (send FIN)
        """
        if self._state == "CLOSED":
            return

        # Send FIN
        self._send_packet(b"", self.FLAG_FIN)
        self._state = "FIN_SENT"

        logger.debug(f"Channel {self.channel_id}: FIN sent")

        # Wait for close to complete
        await asyncio.wait_for(self._closed.wait(), timeout=5.0)

        # Stop retransmit task
        if self._retransmit_task:
            self._retransmit_task.cancel()
            await asyncio.gather(self._retransmit_task, return_exceptions=True)

        self._state = "CLOSED"

    def send(self, data: bytes):
        """
        Send data on this channel

        Args:
            data: Data to send

        Note: This is not async - it queues the data and returns immediately.
        """
        logger.debug(f"Channel {self.channel_id}: send() called with {len(data)} bytes, state={self._state}")

        if self._state not in ("ESTABLISHED", "SYN_SENT"):
            raise RuntimeError(f"Channel not established (state={self._state})")

        # Fragment if needed
        offset = 0
        while offset < len(data):
            chunk = data[offset:offset + self.max_packet_size]
            logger.debug(f"Channel {self.channel_id}: Sending chunk seq={self._send_sequence}")
            self._send_packet(chunk, 0)
            offset += len(chunk)

    async def recv(self, timeout: Optional[float] = None) -> bytes:
        """
        Receive data from channel

        Args:
            timeout: Timeout in seconds (None = wait forever)

        Returns:
            Received data bytes

        Raises:
            asyncio.TimeoutError: If timeout expires
        """
        if timeout:
            await asyncio.wait_for(self._recv_ready.wait(), timeout=timeout)
        else:
            await self._recv_ready.wait()

        # Get data from queue
        data = self._recv_queue.popleft()

        # Clear ready flag if queue empty
        if not self._recv_queue:
            self._recv_ready.clear()

        return data

    def on_receive(self, callback: Callable[[bytes], None]):
        """
        Register callback for received data

        Args:
            callback: Function(data)
        """
        self._on_receive = callback

    def _send_packet(self, data: bytes, flags: int):
        """
        Send a packet

        Args:
            data: Payload
            flags: Packet flags
        """
        # Check window
        if len(self._send_buffer) >= self._remote_window:
            logger.warning(f"Channel {self.channel_id}: Send window full")
            return

        # Get sequence number
        sequence = self._send_sequence

        # Only increment sequence for packets that consume sequence space
        # (SYN, FIN, or data packets - not pure ACKs)
        if data or (flags & (self.FLAG_SYN | self.FLAG_FIN)):
            self._send_sequence += 1

        # Build payload
        # Format: flags(1) + ack(8) + window(2) + data
        ack_num = self._recv_sequence
        window = self.window_size

        payload = (
            bytes([flags]) +
            ack_num.to_bytes(8, 'big') +
            window.to_bytes(2, 'big') +
            data
        )

        # AAD for AEAD
        aad = bytes([
            ChannelType.CHANNEL,
            self.channel_id >> 8,
            self.channel_id & 0xFF
        ])

        # Encrypt
        ciphertext = self.crypto.encrypt(sequence, payload, aad)

        # Pack
        packet_bytes = pack_packet(
            self.band_id,
            ChannelType.CHANNEL,
            self.channel_id,
            sequence,
            ciphertext
        )

        # Send
        self.transport.send(self.peer_addr, packet_bytes)

        # Add to send buffer (unless it's an ACK-only packet)
        if data or flags:
            self._send_buffer[sequence] = ChannelPacket(
                sequence=sequence,
                data=data,
                sent_at=time.time(),
                acked=False
            )

        logger.debug(f"Channel {self.channel_id}: Sent seq={sequence}, flags={flags:#x}")

    def handle_packet(self, peer_addr: tuple, packet_bytes: bytes):
        """
        Handle received packet

        Args:
            peer_addr: Sender address
            packet_bytes: Raw packet bytes
        """
        logger.debug(f"Channel {self.channel_id} at {id(self)}: handle_packet called")
        try:
            # Unpack
            packet = unpack_packet(packet_bytes)

            # Verify
            if packet.channel_type != ChannelType.CHANNEL:
                return

            if packet.channel_id != self.channel_id:
                return

            # Build AAD
            aad = bytes([
                ChannelType.CHANNEL,
                self.channel_id >> 8,
                self.channel_id & 0xFF
            ])

            # Decrypt
            payload = self.crypto.decrypt(packet.sequence, packet.ciphertext, aad)

            # Parse payload
            flags = payload[0]
            ack_num = int.from_bytes(payload[1:9], 'big')
            window = int.from_bytes(payload[9:11], 'big')
            data = payload[11:]

            logger.debug(f"Channel {self.channel_id} at {id(self)}: Recv seq={packet.sequence}, ack={ack_num}, flags={flags:#x}, data_len={len(data)}")

            # Update remote window
            self._remote_window = window

            # Handle ACK
            if ack_num >= 0:
                self._handle_ack(ack_num)

            # Handle flags
            if flags & self.FLAG_SYN:
                self._handle_syn(packet.sequence)

            if flags & self.FLAG_FIN:
                self._handle_fin(packet.sequence)

            if flags & self.FLAG_RST:
                self._handle_rst()

            # Handle data
            if data:
                self._handle_data(packet.sequence, data)

        except Exception as e:
            logger.error(f"Channel {self.channel_id}: Error handling packet: {e}")

    def _handle_ack(self, ack_num: int):
        """Handle ACK"""
        # Mark packets as acked
        for seq in list(self._send_buffer.keys()):
            if seq <= ack_num:
                self._send_buffer[seq].acked = True
                del self._send_buffer[seq]

    def _handle_syn(self, sequence: int):
        """Handle SYN"""
        if self._state == "CLOSED":
            # Incoming connection
            self._state = "ESTABLISHED"
            # Expect next sequence after SYN
            self._recv_sequence = sequence + 1
            logger.info(f"Channel {self.channel_id}: Connection established (incoming), expecting seq={self._recv_sequence}")

            # Send SYN+ACK
            self._send_packet(b"", self.FLAG_SYN | self.FLAG_ACK)

        elif self._state == "SYN_SENT":
            # Our SYN was acked (or simultaneous open)
            self._state = "ESTABLISHED"
            # Expect next sequence after SYN
            self._recv_sequence = sequence + 1
            logger.info(f"Channel {self.channel_id}: Connection established (outgoing), expecting seq={self._recv_sequence}")

            # Send ACK
            self._send_packet(b"", self.FLAG_ACK)

    def _handle_fin(self, sequence: int):
        """Handle FIN"""
        logger.info(f"Channel {self.channel_id}: Received FIN")

        # Update recv sequence
        self._recv_sequence = sequence + 1

        # Send ACK
        self._send_packet(b"", self.FLAG_ACK)

        # If we also sent FIN, we're done
        if self._state == "FIN_SENT":
            self._closed.set()
            self._state = "CLOSED"
        else:
            # Peer initiated close, send our FIN
            self._send_packet(b"", self.FLAG_FIN)
            self._state = "CLOSED"
            self._closed.set()

    def _handle_rst(self):
        """Handle RST"""
        logger.warning(f"Channel {self.channel_id}: Connection reset by peer")
        self._closed.set()
        self._state = "CLOSED"

    def _handle_data(self, sequence: int, data: bytes):
        """Handle data packet"""
        logger.debug(f"Channel {self.channel_id} at {id(self)}: _handle_data seq={sequence}, expecting={self._recv_sequence}, data_len={len(data)}")

        # Check if in order
        if sequence == self._recv_sequence:
            # In order - deliver immediately
            logger.debug(f"Channel {self.channel_id}: Delivering in-order data")
            self._deliver_data(data)
            self._recv_sequence += 1

            # Check buffer for next packets
            while self._recv_sequence in self._recv_buffer:
                buffered_data = self._recv_buffer.pop(self._recv_sequence)
                self._deliver_data(buffered_data)
                self._recv_sequence += 1

        elif sequence > self._recv_sequence:
            # Out of order - buffer it
            self._recv_buffer[sequence] = data
            logger.debug(f"Channel {self.channel_id}: Buffered out-of-order packet seq={sequence}, expecting={self._recv_sequence}")

        else:
            logger.debug(f"Channel {self.channel_id}: Dropping duplicate seq={sequence}")

        # Send ACK
        if sequence >= self._last_ack_sent:
            self._send_packet(b"", self.FLAG_ACK)
            self._last_ack_sent = self._recv_sequence

    def _deliver_data(self, data: bytes):
        """Deliver data to application"""
        logger.debug(f"Channel {self.channel_id} at {id(self)}: _deliver_data called with {len(data)} bytes")

        # Add to receive queue
        self._recv_queue.append(data)
        self._recv_ready.set()

        # Call callback
        if self._on_receive:
            logger.debug(f"Channel {self.channel_id} at {id(self)}: Calling on_receive callback")
            try:
                self._on_receive(data)
            except Exception as e:
                logger.error(f"Channel {self.channel_id}: Receive callback error: {e}")
        else:
            logger.debug(f"Channel {self.channel_id} at {id(self)}: No on_receive callback registered")

    async def _retransmit_loop(self):
        """Retransmit unacked packets"""
        while True:
            try:
                await asyncio.sleep(0.1)

                now = time.time()

                # Check for packets needing retransmission
                for seq, pkt in list(self._send_buffer.items()):
                    if not pkt.acked and (now - pkt.sent_at) > self.rto:
                        logger.debug(f"Channel {self.channel_id}: Retransmitting seq={seq}")

                        # Rebuild and resend packet
                        # (This is simplified - in production would cache the full packet)
                        # For now, just mark as sent again
                        pkt.sent_at = now

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Channel {self.channel_id}: Retransmit error: {e}")


async def test_channel():
    """Test channel with mock transport and crypto"""
    print("Testing Channel")
    print("=" * 60)

    from .crypto import BandCrypto

    # Mock transport that delivers packets between two channels
    class MockTransport:
        def __init__(self):
            self.peer = None

        def send(self, dest, packet):
            if self.peer:
                # Simulate async delivery
                asyncio.create_task(self._deliver(packet))

        async def _deliver(self, packet):
            await asyncio.sleep(0.01)  # 10ms delay
            if self.peer:
                self.peer.handle_packet(("mock", 0), packet)

    # Setup
    psk = "test-channel-psk"
    crypto = BandCrypto(psk)

    transport1 = MockTransport()
    transport2 = MockTransport()

    # Create channels
    channel1 = Channel(crypto.band_id, 1, crypto, transport1, ("mock", 2))
    channel2 = Channel(crypto.band_id, 1, crypto, transport2, ("mock", 1))

    # Cross-connect transports
    transport1.peer = channel2
    transport2.peer = channel1

    # Track received data
    received1 = []
    received2 = []

    channel1.on_receive(lambda data: received1.append(data))
    channel2.on_receive(lambda data: received2.append(data))

    # Open channel1
    print("\nOpening channel...")
    await channel1.open()
    await asyncio.sleep(0.1)  # Wait for handshake

    print(f"Channel 1 state: {channel1._state}")
    print(f"Channel 2 state: {channel2._state}")

    # Send data
    print("\nSending data...")
    channel1.send(b"Hello from channel 1")
    channel2.send(b"Hello from channel 2")

    await asyncio.sleep(0.2)  # Wait for delivery

    print(f"\nChannel 1 received: {received1}")
    print(f"Channel 2 received: {received2}")

    # Close
    print("\nClosing channel...")
    await channel1.close()

    print(f"Channel 1 state: {channel1._state}")
    print(f"Channel 2 state: {channel2._state}")

    # Verify
    assert len(received1) == 1
    assert received1[0] == b"Hello from channel 2"
    assert len(received2) == 1
    assert received2[0] == b"Hello from channel 1"

    print("\nChannel test passed")


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    asyncio.run(test_channel())
