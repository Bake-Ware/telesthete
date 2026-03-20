"""
Band - the main API for Telesthete

A Band is a PSK-scoped encrypted communication context between peers.
"""

import asyncio
import logging
import socket
from typing import Optional, Dict, Callable, List

from .protocol.crypto import BandCrypto
from .protocol.stream import Stream
from .protocol.control import ControlChannel, ControlMessageType
from .protocol.framing import ChannelType
from .transport.udp import UDPTransport
from .peer import Peer

logger = logging.getLogger(__name__)


class Band:
    """
    A Band represents a group of peers communicating with a shared PSK

    This is the main entry point for the Telesthete API.
    """

    def __init__(
        self,
        psk: str,
        hostname: Optional[str] = None,
        bind_address: str = "0.0.0.0",
        bind_port: int = 9999
    ):
        """
        Initialize a Band

        Args:
            psk: Pre-shared key
            hostname: This machine's hostname (auto-detected if None)
            bind_address: Address to bind UDP socket
            bind_port: Port to bind UDP socket
        """
        self.psk = psk
        self.hostname = hostname or socket.gethostname()
        self.bind_address = bind_address
        self.bind_port = bind_port

        # Crypto
        self.crypto = BandCrypto(psk)
        self.band_id = self.crypto.band_id

        # Transport
        self.transport = UDPTransport(bind_address, bind_port)

        # Control channel
        self.control = ControlChannel(self.band_id, self.crypto, self.transport)

        # Peers
        self.peers: Dict[tuple, Peer] = {}

        # Streams
        self.streams: Dict[int, Stream] = {}
        self._next_stream_id = 1

        # Running state
        self._running = False
        self._tasks = []

        # Register transport handlers
        self._setup_handlers()

    def _setup_handlers(self):
        """Setup packet handlers for transport"""

        # Control packets
        def handle_control(peer_addr, packet_bytes):
            # Update peer last seen
            if peer_addr in self.peers:
                self.peers[peer_addr].update_last_seen()

            # Route to control channel
            self.control.handle_packet(peer_addr, packet_bytes)

        self.transport.register_handler(ChannelType.CONTROL, handle_control)

        # Stream packets
        def handle_stream(peer_addr, packet_bytes):
            # Update peer last seen
            if peer_addr in self.peers:
                self.peers[peer_addr].update_last_seen()

            # Extract stream ID to route
            from .protocol.framing import unpack_packet
            try:
                packet = unpack_packet(packet_bytes)
                stream_id = packet.channel_id

                # Route to stream
                if stream_id in self.streams:
                    self.streams[stream_id].handle_packet(peer_addr, packet_bytes)
                else:
                    logger.warning(f"No stream with ID {stream_id}")
            except Exception as e:
                logger.error(f"Error routing stream packet: {e}")

        self.transport.register_handler(ChannelType.STREAM, handle_stream)

        # Register control message handlers
        self.control.register_handler(ControlMessageType.HELLO, self._on_hello)
        self.control.register_handler(ControlMessageType.HELLO_ACK, self._on_hello_ack)
        self.control.register_handler(ControlMessageType.KEEPALIVE, self._on_keepalive)
        self.control.register_handler(ControlMessageType.GOODBYE, self._on_goodbye)

    def _on_hello(self, peer_addr: tuple, payload: dict):
        """Handle HELLO from peer"""
        hostname = payload.get("hostname", str(peer_addr))

        if peer_addr not in self.peers:
            logger.info(f"New peer joined: {hostname} at {peer_addr}")
            peer = Peer(peer_addr, hostname)
            self.peers[peer_addr] = peer

            # Add to control destinations
            self.control.add_destination(peer_addr)

            # Add to all stream destinations
            for stream in self.streams.values():
                stream.add_destination(peer_addr)

            # Send HELLO_ACK
            self.control.send_hello_ack(self.hostname, peer_addr)
        else:
            self.peers[peer_addr].update_last_seen()

    def _on_hello_ack(self, peer_addr: tuple, payload: dict):
        """Handle HELLO_ACK from peer"""
        hostname = payload.get("hostname", str(peer_addr))

        if peer_addr not in self.peers:
            logger.info(f"Peer ack: {hostname} at {peer_addr}")
            peer = Peer(peer_addr, hostname)
            self.peers[peer_addr] = peer

            # Add to control destinations
            self.control.add_destination(peer_addr)

            # Add to all stream destinations
            for stream in self.streams.values():
                stream.add_destination(peer_addr)

    def _on_keepalive(self, peer_addr: tuple, payload: dict):
        """Handle KEEPALIVE from peer"""
        if peer_addr in self.peers:
            self.peers[peer_addr].update_last_seen()

    def _on_goodbye(self, peer_addr: tuple, payload: dict):
        """Handle GOODBYE from peer"""
        if peer_addr in self.peers:
            logger.info(f"Peer left: {self.peers[peer_addr].hostname}")
            self._remove_peer(peer_addr)

    def _remove_peer(self, peer_addr: tuple):
        """Remove a peer"""
        if peer_addr in self.peers:
            peer = self.peers.pop(peer_addr)

            # Remove from control
            self.control.remove_destination(peer_addr)

            # Remove from all streams
            for stream in self.streams.values():
                stream.remove_destination(peer_addr)

            logger.debug(f"Removed peer: {peer.hostname}")

    def stream(self, stream_id: Optional[int] = None, priority: int = 128) -> Stream:
        """
        Open or get a Stream

        Args:
            stream_id: Stream ID (auto-assigned if None)
            priority: Priority (0=highest, 255=lowest)

        Returns:
            Stream instance
        """
        if stream_id is None:
            stream_id = self._next_stream_id
            self._next_stream_id += 1

        if stream_id in self.streams:
            return self.streams[stream_id]

        # Create new stream
        stream = Stream(self.band_id, stream_id, self.crypto, self.transport, priority)

        # Add all current peers as destinations
        for peer_addr in self.peers.keys():
            stream.add_destination(peer_addr)

        self.streams[stream_id] = stream
        logger.info(f"Opened stream {stream_id} with priority {priority}")

        return stream

    def connect_peer(self, host: str, port: int):
        """
        Connect to a specific peer

        Args:
            host: Peer hostname/IP
            port: Peer port
        """
        peer_addr = (host, port)

        # Send HELLO
        self.control.send_hello(self.hostname, peer_addr)
        logger.info(f"Connecting to peer at {peer_addr}")

    def get_peers(self) -> List[Peer]:
        """Get list of connected peers"""
        return list(self.peers.values())

    async def start(self):
        """
        Start the Band

        This starts the transport and control loops.
        """
        if self._running:
            return

        self._running = True

        # Start transport
        self.transport.start()
        logger.info(f"Band started: {self.band_id.hex()[:16]}... on {self.transport.local_address}")

        # Start transport task
        transport_task = asyncio.create_task(self.transport.run())

        # Start keepalive task
        keepalive_task = asyncio.create_task(self._keepalive_loop())

        self._tasks = [transport_task, keepalive_task]

        # Return immediately, tasks run in background
        logger.info(f"Band {self.hostname} running")

    async def stop(self):
        """Stop the Band"""
        if not self._running:
            return

        self._running = False

        # Send goodbye
        self.control.send_goodbye()
        await asyncio.sleep(0.1)  # Give time for goodbye to send

        # Stop tasks
        for task in self._tasks:
            task.cancel()

        await asyncio.gather(*self._tasks, return_exceptions=True)

        # Stop transport
        await self.transport.stop()

        logger.info("Band stopped")

    async def _keepalive_loop(self):
        """Send periodic keepalives"""
        while self._running:
            try:
                await asyncio.sleep(5.0)
                self.control.send_keepalive()

                # Check for dead peers
                dead_peers = [
                    addr for addr, peer in self.peers.items()
                    if not peer.is_alive()
                ]

                for addr in dead_peers:
                    logger.warning(f"Peer timeout: {self.peers[addr].hostname}")
                    self._remove_peer(addr)

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Keepalive loop error: {e}")

    async def run_forever(self):
        """
        Start the band and run forever

        This is a convenience method for simple applications.
        """
        await self.start()
        try:
            while self._running:
                await asyncio.sleep(1.0)
        except KeyboardInterrupt:
            logger.info("Shutting down...")
        finally:
            await self.stop()


