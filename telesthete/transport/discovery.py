"""
LAN peer discovery via UDP broadcast

Discovers peers on the local network without needing to know their addresses.
"""

import asyncio
import socket
import logging
from collections import OrderedDict
from typing import Callable, Optional, Tuple

from ..protocol.framing import PROTOCOL_VERSION

logger = logging.getLogger(__name__)

MAGIC = b"TELE"  # 0x54454C45 (SPEC §9.2)


def pack_announce(hostname: str, port: int) -> bytes:
    """SPEC §9.2: magic(4) || version(1) || hostname_len(1) || hostname || port(2 BE)."""
    hostname_bytes = hostname.encode("utf-8")
    if len(hostname_bytes) > 255:
        hostname_bytes = hostname_bytes[:255]
    return (MAGIC + bytes([PROTOCOL_VERSION, len(hostname_bytes)])
            + hostname_bytes + port.to_bytes(2, "big"))


def parse_announce(data: bytes) -> Optional[Tuple[str, int]]:
    """Parse a §9.2 announce -> (hostname, port), or None if not ours / not
    speakable. Trailing bytes after the port are ignored (forward-extensible)."""
    if len(data) < 8 or not data.startswith(MAGIC):
        return None
    if data[4] != PROTOCOL_VERSION:
        return None  # a version we cannot speak
    hostname_len = data[5]
    end = 6 + hostname_len
    if len(data) < end + 2:
        return None
    try:
        hostname = data[6:end].decode("utf-8")
    except UnicodeDecodeError:
        return None
    port = int.from_bytes(data[end:end + 2], "big")
    return hostname, port


class Discovery:
    """
    UDP broadcast-based peer discovery for LAN (SPEC §9.2)

    Broadcasts "I exist" messages on the local network and listens for
    responses from other peers.
    """

    DISCOVERY_PORT = 9998  # Port for discovery broadcasts

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

        # Discovered peers (to avoid duplicate callbacks), as an insertion-
        # ordered LRU keyed by (hostname, ip, port). Bounded so unauthenticated
        # LAN beacons — an attacker can spoof unlimited (hostname, port) pairs —
        # cannot grow it without limit; the oldest entry is evicted at the cap.
        self._discovered: "OrderedDict[Tuple[str, str, int], None]" = OrderedDict()
        self._max_discovered = 4096

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
        Send a discovery broadcast packet (SPEC §9.2)
        """
        packet = pack_announce(self.hostname, self.listen_port)

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
            parsed = parse_announce(data)
            if parsed is None:
                return
            hostname, port = parsed

            peer_ip = addr[0]

            # Ignore self
            if hostname == self.hostname:
                return

            # Check if already discovered
            peer_tuple = (hostname, peer_ip, port)
            if peer_tuple in self._discovered:
                return

            # New peer discovered — record, evicting the oldest past the cap.
            logger.info(f"Discovered peer: {hostname} at {peer_ip}:{port}")
            self._discovered[peer_tuple] = None
            while len(self._discovered) > self._max_discovered:
                self._discovered.popitem(last=False)

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
