# Telesthete

**Lightweight P2P communication library for Python**

Telesthete provides encrypted, multiplexed, prioritized transport over UDP with automatic peer discovery.

## Installation

```bash
# Install from GitHub
pip install git+https://github.com/Bake-Ware/telesthete.git

# Or clone and install locally
git clone https://github.com/Bake-Ware/telesthete.git
cd telesthete
pip install -e .
```

**Note:** Not yet published to PyPI. Install from GitHub for now.

## Quick Start

```python
import asyncio
from telesthete import Band

async def main():
    # Create a Band (encrypted P2P context)
    band = Band(psk="my-secret-key", hostname="machine1")
    await band.start()
    
    # Connect to a peer
    band.connect_peer("192.168.1.10", 9999)
    
    # Open a stream (real-time, lossy)
    stream = band.stream(stream_id=1, priority=0)
    stream.on_receive(lambda data, peer, ts: print(f"Received: {data}"))
    stream.send(b"Hello!")
    
    await asyncio.sleep(60)
    await band.stop()

asyncio.run(main())
```

## Features

- **Encrypted by default** - PSK-based XChaCha20-Poly1305 AEAD
- **Multiple channel types** - Lossy streams (real-time) and reliable channels (TCP-like)
- **LAN discovery** - Automatic peer finding via UDP broadcast
- **Priority support** - High-priority traffic never waits
- **No admin required** - Pure userspace, no special permissions
- **Cross-platform** - Windows and Linux

## Use Cases

- **KVM over IP** - Share keyboard/mouse across machines ([telesthete-kvm](https://github.com/Bake-Ware/telesthete-kvm))
- **File synchronization** - Real-time file sync between peers
- **Distributed systems** - Agent communication, state replication
- **Real-time collaboration** - Shared whiteboards, cursors, etc.

## Architecture

**Stream** - Fire-and-forget datagrams (real-time, lossy)
- Priority support (0=highest)
- Drops old packets automatically
- Use for: HID events, real-time state, voice

**Channel** - Reliable, ordered byte streams (coming soon)
- TCP-like semantics over UDP
- Flow control and retransmission
- Use for: Files, clipboard, structured data

**Control** - Internal management
- Peer discovery (HELLO/HELLO_ACK)
- Keepalive (timeout detection)
- State synchronization

## Advanced Usage

### With Discovery

```python
from telesthete import Band
from telesthete.transport.discovery import Discovery

band = Band(psk="secret", hostname="laptop")

def on_peer_found(hostname, ip, port):
    print(f"Found {hostname}")
    band.connect_peer(ip, port)

disc = Discovery("laptop", 9999, on_peer_found)
disc.start()

await band.start()
await disc.run()
```

## Testing

```bash
# Run protocol tests
python tests/test_full_stack.py

# Expected output:
# [PASS] Alpha discovered Beta
# [PASS] Beta discovered Alpha
# [PASS] Alpha received 5/5 stream messages
# [PASS] Beta received 5/5 stream messages
```

## Applications Built on Telesthete

- [telesthete-kvm](https://github.com/Bake-Ware/telesthete-kvm) - Software KVM (keyboard/video/mouse over IP)

## Documentation

- [Protocol Status](PROTOCOL_STATUS.md) - Implementation details
- More docs coming soon

## Status

**Alpha** - Core functionality stable, API may change

- [x] Core protocol (crypto, framing, streams)
- [x] UDP transport  
- [x] LAN discovery
- [x] Control channel
- [x] Reliable channels
- [ ] Relay client (for internet use)
- [ ] WebSocket fallback
- [ ] PyPI publication

## License

MIT License

## Credits

Inspired by Magic Wormhole, QUIC, and libp2p. Built with PyNaCl.

---

**Telesthete** (noun): *One who perceives at a distance*
