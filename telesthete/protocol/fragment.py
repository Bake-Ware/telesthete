"""Channel message fragmentation. See SPEC.md §6.6.

A logical message larger than one Channel payload is split into chunks, each
carried inside its own Telesthete CHANNEL frame and AEAD-encrypted
independently. The fragmentation envelope is the first bytes of the Channel
plaintext, ahead of the chunk data:

    Offset  Size  Field
    0       1     version       0x01
    1       16    fragment_id   random per logical message
    17      2     seq           uint16 BE, 0-based chunk index
    19      2     total         uint16 BE, total chunk count (>= 1)
    21      var   data          raw chunk bytes

Single-chunk (and empty) messages still use the envelope with seq=0 total=1.
Promoted to the reference from the Rook worker implementation in v1.2.
"""

from __future__ import annotations

import logging
import os
import struct
import time
from dataclasses import dataclass, field
from typing import Optional

log = logging.getLogger(__name__)

VERSION = 0x01
_HEADER = struct.Struct(">B16sHH")
HEADER_SIZE = _HEADER.size  # 21
MAX_CHANNEL_DATA = 1024      # SPEC §6.4 / §12.4
MAX_CHUNK_PAYLOAD = MAX_CHANNEL_DATA - HEADER_SIZE  # 1003

DEFAULT_REASSEMBLY_TIMEOUT = 30.0
DEFAULT_BUFFER_LIMIT = 256
# A declared `total` above this is refused outright (SPEC §6.6 hardening). At
# MAX_CHUNK_PAYLOAD per chunk, 16384 chunks is ~16 MB — well past any message
# the reference stack sends — while the wire allows 65535 (~64 MB).
DEFAULT_MAX_FRAGMENTS = 16384
# Bytes buffered across all partial messages. None = unbounded (the historical
# behaviour); the per-message cap above still applies.
DEFAULT_MAX_BUFFERED_BYTES: Optional[int] = None


def pack_chunk(fragment_id: bytes, seq: int, total: int, data: bytes) -> bytes:
    """Pack one §6.6 chunk: 21-byte header + data."""
    if len(fragment_id) != 16:
        raise ValueError("fragment_id must be 16 bytes")
    return _HEADER.pack(VERSION, fragment_id, seq, total) + data


def parse_chunk(chunk: bytes):
    """Parse one chunk -> (fragment_id, seq, total, data), or None if invalid."""
    if len(chunk) < HEADER_SIZE:
        return None
    version, fid, seq, total = _HEADER.unpack_from(chunk, 0)
    if version != VERSION or total == 0 or seq >= total:
        return None
    return fid, seq, total, chunk[HEADER_SIZE:]


def fragment(payload: bytes,
             chunk_size: int = MAX_CHUNK_PAYLOAD,
             fragment_id: Optional[bytes] = None) -> list[bytes]:
    """Split `payload` into wire chunks (HEADER + data) ready to be encrypted
    and framed by the Channel layer. `fragment_id` may be supplied for
    deterministic output (tests / vectors); otherwise it is random."""
    if chunk_size <= 0:
        raise ValueError("chunk_size must be positive")
    fid = fragment_id if fragment_id is not None else os.urandom(16)
    if not payload:
        return [pack_chunk(fid, 0, 1, b"")]
    pieces = [payload[i:i + chunk_size] for i in range(0, len(payload), chunk_size)]
    if len(pieces) > 0xFFFF:
        raise ValueError("payload too large; max 65535 fragments")
    total = len(pieces)
    return [pack_chunk(fid, seq, total, piece) for seq, piece in enumerate(pieces)]


class Fragmenter:
    def __init__(self, chunk_size: int = MAX_CHUNK_PAYLOAD) -> None:
        self.chunk_size = chunk_size

    def split(self, payload: bytes) -> list[bytes]:
        return fragment(payload, self.chunk_size)


@dataclass
class _Partial:
    total: int
    parts: dict = field(default_factory=dict)
    first_seen: float = 0.0
    size: int = 0  # bytes held in `parts`

    def complete(self) -> bool:
        return len(self.parts) == self.total

    def assemble(self) -> bytes:
        return b"".join(self.parts[i] for i in range(self.total))


