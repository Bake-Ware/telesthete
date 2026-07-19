"""Per-sender monotonic sequence source (SPEC §3.3).

The AEAD nonce is ``4 zero bytes || 8-byte big-endian sequence`` (§3.2) and the
key is band-wide (PSK-derived, with no per-sender component, §3.1). So the
*sequence alone* must never repeat under a given key, or two senders — or one
sender across a process restart — reuse an AEAD ``(key, nonce)`` pair, which is
catastrophic for ChaCha20-Poly1305 / AES-GCM (keystream recovery + tag forgery).

A single counter **shared across all of a sender's channels and streams**,
initialized from a CSPRNG, makes such a collision cryptographically negligible
(birthday-bounded over a 63-bit start) instead of guaranteed. SPEC §3.3 mandates
the random initialization.
"""

import os

_MASK = (1 << 64) - 1


class SequenceSource:
    """A monotonic 64-bit sequence source, CSPRNG-seeded (SPEC §3.3).

    One instance is shared by all of a sender's Control/Stream/Channel objects so
    that every packet that sender emits under the band key carries a unique
    sequence (hence a unique nonce).
    """

    def __init__(self, start=None):
        if start is None:
            # 63-bit random start: negligible cross-sender collision probability,
            # with ~2^63 of headroom before wrap. A wrap would reuse nonces, but
            # is unreachable at any realistic packet rate.
            start = int.from_bytes(os.urandom(8), "big") >> 1
        self._seq = start & _MASK

    def next(self) -> int:
        """Return the current sequence value, then advance by one."""
        s = self._seq
        self._seq = (self._seq + 1) & _MASK
        return s

    def peek(self) -> int:
        """The value that the next :meth:`next` call will return."""
        return self._seq
