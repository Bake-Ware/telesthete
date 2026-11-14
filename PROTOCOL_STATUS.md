# Telesthete — Implementation Status

LIBRARY_COMPLETION.md is the verified audit; this table follows it.

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
| Cipher negotiation (end-to-end, §3.5) | §3.5 | ✅ full data path | ✅ |
| Replay protection (MUST, watermark) | §3.3 | ✅ Control + Stream | ✅ Control + Stream |
| **Cross-impl conformance vectors** | §3, §6.6 | ✅ | ✅ |

## Channels

| Feature | Spec | Python | Rust |
|---------|------|--------|------|
| Control (HELLO/ACK/KEEPALIVE/GOODBYE/...) | §4 | ✅ | ✅ |
| KEYFRAME_REQ / RATE_HINT control msgs | §4.9/4.10 | ✅ | ✅ |
| Stream (lossy, prioritized) | §5 | ✅ | ✅ |
| StreamHeader + dmabuf descriptor | §5.4 | ✅ | ✅ |
| Channel (reliable, ordered) | §6 | ✅ | ✅ |
| Message fragmentation envelope | §6.6 | ✅ (+ chunk/total/byte-budget caps, evicted-id memory) | ✅ (same caps, ported) |
| Capability announce (mandatory) | §12.5 | ✅ | ✅ |
| Board / Drop | §7/§8 | ✅ | ✅ |

## Transport

| Feature | Spec | Python | Rust |
|---------|------|--------|------|
| UDP | §9.1 | ✅ | ✅ |
| LAN discovery (broadcast) | §9.2 | ✅ | ✅ |
| AF_UNIX (SEQPACKET + SCM_RIGHTS fds) | §9.4 | ✅ | ✅ |
| WebTransport | §9.6 | ⬜ client | ⬜ client — served by the relay |
| WebSocket fallback | §9.3 | ⬜ client | ⬜ client — served by the relay; clients designed in spec/terminating-endpoints.md |
| Telesthetium hub / relay | §10 | — | ✅ (`telesthitium`, see `rust/telesthitium/CONFORMANCE.md`) |
| Hub addressed routing (Route 0x05 / Addressed 0x06) | §10.1 | ✅ `route_id_for`, `pack_route_declaration`, `pack_addressed` | ✅ hub + `route_id_for`, `ChannelType::Addressed` |

## Notable gaps / next steps

WebSocket and WebTransport clients (none in either library); link lifecycle
across restarts and many keys on one endpoint (spec/terminating-endpoints.md
§4, §7).

## Tests

| Suite | What it proves |
|-------|----------------|
| `tests/test_vectors.py` / `crypto::tests::conformance_vectors_match_python` | Python ⇄ Rust crypto byte-identical (chacha + aes) |
| `tests/test_fragment.py` / `wire::fragment::tests::conformance_vectors_match_python` | Fragmentation byte-identical; reassembly memory bounds |
| `tests/test_route.py` / `framing::tests::route_vectors_match_python` | Route ids, Route declarations and Addressed frames (spec + empty-AAD profiles) byte-identical; old empty-AAD receivers reject declarations and accept empty-AAD Addressed frames |
| `tests/test_negotiation.py` | Live two-Band AES handshake + baseline fallback |
| `tests/test_replay.py` | Control + Stream reject replays/stale |
| `tests/test_end_to_end.py`, `tests/test_crypto_framing.py` | Band data path, AEAD/framing integration |
| `cargo test -p telesthete` | 113 Rust unit/integration tests |
| `cargo test -p telesthitium` | 77 hub tests (see `rust/telesthitium/CONFORMANCE.md`) |

> `tests/test_full_stack.py` exercises LAN-broadcast discovery and needs UDP
> broadcast on the host; the stream/crypto path it covers is also exercised by
> `test_end_to_end` + `test_negotiation` over UDP unicast.
