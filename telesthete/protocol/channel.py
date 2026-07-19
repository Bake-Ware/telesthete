"""Reliable Channel (SPEC §6): ordered byte streams with flow control.

TCP semantics in userspace over UDP. Reliability (acks, ordering, dedup,
retransmission) tracks the INNER per-channel `seq` (§6.1); the OUTER packet
sequence is the AEAD nonce and MUST come from the sender's one shared
per-session SequenceSource (§3.3) — a per-channel outer counter starting at 0
is exactly the nonce-reuse bug the v1.2 redesign removed.
"""

import asyncio
import logging
import struct
import time
from collections import deque
from dataclasses import dataclass, field
from typing import Callable, Dict, Optional

from .framing import pack_packet, unpack_packet, ChannelType
from .fragment import Fragmenter, Reassembler, MAX_CHANNEL_DATA
from .sequence import SequenceSource

logger = logging.getLogger(__name__)

# §6.1 plaintext frame header: flags(1) || ack_num(8 BE) || window(2 BE) ||
# seq(8 BE) || data. `seq` is the inner per-(channel, direction) sequence; the
# outer packet sequence lives in the §1 framing header and is NOT contiguous
# per channel.
_FRAME = struct.Struct(">BQHQ")
FRAME_HEADER_LEN = _FRAME.size  # 19

# §6.2 flags. Bits 4-7 reserved, must be 0.
FLAG_SYN = 0x01
FLAG_FIN = 0x02
FLAG_ACK = 0x04
FLAG_RST = 0x08
_RESERVED_FLAGS = 0xF0

DEFAULT_WINDOW = 32      # §6.4 sliding window (frames)
DEFAULT_RTO = 0.5        # §6.4 retransmission timeout (seconds)
MAX_FRAME_DATA = MAX_CHANNEL_DATA  # §6.4: 1024 bytes per frame
MAX_RETRIES = 10         # give up on a frame after this many RTOs (dead peer)

# Queue sentinel: FIN ordered behind already-queued data so close() cannot
# reorder ahead of a window-blocked send() (§6.3 FIN consumes the next seq).
_FIN_SENTINEL = object()


def pack_frame(flags: int, ack_num: int, window: int, seq: int,
               data: bytes = b"") -> bytes:
    """Pack a §6.1 Channel plaintext frame (the AEAD plaintext)."""
    return _FRAME.pack(flags, ack_num, window, seq) + data


def unpack_frame(payload: bytes):
    """Parse a §6.1 frame -> (flags, ack_num, window, seq, data), or None.

    None for short frames or reserved flag bits set (§6.2: receiver drops)."""
    if len(payload) < FRAME_HEADER_LEN:
        return None
    flags, ack_num, window, seq = _FRAME.unpack_from(payload)
    if flags & _RESERVED_FLAGS:
        return None
    return flags, ack_num, window, seq, payload[FRAME_HEADER_LEN:]


@dataclass
class _Unacked:
    """A sent, unacknowledged frame. Caches (flags, seq, data) so an RTO
    retransmission re-encrypts the SAME frame under a FRESH outer sequence
    (§6.1) — resending the original ciphertext (or re-using its outer sequence
    with an updated ack_num) would be nonce reuse."""
    flags: int
    seq: int
    data: bytes
    sent_at: float = field(default_factory=time.monotonic)
    retries: int = 0


