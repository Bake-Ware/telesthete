# Telesthete Wire Protocol Specification v1.1

Telesthete is a lightweight, encrypted, peer-to-peer transport library.
This document defines the byte-level wire format for all communication.

**Design principles:** Binary wire format. UDP primary, AF_UNIX for
same-host peers, WebSocket fallback. PSK-based encryption. No
timestamps from peers — receiver stamps on arrival. Simple enough to
implement a minimal client in ~200 LOC in any language.

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
**Maximum packet size:** Bounded by UDP MTU (~1472 bytes on Ethernet), AF_UNIX kernel buffer (megabytes), or WebSocket frame limits.

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

### 3.4 Local crypto profile (v1.1)

The frame format and AEAD construction are unchanged. For local trust
domains (single host, AF_UNIX), peers MAY use a fixed PSK of
`"telesthete-local"` (or empty string `""`) so all local processes
derive the same `band_id` and `key`.

The point is to keep one code path, not to imply security: any local
process with the same fixed PSK can spoof. The kernel's filesystem
permissions on the AF_UNIX socket path (§9.4) are the real access
control.

XChaCha20-Poly1305 of a 32-byte descriptor is ≤1 µs on a Cortex-A53.
Carving a separate "no-crypto" lane is not justified by profiling.

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
{"type": 1, "payload": {"hostname": "machine-name", "capabilities": ["..."]}}
```

The `capabilities` field is **optional** (added in v1.1). When absent,
the peer is treated as v1.0. See §12.5.

### 4.4 HELLO_ACK (0x02)

```json
{"type": 2, "payload": {"hostname": "responder-name", "capabilities": ["..."]}}
```

`capabilities` field semantics match §4.3.

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

### 5.1 Plaintext Payload — v1.0 (default)

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

### 5.4 StreamHeader payload format (v1.1)

When a sender advertises capability `dmabuf-v1` (§12.5), Stream
payloads MAY use an extended layout that begins with an 8-byte
`StreamHeader` in place of the §5.1 priority byte. The new layout
allows transmission of GPU buffer descriptors and explicit frame
boundaries.

```
Offset  Size  Field
0       1 B   flags        bitfield (StreamFlags, see §5.4.1)
1       3 B   reserved     must be zero
4       4 B   frame_id     uint32 BE, monotonic per logical frame window
8       var   data         flag-dependent (NAL bytes, dmabuf descriptor, etc.)
```

A receiver determines which layout is in use by checking whether the
sending peer advertised `dmabuf-v1`. v1.0 peers and v1.1 peers without
the capability continue to use §5.1.

Priority handling under the v1.1 layout: per-stream priority is
configured out-of-band (e.g. METACONTROL or per-stream API agreement)
rather than per-packet. Implementations that need wire-level priority
SHOULD continue using §5.1.

#### 5.4.1 StreamFlags bitfield

```
Bit 0  INIT          payload is codec init data (e.g. SPS+PPS, Annex-B framed)
Bit 1  KEYFRAME      payload is a keyframe / IDR slice
Bit 2  END_FRAME     last packet of this `frame_id`
Bit 3  FRAGMENT_CONT continuation of the previous packet's payload
                     (same `frame_id`); receiver appends bytes without
                     prepending a fresh codec start code
Bit 4  DMABUF        payload is a dmabuf descriptor (§5.4.2), not codec bytes
Bit 5  WITH_FENCE    ancillary fd list ends with a sync_file release fence
Bit 6  REUSE         producer hint: same dmabuf as previous frame
Bit 7  EXTENDED      reserved; parsers that don't understand the
                     extension MUST drop the packet
```

Receivers MUST reject packets with `DMABUF`, `WITH_FENCE`, or `REUSE`
set when they have not advertised the corresponding capability.

#### 5.4.2 dmabuf descriptor (DMABUF flag set)

When `flags & DMABUF`, the Stream payload after the 8-byte
StreamHeader is a fixed-layout descriptor:

```
Offset  Size  Field
0       4 B   width            uint32 BE
4       4 B   height           uint32 BE
8       4 B   fourcc           uint32 LE   (DRM convention; e.g. 'XR24' = 0x34325258)
12      8 B   modifier         uint64 BE   (DRM modifier; LINEAR=0, INVALID=0x00FFFFFFFFFFFFFF)
20      1 B   plane_count      uint8       (1..4)
21      1 B   fd_count         uint8       (1..4 plus optional fence)
22      var   plane[i]:        9 B each
              offset           uint32 BE
              stride           uint32 BE
              fd_index         uint8       (0..fd_count-1)
