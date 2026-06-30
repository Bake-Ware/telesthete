# Telesthete

**Lightweight, encrypted, peer-to-peer transport.**

Telesthete provides encrypted, multiplexed, prioritized channels over UDP (with
AF_UNIX for same-host peers and WebTransport/WebSocket for browsers and hostile
networks), plus automatic LAN discovery and a relay fallback. The wire format —
not any one implementation — is the contract: see [`SPEC.md`](SPEC.md).

*Telesthete* (noun): *one who perceives at a distance.*

## Implementations

| Language | Location | Status |
|----------|----------|--------|
| **Python** (reference) | [`telesthete/`](telesthete/) | Protocol + UDP transport + Band API + cipher negotiation |
| **Rust** | [`rust/telesthete/`](rust/telesthete/) | Protocol + UDP/AF_UNIX transport + dmabuf + fragmentation |
| **C ABI** | [`rust/telesthete-c/`](rust/telesthete-c/) | C bindings over the Rust crate |
| **Hub** (Telesthetium) | [`rust/telesthitium/`](rust/telesthitium/) | Relay/signaling server (in progress) |

Both reference implementations are verified **byte-for-byte against shared
conformance vectors** ([`tests/vectors.json`](tests/vectors.json)) so they
interoperate by construction.

## Quick start (Python)

```bash
pip install git+https://github.com/Bake-Ware/telesthete.git   # not yet on PyPI
```

```python
import asyncio
from telesthete import Band

async def main():
    band = Band(psk="my-secret-key", hostname="machine1")
    await band.start()
    band.connect_peer("192.168.1.10", 9999)

    stream = band.stream(stream_id=1, priority=0)        # real-time, lossy
    stream.on_receive(lambda data, peer, ts: print("recv", data))
    stream.send(b"hello!")

    await asyncio.sleep(60)
    await band.stop()

asyncio.run(main())
```

To prefer hardware-accelerated AES where both peers support it:

```python
band = Band(psk="...", ciphers=["aes256-gcm", "chacha20-poly1305"])
```

## Protocol at a glance

- **Encryption** — PSK-derived keys; AEAD is a **negotiated suite**:
  ChaCha20-Poly1305 (IETF) is the mandatory baseline, AES-256-GCM is optional.
  The hub relays opaque ciphertext and cannot decrypt.
- **Channels** — `Stream` (fire-and-forget, prioritized, lossy — HID, audio,
  video), `Channel` (reliable, ordered, TCP-like — files, clipboard), and
  `Control` (HELLO/keepalive/negotiation). `Board` and `Drop` are reserved.
- **Capability negotiation** — every HELLO advertises capabilities + an ordered
  cipher preference list; peers select end-to-end (SPEC §3.5, §12.5).
- **Transports** — UDP (LAN), AF_UNIX (same-host, with dmabuf fd passing),
  WebTransport (preferred for browsers), WebSocket (legacy fallback).
- **Replay protection** — Control and Stream reject sequences at or below a
  per-peer high-water mark (SPEC §3.3).

## Testing

```bash
# Python conformance + protocol tests
python tests/test_vectors.py        # crypto vectors (chacha + aes)
python tests/test_negotiation.py    # live cipher handshake
python tests/test_replay.py
python tests/test_fragment.py

# Rust (includes the same cross-impl vectors)
cd rust && cargo test
```

## Documentation

- [`SPEC.md`](SPEC.md) — the authoritative wire protocol specification (v1.2)
- [`PROTOCOL_STATUS.md`](PROTOCOL_STATUS.md) — implementation status

## License

MIT — see [`LICENSE`](LICENSE). Free for everyone, everywhere, forever.

## Credits

Inspired by Magic Wormhole, QUIC, and libp2p.