class Channel:
    """Reliable, ordered byte stream with flow control (SPEC §6).

    Use for clipboard, file transfer, any operation requiring reliability.
    """

    # Kept as class attributes for API compatibility with prior consumers.
    FLAG_SYN = FLAG_SYN
    FLAG_FIN = FLAG_FIN
    FLAG_ACK = FLAG_ACK
    FLAG_RST = FLAG_RST

    def __init__(
        self,
        band_id: bytes,
        channel_id: int,
        peer_addr: tuple,
        transport=None,
        crypto=None,
        send_crypto=None,
        recv_crypto=None,
        seq_source: Optional[SequenceSource] = None,
    ):
        """
        Args:
            band_id: Band identifier (16 bytes)
            channel_id: Channel identifier (0-65535)
            peer_addr: the single peer this channel talks to
            transport: UDPTransport instance
            crypto: a single BandCrypto (single-cipher / tests)
            send_crypto: resolver(peer_addr) -> BandCrypto for our OWN session
                epoch (SPEC §3.1/§3.3); falls back to `crypto`
            recv_crypto: resolver(peer_addr) -> BandCrypto for the PEER's
                session epoch, or None before its HELLO (packet is dropped)
            seq_source: the sender's shared SequenceSource (SPEC §3.3). One per
                band, shared with Control/Streams so nonces never collide. A
                private one is created if omitted (standalone use).
        """
        self.band_id = band_id
        self.channel_id = channel_id
        self.peer_addr = peer_addr
        self.transport = transport
        # Per-session data keys (SPEC §3.1/§3.3): send under our epoch, recv
        # under the peer's; None recv resolver result -> drop (no session yet).
        if send_crypto is not None:
            self._send_crypto = send_crypto
        else:
            self._send_crypto = lambda addr: crypto
        if recv_crypto is not None:
            self._recv_crypto = recv_crypto
        else:
            self._recv_crypto = lambda addr: crypto
        self.crypto = crypto

        # Shared per-sender sequence source (SPEC §3.3): the outer sequence is
        # the AEAD nonce, so it MUST be drawn from the band-wide source — never
        # a per-channel counter.
        self._seq_source = seq_source if seq_source is not None else SequenceSource()

        self._aad = bytes([
            ChannelType.CHANNEL,
            self.channel_id >> 8,
            self.channel_id & 0xFF,
        ])

        # §6.5 states: CLOSED -> SYN_SENT -> ESTABLISHED -> FIN_SENT -> CLOSED,
        # with CLOSED -> ESTABLISHED on an incoming SYN.
        self._state = "CLOSED"
        self._closed = asyncio.Event()
        self._closing = False

        # Inner sequence spaces (§6.1): 0-based, contiguous per direction.
        self._snd_next = 0   # next inner seq we will consume
        self._rcv_next = 0   # next inner seq expected = ack_num we advertise

        # §6.4 reliability state.
        self.window_size = DEFAULT_WINDOW
        self.rto = DEFAULT_RTO
        self._remote_window = DEFAULT_WINDOW
        self._send_buffer: Dict[int, _Unacked] = {}   # inner seq -> frame
        self._send_queue: deque = deque()             # window-blocked frames
        self._recv_buffer: Dict[int, tuple] = {}      # out-of-order (flags, data)
        self._retransmit_task: Optional[asyncio.Task] = None

        # §3.3 replay protection on the OUTER sequence: strictly increasing per
        # peer. Legitimate inner reordering still arrives with increasing outer
        # sequences (a retransmission always carries a fresh, higher one).
        self._recv_watermark: Dict[tuple, int] = {}

        # Raw byte-level receive path.
        self._on_receive: Optional[Callable[[bytes], None]] = None
        self._recv_queue: deque = deque()
        self._recv_ready = asyncio.Event()

        # §6.6 message path: every logical message uses the fragmentation
        # envelope (even <= 1 chunk) so the parser stays stateless.
        self._fragmenter = Fragmenter()
        self._reassembler = Reassembler()
        self._on_message: Optional[Callable[[bytes], None]] = None
        self._msg_queue: deque = deque()
        self._msg_ready = asyncio.Event()

    # ------------------------------------------------------------------ API

    async def open(self):
        """Open the channel: send SYN (§6.3) and start the retransmit task."""
        if self._state != "CLOSED":
            raise RuntimeError(f"Channel already open (state={self._state})")
        self._state = "SYN_SENT"
        # Initial SYN has flags=SYN only (§6.3); it consumes inner seq 0.
        self._transmit_consuming(FLAG_SYN, b"")
        self._ensure_retransmit_task()

    async def close(self):
        """Close the channel: FIN handshake (§6.3), then stop retransmitting."""
        if self._state == "CLOSED":
            return
        if not self._closing:
            self._closing = True
            # FIN queues behind pending data so it consumes the last inner seq.
            self._send_queue.append(_FIN_SENTINEL)
            self._pump()
        try:
            await asyncio.wait_for(self._closed.wait(), timeout=5.0)
        finally:
            if self._retransmit_task:
                self._retransmit_task.cancel()
                await asyncio.gather(self._retransmit_task, return_exceptions=True)
                self._retransmit_task = None
            self._state = "CLOSED"

    def send(self, data: bytes):
        """Send one logical message reliably. Alias for `send_message`.

        Channel carries §6.6-enveloped messages ONLY — there is no raw
        non-enveloped frame path. A bare byte payload on channel_type 0x02
        would be misparsed as a §6.1 control frame by a conformant receiver
        (forging ACK/window) and dropped by the Rust peer, which delivers
        only reassembled messages; enveloping every send keeps the two
        references interoperable. Fire-and-forget bytes belong on a Stream
        (§5), not a Channel."""
        self.send_message(data)

    def send_message(self, message: bytes):
        """Send one logical message via the §6.6 fragmentation envelope.

        Every message — even one that fits a single chunk — is enveloped, so
        the receiving parser stays stateless. Each chunk rides one frame."""
        if self._closing or self._state not in ("SYN_SENT", "ESTABLISHED"):
            raise RuntimeError(f"Channel not open (state={self._state})")
        for chunk in self._fragmenter.split(message):
            self._send_queue.append(chunk)
        self._pump()

    async def recv(self, timeout: Optional[float] = None) -> bytes:
        """Receive the next in-order raw frame payload."""
        return await self._pop(self._recv_queue, self._recv_ready, timeout)

    async def recv_message(self, timeout: Optional[float] = None) -> bytes:
        """Receive the next fully reassembled §6.6 message."""
        return await self._pop(self._msg_queue, self._msg_ready, timeout)

    async def _pop(self, queue: deque, ready: asyncio.Event,
                   timeout: Optional[float]) -> bytes:
        if timeout is not None:
            await asyncio.wait_for(ready.wait(), timeout=timeout)
        else:
            await ready.wait()
        data = queue.popleft()
        if not queue:
            ready.clear()
        return data

    def on_receive(self, callback: Callable[[bytes], None]):
        """Register callback for each in-order raw frame payload."""
        self._on_receive = callback

    def on_message(self, callback: Callable[[bytes], None]):
        """Register callback for each reassembled §6.6 message."""
        self._on_message = callback

    def reset_peer(self, peer_addr: tuple):
        """Forget the peer's outer-sequence watermark so its next session's
        (fresh, possibly lower) sequences are accepted (SPEC §3.3/§4.3)."""
        self._recv_watermark.pop(peer_addr, None)

    # ------------------------------------------------------------- send path

    def _effective_window(self) -> int:
        # §6.4: our sliding window, bounded by the peer's advertised window.
        return min(self.window_size, self._remote_window)

    def _recv_window(self) -> int:
        # Window we advertise: buffer slots we can still absorb out of order.
        return max(0, self.window_size - len(self._recv_buffer))

    def _pump(self):
        """Transmit queued frames while the send window has room (§6.4)."""
        if self._state not in ("ESTABLISHED", "FIN_SENT"):
            return  # SYN_SENT: data waits for the handshake to finish
        while self._send_queue and len(self._send_buffer) < self._effective_window():
            item = self._send_queue.popleft()
            if item is _FIN_SENTINEL:
                # §6.3: FIN sets ACK after handshake (0x06); consumes one seq.
                self._transmit_consuming(FLAG_FIN | FLAG_ACK, b"")
                self._state = "FIN_SENT"
            else:
                self._transmit_consuming(FLAG_ACK, item)

    def _transmit_consuming(self, flags: int, data: bytes):
        """Consume the next inner seq, cache for retransmission, transmit."""
        seq = self._snd_next
        self._snd_next += 1
        self._send_buffer[seq] = _Unacked(flags, seq, data)
        self._transmit(flags, seq, data)
        self._ensure_retransmit_task()

    def _transmit(self, flags: int, seq: int, data: bytes):
        """Encrypt and send one frame under a FRESH outer sequence (§6.1).

        Called for first transmission AND every retransmission: the outer
        sequence is the AEAD nonce, so it is drawn anew from the shared source
        each time; ack_num/window are the current values."""
        outer = self._seq_source.next()
        payload = pack_frame(flags, self._rcv_next, self._recv_window(), seq, data)
        crypto = self._send_crypto(self.peer_addr)
        ciphertext = crypto.encrypt(outer, payload, self._aad)
        packet = pack_packet(self.band_id, ChannelType.CHANNEL,
                             self.channel_id, outer, ciphertext)
        self.transport.send(self.peer_addr, packet)

    def _send_ack(self):
        # Pure ACK (§6.1): carries our next unconsumed seq WITHOUT consuming
        # it; never buffered or retransmitted (a lost ACK is recovered by the
        # peer's RTO and our dup-ACK on the duplicate).
        self._transmit(FLAG_ACK, self._snd_next, b"")

    # ------------------------------------------------------------- recv path

    def handle_packet(self, peer_addr: tuple, packet_bytes: bytes):
        """Handle one received CHANNEL packet."""
        try:
            packet = unpack_packet(packet_bytes)
        except ValueError:
            return
        if packet.channel_type != ChannelType.CHANNEL:
            return
        if packet.channel_id != self.channel_id:
            return

        # §3.3 outer replay protection: first packet accepted at any (random
        # start) sequence, thereafter strictly increasing. Checked before any
        # state change; the watermark only advances after authentication.
        watermark = self._recv_watermark.get(peer_addr)
        if watermark is not None and packet.sequence <= watermark:
            logger.debug(f"Channel {self.channel_id}: dropping stale outer "
                         f"seq={packet.sequence} (watermark={watermark})")
            return

        # Decrypt under the peer's session data key; None -> no session key
        # yet (its HELLO not seen), so the packet cannot be authenticated: drop.
        rc = self._recv_crypto(peer_addr)
        if rc is None:
            logger.debug(f"Channel {self.channel_id}: no session key for "
                         f"{peer_addr}; dropping")
            return
        try:
            payload = rc.decrypt(packet.sequence, packet.ciphertext, self._aad)
        except Exception:
            logger.debug(f"Channel {self.channel_id}: dropping unauthenticated packet")
            return
        self._recv_watermark[peer_addr] = packet.sequence

        frame = unpack_frame(payload)
        if frame is None:
            return  # short frame or reserved flag bits (§6.2): drop
        self._handle_frame(*frame)

    def _handle_frame(self, flags: int, ack_num: int, window: int, seq: int,
                      data: bytes):
        self._remote_window = window

        if flags & FLAG_RST:
            logger.warning(f"Channel {self.channel_id}: connection reset by peer")
            self._teardown()
            return

        if flags & FLAG_ACK:
            self._handle_ack(ack_num)

        # Pure ACKs carry a seq without consuming it (§6.1); only SYN, FIN and
        # data frames enter the inner-seq reorder/dedup pipeline.
        if not (data or flags & (FLAG_SYN | FLAG_FIN)):
            return

        if seq < self._rcv_next:
            # §6.4 dedup: already-delivered inner seq (a spurious retransmit —
            # our ACK was lost or late). Re-ACK so the sender stops resending.
            self._send_ack()
            return
        if seq > self._rcv_next:
            # §6.4: out-of-order — buffer by inner seq; duplicate buffered
            # frames are dropped. Dup-ACK restates what we still expect. The
            # buffer is bounded by the window we advertise (§6.4): a seq beyond
            # rcv_next + window_size is outside what we promised to hold, so we
            # drop it (and dup-ACK) rather than let a hostile peer grow memory
            # by sending sparse far-future sequences.
            if seq < self._rcv_next + self.window_size and seq not in self._recv_buffer:
                self._recv_buffer[seq] = (flags, data)
            self._send_ack()
            return

        # In order: process, then drain any now-contiguous buffered frames.
        self._rcv_next += 1
        acked = self._process_frame(flags, data)
        while self._rcv_next in self._recv_buffer:
            f, d = self._recv_buffer.pop(self._rcv_next)
            self._rcv_next += 1
            acked = self._process_frame(f, d) or acked
        # Cumulative ACK (§6.1) unless a response frame already carried it
        # (SYN+ACK / FIN+ACK consume a seq and are retransmittable).
        if not acked:
            self._send_ack()

    def _handle_ack(self, ack_num: int):
        # §6.1 cumulative: acknowledges every frame with seq < ack_num. Bound
        # ack_num by what we have actually sent (_snd_next): a frame whose
        # payload happened to decode to a huge ack_num must not clear frames we
        # never sent, which would suppress their retransmission (silent loss).
        ack_num = min(ack_num, self._snd_next)
        acked = [s for s in self._send_buffer if s < ack_num]
        for s in acked:
            del self._send_buffer[s]
        if acked:
            self._pump()  # window space freed: drain queued frames

    def _process_frame(self, flags: int, data: bytes) -> bool:
        """Apply one in-order consumed frame. Returns True if a seq-consuming
        response frame (which itself carries ACK) was transmitted."""
        if flags & FLAG_SYN:
            if self._state == "CLOSED":
                # §6.5 responder: CLOSED -> ESTABLISHED on SYN; reply SYN+ACK.
                self._state = "ESTABLISHED"
                self._transmit_consuming(FLAG_SYN | FLAG_ACK, b"")
                self._pump()
                return True
            if self._state == "SYN_SENT":
                # §6.3 initiator: SYN+ACK completes our side; pure ACK follows
                # (returned False), then any queued data flows.
                self._state = "ESTABLISHED"
                self._pump()
                return False
            return False  # duplicate SYN in other states: just re-ACK

        if flags & FLAG_FIN:
            if self._state == "FIN_SENT":
                # Active close: peer's FIN+ACK arrived; pure ACK ends it (§6.3).
                self._teardown()
                return False
            # Passive close: reply FIN+ACK (one frame = ACK + our FIN, §6.3).
            self._transmit_consuming(FLAG_FIN | FLAG_ACK, b"")
            self._teardown()
            return True

        if data:
            self._deliver(data)
        return False

    def _deliver(self, data: bytes):
        # Every Channel data frame is a §6.6 envelope chunk; a completed
        # reassembly emits one logical message on BOTH the recv() and
        # recv_message() paths (they are the same message stream, kept as two
        # names for API compatibility). on_receive and on_message both fire.
        message = self._reassembler.feed(data)
        if message is None:
            return
        for queue, ready in ((self._recv_queue, self._recv_ready),
                             (self._msg_queue, self._msg_ready)):
            queue.append(message)
            ready.set()
        for cb in (self._on_receive, self._on_message):
            if cb:
                try:
                    cb(message)
                except Exception as e:
                    logger.error(f"Channel {self.channel_id}: receive callback error: {e}")

    def _teardown(self):
        # Stop guaranteeing delivery: a CLOSED/reset channel must not keep
        # retransmitting its unacked frames forever (that leaks the task and
        # floods the peer). Clearing both buffers lets _retransmit_loop exit.
        self._state = "CLOSED"
        self._send_buffer.clear()
        self._send_queue.clear()
        self._closed.set()

    # --------------------------------------------------------- retransmission

    def _ensure_retransmit_task(self):
        if self._retransmit_task is not None and not self._retransmit_task.done():
            return
        try:
            loop = asyncio.get_running_loop()
        except RuntimeError:
            return  # no loop (sync/standalone use): retransmission inert
        self._retransmit_task = loop.create_task(self._retransmit_loop())

    async def _retransmit_loop(self):
        """§6.4: resend unacked frames after RTO, each re-encrypted under a
        fresh outer sequence via _transmit (never the original ciphertext).

        Bounded: a frame retransmitted MAX_RETRIES times with no ACK means the
        peer is gone (it never sent RST). We give up and tear the channel down
        rather than resend forever — otherwise a vanished peer leaves this task
        flooding the network and consuming outer sequences indefinitely."""
        while True:
            try:
                await asyncio.sleep(self.rto / 4)
                if self._state == "CLOSED" and not self._send_buffer:
                    return  # fully closed and acked: nothing left to guarantee
                now = time.monotonic()
                for seq in sorted(self._send_buffer):
                    frame = self._send_buffer.get(seq)
                    if frame is not None and (now - frame.sent_at) >= self.rto:
                        if frame.retries >= MAX_RETRIES:
                            logger.warning(f"Channel {self.channel_id}: peer "
                                           f"unresponsive after {MAX_RETRIES} "
                                           f"retransmits; tearing down")
                            self._teardown()
                            return
                        frame.retries += 1
                        frame.sent_at = now
                        self._transmit(frame.flags, frame.seq, frame.data)
            except asyncio.CancelledError:
                return
            except Exception as e:
                logger.error(f"Channel {self.channel_id}: retransmit error: {e}")
