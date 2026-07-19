"""
Drop channel implementation (SPEC §8)

Chunked, resumable file distribution. Receiver-driven: the receiver
requests exactly the chunk ranges it lacks, so resume-after-restart is
free and the sender stays stateless beyond the file bytes.
"""

import hashlib
import json
import logging
import time
from typing import Callable, Dict, List, Optional, Tuple

from .framing import pack_packet, unpack_packet, ChannelType
from .sequence import SequenceSource

logger = logging.getLogger(__name__)

CHUNK_SIZE = 1024        # §8.2: the §6.4 frame-data budget
MAX_RANGE_CHUNKS = 64    # §8.1: a range spans at most 64 chunks
REREQUEST_TIMEOUT = 1.0  # §8.2 reference: re-request an idle range after 1 s


class DropFrameType:
    OFFER = 0x01
    REQUEST = 0x02
    CHUNK = 0x03
    DONE = 0x04


class _DropBase:
    """Shared wire plumbing for both roles (same resolver pattern as Stream)."""

    def __init__(self, band_id, drop_id, crypto=None, transport=None,
                 send_crypto=None, recv_crypto=None, seq_source=None):
        self.band_id = band_id
        self.drop_id = drop_id
        if send_crypto is not None and recv_crypto is not None:
            self._send_crypto = send_crypto
            self._recv_crypto = recv_crypto
        else:
            self._send_crypto = lambda addr: crypto
            self._recv_crypto = lambda addr: crypto
        self.transport = transport
        self._seq_source = seq_source if seq_source is not None else SequenceSource()
        self._recv_watermark: Dict[tuple, int] = {}

    def reset_peer(self, peer_addr: tuple):
        self._recv_watermark.pop(peer_addr, None)

    def _aad(self) -> bytes:
        return bytes([ChannelType.DROP, self.drop_id >> 8, self.drop_id & 0xFF])

    def _send_frame(self, dest: tuple, frame: bytes):
        if self.transport is None:
            return
        crypto = self._send_crypto(dest)
        if crypto is None:
            return
        sequence = self._seq_source.next()
        ciphertext = crypto.encrypt(sequence, frame, self._aad())
        self.transport.send(dest, pack_packet(
            self.band_id, ChannelType.DROP, self.drop_id, sequence, ciphertext))

    def _send_json(self, dest: tuple, type_: int, payload: dict):
        self._send_frame(dest, bytes([type_]) + json.dumps(payload).encode("utf-8"))

    def _decrypt(self, peer_addr: tuple, packet_bytes: bytes) -> Optional[bytes]:
        packet = unpack_packet(packet_bytes)
        if packet.channel_type != ChannelType.DROP or packet.channel_id != self.drop_id:
            return None
        watermark = self._recv_watermark.get(peer_addr)
        if watermark is not None and packet.sequence <= watermark:
            return None  # replayed/stale (SPEC §3.3)
        rc = self._recv_crypto(peer_addr)
        if rc is None:
            return None
        plaintext = rc.decrypt(packet.sequence, packet.ciphertext, self._aad())
        self._recv_watermark[peer_addr] = packet.sequence
        return plaintext