async def test_band():
    """Test Band with two instances"""
    print("Testing Band")
    print("=" * 60)

    # Create two bands with same PSK
    band1 = Band(psk="test-band-psk", hostname="machine1", bind_port=10001)
    band2 = Band(psk="test-band-psk", hostname="machine2", bind_port=10002)

    # Start both
    await band1.start()
    await band2.start()

    print(f"Band 1 on {band1.transport.local_address}")
    print(f"Band 2 on {band2.transport.local_address}")

    # Connect band1 to band2
    band1.connect_peer("127.0.0.1", 10002)

    # Give time for connection
    await asyncio.sleep(0.5)

    # Check peers
    print(f"\nBand 1 peers: {band1.get_peers()}")
    print(f"Band 2 peers: {band2.get_peers()}")

    # Open streams
    stream1 = band1.stream(stream_id=1, priority=0)
    stream2 = band2.stream(stream_id=1, priority=0)

    # Track received data
    received1 = []
    received2 = []

    stream1.on_receive(lambda data, peer, ts: received1.append((data, peer)))
    stream2.on_receive(lambda data, peer, ts: received2.append((data, peer)))

    # Send data
    print("\nSending data...")
    stream1.send(b"Hello from machine1")
    stream2.send(b"Hello from machine2")

    # Wait for receive
    await asyncio.sleep(0.3)

    print(f"\nBand 1 received: {received1}")
    print(f"Band 2 received: {received2}")

    # Cleanup
    await band1.stop()
    await band2.stop()

    print("\nBand test complete")


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    asyncio.run(test_band())
