"""Addressed hub routing helpers. See SPEC.md §10.1.

A relay hub broadcasts every frame to every peer in the band unless told
otherwise. A peer opts in to *addressed* delivery by declaring a 16-bit route
id with a ROUTE (0x05) frame whose ``channel_id`` is that id; the hub records
it, does not relay the declaration, and thereafter delivers CHANNEL (0x02)
frames whose ``channel_id`` equals the id only to the peer(s) that declared it.
``channel_id`` 0 (and any id nobody declared) keeps the broadcast behaviour.

Hub state is soft (forgotten on eviction, disconnect, or hub restart), so a
peer SHOULD re-send its declaration periodically — e.g. with every keepalive.

Usage (worker named "kaiju")::

    rid = route_id_for("kaiju")
    sock.sendto(pack_route_declaration(crypto, seqs.next(), rid), hub)
    # anyone replying to kaiju frames its chunks with channel_id=rid:
    pack_packet(band_id, ChannelType.CHANNEL, rid, seq, ciphertext)
"""

from __future__ import annotations

import hashlib

from .crypto import BandCrypto, build_aad
from .framing import ChannelType, pack_packet

# Domain separation, so a route id is never accidentally equal to some other
# SHA-256-derived value of the same name. Matches the Rust ROUTE_ID_DOMAIN.
ROUTE_ID_DOMAIN = b"telesthete-route-v1:"

# channel_id 0 is "broadcast"; declaring it withdraws a declaration (§10.1).
ROUTE_BROADCAST = 0


def route_id_for(name: str) -> int:
    """Stable, nonzero 16-bit route id for ``name`` (SPEC §10.1).

    ``SHA-256(ROUTE_ID_DOMAIN || utf8(name))`` read as big-endian 16-bit words;
    the first nonzero word wins (0 is reserved for broadcast), with 1 as the
    fallback for an all-zero digest. Byte-identical with Rust
    ``telesthete::route_id_for`` (pinned by tests/vectors.json ``route``).

    Ids are only 16 bits, so distinct names can collide; the hub then delivers
    to every peer sharing the id, and each receiver must still check that a
    message is meant for it (as it already does under broadcast).
    """
    digest = hashlib.sha256(ROUTE_ID_DOMAIN + name.encode("utf-8")).digest()
    for i in range(0, len(digest), 2):
        word = int.from_bytes(digest[i:i + 2], "big")
        if word:
            return word
    return 1


def pack_route_declaration(crypto: BandCrypto, sequence: int, route_id: int) -> bytes:
    """Build a ROUTE frame declaring ``route_id`` for the sending peer.

    The payload is empty: the frame is exactly ``MIN_PACKET_SIZE`` (43) bytes,
    an AEAD tag over nothing, sealed with ``crypto``'s key under the §3.2 AAD
    ``[0x05, id_hi, id_lo]``. The hub never decrypts it (it routes on the
    cleartext header); the tag only makes the frame a legal, authenticated
    Telesthete packet for anyone holding the key. Because of that AAD, a
    receiver that decrypts with empty AAD (Rook's transport, the ESP32
    firmware) fails authentication and drops a declaration an *old* hub
    relays to it — and spec receivers drop it by channel type (§2).

    ``sequence`` MUST come from the sender's single shared sequence source
    (§3.3), like every other frame it sends. ``route_id`` 0 withdraws.
    """
    if not 0 <= route_id <= 0xFFFF:
        raise ValueError(f"route_id must be 0-65535, got {route_id}")
    aad = build_aad(ChannelType.ROUTE, route_id)
    ciphertext = crypto.encrypt(sequence, b"", associated_data=aad)
    return pack_packet(crypto.band_id, ChannelType.ROUTE, route_id, sequence, ciphertext)


__all__ = ["ROUTE_ID_DOMAIN", "ROUTE_BROADCAST", "route_id_for", "pack_route_declaration"]
