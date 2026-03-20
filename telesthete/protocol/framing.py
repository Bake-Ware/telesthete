"""
Packet framing and serialization

Wire format:
┌─────────────────────────────────────┐
│ band_id          16 bytes           │ ← Cleartext for relay routing
│ channel_type      1 byte            │ ← Cleartext for multiplexing
│ channel_id        2 bytes           │ ← Cleartext for multiplexing
│ sequence          8 bytes           │ ← Cleartext (used as nonce)
│ ciphertext + tag  variable          │ ← Encrypted payload
└─────────────────────────────────────┘

Total header: 27 bytes + ciphertext
"""

import struct
from typing import Tuple, NamedTuple
from enum import IntEnum


class ChannelType(IntEnum):
    """Channel type identifiers"""
    CONTROL = 0x00
    STREAM = 0x01
    CHANNEL = 0x02
    BOARD = 0x03
    DROP = 0x04


class Packet(NamedTuple):
    """Unpacked packet structure"""
    band_id: bytes          # 16 bytes
    channel_type: int       # 1 byte
    channel_id: int         # 2 bytes (0-65535)
    sequence: int           # 8 bytes (0-2^64)
    ciphertext: bytes       # Variable length


# Packet header format: band_id(16) + channel_type(1) + channel_id(2) + sequence(8)
HEADER_FORMAT = "!16s B H Q"  # Big-endian: 16 bytes, uint8, uint16, uint64
HEADER_SIZE = struct.calcsize(HEADER_FORMAT)  # Should be 27 bytes


def pack_packet(
    band_id: bytes,
    channel_type: int,
    channel_id: int,
    sequence: int,
    ciphertext: bytes
) -> bytes:
    """
    Pack a packet into wire format

    Args:
        band_id: 16-byte band identifier
        channel_type: Channel type (0-255)
        channel_id: Channel identifier (0-65535)
        sequence: Packet sequence number (0-2^64)
        ciphertext: Encrypted payload (already encrypted with auth tag)

    Returns:
        Packed packet bytes ready to send over UDP

    Raises:
        ValueError: If parameters are invalid
    """
    # Validate inputs
    if len(band_id) != 16:
        raise ValueError(f"band_id must be 16 bytes, got {len(band_id)}")

    if not 0 <= channel_type <= 255:
        raise ValueError(f"channel_type must be 0-255, got {channel_type}")

    if not 0 <= channel_id <= 65535:
        raise ValueError(f"channel_id must be 0-65535, got {channel_id}")

    if not 0 <= sequence < 2**64:
        raise ValueError(f"sequence must be 0-2^64, got {sequence}")

    # Pack header
    header = struct.pack(
        HEADER_FORMAT,
        band_id,
        channel_type,
        channel_id,
        sequence
    )

    # Append ciphertext
    return header + ciphertext


def unpack_packet(data: bytes) -> Packet:
    """
    Unpack a packet from wire format

    Args:
        data: Raw packet bytes received from UDP

    Returns:
        Packet namedtuple with unpacked fields

    Raises:
        ValueError: If packet is malformed
    """
    if len(data) < HEADER_SIZE:
        raise ValueError(f"Packet too short: {len(data)} bytes, expected at least {HEADER_SIZE}")

    # Unpack header
    header = data[:HEADER_SIZE]
    ciphertext = data[HEADER_SIZE:]

    band_id, channel_type, channel_id, sequence = struct.unpack(HEADER_FORMAT, header)

    return Packet(
        band_id=band_id,
        channel_type=channel_type,
        channel_id=channel_id,
        sequence=sequence,
        ciphertext=ciphertext
    )


def pack_control_message(band_id: bytes, sequence: int, ciphertext: bytes) -> bytes:
    """
    Helper: Pack a control channel message (channel_type=0, channel_id=0)
    """
    return pack_packet(band_id, ChannelType.CONTROL, 0, sequence, ciphertext)


def pack_stream_message(
    band_id: bytes,
    stream_id: int,
    sequence: int,
    ciphertext: bytes
) -> bytes:
    """
    Helper: Pack a stream channel message (channel_type=1)
    """
    return pack_packet(band_id, ChannelType.STREAM, stream_id, sequence, ciphertext)


def test_framing():
    """Quick test of framing functions"""
    print(f"Header size: {HEADER_SIZE} bytes")

    # Test pack/unpack
    band_id = b"0123456789abcdef"
    channel_type = ChannelType.STREAM
    channel_id = 42
    sequence = 12345678
    ciphertext = b"encrypted_payload_with_auth_tag"

    # Pack
    packed = pack_packet(band_id, channel_type, channel_id, sequence, ciphertext)
    print(f"Packed packet: {len(packed)} bytes")

    # Unpack
    unpacked = unpack_packet(packed)
    print(f"Unpacked: {unpacked}")

    # Verify
    assert unpacked.band_id == band_id
    assert unpacked.channel_type == channel_type
    assert unpacked.channel_id == channel_id
    assert unpacked.sequence == sequence
    assert unpacked.ciphertext == ciphertext

    # Test helpers
    control_packet = pack_control_message(band_id, 1, b"control_data")
    stream_packet = pack_stream_message(band_id, 5, 100, b"stream_data")

    print(f"Control packet: {len(control_packet)} bytes")
    print(f"Stream packet: {len(stream_packet)} bytes")

    # Test error cases
    try:
        pack_packet(b"short", 0, 0, 0, b"")
        assert False, "Should fail with short band_id"
    except ValueError as e:
        print(f"Correctly rejected short band_id: {e}")

    try:
        unpack_packet(b"too_short")
        assert False, "Should fail with short packet"
    except ValueError as e:
        print(f"Correctly rejected short packet: {e}")

    print("\nAll framing tests passed!")


if __name__ == "__main__":
    test_framing()