class DropSender(_DropBase):
    """Offers one file and serves whatever ranges receivers request (§8.2)."""

    def __init__(self, band_id, drop_id, name: str, data: bytes, **kw):
        super().__init__(band_id, drop_id, **kw)
        self.name = name
        self.data = data
        self.total_chunks = max(1, -(-len(data) // CHUNK_SIZE))
        self.sha256 = hashlib.sha256(data).hexdigest()
        # dest -> receiver's DONE verdict (True = digest matched).
        self.completed: Dict[tuple, bool] = {}
        self._on_complete: Optional[Callable[[tuple, bool], None]] = None

    def on_complete(self, callback: Callable[[tuple, bool], None]):
        self._on_complete = callback

    def offer(self, dest: tuple):
        self._send_json(dest, DropFrameType.OFFER, {
            "name": self.name, "size": len(self.data), "chunk_size": CHUNK_SIZE,
            "total_chunks": self.total_chunks, "sha256": self.sha256,
        })

    def handle_packet(self, peer_addr: tuple, packet_bytes: bytes):
        try:
            frame = self._decrypt(peer_addr, packet_bytes)
            if not frame:
                return
            type_ = frame[0]
            if type_ == DropFrameType.REQUEST:
                payload = json.loads(frame[1:].decode("utf-8"))
                for start, end in payload.get("ranges", []):
                    self._serve_range(peer_addr, int(start), int(end))
            elif type_ == DropFrameType.DONE:
                payload = json.loads(frame[1:].decode("utf-8"))
                ok = payload.get("sha256") == self.sha256
                self.completed[peer_addr] = ok
                if not ok:
                    logger.warning(f"Drop {self.drop_id}: digest mismatch from {peer_addr}")
                if self._on_complete:
                    self._on_complete(peer_addr, ok)
        except Exception as e:
            logger.error(f"Drop {self.drop_id} sender: error handling packet: {e}")

    def _serve_range(self, dest: tuple, start: int, end: int):
        # Bound hostile/buggy requests: clamp to the file and the §8.1 window.
        start = max(0, start)
        end = min(end, self.total_chunks, start + MAX_RANGE_CHUNKS)
        for index in range(start, end):
            chunk = self.data[index * CHUNK_SIZE:(index + 1) * CHUNK_SIZE]
            self._send_frame(dest, bytes([DropFrameType.CHUNK])
                             + index.to_bytes(4, "big") + chunk)


class DropReceiver(_DropBase):
    """Pulls a offered file chunk-window by chunk-window, verifying at the end
    (§8.2). `have` may be pre-seeded with persisted chunks to resume."""

    def __init__(self, band_id, drop_id, have: Optional[Dict[int, bytes]] = None, **kw):
        super().__init__(band_id, drop_id, **kw)
        self.offer: Optional[dict] = None
        self.have: Dict[int, bytes] = dict(have) if have else {}
        self.sender_addr: Optional[tuple] = None
        self.verified: Optional[bool] = None  # None until complete
        self._outstanding: Optional[Tuple[int, int]] = None  # one window (§8.2)
        self._last_progress = 0.0
        self._on_complete: Optional[Callable[[bytes, bool], None]] = None

    def on_complete(self, callback: Callable[[bytes, bool], None]):
        """callback(data, sha_ok) once every chunk arrived and was verified."""
        self._on_complete = callback

    def _valid_offer(self, offer: dict) -> bool:
        """§8.1: total_chunks must be the exact ceil(size/chunk_size), chunk_size
        the fixed 1024, size non-negative. A malformed OFFER (e.g. total_chunks
        wildly larger than size implies) would drive a request/allocation storm."""
        try:
            size = int(offer["size"])
            chunk_size = int(offer["chunk_size"])
            total = int(offer["total_chunks"])
        except (KeyError, TypeError, ValueError):
            return False
        if size < 0 or chunk_size != CHUNK_SIZE:
            return False
        # A zero-byte file is one empty chunk (matches DropSender), so the
        # count is never zero.
        expected = max(1, -(-size // chunk_size))
        return total == expected

    def _chunk_len(self, index: int) -> int:
        """Expected byte length of chunk `index` for the current offer."""
        size = self.offer["size"]
        total = self.offer["total_chunks"]
        if index < total - 1:
            return CHUNK_SIZE
        return size - (total - 1) * CHUNK_SIZE  # final (possibly short) chunk

    def missing_ranges(self) -> List[Tuple[int, int]]:
        """Contiguous [start, end) runs of chunks we lack, split to §8.1 size."""
        total = self.offer["total_chunks"] if self.offer else 0
        out: List[Tuple[int, int]] = []
        run_start = None
        for i in range(total + 1):
            lacking = i < total and i not in self.have
            if lacking and run_start is None:
                run_start = i
            elif not lacking and run_start is not None:
                for s in range(run_start, i, MAX_RANGE_CHUNKS):
                    out.append((s, min(s + MAX_RANGE_CHUNKS, i)))
                run_start = None
        return out

    def request_missing(self):
        """Ask for the next missing window (one outstanding window, §8.2).
        Call again after REREQUEST_TIMEOUT of no progress to re-pull losses."""
        if self.offer is None or self.sender_addr is None or self.verified is not None:
            return
        ranges = self.missing_ranges()[:1]
        if ranges:
            self._outstanding = ranges[0]
            self._send_json(self.sender_addr, DropFrameType.REQUEST,
                            {"ranges": [list(r) for r in ranges]})

    def tick(self, now: Optional[float] = None):
        """Drive re-requests: if nothing arrived for REREQUEST_TIMEOUT, ask again."""
        now = time.monotonic() if now is None else now
        if (self.offer is not None and self.verified is None
                and now - self._last_progress > REREQUEST_TIMEOUT):
            self._last_progress = now
            self.request_missing()

    def handle_packet(self, peer_addr: tuple, packet_bytes: bytes):
        try:
            frame = self._decrypt(peer_addr, packet_bytes)
            if not frame:
                return
            type_ = frame[0]
            if type_ == DropFrameType.OFFER:
                offer = json.loads(frame[1:].decode("utf-8"))
                if not self._valid_offer(offer):
                    logger.warning(f"Drop {self.drop_id}: rejecting malformed OFFER")
                    return
                if self.offer is not None and (
                        offer.get("sha256") != self.offer.get("sha256")
                        or offer.get("size") != self.offer.get("size")):
                    # A different file on the same drop_id: start over (§8.2
                    # resume only applies to a recognized (name, size, sha256)).
                    self.have.clear()
                    self.verified = None
                self.offer = offer
                # Drop any pre-seeded (resumed) chunk that this OFFER doesn't
                # cover: an out-of-range index would KeyError in _finish, and a
                # wrong-length chunk would corrupt the assembled file (§8.2
                # resume applies only to chunks this offer actually defines).
                total = offer["total_chunks"]
                self.have = {i: c for i, c in self.have.items()
                             if 0 <= i < total and len(c) == self._chunk_len(i)}
                self.sender_addr = peer_addr
                self._last_progress = time.monotonic()
                if all(i in self.have for i in range(total)):
                    self._finish()  # resume already had everything
                else:
                    self.request_missing()
            elif type_ == DropFrameType.CHUNK and self.offer is not None:
                index = int.from_bytes(frame[1:5], "big")
                if index >= self.offer["total_chunks"]:
                    return
                if index not in self.have and len(frame[5:]) == self._chunk_len(index):
                    self.have[index] = frame[5:]
                    self._last_progress = time.monotonic()
                if all(i in self.have for i in range(self.offer["total_chunks"])):
                    self._finish()
                elif self._outstanding is not None and all(
                        i in self.have for i in range(*self._outstanding)):
                    self.request_missing()  # window drained -> pull the next
        except Exception as e:
            logger.error(f"Drop {self.drop_id} receiver: error handling packet: {e}")

    def _finish(self):
        data = b"".join(self.have[i] for i in range(self.offer["total_chunks"]))
        data = data[:self.offer["size"]]
        ok = hashlib.sha256(data).hexdigest() == self.offer["sha256"]
        self.verified = ok
        self._send_json(self.sender_addr, DropFrameType.DONE,
                        {"sha256": hashlib.sha256(data).hexdigest()})
        if self._on_complete:
            try:
                self._on_complete(data, ok)
            except Exception as e:
                logger.error(f"Drop {self.drop_id}: on_complete error: {e}")
