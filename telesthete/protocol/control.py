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
from .sequence import SequenceSource

logger = logging.getLogger(__name__)


class ControlMessageType(IntEnum):
    """Control message types"""
    HELLO = 0x01           # Peer introduction
    HELLO_ACK = 0x02       # Hello acknowledgment
    KEEPALIVE = 0x03       # Heartbeat
    FOCUS_CHANGE = 0x04    # Focus state change
    METACONTROL = 0x05     # Settings/config sync (monitor layout, etc)
    GOODBYE = 0x06         # Peer disconnect
    KEYFRAME_REQ = 0x07    # Stream consumer -> producer: request a fresh keyframe (§4.9)
    RATE_HINT = 0x08       # Stream consumer -> producer: bitrate/loss feedback (§4.10)


class ControlChannel:
    """
    Control channel for Band management

    Always uses channel_type=0, channel_id=0
    Always reliable (we'll add retransmission later if needed)
    """

    def __init__(self, band_id: bytes, crypto=None, transport=None, crypto_for=None,
                 seq_source=None, on_new_session=None,
                 base_crypto=None, send_crypto=None, recv_crypto=None):
        """
        Initialize control channel

        Args:
            band_id: Band identifier
            crypto: a single BandCrypto (single-cipher / tests)
            transport: UDPTransport instance
            crypto_for: optional resolver(peer_addr, cipher_id=None) -> BandCrypto
                for per-peer negotiated ciphers (SPEC §3.5)
            seq_source: the sender's shared :class:`SequenceSource` (SPEC §3.3).
                One per band, shared with the Streams/Channels so nonces never
                collide. A private one is created if omitted (standalone use).
            on_new_session: optional callback(peer_addr) fired when a peer
                (re)starts a session, so the Band can rebase that peer's Stream
                watermarks too.
        """
        self.band_id = band_id
        self.transport = transport
        # Per-session data keys (SPEC §3.1/§3.3): HELLO/HELLO_ACK use the base
        # key so a receiver can bootstrap the peer's epoch; other messages use
        # the session data key (send=our epoch, recv=peer epoch). Legacy
        # fallback to a single `crypto` / `crypto_for` for standalone/test use.
        if base_crypto is not None:
            self._base_crypto = base_crypto
            self._send_crypto = send_crypto
            self._recv_crypto = recv_crypto
        else:
            resolve = crypto_for if crypto_for is not None else (
                lambda addr, cipher_id=None: crypto)
            self._base_crypto = lambda: resolve(None)
            self._send_crypto = lambda addr: resolve(addr)
            self._recv_crypto = lambda addr: resolve(addr)
        self.crypto = crypto

        # Shared per-sender sequence source (SPEC §3.3).
        self._seq_source = seq_source if seq_source is not None else SequenceSource()
        self._on_new_session = on_new_session

        # Peer destinations
        self._destinations = []

        # Callbacks for different message types
        self._handlers: Dict[int, list] = {}

        # Keepalive tracking
        self._last_keepalive_sent = 0
        self._keepalive_interval = 5.0  # seconds

        # Replay protection: highest accepted sequence per peer (SPEC §3.3).
        # A missing entry means "not yet seen" -> the first packet is accepted at
        # whatever (random) sequence the sender started from; thereafter strictly
        # increasing. `_peer_session` records the peer's session epoch so a
        # restarted peer (lower random start, newer epoch) rebases rather than
        # being locked out.
        self._recv_watermark: Dict[tuple, int] = {}
        self._peer_session: Dict[tuple, int] = {}

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

        # Draw one sequence from the shared per-sender source (SPEC §3.3),
        # reused across every destination of this message.
        sequence = self._seq_source.next()

        message = {
            "type": int(msg_type),
            "timestamp": int(time.time() * 1000),
            "payload": payload
        }
        message_bytes = json.dumps(message).encode('utf-8')
        aad = bytes([ChannelType.CONTROL, 0, 0])

        # HELLO/HELLO_ACK bootstrap on the base key (§3.5); every other message
        # uses the per-session data key for that peer's negotiated suite (§3.1).
        # Encrypt per destination because peers may differ in suite/epoch.
        destinations = [dest] if dest else list(self._destinations)
        for d in destinations:
            crypto = self._base_crypto() if baseline else self._send_crypto(d)
            ciphertext = crypto.encrypt(sequence, message_bytes, aad)
            packet = pack_control_message(self.band_id, sequence, ciphertext)
            self.transport.send(d, packet)

    def send_hello(self, hostname: str, dest: tuple,
                   capabilities=None, ciphers=None, session: int = 0):
        """Send HELLO with mandatory capabilities + ordered ciphers (SPEC §4.3).
        Always on the baseline suite. `session` is this sender's session epoch
        (SPEC §4.3), which lets a receiver rebase its replay watermark on a
        restart instead of locking the peer out."""
        self.send_message(ControlMessageType.HELLO, {
            "hostname": hostname,
            "capabilities": capabilities or [],
            "ciphers": ciphers or [BASELINE_CIPHER],
            "session": int(session),
        }, dest, baseline=True)

    def send_hello_ack(self, hostname: str, dest: tuple,
                       capabilities=None, ciphers=None, cipher=None, session: int = 0):
        """Send HELLO_ACK committing the negotiated `cipher` (SPEC §4.4)."""
        self.send_message(ControlMessageType.HELLO_ACK, {
            "hostname": hostname,
            "capabilities": capabilities or [],
            "ciphers": ciphers or [BASELINE_CIPHER],
            "cipher": cipher or BASELINE_CIPHER,
            "session": int(session),
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

    def send_keyframe_req(self, stream_id: int, dest: Optional[tuple] = None):
        """Stream consumer -> producer: request a fresh keyframe (SPEC §4.9).
        Carried on Control (reliable) so it is not subject to Stream drop."""
        self.send_message(ControlMessageType.KEYFRAME_REQ,
                          {"stream_id": int(stream_id)}, dest)

    def send_rate_hint(self, stream_id: int, target_bps: int, loss: float,
                       dest: Optional[tuple] = None):
        """Stream consumer -> producer: application-level congestion feedback for
        a lossy Stream (SPEC §4.10). Advisory."""
        self.send_message(ControlMessageType.RATE_HINT, {
            "stream_id": int(stream_id),
            "target_bps": int(target_bps),
            "loss": float(loss),
        }, dest)

    def _decrypt_control(self, peer_addr: tuple, sequence: int, ciphertext: bytes, aad: bytes) -> bytes:
        """Authenticate a control packet: try the peer's per-session data key
        (keepalives and other data), then the base key (HELLO/HELLO_ACK
        bootstrap). An old-session packet fails the current data key AND the
        base key, so it is rejected. Raises if neither authenticates."""
        rc = self._recv_crypto(peer_addr)
        if rc is not None:
            try:
                return rc.decrypt(sequence, ciphertext, aad)
            except Exception:
                pass
        return self._base_crypto().decrypt(sequence, ciphertext, aad)

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

            aad = bytes([ChannelType.CONTROL, 0, 0])

            # Authenticate FIRST (SPEC §3.5 negotiated suite, baseline fallback).
            # We decrypt before applying the replay watermark so a HELLO/HELLO_ACK
            # from a restarted peer — whose fresh random sequence may fall below
            # our stale watermark — can still be recognized and rebase the mark.
            # A forged packet fails AEAD here and never touches any state.
            try:
                message_bytes = self._decrypt_control(
                    peer_addr, packet.sequence, packet.ciphertext, aad)
            except Exception:
                logger.debug("Dropping unauthenticated control packet")
                return

            message = json.loads(message_bytes.decode('utf-8'))
            try:
                msg_type = ControlMessageType(message["type"])
            except ValueError:
                # SPEC §4.2: unknown control types MUST be ignored (handled fully
                # in the Control-completeness phase; ignore quietly for now).
                logger.debug(f"Ignoring unknown control type {message.get('type')}")
                return
            payload = message.get("payload", {})
            seq = packet.sequence
            wm = self._recv_watermark.get(peer_addr)

            # Session (re)start handling (SPEC §3.3/§4.3): a HELLO/HELLO_ACK with
            # a newer session epoch rebases this peer's watermark, so a restart
            # is not a permanent lockout. An older/equal epoch falls through to
            # the normal replay check (a replayed HELLO is still rejected).
            if msg_type in (ControlMessageType.HELLO, ControlMessageType.HELLO_ACK):
                epoch = int(payload.get("session", 0))
                prev_epoch = self._peer_session.get(peer_addr, -1)
                if epoch < prev_epoch:
                    # A HELLO from an OLDER epoch is a replay captured before the
                    # peer restarted (HELLO is base-key encrypted, so it stays
                    # decryptable forever). It must NOT reach the generic replay
                    # check below: because HELLO sequences are independent random
                    # starts, the stale seq is ~50% likely to exceed the live
                    # watermark, which would ratchet the watermark far past the
                    # live session and silently drop every real control packet
                    # (permanent control-plane DoS). Drop it outright. (Matches
                    # rust/telesthete/src/control.rs.)
                    logger.debug(f"Dropping stale-epoch control from {peer_addr} "
                                 f"(epoch {epoch} < {prev_epoch})")
                    return
                if epoch > prev_epoch:
                    self._peer_session[peer_addr] = epoch
                    self._recv_watermark[peer_addr] = seq
                    if self._on_new_session:
                        try:
                            self._on_new_session(peer_addr)
                        except Exception as e:
                            logger.error(f"on_new_session error: {e}")
                    self._dispatch(msg_type, peer_addr, payload)
                    return

            # Replay protection (SPEC §3.3): accept the first packet seen from a
            # peer at any sequence, then require strictly increasing sequences.
            if wm is not None and seq <= wm:
                logger.debug(
                    f"Dropping replayed/stale control packet: seq={seq}, watermark={wm}")
                return
            self._recv_watermark[peer_addr] = seq
            self._dispatch(msg_type, peer_addr, payload)

        except Exception as e:
            logger.error(f"Error handling control packet: {e}")

    def _dispatch(self, msg_type, peer_addr, payload):
        """Invoke registered handlers for an authenticated control message."""
        logger.debug(f"Control message from {peer_addr}: type={msg_type.name}")
        for handler in self._handlers.get(msg_type, []):
            try:
                handler(peer_addr, payload)
            except Exception as e:
                logger.error(f"Control handler error: {e}")

    def reset_peer(self, peer_addr: tuple):
        """Forget a peer's replay/session state (e.g. on disconnect)."""
        self._recv_watermark.pop(peer_addr, None)
        self._peer_session.pop(peer_addr, None)


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
