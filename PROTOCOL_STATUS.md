# Telesthete — Implementation Status

Tracks the two reference implementations against [`SPEC.md`](SPEC.md) (wire v1.2,
`PROTOCOL_VERSION = 3`). The wire format is the contract; both impls are pinned
to it by shared conformance vectors in [`tests/vectors.json`](tests/vectors.json).

Legend: ✅ done · ◑ partial · ⬜ not yet · — n/a

## Core protocol

| Feature | Spec | Python | Rust |
|---------|------|--------|------|
| Frame format (27-byte header) | §1 | ✅ | ✅ |
| PSK key derivation (band_id, per-cipher HKDF) | §3.1 | ✅ | ✅ |
| AEAD baseline ChaCha20-Poly1305 (IETF) | §3.2 | ✅ | ✅ |
| AEAD optional AES-256-GCM | §3.2 | ✅ (`cryptography`) | ✅ (`aes-gcm`) |
| 12-byte nonce, real 3-byte AAD | §3.2 | ✅ | ✅ |
| Cipher negotiation (end-to-end, §3.5) | §3.5 | ✅ full data path | ◑ structs + `select_cipher` |
| Replay protection (MUST, watermark) | §3.3 | ✅ Control + Stream | ✅ Control + Stream |
| **Cross-impl conformance vectors** | §3, §6.6 | ✅ | ✅ |

## Channels

| Feature | Spec | Python | Rust |
|---------|------|--------|------|
| Control (HELLO/ACK/KEEPALIVE/GOODBYE/...) | §4 | ✅ | ✅ |
| KEYFRAME_REQ / RATE_HINT control msgs | §4.9/4.10 | ⬜ | ⬜ |
| Stream (lossy, prioritized) | §5 | ✅ | ✅ |
| StreamHeader + dmabuf descriptor | §5.4 | ⬜ | ✅ |
| Channel (reliable, ordered) | §6 | ◑ PoC (no real retransmit) | ✅ |
| Message fragmentation envelope | §6.6 | ✅ | ✅ |
| Capability announce (mandatory) | §12.5 | ✅ | ✅ structs |
| Board / Drop | §7/§8 | ⬜ | ⬜ |

## Transport

| Feature | Spec | Python | Rust |
|---------|------|--------|------|
| UDP | §9.1 | ✅ | ✅ |
| LAN discovery (broadcast) | §9.2 | ✅ | ⬜ |
| AF_UNIX (SEQPACKET + SCM_RIGHTS fds) | §9.4 | ⬜ | ✅ |
| WebTransport | §9.6 | ⬜ | ⬜ |
| WebSocket fallback | §9.3 | ⬜ | ⬜ |
| Telesthetium hub / relay | §10 | ⬜ | ◑ (`telesthitium`) |

## Notable gaps / next steps

- **Rust cipher negotiation data path** — the wire structs, `select_cipher`, and
  capability advertising are in place, but the Rust transport still encrypts with
  the baseline key. Per-peer negotiated-key wiring is the next step (Python
  already does the full per-destination path).
- **WebTransport (§9.6) + WebCodecs client** — specced; not yet implemented.
- **KEYFRAME_REQ / RATE_HINT (§4.9/4.10)** — specced; not yet wired into either
  Stream consumer.
- **Channel reliability (§6)** — the Python `Channel` retransmit loop is a stub
  (marks packets resent without re-sending); a real retransmit MUST allocate a
  fresh sequence to avoid nonce reuse. The Rust `Channel` is more complete.

## Tests

| Suite | What it proves |
|-------|----------------|
| `tests/test_vectors.py` / `crypto::tests::conformance_vectors_match_python` | Python ⇄ Rust crypto byte-identical (chacha + aes) |
| `tests/test_fragment.py` / `wire::fragment::tests::conformance_vectors_match_python` | Fragmentation byte-identical |
| `tests/test_negotiation.py` | Live two-Band AES handshake + baseline fallback |
| `tests/test_replay.py` | Control + Stream reject replays/stale |
| `tests/test_end_to_end.py`, `tests/test_crypto_framing.py` | Band data path, AEAD/framing integration |
| `cargo test -p telesthete` | 41 Rust unit/integration tests |

> `tests/test_full_stack.py` exercises LAN-broadcast discovery and needs UDP
> broadcast on the host; the stream/crypto path it covers is also exercised by
> `test_end_to_end` + `test_negotiation` over UDP unicast.
