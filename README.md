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

## Installing a prebuilt relay

Every `v*` tag publishes static (musl) `telesthete-hub` binaries for Linux on
the [Releases](https://github.com/Bake-Ware/telesthete/releases) page. No Rust
toolchain needed:

| `uname -m` | Asset |
|------------|-------|
| `x86_64` | `telesthete-hub-x86_64-linux.tar.gz` |
| `aarch64` (Raspberry Pi 4/5 on a 64-bit OS, ARM servers) | `telesthete-hub-aarch64-linux.tar.gz` |
| `armv7l` (32-bit Raspberry Pi OS; best effort) | `telesthete-hub-armv7-linux.tar.gz` |

Each archive has a `.sha256` beside it (plus one `SHA256SUMS` per release) and
holds `telesthete-hub`, `LICENSE` and a `VERSION` file. Each archive also has a
build provenance attestation: `gh attestation verify <archive> -R Bake-Ware/telesthete`
checks it was built by this repository's release workflow.

```bash
arch=$(uname -m); [ "$arch" = armv7l ] && arch=armv7
base=https://github.com/Bake-Ware/telesthete/releases/latest/download   # or .../download/v0.3.0
curl -fsSLO "$base/telesthete-hub-$arch-linux.tar.gz"
curl -fsSLO "$base/telesthete-hub-$arch-linux.tar.gz.sha256"
sha256sum -c "telesthete-hub-$arch-linux.tar.gz.sha256"
sudo tar -xzf "telesthete-hub-$arch-linux.tar.gz" -C /usr/local/bin telesthete-hub

HUB_BIND=0.0.0.0:7474 telesthete-hub     # UDP relay on 7474 (the default)
```

The relay is configured entirely by environment variables: `HUB_BIND`
(UDP, default `0.0.0.0:7474`, `off` to disable), `HUB_PEER_TTL_SECS` (default
15; raise it to 60 for Rook, whose peers keep alive every 20 s),
`HUB_PRUNE_SECS`, `HUB_WS_BIND`/`HUB_WS_TLS`, `HUB_WT_BIND`, `HUB_UNIX_DIR`,
`HUB_TLS_CERT`/`HUB_TLS_KEY`, and `RUST_LOG`. It holds no keys and cannot
decrypt what it relays. [Rook](https://github.com/Bake-Ware/rook)'s hub
installer and Docker image use these same assets.

To build it yourself instead: `cd rust && cargo build --release -p telesthitium`
(Rust 1.88+); the binary lands in `rust/target/release/telesthete-hub`.

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
- **Addressed hub routing (optional)** — a peer declares a 16-bit route id
  (`Route`, 0x05) and others send it `Addressed` (0x06) frames that the hub
  delivers only to the declarer; everything else is broadcast (SPEC §10.1).

## Changelog

### 0.3.0 (unreleased)

- **Addressed hub routing (SPEC §10.1).** New channel types `ROUTE` (0x05)
  and `ADDRESSED` (0x06); `telesthete.protocol.route.route_id_for`,
  `pack_route_declaration`, `pack_addressed`; Rust `route_id_for`,
  `ChannelType::{Route, Addressed}`. The hub routes only `ADDRESSED` frames
  by `channel_id`, only to validated declarers; `CHANNEL` frames are always
  broadcast, so library Channel connection ids are never treated as
  addresses.
- **Hub memory.** `HUB_UDP_PEER_QUEUE` defaults to 256 (was
  `HUB_CONN_QUEUE`, 1024); new hub-wide `HUB_UDP_QUEUE_BYTES` (32 MiB); UDP
  queues are created only for validated peers.
- **Behaviour change — `Reassembler` defaults.** `Reassembler()` now drops
  chunks declaring more than 16,384 chunks and non-final chunks over 1,003 B
  (`MAX_CHUNK_PAYLOAD`), and remembers evicted messages so their late chunks
  are dropped. Callers receiving from a `Fragmenter` with a larger
  `chunk_size`, or messages over ~16 MB, should build their reassembler with
  `Fragmenter.reassembler()` (Rust: `Reassembler::for_chunk_size`), which
  accepts everything that fragmenter can emit and bounds memory with a
  64 MiB byte budget. The library's own `Channel` and `Board` do this. The
  Rust `Reassembler` gains the same caps.

## Testing

```bash
# Python conformance + protocol tests
python tests/test_vectors.py        # crypto vectors (chacha + aes)
python tests/test_negotiation.py    # live cipher handshake
python tests/test_replay.py
python tests/test_fragment.py
python -m pytest -q tests/test_route.py   # addressed routing (§10.1)

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
