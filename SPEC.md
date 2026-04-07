# Telesthete Wire Protocol Specification v1.0

Telesthete is a lightweight, encrypted, peer-to-peer transport library.
This document defines the byte-level wire format for all communication.

**Design principles:** Binary wire format. UDP primary, WebSocket fallback.
PSK-based encryption. No timestamps from peers — receiver stamps on arrival.
Simple enough to implement a minimal client in ~200 LOC in any language.

---

## 1. Frame Format

Every Telesthete packet on the wire has the same outer frame:

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                                                               |
|                         band_id (128 bits)                    |
|                                                               |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| channel_type  |        channel_id             |               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+               +
|                                                               |
|                      sequence (64 bits)                       |
|                         +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                         |                                     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+                                     +
|                                                               |
|                    ciphertext (variable)                      |
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

| Offset | Size    | Field          | Encoding       | Description                          |
|--------|---------|----------------|----------------|--------------------------------------|
| 0      | 16 B    | `band_id`      | raw bytes      | Cleartext. Relay routing identifier. |
| 16     | 1 B     | `channel_type` | uint8          | Cleartext. Multiplexing selector.    |
| 17     | 2 B     | `channel_id`   | uint16 BE      | Cleartext. Sub-channel identifier.   |
| 19     | 8 B     | `sequence`     | uint64 BE      | Cleartext. Monotonic per-sender. Also used as AEAD nonce. |
| 27     | var     | `ciphertext`   | XChaCha20-Poly1305 output | Encrypted payload + 16-byte auth tag. |

**Header size:** 27 bytes fixed.
**Minimum packet size:** 27 (header) + 16 (auth tag) + 0 (empty payload) = 43 bytes.
**Maximum packet size:** Bounded by UDP MTU (~1472 bytes on Ethernet) or WebSocket frame limits.

### 1.1 Struct Pack Format

```
Big-endian: "!16s B H Q"
  16s = band_id (16 bytes)
  B   = channel_type (uint8)
  H   = channel_id (uint16)
  Q   = sequence (uint64)
```

---

## 2. Channel Types

| Value | Name      | Semantics                                | Phase |
|-------|-----------|------------------------------------------|-------|
| 0x00  | Control   | Band management, signaling. Always reliable. | 1   |
| 0x01  | Stream    | Real-time, lossy, prioritized datagrams. | 1     |
| 0x02  | Channel   | Reliable, ordered byte streams (TCP-over-UDP). | 1  |
| 0x03  | Board     | Replicated state / distributed log.      | Future |
| 0x04  | Drop      | Chunked, resumable file distribution.    | Future |

---

## 3. Encryption

### 3.1 Key Material

All derived from a single Pre-Shared Key (PSK) string.

```
band_id = SHA256(PSK)[:16]                          # 16 bytes, cleartext routing
key     = HKDF-SHA256(salt="telesthete-v1",          # 32 bytes, encryption key
                       ikm=PSK,
                       info="encryption")
```

Both peers with the same PSK derive identical `band_id` and `key`.
The relay (Telesthetium) sees `band_id` for routing but never possesses
the PSK and cannot decrypt traffic.

### 3.2 AEAD Construction

**Algorithm:** XChaCha20-Poly1305

**Nonce:** 24 bytes. Constructed from the 64-bit `sequence` number,
zero-padded on the left:

```
nonce = 0x0000000000000000 || 0x0000000000000000 || sequence(8 bytes BE)
        ^^^^^^^^^^^^^^^^     ^^^^^^^^^^^^^^^^     ^^^^^^^^^^^^^^^^^^
        8 bytes zero         8 bytes zero         8 bytes sequence
```

**Associated Authenticated Data (AAD):** 3 bytes, not encrypted but
authenticated:

```
AAD = [channel_type (1B)] [channel_id high byte] [channel_id low byte]
```

**Ciphertext output:** Encrypted plaintext + 16-byte Poly1305 auth tag.

### 3.3 Security Notes

- Sequence numbers MUST be monotonically increasing per sender per Band.
  Reuse of a sequence number with the same key breaks XChaCha20-Poly1305 security.
- The 64-bit sequence space (2^64 packets) is effectively inexhaustible.
- Receivers SHOULD reject packets with sequence numbers at or below
  their high-water mark to prevent replay.

---

## 4. Control Channel (type 0x00)

**channel_id:** Always 0.

### 4.1 Plaintext Payload (inside ciphertext)

JSON-encoded UTF-8 string:

```json
{
  "type": <uint8>,
  "payload": { ... }
}
```

### 4.2 Control Message Types

