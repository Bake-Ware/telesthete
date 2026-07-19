"""
Board channel implementation (SPEC §7)

Replicated last-writer-wins key-value state across a Band. Fire-and-forget
datagrams like Streams; convergence comes from idempotent LWW merges plus
digest-driven anti-entropy, not per-packet reliability.
"""

import hashlib
import json
import logging
from dataclasses import dataclass
from typing import Any, Callable, Dict, Optional, Tuple

from .framing import pack_packet, unpack_packet, ChannelType
from .sequence import SequenceSource
from .fragment import fragment, Reassembler, MAX_CHUNK_PAYLOAD

logger = logging.getLogger(__name__)


class BoardMessageType:
    SET = 0x01
    DIGEST = 0x02
    SYNC_REQ = 0x03
    SNAPSHOT = 0x04


@dataclass
class BoardEntry:
    value: Any
    lamport: int
    actor: str
    deleted: bool = False

    def ts(self) -> Tuple[int, str]:
        return (self.lamport, self.actor)

    def to_payload(self, key: str) -> dict:
        return {"key": key, "value": self.value, "ts": [self.lamport, self.actor],
                "deleted": self.deleted}


class Board:
    """
    One replicated LWW map (SPEC §7). `actor` is this writer's hostname —
    the total-order tiebreak for equal Lamport clocks (§7.3).
    """

    DIGEST_INTERVAL = 10.0  # seconds (§7.4 reference value)

    def __init__(
        self,
        band_id: bytes,
        board_id: int,
        actor: str,
        crypto=None,
        transport=None,
        send_crypto=None,
        recv_crypto=None,
        seq_source=None,
    ):
        self.band_id = band_id
        self.board_id = board_id
        self.actor = actor
        # Same resolver pattern as Stream/Control: per-peer session data keys
        # (SPEC §3.1/§3.3), with a fixed crypto fallback for standalone/tests.
        if send_crypto is not None and recv_crypto is not None:
            self._send_crypto = send_crypto
            self._recv_crypto = recv_crypto
        else:
            self._send_crypto = lambda addr: crypto
            self._recv_crypto = lambda addr: crypto
        self.transport = transport
        self._seq_source = seq_source if seq_source is not None else SequenceSource()

        self._entries: Dict[str, BoardEntry] = {}
        self._lamport = 0

        # Replay watermark per peer (SPEC §3.3): accept-first, then strictly
        # increasing. Merges are idempotent, but replay of an old SET must not
        # be able to re-trigger side effects (on_change) forever.
        self._recv_watermark: Dict[tuple, int] = {}
        self._reassemblers: Dict[tuple, Reassembler] = {}

        self._destinations = []
        self._on_change: Optional[Callable[[str, Any, bool], None]] = None

    # -- local API -----------------------------------------------------------

    def add_destination(self, peer_addr: tuple):
        if peer_addr not in self._destinations:
            self._destinations.append(peer_addr)

    def remove_destination(self, peer_addr: tuple):
        if peer_addr in self._destinations:
            self._destinations.remove(peer_addr)

    def reset_peer(self, peer_addr: tuple):
        """Forget a peer's watermark on session restart (SPEC §3.3)."""
        self._recv_watermark.pop(peer_addr, None)

    def on_change(self, callback: Callable[[str, Any, bool], None]):
        """callback(key, value, deleted) after any entry changes (local or remote)."""
        self._on_change = callback

    def set(self, key: str, value: Any):
        self._lamport += 1
        entry = BoardEntry(value=value, lamport=self._lamport, actor=self.actor)
        self._entries[key] = entry
        self._notify(key, entry)
        self._send_json(BoardMessageType.SET, entry.to_payload(key))

    def delete(self, key: str):
        self._lamport += 1
        entry = BoardEntry(value=None, lamport=self._lamport, actor=self.actor,
                           deleted=True)
        self._entries[key] = entry  # tombstone (§7.3): must propagate
        self._notify(key, entry)
        self._send_json(BoardMessageType.SET, entry.to_payload(key))

    def get(self, key: str, default=None):
        e = self._entries.get(key)
        return default if e is None or e.deleted else e.value

    def items(self) -> Dict[str, Any]:
        return {k: e.value for k, e in self._entries.items() if not e.deleted}

    # -- anti-entropy (§7.4) -------------------------------------------------

    def digest(self) -> Tuple[int, str]:
        """(count, hash): SHA-256 over sorted key || lamport_be8 || actor ||
        deleted_byte. Values are excluded — (lamport, actor) uniquely versions
        an entry (§7.4)."""
        h = hashlib.sha256()
        for key in sorted(self._entries):
            e = self._entries[key]
            h.update(key.encode("utf-8"))
            h.update(e.lamport.to_bytes(8, "big"))
            h.update(e.actor.encode("utf-8"))
            h.update(b"\x01" if e.deleted else b"\x00")
        return len(self._entries), h.hexdigest()

    def send_digest(self, dest: Optional[tuple] = None):
        count, hexdigest = self.digest()
        self._send_json(BoardMessageType.DIGEST,
                        {"count": count, "hash": hexdigest}, dest=dest)

    def send_snapshot(self, dest: tuple):
        payload = {"entries": [e.to_payload(k) for k, e in self._entries.items()]}
        env = json.dumps({"type": BoardMessageType.SNAPSHOT,
                          "payload": payload}).encode("utf-8")
        # SNAPSHOT always rides the §6.6 envelope, even single-chunk (§7.4) —
        # a Board frame starting with the envelope version byte 0x01 is a
        # snapshot chunk; direct JSON frames start with '{'.
        import os as _os
        for chunk in fragment(env, MAX_CHUNK_PAYLOAD, _os.urandom(16)):
            self._send_raw(chunk, dest)

    # -- merge (§7.3) --------------------------------------------------------

    def merge_entry(self, payload: dict) -> bool:
        """Apply one SET payload. Returns True if it changed local state."""
        key = payload["key"]
        lamport, actor = int(payload["ts"][0]), str(payload["ts"][1])
        # The Lamport clock is a uint64 on the wire and in the digest (§7.4
        # encodes it as 8 BE bytes). A negative or >2^64-1 value would raise in
        # to_bytes(8) and permanently break digest() — an anti-entropy DoS. A
        # conformant clock never leaves [0, 2^64); reject anything that does.
        if not 0 <= lamport < (1 << 64):
            logger.warning(f"Board {self.board_id}: rejecting out-of-range "
                           f"lamport {lamport} for key {key!r}")
            return False
        incoming = BoardEntry(value=payload.get("value"), lamport=lamport,
                              actor=actor, deleted=bool(payload.get("deleted", False)))
        self._lamport = max(self._lamport, lamport)  # clock advance (§7.3)
        current = self._entries.get(key)
        if current is not None and incoming.ts() <= current.ts():
            return False  # strictly-greater wins; equal ts implies equal value
        self._entries[key] = incoming
        self._notify(key, incoming)
        return True

    # -- wire ----------------------------------------------------------------

    def handle_packet(self, peer_addr: tuple, packet_bytes: bytes):
        try:
            packet = unpack_packet(packet_bytes)
            if packet.channel_type != ChannelType.BOARD or packet.channel_id != self.board_id:
                return
            watermark = self._recv_watermark.get(peer_addr)
            if watermark is not None and packet.sequence <= watermark:
                return  # replayed/stale (SPEC §3.3)
            rc = self._recv_crypto(peer_addr)
            if rc is None:
                return  # no session key yet
            plaintext = rc.decrypt(packet.sequence, packet.ciphertext, self._aad())
            self._recv_watermark[peer_addr] = packet.sequence

            if plaintext[:1] == b"\x01":
                # §6.6 chunk of a SNAPSHOT (§7.4).
                r = self._reassemblers.setdefault(peer_addr, Reassembler())
                full = r.feed(plaintext)
                if full is None:
                    return
                plaintext = full
            env = json.loads(plaintext.decode("utf-8"))
            self._dispatch(peer_addr, int(env["type"]), env.get("payload", {}))
        except Exception as e:
            logger.error(f"Board {self.board_id}: error handling packet: {e}")

    def _dispatch(self, peer_addr: tuple, type_: int, payload: dict):
        if type_ == BoardMessageType.SET:
            self.merge_entry(payload)
        elif type_ == BoardMessageType.DIGEST:
            count, hexdigest = self.digest()
            if (int(payload.get("count", -1)), payload.get("hash")) != (count, hexdigest):
                self._send_json(BoardMessageType.SYNC_REQ, {}, dest=peer_addr)
        elif type_ == BoardMessageType.SYNC_REQ:
            self.send_snapshot(peer_addr)
        elif type_ == BoardMessageType.SNAPSHOT:
            for entry_payload in payload.get("entries", []):
                self.merge_entry(entry_payload)
        else:
            logger.debug(f"Board {self.board_id}: unknown message type {type_}")

    def _notify(self, key: str, entry: BoardEntry):
        if self._on_change:
            try:
                self._on_change(key, entry.value, entry.deleted)
            except Exception as e:
                logger.error(f"Board {self.board_id}: on_change error: {e}")

    def _aad(self) -> bytes:
        return bytes([ChannelType.BOARD, self.board_id >> 8, self.board_id & 0xFF])

    def _send_json(self, type_: int, payload: dict, dest: Optional[tuple] = None):
        body = json.dumps({"type": type_, "payload": payload}).encode("utf-8")
        self._send_raw(body, dest)

    def _send_raw(self, plaintext: bytes, dest: Optional[tuple] = None):
        if self.transport is None:
            return
        dests = [dest] if dest is not None else list(self._destinations)
        # One shared outer sequence per packet (SPEC §3.3); same sequence to
        # every destination, like Stream.
        for d in dests:
            crypto = self._send_crypto(d)
            if crypto is None:
                continue
            sequence = self._seq_source.next()
            ciphertext = crypto.encrypt(sequence, plaintext, self._aad())
            self.transport.send(d, pack_packet(
                self.band_id, ChannelType.BOARD, self.board_id, sequence, ciphertext))
