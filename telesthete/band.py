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
from .protocol.channel import (
    Channel, ResetReason, rst_packet, unpack_frame,
    FLAG_SYN, FLAG_ACK, FLAG_FIN, FLAG_RST,
)
from .protocol.control import ControlChannel, ControlMessageType
from .protocol.framing import ChannelType
from .transport.udp import UDPTransport
from .peer import Peer

logger = logging.getLogger(__name__)

# SPEC §4.11 reconnection: HELLO resent to an unanswered target after these
# delays, then every _KEEPALIVE_INTERVAL (§4.5); a target from which nothing
# authenticated has arrived for _PEER_TIMEOUT (§4.5, Peer.keepalive_timeout)
# is lost.
_HELLO_BACKOFF = (0.5, 1.0, 2.0, 4.0)
_KEEPALIVE_INTERVAL = 5.0
_PEER_TIMEOUT = 15.0


class _Target:
    """An address connect_peer was asked to reach (SPEC §4.11 step 1)."""

    def __init__(self):
        self.answered = False   # a HELLO or HELLO_ACK has arrived from it
        self.attempt = 0        # resends since the last (re)start of step 2
        self.next_at = 0.0      # time.time() of the next resend
        self.heard_at = 0.0     # time.time() of its last authenticated Control message

    def restart(self, now: float):
        self.answered = False
        self.attempt = 0
        self.next_at = now + _HELLO_BACKOFF[0]