| Value | Name          | Direction   | Description                          |
|-------|---------------|-------------|--------------------------------------|
| 0x01  | HELLO         | peer→peer   | Initial introduction                 |
| 0x02  | HELLO_ACK     | peer→peer   | Acknowledgment of HELLO              |
| 0x03  | KEEPALIVE     | peer→peer   | Heartbeat (broadcast to all peers)   |
| 0x04  | FOCUS_CHANGE  | peer→peer   | Focus state change (KVM application) |
| 0x05  | METACONTROL   | peer→peer   | Settings/config sync                 |
| 0x06  | GOODBYE       | peer→peer   | Graceful disconnect                  |

### 4.3 HELLO (0x01)

```json
{"type": 1, "payload": {"hostname": "machine-name"}}
```

### 4.4 HELLO_ACK (0x02)

```json
{"type": 2, "payload": {"hostname": "responder-name"}}
```

### 4.5 KEEPALIVE (0x03)

```json
{"type": 3, "payload": {}}
```

Sent every 5 seconds. Peer is considered dead after 3 missed intervals (15s).

### 4.6 FOCUS_CHANGE (0x04)

```json
{"type": 4, "payload": {"focused_peer": "hostname-or-null"}}
```

### 4.7 METACONTROL (0x05)

```json
{"type": 5, "payload": {"key": "value", ...}}
```

Freeform settings dictionary. Application-defined.

### 4.8 GOODBYE (0x06)

```json
{"type": 6, "payload": {}}
```

---

## 5. Stream Channel (type 0x01)

Real-time, lossy, prioritized datagrams. Fire-and-forget.
No retransmission. Late packets dropped via high-water mark.

**channel_id:** Caller-chosen stream identifier (0-65535).

### 5.1 Plaintext Payload (inside ciphertext)

```
Offset  Size  Field
0       1 B   priority          uint8, 0=highest, 255=lowest
1       var   data              application payload
```

### 5.2 Receive Behavior

Receiver tracks the highest sequence number seen per (peer, stream_id).
Packets with `sequence <= watermark` are silently dropped.
This ensures only the freshest data is delivered.

### 5.3 Multiplexing Priority

When multiple Streams are active, the send loop services them in
priority order (lowest priority value first). This ensures latency-
critical streams (e.g., HID at priority 0) are never starved by
bulk streams.

---

## 6. Channel (type 0x02)

Reliable, ordered byte streams with flow control. TCP semantics
implemented in userspace over UDP.

**channel_id:** Negotiated between peers (0-65535).

### 6.1 Plaintext Payload (inside ciphertext)

```
Offset  Size  Field
0       1 B   flags             bitfield (see below)
1       8 B   ack_num           uint64 BE, highest in-order seq received
9       2 B   window            uint16 BE, receiver's available window (packets)
11      var   data              application payload (may be empty for pure ACKs)
```

### 6.2 Flags

```
Bit 0 (0x01): SYN   — Open connection
Bit 1 (0x02): FIN   — Close connection
Bit 2 (0x04): ACK   — Acknowledgment
Bit 3 (0x08): RST   — Reset connection
Bits 4-7:     Reserved (must be 0)
```

### 6.3 Connection Lifecycle

```
Initiator                           Responder
    |                                   |
    |--- SYN (seq=0) ----------------->|
    |                                   |
    |<-- SYN+ACK (seq=0, ack=1) -------|
    |                                   |
    |--- ACK (ack=1) ----------------->|
    |                                   |
    |        ESTABLISHED                |
    |                                   |
    |--- DATA (seq=1) ---------------->|
    |<-- ACK (ack=2) ------------------|
    |                                   |
    |--- FIN ------------------------->|
    |<-- ACK + FIN --------------------|
    |                                   |
    |        CLOSED                     |
```

### 6.4 Reliability

- Sliding window flow control (default window: 32 packets).
- Out-of-order packets buffered and reordered.
- Unacknowledged packets retransmitted after RTO (default: 500ms).
- Maximum packet payload: 1024 bytes (fragments larger sends).

### 6.5 States

```
CLOSED → SYN_SENT → ESTABLISHED → FIN_SENT → CLOSED
                ↑                       ↓
                └── (incoming SYN) ─────┘
```

---

## 7. Board (type 0x03) — Future

Replicated state across all peers in a Band. Append-only distributed log.
Design space reserved. Not yet implemented.

---

## 8. Drop (type 0x04) — Future

Chunked, resumable file distribution. Design space reserved.
Not yet implemented. MVP uses Channels for file transfer.

---

## 9. Transport Layer

### 9.1 UDP (Primary)

Default transport. One socket per peer, all channel types multiplexed.

