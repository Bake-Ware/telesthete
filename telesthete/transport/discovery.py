"""
LAN peer discovery via UDP broadcast

Discovers peers on the local network without needing to know their addresses.
"""

import asyncio
import socket
import logging
from typing import Callable, Optional, Set, Tuple

logger = logging.getLogger(__name__)


class Discovery:
    """
    UDP broadcast-based peer discovery for LAN

    Broadcasts "I exist" messages on the local network and listens for
    responses from other peers.
    """

    DISCOVERY_PORT = 9998  # Port for discovery broadcasts
    MAGIC = b"TELESTHETE"  # Magic bytes to identify our broadcasts
    VERSION = 1

    def __init__(self, hostname: str, listen_port: int, on_peer_found: Callable[[str, str, int], None]):
        """
        Initialize discovery

        Args:
            hostname: This machine's hostname
            listen_port: Port this machine is listening on for Band connections
            on_peer_found: Callback(hostname, ip, port) when peer discovered
        """
        self.hostname = hostname
        self.listen_port = listen_port
        self.on_peer_found = on_peer_found

        # Socket for sending/receiving broadcasts
        self.socket: Optional[socket.socket] = None

        # Discovered peers (to avoid duplicate callbacks)
        # Set of (hostname, ip, port)
        self._discovered: Set[Tuple[str, str, int]] = set()

        # Running state
        self._running = False
        self._tasks = []

    def start(self):
        """
        Create and configure UDP socket for broadcast
        """
        if self.socket:
            raise RuntimeError("Discovery already started")

        # Create UDP socket
        self.socket = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)

        # Enable broadcast
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)

        # Bind to discovery port
        try:
            self.socket.bind(("0.0.0.0", self.DISCOVERY_PORT))
            logger.info(f"Discovery listening on port {self.DISCOVERY_PORT}")
        except OSError as e:
            logger.warning(f"Could not bind to discovery port {self.DISCOVERY_PORT}: {e}")
            # Continue anyway - we can still send broadcasts even if we can't receive

        # Set non-blocking
        self.socket.setblocking(False)

    async def run(self):
        """
        Start discovery loops (broadcast and listen)
        """
        if not self.socket:
            raise RuntimeError("Discovery not started. Call start() first.")

        self._running = True

        # Start broadcast and listen loops
        self._tasks = [
            asyncio.create_task(self._broadcast_loop()),
            asyncio.create_task(self._listen_loop())
        ]

        logger.info("Discovery running")

        try:
            await asyncio.gather(*self._tasks)
        except asyncio.CancelledError:
            logger.info("Discovery stopped")

    async def stop(self):
        """Stop discovery"""
        self._running = False

        for task in self._tasks:
            task.cancel()

        await asyncio.gather(*self._tasks, return_exceptions=True)

        if self.socket:
            self.socket.close()
            self.socket = None

        logger.info("Discovery stopped")

    async def _broadcast_loop(self):
        """
        Periodically broadcast discovery packets
        """
        while self._running:
            try:
                self._send_broadcast()
                await asyncio.sleep(5.0)  # Broadcast every 5 seconds
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Broadcast error: {e}")
                await asyncio.sleep(1.0)

    async def _listen_loop(self):
        """
        Listen for discovery packets from peers
        """
        loop = asyncio.get_event_loop()

        while self._running:
            try:
                # Receive packet
                data, addr = await loop.sock_recvfrom(self.socket, 1024)

                # Parse packet
                self._handle_packet(data, addr)

            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.debug(f"Listen error: {e}")
                await asyncio.sleep(0.1)

    def _send_broadcast(self):
        """
        Send a discovery broadcast packet
        """
        # Packet format:
        # MAGIC (10 bytes) + VERSION (1 byte) + HOSTNAME_LEN (1 byte) + HOSTNAME + PORT (2 bytes)

        hostname_bytes = self.hostname.encode('utf-8')[:255]
        hostname_len = len(hostname_bytes)

        packet = (
            self.MAGIC +
            bytes([self.VERSION]) +
            bytes([hostname_len]) +
            hostname_bytes +
            self.listen_port.to_bytes(2, 'big')
        )

        # Broadcast to local network
        # Using 255.255.255.255 for simplicity
        # Could also enumerate network interfaces and broadcast on each
        try:
            self.socket.sendto(packet, ('<broadcast>', self.DISCOVERY_PORT))
            logger.debug(f"Sent discovery broadcast: {self.hostname}:{self.listen_port}")
        except Exception as e:
            logger.debug(f"Broadcast send error: {e}")

    def _handle_packet(self, data: bytes, addr: Tuple[str, int]):
        """
        Handle received discovery packet

        Args:
            data: Packet bytes
            addr: Sender (ip, port) - note this is the discovery port, not listen port
        """
        try:
            # Check magic
            if not data.startswith(self.MAGIC):
                return

            offset = len(self.MAGIC)

            # Check version
            version = data[offset]
            if version != self.VERSION:
                logger.debug(f"Discovery packet with wrong version: {version}")
                return

            offset += 1

            # Parse hostname
            hostname_len = data[offset]
            offset += 1

            hostname = data[offset:offset + hostname_len].decode('utf-8')
            offset += hostname_len

            # Parse port
            port = int.from_bytes(data[offset:offset + 2], 'big')

            peer_ip = addr[0]

            # Ignore self
            if hostname == self.hostname:
                return

            # Check if already discovered
            peer_tuple = (hostname, peer_ip, port)
            if peer_tuple in self._discovered:
                return

            # New peer discovered
            logger.info(f"Discovered peer: {hostname} at {peer_ip}:{port}")
            self._discovered.add(peer_tuple)

            # Call callback
            self.on_peer_found(hostname, peer_ip, port)

        except Exception as e:
            logger.debug(f"Error parsing discovery packet: {e}")

    def clear_discovered(self):
        """Clear the discovered peers set (useful for testing)"""
        self._discovered.clear()


async def test_discovery():
    """
    Test discovery with two instances
    """
    print("Testing Discovery")
    print("=" * 60)

    discovered_by_1 = []
    discovered_by_2 = []

    def peer_found_1(hostname, ip, port):
        print(f"Discovery 1 found: {hostname} at {ip}:{port}")
        discovered_by_1.append((hostname, ip, port))

    def peer_found_2(hostname, ip, port):
        print(f"Discovery 2 found: {hostname} at {ip}:{port}")
        discovered_by_2.append((hostname, ip, port))

    # Create two discovery instances
    disc1 = Discovery("machine1", 10001, peer_found_1)
    disc2 = Discovery("machine2", 10002, peer_found_2)

    disc1.start()
    disc2.start()

    # Run discovery in background
    task1 = asyncio.create_task(disc1.run())
    task2 = asyncio.create_task(disc2.run())

    # Wait for discoveries
    await asyncio.sleep(1.0)

    print(f"\nDiscovery 1 found {len(discovered_by_1)} peers")
    print(f"Discovery 2 found {len(discovered_by_2)} peers")

    # Cleanup
    await disc1.stop()
    await disc2.stop()

    # Verify
    assert len(discovered_by_1) == 1, f"Expected 1 peer, found {len(discovered_by_1)}"
    assert discovered_by_1[0][0] == "machine2"

    assert len(discovered_by_2) == 1, f"Expected 1 peer, found {len(discovered_by_2)}"
    assert discovered_by_2[0][0] == "machine1"

    print("\nDiscovery test passed")


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO)
    asyncio.run(test_discovery())