class Reassembler:
    """Collect fragments and emit full payloads. Single-task; not thread-safe.

    Memory is bounded three ways (SPEC §6.6), all applied before a chunk is
    buffered so a hostile or corrupt sender can't make us hold more:

    * ``buffer_limit`` — concurrent partial messages; the oldest is evicted.
    * ``max_fragments`` — a chunk declaring ``total`` above this is dropped,
      as is a chunk of a multi-chunk message whose data exceeds
      ``max_chunk_payload`` (default ``MAX_CHUNK_PAYLOAD``). Together they cap
      one buffered message at ``max_fragments * max_chunk_payload`` bytes.
      A single-chunk message (``total == 1``) is returned immediately and
      never buffered, so it is exempt from the size cap: its size is bounded
      by the datagram that carried it, and existing senders (the ESP32
      firmware) emit oversize single-chunk messages that must keep working.
    * ``max_buffered_bytes`` — optional budget across all partials; when a
      chunk would exceed it, the oldest *other* partials are evicted, and the
      chunk is dropped if it still doesn't fit. ``None`` (default) = no
      budget beyond the two caps above.

    ``timeout`` discards partials older than that many seconds.
    """

    def __init__(self,
                 timeout: float = DEFAULT_REASSEMBLY_TIMEOUT,
                 buffer_limit: int = DEFAULT_BUFFER_LIMIT,
                 max_fragments: int = DEFAULT_MAX_FRAGMENTS,
                 max_buffered_bytes: Optional[int] = DEFAULT_MAX_BUFFERED_BYTES,
                 max_chunk_payload: int = MAX_CHUNK_PAYLOAD) -> None:
        if max_fragments < 1:
            raise ValueError("max_fragments must be >= 1")
        if max_chunk_payload < 1:
            raise ValueError("max_chunk_payload must be >= 1")
        if max_buffered_bytes is not None and max_buffered_bytes < 0:
            raise ValueError("max_buffered_bytes must be >= 0 or None")
        self._buffers: dict = {}
        self._timeout = timeout
        self._limit = buffer_limit
        self._max_fragments = max_fragments
        self._max_bytes = max_buffered_bytes
        self._max_chunk = max_chunk_payload
        self._bytes = 0  # sum of _Partial.size over _buffers

    @property
    def buffered_bytes(self) -> int:
        """Bytes currently held across all partial messages."""
        return self._bytes

    @property
    def pending(self) -> int:
        """Number of partial messages currently buffered."""
        return len(self._buffers)

    def feed(self, chunk: bytes) -> Optional[bytes]:
        """Feed one decrypted Channel payload. Returns the full message if this
        chunk completes one, else None. Invalid chunks are dropped (None)."""
        parsed = parse_chunk(chunk)
        if parsed is None:
            log.debug("invalid fragment chunk, dropping")
            return None
        fid, seq, total, data = parsed
        if total > self._max_fragments:
            log.debug("fragment %s declares %d chunks (> %d), dropping",
                      fid.hex()[:8], total, self._max_fragments)
            return None

        self._gc_stale()

        buf = self._buffers.get(fid)
        if buf is not None and buf.total != total:
            log.warning("fragment %s total mismatch (%d->%d), resetting",
                        fid.hex()[:8], buf.total, total)
            self._drop(fid)
            buf = None
        if buf is not None and seq in buf.parts:
            return None  # duplicate

        # Single-chunk messages never touch the buffers, so they bypass every
        # budget (they can't accumulate) and never evict a partial message.
        if buf is None and total == 1:
            return data
        if len(data) > self._max_chunk:
            log.debug("fragment %s chunk of %d B exceeds max_chunk_payload, dropping",
                      fid.hex()[:8], len(data))
            return None

        if self._max_bytes is not None:
            if len(data) > self._max_bytes:
                log.warning("fragment %s chunk exceeds the reassembly byte budget, dropping",
                            fid.hex()[:8])
                return None
            while self._bytes + len(data) > self._max_bytes:
                victim = self._oldest(exclude=fid)
                if victim is None:
                    # Only this message's own parts are left: it can't fit.
                    log.warning("fragment %s exceeds the reassembly byte budget, dropping it",
                                fid.hex()[:8])
                    self._drop(fid)
                    return None
                log.warning("reassembly byte budget full; evicting %s", victim.hex()[:8])
                self._drop(victim)

        if buf is None:
            if len(self._buffers) >= self._limit:
                oldest = self._oldest()
                log.warning("reassembly buffer full; evicting %s", oldest.hex()[:8])
                self._drop(oldest)
            buf = _Partial(total=total, first_seen=time.monotonic())
            self._buffers[fid] = buf

        buf.parts[seq] = data
        buf.size += len(data)
        self._bytes += len(data)
        if not buf.complete():
            return None
        self._drop(fid)
        return buf.assemble()

    def _oldest(self, exclude: Optional[bytes] = None) -> Optional[bytes]:
        best = None
        for k, v in self._buffers.items():
            if k != exclude and (best is None or v.first_seen < best[1]):
                best = (k, v.first_seen)
        return None if best is None else best[0]

    def _drop(self, fid: bytes) -> None:
        buf = self._buffers.pop(fid, None)
        if buf is not None:
            self._bytes -= buf.size

    def _gc_stale(self) -> None:
        if not self._buffers:
            return
        cutoff = time.monotonic() - self._timeout
        stale = [k for k, v in self._buffers.items() if v.first_seen < cutoff]
        for k in stale:
            log.warning("dropping incomplete reassembly %s", k.hex()[:8])
            self._drop(k)
