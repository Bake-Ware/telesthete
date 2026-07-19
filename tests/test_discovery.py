"""§9.2 LAN discovery wire format (length-prefixed hostname, v1.2)."""

import os
import sys

sys.path.insert(0, os.path.join(os.path.dirname(__file__), ".."))

from telesthete.protocol.framing import PROTOCOL_VERSION
from telesthete.transport.discovery import pack_announce, parse_announce, MAGIC


def test_announce_layout_matches_spec():
    pkt = pack_announce("alice", 20001)
    assert pkt[:4] == b"TELE" == MAGIC
    assert pkt[4] == PROTOCOL_VERSION == 3
    assert pkt[5] == 5
    assert pkt[6:11] == b"alice"
    assert pkt[11:13] == (20001).to_bytes(2, "big")
    assert len(pkt) == 13


def test_announce_round_trip():
    assert parse_announce(pack_announce("héllo-host", 65535)) == ("héllo-host", 65535)
    assert parse_announce(pack_announce("", 1)) == ("", 1)


def test_parse_rejects_foreign_and_stale():
    assert parse_announce(b"NOPE" + b"\x03\x00\x00\x00") is None
    stale = bytearray(pack_announce("x", 1))
    stale[4] = PROTOCOL_VERSION + 1  # a version we cannot speak
    assert parse_announce(bytes(stale)) is None
    assert parse_announce(pack_announce("x", 1)[:-1]) is None  # truncated port
    assert parse_announce(b"") is None


def test_parse_ignores_trailing_bytes():
    # Forward-extensible: extra bytes after the port must not break parsing.
    assert parse_announce(pack_announce("h", 7) + b"future-fields") == ("h", 7)


def test_hostname_truncated_to_255():
    pkt = pack_announce("a" * 300, 9)
    hostname, port = parse_announce(pkt)
    assert hostname == "a" * 255
    assert port == 9
