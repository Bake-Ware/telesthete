"""Addressed hub routing helpers (SPEC §10.1): route ids, the ROUTE (0x05)
declaration frame, and how receivers that predate it treat one.

The Rust crate checks the same `route` section of tests/vectors.json
(framing::tests::route_vectors_match_python).
"""

import json
import os
import sys

import pytest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from telesthete.protocol import framing, route
from telesthete.protocol.crypto import BandCrypto, build_aad
from telesthete.protocol.framing import ChannelType, unpack_packet

VECTORS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "vectors.json")


def _vectors():
    with open(VECTORS, encoding="utf-8") as f:
        return json.load(f)


def test_channel_type_value():
    # 0x05/0x06 are the next free values after DROP (SPEC §2); must never move.
    assert ChannelType.ROUTE == 0x05
    assert ChannelType.ADDRESSED == 0x06
    assert len({int(t) for t in ChannelType}) == len(list(ChannelType))


def test_addressed_frame_shape_and_aad():
    crypto = BandCrypto("route-test")
    rid = route.route_id_for("kaiju")
    frame = route.pack_addressed(crypto, 9, rid, b"chunk")
    pkt = unpack_packet(frame)
    assert (pkt.channel_type, pkt.channel_id, pkt.sequence) == (ChannelType.ADDRESSED, rid, 9)
    # Default: spec AAD, so a rewritten destination fails authentication.
    assert crypto.decrypt(9, pkt.ciphertext, associated_data=build_aad(0x06, rid)) == b"chunk"
    with pytest.raises(Exception):
        crypto.decrypt(9, pkt.ciphertext, associated_data=build_aad(0x06, rid ^ 1))
    with pytest.raises(ValueError):
        route.pack_addressed(crypto, 1, 0x10000, b"")


def test_addressed_empty_aad_profile_reaches_old_rook_receivers():
    # Compat (SPEC §10.1): Rook's master transport and the ESP32 firmware
    # decrypt every frame with EMPTY AAD and never look at channel_type. An
    # ADDRESSED frame sealed for that profile (associated_data=b"") is
    # therefore accepted by them when an old, broadcasting hub relays it.
    crypto = BandCrypto("route-test")
    frame = route.pack_addressed(crypto, 11, 1234, b"hello", associated_data=b"")
    pkt = unpack_packet(frame)
    assert pkt.channel_type == ChannelType.ADDRESSED
    assert crypto.decrypt(pkt.sequence, pkt.ciphertext) == b"hello"


def test_route_id_vectors():
    for case in _vectors()["route"]["ids"]:
        assert route.route_id_for(case["name"]) == case["route_id"], case["name"]


def test_route_id_properties():
    ids = {route.route_id_for(f"worker-{i}") for i in range(2000)}
    assert 0 not in ids and all(0 < i <= 0xFFFF for i in ids)
    assert route.route_id_for("kaiju") == route.route_id_for("kaiju")  # stable
    # A name whose first digest word is 0 takes the next nonzero word.
    assert route.route_id_for("zero-prefix-28041") == 65152


def test_declaration_vectors():
    v = _vectors()
    crypto = BandCrypto(v["psk"])
    for case in v["route"]["declarations"]:
        frame = route.pack_route_declaration(crypto, case["seq"], case["route_id"])
        assert frame.hex() == case["frame_hex"]


def test_declaration_shape_and_authentication():
    crypto = BandCrypto("route-test")
    rid = route.route_id_for("kaiju")
    frame = route.pack_route_declaration(crypto, 77, rid)
    assert len(frame) == framing.MIN_PACKET_SIZE  # empty payload, tag only
    pkt = unpack_packet(frame)
    assert (pkt.band_id, pkt.channel_type, pkt.channel_id, pkt.sequence) == \
        (crypto.band_id, ChannelType.ROUTE, rid, 77)
    # Spec AAD binds the cleartext type + id: a peer holding the key can
    # authenticate it, and a header rewrite breaks it.
    assert crypto.decrypt(77, pkt.ciphertext, associated_data=build_aad(0x05, rid)) == b""
    with pytest.raises(Exception):
        crypto.decrypt(77, pkt.ciphertext, associated_data=build_aad(0x05, rid ^ 1))


def test_old_empty_aad_receiver_drops_declaration():
    # Compat: an OLD hub relays a declaration to everyone. Rook's worker
    # transport and the ESP32 firmware decrypt every frame with EMPTY AAD and
    # ignore channel_type, so the declaration must fail authentication there
    # rather than surface as a (garbage) message.
    crypto = BandCrypto("route-test")
    frame = route.pack_route_declaration(crypto, 5, route.route_id_for("kaiju"))
    with pytest.raises(Exception):
        crypto.decrypt(5, unpack_packet(frame).ciphertext)


def test_route_id_range_checked():
    crypto = BandCrypto("route-test")
    with pytest.raises(ValueError):
        route.pack_route_declaration(crypto, 1, 0x10000)
    with pytest.raises(ValueError):
        route.pack_route_declaration(crypto, 1, -1)
    # 0 is legal: it withdraws a declaration.
    assert unpack_packet(route.pack_route_declaration(crypto, 1, 0)).channel_id == 0


async def test_spec_band_drops_declaration_at_dispatch():
    # Spec peers dispatch by channel_type and nothing registers ROUTE, so a
    # declaration an old hub relays is dropped at dispatch (SPEC §2 MUST-drop).
    from telesthete.band import Band
    b = Band("route-test", bind_address="127.0.0.1", bind_port=0)
    await b.start()
    try:
        b.channel(1, ("127.0.0.1", 9))  # register the lazy CHANNEL handler too
        handlers = b.transport._handlers
        assert ChannelType.CHANNEL in handlers
        assert not handlers.get(ChannelType.ROUTE), "nothing may consume ROUTE"
        # Band has no ADDRESSED consumer either: it is an application-level
        # type (Rook's transport), dropped at dispatch by a plain Band.
        assert not handlers.get(ChannelType.ADDRESSED)
    finally:
        await b.stop()