class Band:
    """
    A Band represents a group of peers communicating with a shared PSK

    This is the main entry point for the Telesthete API.
    """

    # Hard cap on the peer registry (defense against spoofed-HELLO-replay
    # growth; keepalive eviction handles the steady state). Displaces the
    # least-recently-seen peer when full.
    MAX_PEERS = 4096

    def __init__(
        self,
        psk: str,
        hostname: Optional[str] = None,
        bind_address: str = "0.0.0.0",
        bind_port: int = 9999,
        capabilities: Optional[List[str]] = None,
        ciphers: Optional[List[str]] = None,
        session_epoch: Optional[int] = None,
        reconnect: bool = False,
        transport=None
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
            reconnect: SPEC §4.11 reconnection and the §6.7 Channel
                lifecycle: HELLO is resent until answered and after silence,
                and Channels are reset when a peer's session changes, on
                GOODBYE, on RST and on give-up, and can then be reopened.
            transport: an object to use instead of building a UDPTransport
                (bind_address and bind_port are then ignored). It provides
                start(), async run(), async stop(), register_handler(),
                send() and local_address, as UDPTransport does; for example
                telesthete.transport.websocket.WebSocketTransport (§9.3)
        """
        self.psk = psk
        self.reconnect = reconnect
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

        # One sequence source for this sender, shared across Control, every
        # Stream AND every reliable Channel, so no two packets we emit reuse an
        # AEAD (key, nonce) — the nonce is the sequence (SPEC §3.3).
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
        if transport is None:
            self.transport = UDPTransport(bind_address, bind_port)
        else:
            self.transport = transport
            # A connection-oriented transport (WebSocket, §9.3) calls this
            # after each (re)connect so HELLO is the connection's first frame.
            if hasattr(transport, "on_connected"):
                transport.on_connected = self._on_transport_connected

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

        # Reliable Channels (SPEC §6), keyed by channel_id. The CHANNEL
        # transport handler is registered lazily on first use.
        self.channels: Dict[int, Channel] = {}
        self._channel_handler_registered = False
        self._on_channel: Optional[Callable[[Channel], None]] = None

        # SPEC §4.11: addresses connect_peer was asked to reach, and peers
        # whose session we have seen (so a later newer epoch is a restart).
        self._targets: Dict[tuple, _Target] = {}
        self._sessions_seen: set = set()

        # Boards (§7) and Drops (§8), keyed by board_id / drop_id. Handlers
        # registered lazily on first use, like Channel.
        self.boards: Dict[int, "Board"] = {}
        self._board_handler_registered = False
        self.drops: Dict[int, object] = {}
        self._drop_handler_registered = False

        # Running state
        self._running = False
        self._tasks = []

        # Register transport handlers
        self._setup_handlers()
        if self.reconnect:
            # §6.7 (f): data for a channel we hold no Channel for draws an RST.
            self._register_channel_handler()
            # §4.11 step 3: a target is alive while authenticated Control
            # messages arrive from it (every peer keeps alive every 5 s, §4.5).
            for msg_type in ControlMessageType:
                self.control.register_handler(msg_type, self._target_heard)

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
            # Bound the registry: a replayed HELLO spoofed from many source
            # addresses could otherwise grow it without limit before keepalive
            # eviction catches up. Past the cap, evict the least-recently-seen
            # peer so a genuine newcomer can still displace a stale phantom.
            if len(self.peers) >= self.MAX_PEERS:
                stale = min(self.peers.values(), key=lambda p: p.last_seen)
                logger.warning(f"Peer registry full ({self.MAX_PEERS}); "
                               f"evicting least-recently-seen {stale.address}")
                self._remove_peer(stale.address)
            logger.info(f"Peer joined: {hostname} at {peer_addr}")
            peer = Peer(peer_addr, hostname)
            self.peers[peer_addr] = peer
            self.control.add_destination(peer_addr)
            for stream in self.streams.values():
                stream.add_destination(peer_addr)
            for board in self.boards.values():
                board.add_destination(peer_addr)
        return peer

    def _on_hello(self, peer_addr: tuple, payload: dict):
        """Handle HELLO: as responder, select the cipher and commit it (§3.5)."""
        hostname = payload.get("hostname", str(peer_addr))
        init_ciphers = payload.get("ciphers", [BASELINE_CIPHER])
        selected = select_cipher(init_ciphers, self.ciphers)

        peer = self._ensure_peer(peer_addr, hostname)
        # Absent session defaults to 0 (matching the Rust serde default), NOT the
        # peer's -1 sentinel: a peer that omits the field must still get a usable
        # epoch, else recv_crypto stays None and every data packet is dropped
        # forever. A conformant peer always sends session (§4.3).
        epoch = int(payload.get("session", 0))
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
        self._target_answered(peer_addr)

        # Commit the negotiated suite back to the initiator, with our epoch.
        self.control.send_hello_ack(self.hostname, peer_addr,
                                    capabilities=self.capabilities,
                                    ciphers=self.ciphers, cipher=selected,
                                    session=self.session_epoch)

    def _on_hello_ack(self, peer_addr: tuple, payload: dict):
        """Handle HELLO_ACK: adopt the cipher the responder committed (§3.5)."""
        hostname = payload.get("hostname", str(peer_addr))
        peer = self._ensure_peer(peer_addr, hostname)
        epoch = int(payload.get("session", 0))  # see _on_hello: 0 default, not -1
        if epoch < peer.session_epoch:
            # §4.3 monotonicity — see _on_hello.
            logger.debug(f"Ignoring stale-epoch HELLO_ACK from {peer_addr} (epoch {epoch})")
            return
        peer.capabilities = payload.get("capabilities", [])
        committed = payload.get("cipher", BASELINE_CIPHER)
        if committed not in self.ciphers:
            # §3.5: the responder must commit a suite we offered. An unknown or
            # unsupported string would derive a key we can't reproduce and wedge
            # the data path — fall back to the mandatory baseline instead.
            logger.warning(f"Peer {peer_addr} committed unsupported cipher "
                           f"{committed!r}; using baseline")
            committed = BASELINE_CIPHER
        peer.cipher = committed
        peer.session_epoch = epoch
        peer.update_last_seen()
        self._target_answered(peer_addr)

    def _target_heard(self, peer_addr: tuple, payload: dict):
        target = self._targets.get(peer_addr)
        if target is not None:
            target.heard_at = time.time()

    def _target_answered(self, peer_addr: tuple):
        """§4.11 step 2: a HELLO or HELLO_ACK from a target stops the resends."""
        target = self._targets.get(peer_addr)
        if target is not None:
            target.answered = True
            target.heard_at = time.time()

    def _on_new_session(self, peer_addr: tuple):
        """A peer (re)started its session: rebase its Stream and Channel
        watermarks so its fresh (possibly lower) sequences are accepted
        (SPEC §3.3/§4.3). The Control watermark is rebased by the
        ControlChannel itself."""
        for stream in self.streams.values():
            stream.reset_peer(peer_addr)
        for channel in self.channels.values():
            channel.reset_peer(peer_addr)
        for board in self.boards.values():
            board.reset_peer(peer_addr)
        for drop_ep in self.drops.values():
            drop_ep.reset_peer(peer_addr)
        # §4.11 step 4: a newer epoch from a peer whose session we already
        # knew means it restarted; its Channels start over (§6.7 (c)).
        if peer_addr in self._sessions_seen:
            if self.reconnect:
                self._reset_channels(peer_addr, ResetReason.PEER_RESTARTED)
        else:
            self._sessions_seen.add(peer_addr)

    def _reset_channels(self, peer_addr: tuple, reason: ResetReason):
        """Reset every Channel with `peer_addr` (SPEC §4.11 step 4, §6.7 (c))."""
        for channel in list(self.channels.values()):
            if channel.peer_addr == peer_addr:
                channel._reset(reason)

    def _on_keepalive(self, peer_addr: tuple, payload: dict):
        """Handle KEEPALIVE from peer"""
        if peer_addr in self.peers:
            self.peers[peer_addr].update_last_seen()

    def _on_goodbye(self, peer_addr: tuple, payload: dict):
        """Handle GOODBYE from peer"""
        if self.reconnect:
            self._reset_channels(peer_addr, ResetReason.PEER_LEFT)  # §4.11 step 4
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

    def channel(self, channel_id: int, peer_addr: tuple) -> Channel:
        """
        Open or get a reliable Channel to a peer (SPEC §6)

        Args:
            channel_id: Channel ID (0-65535)
            peer_addr: (host, port) of the single peer this channel talks to

        Returns:
            Channel instance (call `await channel.open()` to start the §6.3
            handshake on the initiating side)
        """
        if channel_id in self.channels:
            return self.channels[channel_id]

        # Per-session data keys (send=own epoch, recv=peer epoch) + the band's
        # shared sequence source, so Channel outer sequences (= AEAD nonces)
        # never collide with Control/Stream traffic (SPEC §3.1/§3.3).
        channel = Channel(self.band_id, channel_id, peer_addr,
                          transport=self.transport,
                          send_crypto=self.send_crypto,
                          recv_crypto=self.recv_crypto,
                          seq_source=self.seq_source)
        channel._lifecycle = self.reconnect
        self.channels[channel_id] = channel
        self._register_channel_handler()
        logger.info(f"Opened channel {channel_id} to {peer_addr}")
        return channel

    def on_channel(self, callback: Callable[[Channel], None]):
        """Accept Channels the peer opens (SPEC §6.7 (b)).

        When a SYN arrives for a channel_id this Band has no Channel for, from
        a peer whose session it knows, the Channel is created, passed to
        `callback(channel)`, and then the SYN is processed. Without a
        callback such a SYN is dropped, as before."""
        self._on_channel = callback
        self._register_channel_handler()

    def _unknown_channel(self, peer_addr: tuple, packet, packet_bytes: bytes) -> None:
        """A CHANNEL frame for an id we hold no Channel for: accept it
        (§6.7 (b)), answer it with RST (§6.7 (f)), or drop it."""
        if self._on_channel is None and not self.reconnect:
            logger.warning(f"No channel with ID {packet.channel_id}")
            return
        rc = self.recv_crypto(peer_addr)
        if rc is None:
            return  # session unknown: cannot authenticate
        aad = bytes([ChannelType.CHANNEL, packet.channel_id >> 8,
                     packet.channel_id & 0xFF])
        try:
            frame = unpack_frame(rc.decrypt(packet.sequence, packet.ciphertext, aad))
        except Exception:
            return
        if frame is None:
            return
        flags, _, _, _, data = frame
        if flags & FLAG_SYN and not flags & FLAG_ACK:
            # Never an RST for a SYN: without the hook it is dropped, so a SYN
            # that beats the application's channel() call wins on retransmit.
            if self._on_channel is not None:
                channel = self.channel(packet.channel_id, peer_addr)
                try:
                    self._on_channel(channel)
                except Exception as e:
                    logger.error(f"on_channel callback error: {e}")
                channel.handle_packet(peer_addr, packet_bytes)
            return
        if self.reconnect and not flags & FLAG_RST and (data or flags & FLAG_FIN):
            self.transport.send(peer_addr, rst_packet(
                self.band_id, packet.channel_id, self.seq_source.next(),
                self.send_crypto(peer_addr)))

    def _register_channel_handler(self):
        """Register CHANNEL routing on first use (idempotent)."""
        if self._channel_handler_registered:
            return
        self._channel_handler_registered = True

        def handle_channel(peer_addr, packet_bytes):
            if peer_addr in self.peers:
                self.peers[peer_addr].update_last_seen()
            from .protocol.framing import unpack_packet
            try:
                packet = unpack_packet(packet_bytes)
            except ValueError:
                return
            channel = self.channels.get(packet.channel_id)
            if channel is None:
                self._unknown_channel(peer_addr, packet, packet_bytes)
                return
            # A channel is bound to one peer; another sender's packet on this
            # channel_id must not reach its state machine.
            if peer_addr != channel.peer_addr:
                logger.debug(f"Dropping channel {packet.channel_id} packet "
                             f"from non-peer {peer_addr}")
                return
            channel.handle_packet(peer_addr, packet_bytes)

        self.transport.register_handler(ChannelType.CHANNEL, handle_channel)

    def board(self, board_id: int) -> "Board":
        """
        Open or get a replicated Board (SPEC §7)

        Every known peer is a replication destination; new peers are added as
        they HELLO. Advertises `board-v1` (§12.5).
        """
        from .protocol.board import Board
        if board_id in self.boards:
            return self.boards[board_id]
        board = Board(self.band_id, board_id, self.hostname,
                      transport=self.transport,
                      send_crypto=self.send_crypto,
                      recv_crypto=self.recv_crypto,
                      seq_source=self.seq_source)
        for peer_addr in self.peers:
            board.add_destination(peer_addr)
        self.boards[board_id] = board
        if "board-v1" not in self.capabilities:
            self.capabilities.append("board-v1")
        self._register_board_handler()
        return board

    def _register_board_handler(self):
        if self._board_handler_registered:
            return
        self._board_handler_registered = True

        def handle_board(peer_addr, packet_bytes):
            if peer_addr in self.peers:
                self.peers[peer_addr].update_last_seen()
            from .protocol.framing import unpack_packet
            try:
                packet = unpack_packet(packet_bytes)
            except ValueError:
                return
            board = self.boards.get(packet.channel_id)
            if board is not None:
                board.handle_packet(peer_addr, packet_bytes)

        self.transport.register_handler(ChannelType.BOARD, handle_board)

    def drop_sender(self, drop_id: int, name: str, data: bytes) -> "DropSender":
        """Offer a file on a Drop (SPEC §8). Advertises `drop-v1` (§12.5)."""
        from .protocol.drop import DropSender
        sender = DropSender(self.band_id, drop_id, name, data,
                            transport=self.transport,
                            send_crypto=self.send_crypto,
                            recv_crypto=self.recv_crypto,
                            seq_source=self.seq_source)
        self.drops[drop_id] = sender
        if "drop-v1" not in self.capabilities:
            self.capabilities.append("drop-v1")
        self._register_drop_handler()
        return sender

    def drop_receiver(self, drop_id: int, have=None) -> "DropReceiver":
        """Receive a file offered on a Drop (SPEC §8); `have` resumes from
        persisted chunks."""
        from .protocol.drop import DropReceiver
        receiver = DropReceiver(self.band_id, drop_id, have=have,
                                transport=self.transport,
                                send_crypto=self.send_crypto,
                                recv_crypto=self.recv_crypto,
                                seq_source=self.seq_source)
        self.drops[drop_id] = receiver
        if "drop-v1" not in self.capabilities:
            self.capabilities.append("drop-v1")
        self._register_drop_handler()
        return receiver

    def _register_drop_handler(self):
        if self._drop_handler_registered:
            return
        self._drop_handler_registered = True

        def handle_drop(peer_addr, packet_bytes):
            if peer_addr in self.peers:
                self.peers[peer_addr].update_last_seen()
            from .protocol.framing import unpack_packet
            try:
                packet = unpack_packet(packet_bytes)
            except ValueError:
                return
            endpoint = self.drops.get(packet.channel_id)
            if endpoint is not None:
                endpoint.handle_packet(peer_addr, packet_bytes)

        self.transport.register_handler(ChannelType.DROP, handle_drop)

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
        self._send_hello(peer_addr)
        logger.info(f"Connecting to peer at {peer_addr}")
        if self.reconnect:
            # §4.11 step 1: a target for as long as the band runs.
            self._targets.setdefault(peer_addr, _Target()).restart(time.time())

    def _send_hello(self, peer_addr: tuple):
        self.control.send_hello(self.hostname, peer_addr,
                                capabilities=self.capabilities, ciphers=self.ciphers,
                                session=self.session_epoch)

    def _on_transport_connected(self):
        """The transport (re)connected to its server: send HELLO to it at
        once, so the server learns the band from this frame (SPEC §9.3)."""
        self.connect_peer(*self.transport.peer_address)

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
        if self.reconnect:
            self._tasks.append(asyncio.create_task(self._reconnect_loop()))

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

        # Cancel per-Channel retransmit tasks: they hold the transport and would
        # keep resending (and stay alive) after the socket closes otherwise.
        channel_tasks = [c._retransmit_task for c in self.channels.values()
                         if c._retransmit_task is not None]
        for t in channel_tasks:
            t.cancel()

        # Stop tasks
        for task in self._tasks:
            task.cancel()

        await asyncio.gather(*self._tasks, *channel_tasks, return_exceptions=True)

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

    async def _reconnect_loop(self):
        """SPEC §4.11 steps 2 and 3, for every target."""
        while self._running:
            try:
                await asyncio.sleep(0.1)
                now = time.time()
                for addr, target in list(self._targets.items()):
                    if target.answered:
                        if now - target.heard_at >= _PEER_TIMEOUT:
                            # Step 3: lost. Evict (§4.5) and resume step 2.
                            logger.warning(f"Peer at {addr} lost; reconnecting")
                            self._remove_peer(addr)
                            target.restart(now)
                            self._send_hello(addr)
                    elif now >= target.next_at:
                        # Step 2: resend; fresh sequence, same epoch.
                        self._send_hello(addr)
                        target.attempt += 1
                        delay = (_HELLO_BACKOFF[target.attempt]
                                 if target.attempt < len(_HELLO_BACKOFF)
                                 else _KEEPALIVE_INTERVAL)
                        target.next_at = now + delay
            except asyncio.CancelledError:
                break
            except Exception as e:
                logger.error(f"Reconnect loop error: {e}")

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
