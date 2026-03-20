# Telesthete

**Lightweight P2P communication library for Python**

Telesthete provides encrypted, multiplexed, prioritized transport over UDP with automatic peer discovery.

## Installation

```bash
pip install telesthete
```

## Quick Start

```python
import asyncio
from telesthete import Band

async def main():
    band = Band(psk="my-secret-key", hostname="machine1")
    await band.start()
    
    band.connect_peer("192.168.1.10", 9999)
    
    stream = band.stream(stream_id=1, priority=0)
    stream.on_receive(lambda data, peer, ts: print(f"Received: {data}"))
    stream.send(b"Hello!")
    
    await asyncio.sleep(60)
    await band.stop()

asyncio.run(main())
```

## Features

- Encrypted by default (PSK-based XChaCha20-Poly1305)
- Multiple channel types (lossy streams, reliable channels)
- LAN discovery via UDP broadcast
- Priority support
- No admin required

## Applications

- [telesthete-kvm](https://github.com/Bake-Ware/telesthete-kvm) - Software KVM over IP

## License

MIT