```

Total payload size (including the StreamHeader): `8 + 22 + 9 *
plane_count` bytes. Multi-plane formats (NV12, YUV420) may share a
single fd via differing `offset`/`stride` values.

The actual `fd_count` file descriptors arrive in a single
`SCM_RIGHTS` ancillary message on the same `recvmsg`. They MUST be
in the same order as `fd_index` references them. Only valid on
AF_UNIX transport (§9.4); UDP cannot carry fds.

If `flags & WITH_FENCE`, the **last** fd (index `fd_count - 1`) is
a `sync_file` for read-after-write GPU synchronization. The
consumer MUST wait on that fence before sampling the dmabuf. This
fd is **not** a plane; planes only reference fd_index ∈
`[0, fd_count - 2]` in this case.

If `flags & REUSE`, `fd_count` MUST be 0 (or 1 if WITH_FENCE is
also set, carrying only the new fence). The consumer is required
to have cached the dmabuf import keyed on the descriptor; the
producer is asserting the buffer contents have been updated in
place but the underlying GEM object is the same.

Receiver-side caching: import the dmabuf as a `VkImage` via
`VK_EXT_external_memory_fd` + `VK_EXT_image_drm_format_modifier`,
and cache the import keyed on `fstat(fd).st_dev` and `st_ino`. This
is the wlroots model; reused buffers cost a `fstat` per frame, not
a re-import.

`fourcc` is little-endian by DRM tradition (the four ASCII bytes
'X', 'R', '2', '4' are stored as `0x34325258` so `read32_le` yields
'XR24'). Spec the field as LE explicitly to avoid a foot-gun.

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

Default transport for cross-host peers. One socket per peer, all channel types multiplexed.

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

### 9.4 AF_UNIX (v1.1)

Default for same-host Stream/Control traffic. Same frame format (§1),
same AEAD (§3, with the local profile of §3.4).

- **Socket type:** `SOCK_SEQPACKET` (preserves message boundaries,
  ordered, no fragmentation under load). `SOCK_DGRAM` permitted but
  not recommended; ordering is not guaranteed when the kernel receive
  buffer fills.
- **Address:** filesystem path
  `$XDG_RUNTIME_DIR/telesthete/<band_id_hex>.sock`. The server binds;
  clients connect. Permissions on the directory are the primary
  access control.
- **Discovery:** §9.2 LAN broadcast does not apply. Local clients
  enumerate `$XDG_RUNTIME_DIR/telesthete/` and connect by name.
- **Datagrams:** a single AF_UNIX message carries one Telesthete
  packet. No 1472-byte MTU; size is bounded only by the kernel
  socket buffer (megabytes, tunable via `net.unix.max_dgram_qlen`
  and `SO_SNDBUF`).
- **Reception:** peers MUST use `recvmsg` (not `recvfrom`) so that
  `SCM_RIGHTS` ancillary messages can arrive with a Stream packet.
  Allocate a control buffer of at least
  `CMSG_SPACE(sizeof(int) * MAX_FDS_PER_PACKET)` where
  `MAX_FDS_PER_PACKET = 5` (4 planes + 1 fence).
- **Cleanup:** leaked dmabuf fds in dropped packets are a real risk.
  Receivers SHOULD wrap incoming fds in an `OwnedFd` (or
  language-equivalent) on parse so dropped messages close them.

### 9.5 Send-loop priority across transports (v1.1)

When a peer maintains both a UDP socket and an AF_UNIX socket (e.g.
local cockpit + remote source), the send loop services AF_UNIX peers
first within each priority class. Rationale: local peers have lower
steady-state queue depth and tighter latency budgets; favouring them
does not starve UDP because UDP traffic is already gated by network
MTU and ACK pacing.

This is a SHOULD, not a MUST. Implementations that maintain only one
transport at a time can ignore it.

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
│ Application (Rook, KVM, GPU surface stream)  │
├──────────────────────────────────────────────┤
│ Telesthete Channel / Stream                  │
│ (reliability, ordering, priority)            │
├──────────────────────────────────────────────┤
│ Telesthete Framing + Crypto                  │
│ (27B header, XChaCha20-Poly1305)             │
├──────────────────────────────────────────────┤
│ UDP / AF_UNIX / WebSocket                    │
└──────────────────────────────────────────────┘
```

### 11.1 Rook Protocol

Rook's agent protocol uses msgpack-encoded messages inside Telesthete payloads.
Reliable messages (commands, events, chat, registration) use Channels.
Real-time modality data (audio, video, sensor feeds) uses Streams.

See the Rook repository for the full Rook application protocol spec.

---

## 12. Cross-Language Implementation Guide

The wire format is the spec. The Python library is the original
reference implementation; the Rust crate under `rust/telesthete` is
the v1.1 reference implementation (adds AF_UNIX, dmabuf, capability
negotiation). Any language can implement a conformant peer.

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
| Rust (ref v1.1) | UDP + AF_UNIX + tokio | ~3000 | Full library, dmabuf, fds    |
| Kotlin/Android  | OkHttp WebSocket   | ~500     | Stream + Control only        |
| ESP32/C         | WebSocket          | ~300     | Stream + Control only        |
| Browser/JS      | Native WebSocket   | ~200     | Stream + Control only        |

### 12.3 Byte Order

All multi-byte integers are **big-endian** (network byte order), with
one explicit exception: dmabuf descriptor `fourcc` (§5.4.2) is
little-endian to match DRM tooling.

