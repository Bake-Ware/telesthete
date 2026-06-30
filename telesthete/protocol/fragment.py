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

    def complete(self) -> bool:
        return len(self.parts) == self.total

    def assemble(self) -> bytes:
        return b"".join(self.parts[i] for i in range(self.total))


class Reassembler:
    """Collect fragments and emit full payloads. Single-task; not thread-safe."""

    def __init__(self,
                 timeout: float = DEFAULT_REASSEMBLY_TIMEOUT,
                 buffer_limit: int = DEFAULT_BUFFER_LIMIT) -> None:
        self._buffers: dict = {}
        self._timeout = timeout
        self._limit = buffer_limit

    def feed(self, chunk: bytes) -> Optional[bytes]:
        """Feed one decrypted Channel payload. Returns the full message if this
        chunk completes one, else None. Invalid chunks are dropped (None)."""
        parsed = parse_chunk(chunk)
        if parsed is None:
            log.debug("invalid fragment chunk, dropping")
            return None
        fid, seq, total, data = parsed

        self._gc_stale()

        buf = self._buffers.get(fid)
        if buf is None:
            if len(self._buffers) >= self._limit:
                oldest = min(self._buffers.items(), key=lambda kv: kv[1].first_seen)
                log.warning("reassembly buffer full; evicting %s", oldest[0].hex()[:8])
                del self._buffers[oldest[0]]
            buf = _Partial(total=total, first_seen=time.monotonic())
            self._buffers[fid] = buf
        elif buf.total != total:
            log.warning("fragment %s total mismatch (%d->%d), resetting",
                        fid.hex()[:8], buf.total, total)
            buf = _Partial(total=total, first_seen=time.monotonic())
            self._buffers[fid] = buf

        if seq in buf.parts:
            return None  # duplicate
        buf.parts[seq] = data
        if not buf.complete():
            return None
        del self._buffers[fid]
        return buf.assemble()

    def _gc_stale(self) -> None:
        if not self._buffers:
            return
        cutoff = time.monotonic() - self._timeout
        stale = [k for k, v in self._buffers.items() if v.first_seen < cutoff]
        for k in stale:
            log.warning("dropping incomplete reassembly %s", k.hex()[:8])
            del self._buffers[k]
