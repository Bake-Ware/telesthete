"""Phase 5 — §5.4 StreamHeader + §5.4.2 dmabuf descriptor (Python side).

Byte-identical with the Rust wire implementation (rust/telesthete/src/wire/
stream.rs and wire/dmabuf.rs); the shared vectors in vectors.json pin both.
"""

import json
import os
import sys

import pytest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.protocol.streamheader import (
    STREAM_HEADER_LEN,
    HEADER_BYTES,
    PLANE_BYTES,
    MAX_PLANES,
    MAX_FDS_PER_PACKET,
    DRM_FORMAT_MOD_LINEAR,
    StreamFlags,
    StreamHeader,
    DmabufPlane,
    DmabufDescriptor,
    StreamHeaderError,
    DmabufError,
    fourcc,
)

VECTORS = json.load(open(os.path.join(os.path.dirname(__file__), "vectors.json")))


# ---------------------------------------------------------------- StreamHeader

def test_header_round_trip():
    h = StreamHeader(flags=StreamFlags.KEYFRAME | StreamFlags.END_FRAME,
                     frame_id=0xDEADBEEF)
    buf = h.pack() + b"abc"
    parsed, rest = StreamHeader.parse(buf)
    assert parsed == h
    assert rest == b"abc"
    assert len(h.pack()) == STREAM_HEADER_LEN == 8


def test_header_rejects_short_input():
    with pytest.raises(StreamHeaderError):
        StreamHeader.parse(b"\x00" * 7)


def test_header_rejects_nonzero_reserved():
    buf = bytearray(StreamHeader(flags=StreamFlags.INIT, frame_id=1).pack())
    buf[2] = 1
    with pytest.raises(StreamHeaderError):
        StreamHeader.parse(bytes(buf))


def test_header_all_flag_bits_are_defined():
    # v1.1 defines all 8 bits, so no value is "unknown" — but EXTENDED (bit 7)
    # must surface so callers can drop per §5.4.1.
    buf = bytearray(StreamHeader(flags=StreamFlags.EXTENDED, frame_id=2).pack())
    parsed, _ = StreamHeader.parse(bytes(buf))
    assert parsed.flags & StreamFlags.EXTENDED


def test_header_wire_layout_matches_spec():
    h = StreamHeader(flags=StreamFlags.INIT, frame_id=0x01020304)
    b = h.pack()
    assert b == bytes([0x01, 0, 0, 0, 0x01, 0x02, 0x03, 0x04])


# ------------------------------------------------------------------- dmabuf

def _xr24():
    return DmabufDescriptor(
        width=1920, height=1080, fourcc=fourcc(b"XR24"),
        modifier=DRM_FORMAT_MOD_LINEAR,
        planes=[DmabufPlane(offset=0, stride=1920 * 4, fd_index=0)],
        fd_count=1,
    )


def test_dmabuf_round_trip_and_size():
    d = _xr24()
    b = d.pack()
    assert len(b) == HEADER_BYTES + PLANE_BYTES == 31
    assert DmabufDescriptor.parse(b) == d


def test_dmabuf_fourcc_is_little_endian_ascii():
    b = _xr24().pack()
    assert b[8:12] == b"XR24"


def test_dmabuf_pack_rejects_zero_and_excess_planes():
    d = _xr24()
    d.planes = []
    with pytest.raises(DmabufError):
        d.pack()
    d.planes = [DmabufPlane(0, 4, 0)] * (MAX_PLANES + 1)
    with pytest.raises(DmabufError):
        d.pack()


def test_dmabuf_pack_rejects_260_planes_no_truncation():
    # 260 & 0xFF == 4: a u8-cast-first implementation would emit a header
    # claiming 4 planes over a 260-plane body.
    d = _xr24()
    d.planes = [DmabufPlane(0, 4, 0)] * 260
    with pytest.raises(DmabufError):
        d.pack()


def test_dmabuf_fd_count_bounds():
    d = _xr24()
    d.fd_count = MAX_FDS_PER_PACKET + 1
    with pytest.raises(DmabufError):
        d.pack()
    good = bytearray(_xr24().pack())
    good[21] = 200
    with pytest.raises(DmabufError):
        DmabufDescriptor.parse(bytes(good))


def test_dmabuf_parse_rejects_bad_plane_count_and_truncation():
    b = bytearray(_xr24().pack())
    b[20] = 0
    with pytest.raises(DmabufError):
        DmabufDescriptor.parse(bytes(b))
    b[20] = MAX_PLANES + 1
    with pytest.raises(DmabufError):
        DmabufDescriptor.parse(bytes(b))
    with pytest.raises(DmabufError):
        DmabufDescriptor.parse(_xr24().pack()[:-1])


def test_dmabuf_parse_rejects_fd_index_out_of_range():
    b = bytearray(_xr24().pack())
    b[HEADER_BYTES + 8] = 5  # plane fd_index=5 but fd_count=1
    with pytest.raises(DmabufError):
        DmabufDescriptor.parse(bytes(b))


def test_check_flags_fence_and_reuse():
    d = _xr24()
    d.fd_count = 2  # plane fd + fence
    d.check_flags(StreamFlags.DMABUF | StreamFlags.WITH_FENCE)

    reuse = _xr24()
    reuse.fd_count = 0
    reuse.check_flags(StreamFlags.DMABUF | StreamFlags.REUSE)
    reuse.fd_count = 1
    with pytest.raises(DmabufError):
        reuse.check_flags(StreamFlags.DMABUF | StreamFlags.REUSE)
    # REUSE + WITH_FENCE carries exactly the fence fd.
    reuse.check_flags(StreamFlags.DMABUF | StreamFlags.REUSE | StreamFlags.WITH_FENCE)


def test_check_flags_fence_reduces_plane_budget():
    # fd_count=1 with WITH_FENCE: the only fd is the fence, so a plane
    # referencing fd_index 0 has no fd to use.
    d = _xr24()
    d.fd_count = 1
    with pytest.raises(DmabufError):
        d.check_flags(StreamFlags.DMABUF | StreamFlags.WITH_FENCE)


# ------------------------------------------------------------- shared vectors

def test_stream_header_vectors_match():
    for v in VECTORS["stream_header"]:
        h = StreamHeader(flags=StreamFlags(v["flags"]), frame_id=v["frame_id"])
        assert h.pack().hex() == v["packed_hex"], v


def test_dmabuf_vectors_match():
    for v in VECTORS["dmabuf"]:
        d = DmabufDescriptor(
            width=v["width"], height=v["height"],
            fourcc=fourcc(v["fourcc"].encode()), modifier=int(v["modifier"], 16),
            planes=[DmabufPlane(**p) for p in v["planes"]],
            fd_count=v["fd_count"],
        )
        assert d.pack().hex() == v["packed_hex"], v
        assert DmabufDescriptor.parse(bytes.fromhex(v["packed_hex"])) == d
