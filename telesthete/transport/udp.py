"""
UDP transport layer

Manages socket I/O and packet routing
"""

import asyncio
import socket
from typing import Callable, Optional, Dict, Tuple
from collections import defaultdict
import logging

logger = logging.getLogger(__name__)


class UDPTransport:
    """
    UDP socket manager with async send/receive loops

    Routes received packets to registered handlers based on channel_type.
    """

    def __init__(self, bind_address: str = "0.0.0.0", bind_port: int = 0):
        """
        Initialize UDP transport

        Args:
            bind_address: Address to bind to (default: all interfaces)
            bind_port: Port to bind to (0 = auto-assign)
        """
        self.bind_address = bind_address
        self.bind_port = bind_port
        self.socket: Optional[socket.socket] = None
        self.local_address: Optional[Tuple[str, int]] = None

        # Handlers: channel_type -> list of callbacks
        # Each callback receives (peer_addr, packet_bytes)
        self._handlers: Dict[int, list] = defaultdict(list)

        # Send queue for outbound packets
        # Items: (destination_addr, packet_bytes)
        self._send_queue: asyncio.Queue = asyncio.Queue()

        # Running state
        self._running = False
        self._tasks = []

    def start(self):
        """
        Create and bind UDP socket
        """
        if self.socket:
            raise RuntimeError("Transport already started")

        # Create UDP socket
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)

        # Bind
        self.socket.bind((self.bind_address, self.bind_port))
        self.local_address = self.socket.getsockname()

        # Set non-blocking for asyncio
        self.socket.setblocking(False)

        logger.info(f"UDP transport bound to {self.local_address}")

    async def run(self):
        """
        Start send/receive loops

        This should be called from an asyncio event loop.
        """
        if not self.socket:
            raise RuntimeError("Transport not started. Call start() first.")

        self._running = True

        # Start send and receive loops
        self._tasks = [
            asyncio.create_task(self._send_loop()),
            asyncio.create_task(self._recv_loop())
        ]

        logger.info("UDP transport running")

        # Wait for both tasks (they run forever until stopped)
        try:
            await asyncio.gather(*self._tasks)
        except asyncio.CancelledError:
            logger.info("UDP transport stopped")

    async def stop(self):
        """
        Stop send/receive loops and close socket
        """
        self._running = False

        # Cancel tasks
        for task in self._tasks:
            task.cancel()

        # Wait for cancellation
        await asyncio.gather(*self._tasks, return_exceptions=True)

        # Close socket
        if self.socket:
            self.socket.close()
            self.socket = None

        logger.info("UDP transport stopped")

    def register_handler(self, channel_type: int, handler: Callable):
        """
        Register a handler for a channel type

        Args:
            channel_type: Channel type to handle (0-255)
            handler: Callback function(peer_addr, packet_bytes)
        """
        self._handlers[channel_type].append(handler)
        logger.debug(f"Registered handler for channel_type={channel_type}")

    def send(self, destination: Tuple[str, int], packet_bytes: bytes):
        """
        Queue a packet for sending

        Args:
            destination: (host, port) tuple
            packet_bytes: Raw packet bytes (already encrypted and framed)
        """
        # Non-blocking enqueue
        try:
            self._send_queue.put_nowait((destination, packet_bytes))
        except asyncio.QueueFull:
            logger.warning(f"Send queue full, dropping packet to {destination}")

    async def _send_loop(self):
        """
        Send loop: pull packets from queue and send over UDP
        """
        loop = asyncio.get_event_loop()

        while self._running:
            try:
                # Wait for packet to send
                destination, packet_bytes = await self._send_queue.get()

                # Send via socket (non-blocking)
                await loop.sock_sendto(self.socket, packet_bytes, destination)

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Error in send loop: {e}")
                await asyncio.sleep(0.1)

    async def _recv_loop(self):
        """
        Receive loop: receive packets from UDP and route to handlers
        """
        loop = asyncio.get_event_loop()

        while self._running:
            try:
                # Receive packet (non-blocking, waits for data)
                packet_bytes, peer_addr = await loop.sock_recvfrom(self.socket, 65535)

                # Route to handlers based on channel_type
                # We need to peek at channel_type without unpacking the whole thing
                # Wire format: band_id(16) + channel_type(1) + ...
                if len(packet_bytes) < 17:
                    logger.warning(f"Received undersized packet from {peer_addr}: {len(packet_bytes)} bytes")
                    continue

                channel_type = packet_bytes[16]  # Byte 16 is channel_type

                # Call all registered handlers for this channel type
                handlers = self._handlers.get(channel_type, [])
                for handler in handlers:
                    try:
                        # Call handler (may be async or sync)
                        result = handler(peer_addr, packet_bytes)
                        if asyncio.iscoroutine(result):
                            await result
                    except Exception as e:
                        logger.error(f"Handler error for channel_type={channel_type}: {e}")

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Error in recv loop: {e}")
                await asyncio.sleep(0.1)


async def test_udp_transport():
    """
    Test UDP transport by sending packets between two transports on localhost
    """
    print("Testing UDP transport")
    print("=" * 60)

    # Create two transports
    transport1 = UDPTransport(bind_address="127.0.0.1", bind_port=0)
    transport2 = UDPTransport(bind_address="127.0.0.1", bind_port=0)

    transport1.start()
    transport2.start()

    print(f"Transport 1 bound to: {transport1.local_address}")
    print(f"Transport 2 bound to: {transport2.local_address}")

    # Track received packets
    received = []

    def handler1(peer_addr, packet_bytes):
        print(f"Transport 1 received from {peer_addr}: {packet_bytes[:20]}...")
        received.append(("t1", peer_addr, packet_bytes))

    def handler2(peer_addr, packet_bytes):
        print(f"Transport 2 received from {peer_addr}: {packet_bytes[:20]}...")
        received.append(("t2", peer_addr, packet_bytes))

    # Register handlers for channel_type=1
    transport1.register_handler(1, handler1)
    transport2.register_handler(1, handler2)

    # Start transports
    async def run_transports():
        await asyncio.gather(
            transport1.run(),
            transport2.run()
        )

    transport_task = asyncio.create_task(run_transports())

    # Give them a moment to start
    await asyncio.sleep(0.1)

    # Send packets
    # Packet format: band_id(16) + channel_type(1) + rest
    test_packet_1to2 = b"0" * 16 + bytes([1]) + b"Hello from T1 to T2"
    test_packet_2to1 = b"0" * 16 + bytes([1]) + b"Hello from T2 to T1"

    print("\nSending packets...")
    transport1.send(transport2.local_address, test_packet_1to2)
    transport2.send(transport1.local_address, test_packet_2to1)

    # Wait for packets to be received
    await asyncio.sleep(0.2)

    # Check results
    print("\nReceived packets:")
    for who, peer, packet in received:
        print(f"  {who}: {packet}")

    # Cleanup
    await transport1.stop()
    await transport2.stop()
    transport_task.cancel()

    print("\nTest complete")


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    asyncio.run(test_udp_transport())
