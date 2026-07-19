"""§5.4 StreamHeader + §5.4.2 dmabuf descriptor (v1.1 extended Stream layout).

Pure pack/parse, byte-identical with the Rust reference
(rust/telesthete/src/wire/stream.rs and wire/dmabuf.rs). Used when the
sending peer advertised capability ``dmabuf-v1`` (§12.5); v1.0 peers keep
the §5.1 priority-byte layout. The file descriptors themselves travel
out-of-band via SCM_RIGHTS on AF_UNIX (§9.4); this module never sees them.
"""

import enum
import struct
from dataclasses import dataclass, field
from typing import List, Tuple

STREAM_HEADER_LEN = 8

HEADER_BYTES = 22
PLANE_BYTES = 9
MAX_PLANES = 4
MAX_FDS_PER_PACKET = 5  # 4 planes + 1 sync_file fence

DRM_FORMAT_MOD_LINEAR = 0
DRM_FORMAT_MOD_INVALID = 0x00FF_FFFF_FFFF_FFFF


class StreamFlags(enum.IntFlag):
    """§5.4.1 flag bits."""
    INIT = 0x01
    KEYFRAME = 0x02
    END_FRAME = 0x04
    FRAGMENT_CONT = 0x08
    DMABUF = 0x10
    WITH_FENCE = 0x20
    REUSE = 0x40
    EXTENDED = 0x80


class StreamHeaderError(ValueError):
    pass


class DmabufError(ValueError):
    pass


def fourcc(s: bytes) -> int:
    """FourCC code, DRM convention: little-endian so the four ASCII bytes
    round-trip (b'XR24' -> 0x34325258)."""
    if len(s) != 4:
        raise ValueError(f"fourcc needs 4 bytes, got {len(s)}")
    return int.from_bytes(s, "little")


def fourcc_to_bytes(code: int) -> bytes:
    return code.to_bytes(4, "little")


@dataclass(frozen=True)
class StreamHeader:
    """8-byte header: flags(1) || reserved(3, zero) || frame_id(4 BE)."""
    flags: StreamFlags
    frame_id: int

    def pack(self) -> bytes:
        return struct.pack(">B3xI", int(self.flags), self.frame_id)

    @classmethod
    def parse(cls, data: bytes) -> Tuple["StreamHeader", bytes]:
        if len(data) < STREAM_HEADER_LEN:
            raise StreamHeaderError(
                f"stream payload too short: {len(data)} bytes (need {STREAM_HEADER_LEN})")
        if data[1] or data[2] or data[3]:
            raise StreamHeaderError("reserved bytes nonzero")
        frame_id = struct.unpack(">I", data[4:8])[0]
        return cls(flags=StreamFlags(data[0]), frame_id=frame_id), data[STREAM_HEADER_LEN:]


@dataclass(frozen=True)
class DmabufPlane:
    offset: int
    stride: int
    fd_index: int  # index into the SCM_RIGHTS fd array


@dataclass
class DmabufDescriptor:
    """§5.4.2 fixed-layout descriptor carried after the StreamHeader when
    the DMABUF flag is set."""
    width: int
    height: int
    fourcc: int
    modifier: int
    planes: List[DmabufPlane] = field(default_factory=list)
    # fds in the accompanying SCM_RIGHTS message; equals len(planes), plus
    # one if WITH_FENCE, or zero if REUSE.
    fd_count: int = 0

    @staticmethod
    def encoded_len(plane_count: int) -> int:
        return HEADER_BYTES + PLANE_BYTES * plane_count

    def pack(self) -> bytes:
        # Bounds-check the real count before it is narrowed to one byte
        # (260 & 0xFF == 4 would otherwise lie about the body length).
        if not 1 <= len(self.planes) <= MAX_PLANES:
            raise DmabufError(
                f"plane_count {len(self.planes)} out of range (1..={MAX_PLANES})")
        if not 0 <= self.fd_count <= MAX_FDS_PER_PACKET:
            raise DmabufError(
                f"fd_count {self.fd_count} exceeds MAX_FDS_PER_PACKET ({MAX_FDS_PER_PACKET})")
        out = struct.pack(">II", self.width, self.height)
        out += fourcc_to_bytes(self.fourcc)
        out += struct.pack(">QBB", self.modifier, len(self.planes), self.fd_count)
        for p in self.planes:
            out += struct.pack(">IIB", p.offset, p.stride, p.fd_index)
        return out

    @classmethod
    def parse(cls, data: bytes) -> "DmabufDescriptor":
        if len(data) < HEADER_BYTES:
            raise DmabufError(
                f"dmabuf payload too short: {len(data)} bytes, need {HEADER_BYTES}")
        width, height = struct.unpack(">II", data[0:8])
        fcc = int.from_bytes(data[8:12], "little")
        modifier, plane_count, fd_count = struct.unpack(">QBB", data[12:22])
        if not 1 <= plane_count <= MAX_PLANES:
            raise DmabufError(f"plane_count {plane_count} out of range (1..={MAX_PLANES})")
        if fd_count > MAX_FDS_PER_PACKET:
            raise DmabufError(
                f"fd_count {fd_count} exceeds MAX_FDS_PER_PACKET ({MAX_FDS_PER_PACKET})")
        need = cls.encoded_len(plane_count)
        if len(data) < need:
            raise DmabufError(f"dmabuf payload too short: {len(data)} bytes, need {need}")
        planes = []
        off = HEADER_BYTES
        for _ in range(plane_count):
            offset, stride, fd_index = struct.unpack(">IIB", data[off:off + PLANE_BYTES])
            if fd_count > 0 and fd_index >= fd_count:
                raise DmabufError(f"fd_count {fd_count} cannot serve plane fd_index {fd_index}")
            planes.append(DmabufPlane(offset=offset, stride=stride, fd_index=fd_index))
            off += PLANE_BYTES
        return cls(width=width, height=height, fourcc=fcc, modifier=modifier,
                   planes=planes, fd_count=fd_count)

    def check_flags(self, flags: StreamFlags) -> None:
        """Validate descriptor consistency against the carrying packet's
        StreamFlags (§5.4.2). Call after parse."""
        fence_fds = 1 if flags & StreamFlags.WITH_FENCE else 0
        if flags & StreamFlags.REUSE:
            # Consumer already imported the buffer; only a fresh fence may ride.
            if self.fd_count != fence_fds:
                raise DmabufError(
                    f"REUSE set but fd_count={self.fd_count} "
                    f"(expected {fence_fds} with these flags)")
        else:
            plane_fds_needed = max((p.fd_index + 1 for p in self.planes), default=0)
            plane_fds_available = max(self.fd_count - fence_fds, 0)
            if plane_fds_needed > plane_fds_available:
                raise DmabufError(
                    f"fd_count {self.fd_count} cannot serve plane fd_index "
                    f"{plane_fds_needed - 1}")