### 12.4 Constants

```
HEADER_SIZE        = 27
AUTH_TAG_SIZE      = 16
MIN_PACKET_SIZE    = 43      (header + tag)
MAGIC_DISCOVERY    = 0x54454C45
PROTOCOL_VERSION   = 2       (was 1 in v1.0)
KEEPALIVE_INTERVAL = 5       (seconds)
PEER_TIMEOUT       = 15      (seconds, 3x keepalive)
DEFAULT_WINDOW     = 32      (packets)
DEFAULT_RTO        = 500     (milliseconds)
MAX_CHANNEL_DATA   = 1024    (bytes per Channel packet)
MAX_FDS_PER_PACKET = 5       (v1.1; 4 planes + 1 fence)
LOCAL_PSK          = "telesthete-local"   (v1.1 sentinel for §3.4)
STREAM_HEADER_LEN  = 8       (v1.1 §5.4)
```

A v1.1 peer detecting a v1.0 peer (no `capabilities` in HELLO) MUST
restrict itself to v1.0 behaviour for that peer. Mixed Bands work
fine; the constraint is per-peer, not per-Band.

### 12.5 Capability negotiation (v1.1)

`HELLO` and `HELLO_ACK` payloads (§4.3, §4.4) carry an optional
`capabilities` field — an array of strings. Defined strings:

| Capability   | Meaning                                                         |
|--------------|-----------------------------------------------------------------|
| `dmabuf-v1`  | Peer can produce and/or consume Stream packets with `DMABUF`.   |
| `af-unix`    | Peer reachable via AF_UNIX (§9.4).                              |
| `sync-file`  | Peer honors `WITH_FENCE`. Implies `dmabuf-v1`.                  |
| `reuse-v1`   | Peer honors `REUSE`. Implies `dmabuf-v1`.                       |

Absent capability = not supported. Senders MUST fall back to v1.0
behaviour (§5.1 Stream payload, UDP) if a peer omits the capability
they need.

Capability strings are forward-extensible. Unknown capabilities are
ignored.

---

## 13. Compatibility Matrix

|                        | v1.0 peer            | v1.1 peer (no caps)     | v1.1 peer (full caps) |
|------------------------|----------------------|-------------------------|-----------------------|
| Frame parse            | OK                   | OK                      | OK                    |
| §5.1 Stream payload    | OK                   | OK                      | OK                    |
| §5.4 StreamHeader      | reject               | reject (no `dmabuf-v1`) | OK                    |
| `DMABUF` flag          | reject (unknown bit) | reject (no cap)         | OK                    |
| AF_UNIX                | n/a (UDP only)       | UDP only                | OK                    |
| `WITH_FENCE`           | reject               | reject                  | OK                    |

A v1.0 peer never sees a v1.1-only flag because the v1.1 peer checks
capabilities before sending.

---

## 14. Dependencies

Reference implementations:
- **Python** (`telesthete/`) — PyNaCl (XChaCha20-Poly1305), Python 3.10+
- **Rust** (`rust/telesthete/`) — `chacha20poly1305`, `hkdf`, `sha2`,
  `nix` (AF_UNIX + SCM_RIGHTS), `tokio`

Cross-language: any libsodium binding (C, Rust, Go, JS, Java,
Kotlin, Swift, etc.).

---

## 15. Open questions (v1.1)

1. Should `fourcc` be BE or LE? DRM uses LE-as-bytes-of-ASCII; the
   rest of the wire is BE. Inconsistent but matches DRM tooling.
   Resolved as LE; called out in §5.4.2 and §12.3.
2. Is `REUSE` worth a flag? `fstat(fd).st_ino` caching catches it
   at zero wire cost. Kept; remove in v1.2 if profiling shows the
   dup+fstat is irrelevant.
3. Is `WITH_FENCE` mandatory for correctness, or do we mandate
   double-buffering and skip the sync_file? For wlroots-style
   compositors a fence-per-frame is normal; for direct render
   targets it may be skipped. Optional in v1.1.
4. `SOCK_SEQPACKET` vs `SOCK_DGRAM` — SEQPACKET is correct but less
   universally supported in async UDS APIs. Implementations may
   start with `SOCK_DGRAM` and switch later.
5. Multi-process security on the AF_UNIX socket: do we want
   `SO_PEERCRED` checks on connect? Likely yes for any
   "untrusted-producer" scenario; out of scope for v1.1 if
   everything runs as the same UID.

---

## 16. Changelog

| Version | Date       | Changes                                                                                                                                                                                                                |
|---------|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1.0     | 2026-04-07 | Initial spec from Python reference implementation.                                                                                                                                                                     |
| 1.1     | 2026-05-11 | Added §3.4 local crypto profile, §5.4 StreamHeader + dmabuf descriptor (capability-gated), §9.4 AF_UNIX transport, §9.5 send-loop priority across transports, §12.5 capability negotiation. `PROTOCOL_VERSION` → 2. Backwards-compatible: v1.0 peers continue to interoperate. |