**Send loop priority order:**
1. Control — always first
2. Stream — ordered by stream priority field
3. Channel — fair-queued across open channels
4. Board — lowest data priority
5. Drop — yields to everything

### 9.2 LAN Discovery

UDP broadcast. Packet format:

```
Offset  Size  Field
0       4 B   magic             0x54454C45 ("TELE")
4       1 B   version           protocol version (currently 1)
5       var   hostname          UTF-8 string, null-terminated
var     2 B   port              uint16 BE, listening port
```

Broadcast interval: 5 seconds. Duplicate detection by (hostname, ip, port).

### 9.3 WebSocket (Fallback) — Future

For hostile networks (corporate firewalls, symmetric NAT). Connect outbound
to Telesthetium hub over WSS (port 443). Same frame format as UDP, carried
as WebSocket binary frames. Not yet implemented.

---

## 10. Telesthetium Hub — Future

Self-hosted relay/signaling server. Peers connect outbound, identify by
`band_id`. Hub matches peers in the same Band and bridges traffic.
Hub sees only `band_id` + opaque ciphertext. Cannot decrypt.

Connection modes:
- **LAN:** Direct UDP, no hub needed.
- **Tunnel:** Both peers connect to hub, traffic relayed.
- **Hybrid:** Try LAN first, fall back to hub, prefer direct.

---

## 11. Application Protocol Layering

Telesthete is a transport primitive. Applications ride inside the `data`
field of Channel and Stream payloads.

```
┌──────────────────────────────────────────────┐
│ Application (Rook, KVM, custom)              │
├──────────────────────────────────────────────┤
│ Telesthete Channel / Stream                  │
│ (reliability, ordering, priority)            │
├──────────────────────────────────────────────┤
│ Telesthete Framing + Crypto                  │
│ (27B header, XChaCha20-Poly1305)             │
├──────────────────────────────────────────────┤
│ UDP / WebSocket                              │
└──────────────────────────────────────────────┘
```

### 11.1 Rook Protocol

Rook's agent protocol uses msgpack-encoded messages inside Telesthete payloads.
Reliable messages (commands, events, chat, registration) use Channels.
Real-time modality data (audio, video, sensor feeds) uses Streams.

See the Rook repository for the full Rook application protocol spec.

---

## 12. Cross-Language Implementation Guide

The wire format is the spec. The Python library is the reference
implementation, but any language can implement a conformant peer.

### 12.1 Minimal Client Requirements

A minimal client needs:
1. Pack/unpack the 27-byte frame header
2. XChaCha20-Poly1305 encrypt/decrypt (libsodium available everywhere)
3. Derive band_id and key from PSK (SHA256 + HKDF)
4. WebSocket transport (if not on LAN)

That's it. No need to implement Channel reliability, Board replication,
or the full multiplexer. A WebSocket-only client that speaks Streams
and Control is a first-class peer.

### 12.2 Implementation Estimates

| Platform        | Transport          | Est. LOC | Notes                        |
|-----------------|--------------------|----------|------------------------------|
| Python (ref)    | UDP + asyncio      | ~2000    | Full library                 |
| Rust            | UDP + tokio        | ~1500    | Full library                 |
| Kotlin/Android  | OkHttp WebSocket   | ~500     | Stream + Control only        |
| ESP32/C         | WebSocket          | ~300     | Stream + Control only        |
| Browser/JS      | Native WebSocket   | ~200     | Stream + Control only        |

### 12.3 Byte Order

All multi-byte integers are **big-endian** (network byte order).

### 12.4 Constants

```
HEADER_SIZE        = 27
AUTH_TAG_SIZE      = 16
MIN_PACKET_SIZE    = 43      (header + tag)
MAGIC_DISCOVERY    = 0x54454C45
PROTOCOL_VERSION   = 1
KEEPALIVE_INTERVAL = 5       (seconds)
PEER_TIMEOUT       = 15      (seconds, 3x keepalive)
DEFAULT_WINDOW     = 32      (packets)
DEFAULT_RTO        = 500     (milliseconds)
MAX_CHANNEL_DATA   = 1024    (bytes per Channel packet)
```

---

## 13. Dependencies

Reference implementation (Python):
- **PyNaCl** — XChaCha20-Poly1305 via libsodium
- Python 3.10+
- No other dependencies (SQLite philosophy)

Cross-language:
- Any libsodium binding (available for C, Rust, Go, JS, Java, Kotlin, Swift, etc.)

---

## 14. Changelog

| Version | Date       | Changes                                          |
|---------|------------|--------------------------------------------------|
| 1.0     | 2026-04-07 | Initial spec from reference implementation.      |
