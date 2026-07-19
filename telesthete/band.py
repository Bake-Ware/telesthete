"""
Band - the main API for Telesthete

A Band is a PSK-scoped encrypted communication context between peers.
"""

import asyncio
import logging
import socket
import time
from typing import Optional, Dict, Callable, List

from .protocol.crypto import BandCrypto, select_cipher, BASELINE_CIPHER
from .protocol.sequence import SequenceSource
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
        bind_port: int = 9999,
        capabilities: Optional[List[str]] = None,
        ciphers: Optional[List[str]] = None,
        session_epoch: Optional[int] = None
    ):
        """
        Initialize a Band

        Args:
            psk: Pre-shared key
            hostname: This machine's hostname (auto-detected if None)
            bind_address: Address to bind UDP socket
            bind_port: Port to bind UDP socket
            capabilities: capability strings to advertise (SPEC §12.5)
            ciphers: ordered AEAD preference list (SPEC §3.5); baseline is
                always included
        """
        self.psk = psk
        self.hostname = hostname or socket.gethostname()
        self.bind_address = bind_address
        self.bind_port = bind_port

        # Capabilities + ordered cipher preferences (SPEC §3.5, §12.5).
        self.capabilities = list(capabilities) if capabilities else []
        self.ciphers = list(ciphers) if ciphers else [BASELINE_CIPHER]
        if BASELINE_CIPHER not in self.ciphers:
            self.ciphers.append(BASELINE_CIPHER)  # baseline is mandatory

        # BandCrypto cache keyed by (cipher_id, session_epoch). session=None is
        # the base key used for HELLO/HELLO_ACK; a session epoch gives the
        # per-session data key (SPEC §3.1/§3.3).
        self._cryptos: Dict[tuple, BandCrypto] = {}
        self.crypto = self._crypto(BASELINE_CIPHER, None)  # base
        self.band_id = self.crypto.band_id

        # One sequence source for this sender, shared across the live channels
        # (Control + Stream) so no two packets we emit reuse an AEAD (key, nonce)
        # — the nonce is the sequence (SPEC §3.3). The reliable Channel is
        # deferred to a later phase and is intentionally NOT wired into the Band.
        self.seq_source = SequenceSource()

        # This band instance's session epoch (SPEC §4.3). Advertised in HELLO so
        # a peer that has seen an earlier session rebases its replay watermark
        # after we restart, instead of dropping our fresh (lower-sequence) HELLO.
        #
        # It MUST increase on every restart (§4.3). The default — milliseconds
        # since the Unix epoch — is monotonic on a roughly-synced clock. A
        # consumer whose host clock can step backward or start unset (embedded /
        # no-RTC devices) MUST pass a persisted monotonic value instead, e.g.
        # ``max(last_saved + 1, now_ms)``, or a peer that saw a higher epoch will
        # refuse to rebase and lock this instance out until it ages out.
        self.session_epoch = (
            int(session_epoch) if session_epoch is not None
            else int(time.time() * 1000))

        # Transport
        self.transport = UDPTransport(bind_address, bind_port)

        # Control channel (per-peer negotiated cipher resolver). Restart of a
        # peer (new session epoch) rebases that peer's Stream watermarks too.
        self.control = ControlChannel(self.band_id, transport=self.transport,
                                      base_crypto=self.base_crypto,
                                      send_crypto=self.send_crypto,
                                      recv_crypto=self.recv_crypto,
                                      seq_source=self.seq_source,
                                      on_new_session=self._on_new_session)

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

    def _crypto(self, cipher_id: str, session: Optional[int]) -> BandCrypto:
        key = (cipher_id, session)
        c = self._cryptos.get(key)
        if c is None:
            c = BandCrypto(self.psk, cipher_id, session=session)
            self._cryptos[key] = c
        return c

    def _peer_cipher(self, peer_addr: tuple) -> str:
        peer = self.peers.get(peer_addr)
        return peer.cipher if (peer and peer.cipher) else BASELINE_CIPHER

    def base_crypto(self) -> BandCrypto:
        """Base key (baseline suite, no session) for HELLO/HELLO_ACK (SPEC §3.5)."""
        return self._crypto(BASELINE_CIPHER, None)

    def send_crypto(self, peer_addr: tuple) -> BandCrypto:
        """Data key for sending to a peer: our OWN session epoch + the peer's
        negotiated suite (SPEC §3.1/§3.3)."""
        return self._crypto(self._peer_cipher(peer_addr), self.session_epoch)

    def recv_crypto(self, peer_addr: tuple) -> Optional[BandCrypto]:
        """Data key for decrypting from a peer: the PEER's session epoch (learned
        from its HELLO) + its negotiated suite. `None` until we've seen the
        peer's HELLO — a data packet that arrives first is simply dropped."""
        peer = self.peers.get(peer_addr)
        if peer is None or peer.session_epoch < 0:
            return None
        return self._crypto(self._peer_cipher(peer_addr), peer.session_epoch)

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

    def _ensure_peer(self, peer_addr: tuple, hostname: str) -> Peer:
        peer = self.peers.get(peer_addr)
        if peer is None:
            logger.info(f"Peer joined: {hostname} at {peer_addr}")
            peer = Peer(peer_addr, hostname)
            self.peers[peer_addr] = peer
            self.control.add_destination(peer_addr)
            for stream in self.streams.values():
                stream.add_destination(peer_addr)
        return peer

    def _on_hello(self, peer_addr: tuple, payload: dict):
        """Handle HELLO: as responder, select the cipher and commit it (§3.5)."""
        hostname = payload.get("hostname", str(peer_addr))
        init_ciphers = payload.get("ciphers", [BASELINE_CIPHER])
        selected = select_cipher(init_ciphers, self.ciphers)

        peer = self._ensure_peer(peer_addr, hostname)
        epoch = int(payload.get("session", peer.session_epoch))
        if epoch < peer.session_epoch:
            # §4.3 monotonicity: an older-epoch HELLO is a replay from before a
            # restart. Adopting it would downgrade the peer's session key and
            # wedge its live session, so ignore it entirely (no ACK either).
            logger.debug(f"Ignoring stale-epoch HELLO from {peer_addr} (epoch {epoch})")
            return
        peer.capabilities = payload.get("capabilities", [])
        peer.cipher = selected
        peer.session_epoch = epoch
        peer.update_last_seen()

        # Commit the negotiated suite back to the initiator, with our epoch.
        self.control.send_hello_ack(self.hostname, peer_addr,
                                    capabilities=self.capabilities,
                                    ciphers=self.ciphers, cipher=selected,
                                    session=self.session_epoch)

    def _on_hello_ack(self, peer_addr: tuple, payload: dict):
        """Handle HELLO_ACK: adopt the cipher the responder committed (§3.5)."""
        hostname = payload.get("hostname", str(peer_addr))
        peer = self._ensure_peer(peer_addr, hostname)
        epoch = int(payload.get("session", peer.session_epoch))
        if epoch < peer.session_epoch:
            # §4.3 monotonicity — see _on_hello.
            logger.debug(f"Ignoring stale-epoch HELLO_ACK from {peer_addr} (epoch {epoch})")
            return
        peer.capabilities = payload.get("capabilities", [])
        peer.cipher = payload.get("cipher", BASELINE_CIPHER)
        peer.session_epoch = epoch
        peer.update_last_seen()

    def _on_new_session(self, peer_addr: tuple):
        """A peer (re)started its session: rebase its Stream watermarks so its
        fresh (possibly lower) sequences are accepted (SPEC §3.3/§4.3). The
        Control watermark is rebased by the ControlChannel itself."""
        for stream in self.streams.values():
            stream.reset_peer(peer_addr)

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

        # Create new stream: per-session data keys (send=own epoch, recv=peer
        # epoch) + the band's shared sequence source so nonces never collide
        # with Control/other streams (SPEC §3.1/§3.3).
        stream = Stream(self.band_id, stream_id, transport=self.transport,
                        priority=priority,
                        send_crypto=self.send_crypto, recv_crypto=self.recv_crypto,
                        seq_source=self.seq_source)

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

        # Send HELLO advertising our capabilities + ordered ciphers (§3.5) and
        # our session epoch (§4.3).
        self.control.send_hello(self.hostname, peer_addr,
                                capabilities=self.capabilities, ciphers=self.ciphers,
                                session=self.session_epoch)
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
