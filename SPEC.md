# Telesthete Wire Protocol Specification v1.2

Telesthete is a lightweight, encrypted, peer-to-peer transport library.
This document defines the byte-level wire format for all communication.

**Design principles:** Binary wire format. UDP primary, AF_UNIX for
same-host peers, WebTransport for browsers, WebSocket as the legacy
fallback. PSK-based encryption. No timestamps from peers — receiver
stamps on arrival. Simple enough to implement a minimal client in
~200 LOC in any language. The same 27-byte frame rides every transport
unchanged, so a peer is defined by the frame it speaks, not the socket
it speaks it on.

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
| 27     | var     | `ciphertext`   | AEAD suite output (§3.2) | Encrypted payload + 16-byte auth tag. |

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
key     = HKDF-SHA256(salt="telesthete-v1",          # 32 bytes, per-cipher key
                       ikm=PSK,
                       info="encryption-" + cipher_id)
```

`cipher_id` is the negotiated AEAD suite name (§3.2, e.g.
`"chacha20-poly1305"` or `"aes256-gcm"`). Binding key derivation to the
cipher means the ChaCha key and the AES key are distinct 32-byte values,
so a packet under one suite can never be confused with another on the
same band. `band_id` does **not** depend on the cipher — it is constant
per PSK, so the hub routes regardless of which suite a pair negotiates.

Both peers with the same PSK and the same negotiated cipher derive
identical `key`. The relay (Telesthetium) sees `band_id` for routing but
never possesses the PSK and cannot decrypt traffic.

### 3.2 AEAD Construction

**Algorithm:** a negotiated AEAD suite (§3.5). Every conformant peer
MUST implement the baseline; the rest are optional:

| `cipher_id`         | Algorithm                          | Status             |
|---------------------|------------------------------------|--------------------|
| `chacha20-poly1305` | ChaCha20-Poly1305 (IETF, RFC 8439) | MANDATORY baseline |
| `aes256-gcm`        | AES-256-GCM                        | OPTIONAL           |

All suites share the same nonce, AAD, and 16-byte tag, so they are
drop-in for one another — only the cipher core differs. ChaCha20-Poly1305
is the baseline because it is the universal *software* AEAD (constant-time
everywhere, no hardware dependency, embedded-friendly, an IETF standard).
`aes256-gcm` is offered for peers that prefer a hardware-accelerated
(AES-NI) or browser-native (WebCrypto) path. New suites may be added to
the registry without a wire change; they are reached by capability (§12.5).

**Nonce:** 12 bytes (96 bits — the size both suites use). Constructed
from the 64-bit `sequence` number, zero-padded on the left:

```
nonce = 0x00000000 || sequence(8 bytes BE)
        ^^^^^^^^^^     ^^^^^^^^^^^^^^^^^^
        4 bytes zero   8 bytes sequence
```

A 96-bit nonce holds the monotonic 64-bit sequence with room to spare and
never repeats, so no extended-nonce (XChaCha-style) construction is needed
— Telesthete never uses random nonces.

**Associated Authenticated Data (AAD):** 3 bytes, not encrypted but
authenticated. It is passed to the cipher's real AAD parameter, NOT
prepended to the plaintext:

```
AAD = [channel_type (1B)] [channel_id high byte] [channel_id low byte]
```

**Ciphertext output:** Encrypted plaintext + a 16-byte auth tag
(Poly1305 for ChaCha20-Poly1305, GMAC for AES-256-GCM).

### 3.3 Security Notes

- Sequence numbers MUST be monotonically increasing per sender per Band.
  Reuse of a sequence number with the same key is catastrophic for any
  AEAD (both ChaCha20-Poly1305 and AES-256-GCM).
- The 64-bit sequence space (2^64 packets) is effectively inexhaustible.
- Receivers **MUST** reject packets whose sequence number is at or below
  the per-(peer, channel_type, channel_id) high-water mark, and advance
  the mark on each accepted packet. This is replay protection; it is the
  same freshness mechanism Streams use (§5.2). *(SHOULD → MUST in v1.2.)*
- Cipher negotiation (§3.5) is downgrade-resistant: HELLO/HELLO_ACK are
  encrypted under the mandatory baseline key, which an attacker without
  the PSK cannot forge or tamper. Telesthete has no anonymous key
  exchange, so the classic TLS downgrade attacks do not apply.

### 3.4 Local crypto profile (v1.1)

The frame format and AEAD construction are unchanged. For local trust
domains (single host, AF_UNIX), peers MAY use a fixed PSK of
`"telesthete-local"` (or empty string `""`) so all local processes
derive the same `band_id` and `key`.

The point is to keep one code path, not to imply security: any local
process with the same fixed PSK can spoof. The kernel's filesystem
permissions on the AF_UNIX socket path (§9.4) are the real access
control.

ChaCha20-Poly1305 of a 32-byte descriptor is ≤1 µs on a Cortex-A53.
Carving a separate "no-crypto" lane is not justified by profiling.

### 3.5 Cipher negotiation (v1.2)

The AEAD suite is negotiated **end-to-end per peer-pair** via the
mandatory capability handshake (§4.3, §12.5). It is NOT a per-transport
or per-hop property: the hub relays opaque ciphertext between peers that
may be on different transports and never decrypts, so both endpoints of a
conversation must share one suite.

Rules:

1. **Bootstrap.** HELLO and HELLO_ACK are ALWAYS encrypted with the
   mandatory baseline (`chacha20-poly1305`), so first contact always
   succeeds.
2. **Advertise.** Each peer's HELLO/HELLO_ACK carries an ordered
   `ciphers` list (highest preference first). It MUST contain
   `chacha20-poly1305`. A peer orders the list by what is best on its
   platform/transport — a browser/WebCrypto or AES-NI peer puts
   `aes256-gcm` first; an embedded peer puts `chacha20-poly1305` first.
   This is how "AES on the browser side, ChaCha elsewhere" emerges
   without binding a cipher to a transport.
3. **Select.** The chosen suite is the first entry in the *initiator's*
   `ciphers` list that the *responder* also lists. The responder echoes
   the chosen `cipher_id` in HELLO_ACK (`cipher` field) to commit it. If
   there is no overlap beyond the baseline, the baseline is used — it is
   mandatory, so selection never fails.
4. **Apply.** Every packet after the handshake completes uses the
   selected suite and its derived key (§3.1). Selection is session state
   per (peer, band); there is **no cipher identifier on the wire**,
   keeping the frame header and minimal clients simple.

A minimal client MAY implement only the baseline: it advertises
`["chacha20-poly1305"]` and interoperates with everyone.

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
| 0x07  | KEYFRAME_REQ  | consumer→producer | Request a fresh INIT+IDR for a Stream (v1.2) |
| 0x08  | RATE_HINT     | consumer→producer | Bitrate / loss feedback for a lossy Stream (v1.2) |

Receivers MUST ignore Control messages with an unrecognized `type`.
This makes new message types (e.g. 0x07/0x08) forward-compatible: an
older peer silently drops them rather than erroring.

### 4.3 HELLO (0x01)

```json
{"type": 1, "payload": {"hostname": "machine-name",
                         "capabilities": ["af-unix", "webtransport"],
                         "ciphers": ["aes256-gcm", "chacha20-poly1305"]}}
```

`capabilities` and `ciphers` are **mandatory** as of v1.2 — capability
announce is foundational (§12.5). `ciphers` is an ordered preference list
and MUST contain the baseline `chacha20-poly1305` (§3.5). HELLO itself is
always encrypted with the baseline suite. A peer that omits these fields
is non-conformant.

### 4.4 HELLO_ACK (0x02)

```json
{"type": 2, "payload": {"hostname": "responder-name",
                         "capabilities": ["..."],
                         "ciphers": ["..."],
                         "cipher": "aes256-gcm"}}
```

`capabilities` and `ciphers` semantics match §4.3 (mandatory). The
responder additionally sets `cipher` to the single `cipher_id` it has
selected per §3.5, committing the suite both peers use for the rest of
the session.

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

### 4.9 KEYFRAME_REQ (0x07) — v1.2

```json
{"type": 7, "payload": {"stream_id": <uint16>}}
```

Sent by a Stream **consumer** to the **producer**. The producer SHOULD
emit a fresh `INIT` followed by a `KEYFRAME` (§5.4.1) for the named
`stream_id` at its next opportunity.

This is the recovery path for lossy video. Because Streams are
fire-and-forget (§5.2, watermark drop), a consumer that loses the
packets carrying a reference frame cannot decode subsequent deltas; it
requests a new IDR rather than stalling. This is the role WebRTC fills
with PLI/FIR. Carried on Control (reliable), so the request itself is
not subject to Stream drop.

Honored only by peers advertising the `keyframe-req` capability
(§12.5). A producer that does not advertise it MAY still emit keyframes
on its own cadence.

### 4.10 RATE_HINT (0x08) — v1.2

```json
{"type": 8, "payload": {"stream_id": <uint16>, "target_bps": <uint32>, "loss": <float 0..1>}}
```

Consumer→producer feedback for **application-level congestion control**
of a lossy Stream. `target_bps` is the consumer's suggested encoder
bitrate ceiling; `loss` is the observed fraction of dropped packets
since the last hint (0.0–1.0). The producer SHOULD adapt its encoder
accordingly. Advisory; a producer MAY clamp or ignore.

Rationale: reliable Channels inherit QUIC/Channel congestion control,
but lossy Streams (§5, and WebTransport datagrams §9.6) are never
retransmitted and have no built-in rate feedback. RATE_HINT is the
deliberate, minimal substitute — the application closes the loop the
transport intentionally leaves open. Honored only with the `rate-hint`
capability (§12.5).

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

### 5.5 Consuming codec Streams (decoder mapping) — v1.2

This subsection is informative. The §5.4.1 flags exist precisely to
drive a generic hardware video decoder, so the mapping is specified to
keep producers and consumers honest. The canonical browser consumer is
the WebCodecs `VideoDecoder`, but the same shape applies to any
push-style decoder (`avcodec`, VideoToolbox, MediaCodec).

A §5.4 Stream with the `DMABUF` flag **clear** carries a decoder-ready
elementary stream. Reassemble and submit per `frame_id`:

| StreamHeader state | Decoder action |
|--------------------|----------------|
| `INIT`             | Configure the decoder. Annex-B in-band SPS/PPS → WebCodecs `configure({codec})` with no `description` (annex-b mode); e.g. `avc1.640028`, `hev1.1.6.L93.B0`, `av01.0.08M.08`. |
| `KEYFRAME`         | First chunk after INIT; submit as a **key** access unit (`EncodedVideoChunk{type:'key'}`). |
| neither, not `INIT`| Delta access unit (`{type:'delta'}`). |
| `FRAGMENT_CONT`    | Append bytes to the in-progress `frame_id` buffer without inserting a new start code. |
| `END_FRAME`        | The `frame_id` is complete; submit exactly one chunk to the decoder. |

**Loss handling.** Watermark drop (§5.2) means a consumer may receive a
delta whose reference frame never arrived. On a decode error or a
detected `frame_id` gap across a non-keyframe boundary, the consumer
MUST discard deltas until the next `KEYFRAME` and SHOULD send a
`KEYFRAME_REQ` (§4.9). Do not feed a decoder deltas with a missing
reference; you get corruption, not recovery.

**Why this beats bridging to WebRTC for the browser edge.** The
producer hardware-encodes once (e.g. NVENC); the browser
hardware-decodes the same elementary stream via WebCodecs and renders
the `VideoFrame` to a canvas / WebGL texture. No SDP, no SRTP, no
transcode — the bytes on §9.6 WebTransport datagrams are already what
the decoder wants. Audio rides a parallel Stream and is fed to
`AudioDecoder` the same way. Input returns on a Stream at priority 0
(§5.3); decoder feedback returns on Control (§4.9, §4.10).

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

### 6.6 Message fragmentation (v1.2)

A logical message larger than one packet's payload is split into chunks,
each carried inside its own Channel frame and AEAD-encrypted
independently. The fragmentation envelope is the **first bytes of the
Channel plaintext** (inside the ciphertext), ahead of the chunk data:

```
Offset  Size  Field
0       1 B   version       0x01
1       16 B  fragment_id   random per logical message
17      2 B   seq           uint16 BE, 0-based chunk index
19      2 B   total         uint16 BE, total chunk count (>= 1)
21      var   data          raw chunk bytes
```

- Envelope overhead is `FRAG_HEADER_LEN = 21` bytes; max chunk data is
  `MAX_CHUNK_PAYLOAD = MAX_CHANNEL_DATA - 21 = 1003` bytes.
- A message that fits one chunk (including the empty message) still uses
  the envelope with `seq=0, total=1`, so the parser stays stateless.
- At most 65535 chunks per message.

**Reassembly:** buffer chunks by `fragment_id`; emit the concatenation in
`seq` order once `total` distinct chunks have arrived. Receivers MUST drop
chunks shorter than the header, or with `version != 0x01`, `total == 0`,
or `seq >= total`; ignore duplicate `seq`; and reset a buffer if a
`fragment_id`'s `total` changes mid-message (corrupt sender). Bound
memory: cap concurrent in-flight messages (reference: 256, evict oldest)
and discard partial reassemblies older than a timeout (reference: 30 s).

Streams (§5) do NOT use this envelope; they fragment codec frames with
the §5.4.1 `FRAGMENT_CONT` flag instead. This envelope is for Channel
messages only.

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
4       1 B   version           = PROTOCOL_VERSION (§12.4)
5       var   hostname          UTF-8 string, null-terminated
var     2 B   port              uint16 BE, listening port
```

Broadcast interval: 5 seconds. Duplicate detection by (hostname, ip, port).
There is a single wire version number (§12.4); this field carries it so a
peer can ignore broadcasts it cannot speak.

### 9.3 WebSocket (Legacy fallback) — Future

For hostile networks (corporate firewalls, symmetric NAT) and browsers
that lack WebTransport (§9.6). Connect outbound to Telesthetium hub over
WSS (port 443). Same frame format as UDP, carried as WebSocket binary
frames — boundaries are preserved by the WS frame, so no extra length
prefix is needed. Not yet implemented.

**Caveat (the reason §9.6 exists):** WebSocket runs over TCP. A lossy
Stream (§5) carried over TCP is silently turned reliable and
in-order — a single lost segment head-of-line-blocks every Channel and
Stream multiplexed on that socket, and "drop the late frame" (§5.2)
becomes "retransmit the frame you wanted dropped." Acceptable on a LAN
with near-zero loss; pathological for real-time video over a lossy
path. Prefer §9.6 WebTransport whenever the client supports it; fall
back to §9.3 only when it does not.

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

### 9.6 WebTransport (Preferred browser/edge transport) — v1.2

WebTransport (HTTP/3 over QUIC) is the preferred transport for any peer
that cannot open a raw UDP socket — first and foremost the browser, but
also any HTTP/3-reachable client behind a proxy. It is the browser-side
analogue of Telesthete's own duality: QUIC gives the browser
**unreliable datagrams** *and* **reliable ordered streams**, encrypted,
multiplexed without cross-stream head-of-line blocking. That is a near
1:1 fit with Stream and Channel, and it does not have §9.3's TCP
problem.

**Connection.** Outbound to the Telesthetium hub (or a host terminating
HTTP/3 directly) over UDP/443, ALPN `h3`:

```
https://<hub-or-host>/telesthete?band=<band_id_hex>
```

The server matches the `band_id` exactly as the UDP/WS relay does
(§10) — it routes opaque ciphertext and never holds the PSK. A peer
authenticates the *transport* with TLS (public CA cert), or, for direct
host-terminated WebTransport with a self-signed cert, via the browser
`serverCertificateHashes` mechanism (ECDSA P-256, validity ≤ 14 days —
a browser constraint, not ours).

**Channel-type → WebTransport mapping:**

| Telesthete         | WebTransport carrier                                  | Framing |
|--------------------|------------------------------------------------------|---------|
| Stream (0x01)      | Datagrams (`writeDatagram` / `datagrams.readable`)   | One datagram = one Telesthete packet. Boundaries preserved; **no** length prefix. |
| Channel (0x02)     | One bidirectional QUIC stream per `channel_id`        | Byte stream — **length-prefix each packet** (see below). |
| Control (0x00)     | A single dedicated reliable bidi stream              | Length-prefixed, same as Channel. |

**Length prefix on reliable carriers (foot-gun).** QUIC streams are
byte streams, not message streams: unlike UDP, AF_UNIX `SEQPACKET`, WS
binary frames, or QUIC *datagrams*, they do **not** preserve packet
boundaries. On any WebTransport reliable stream, every Telesthete
packet MUST be preceded by a 2-byte big-endian length
(`WT_STREAM_LEN_PREFIX = 2`) giving the byte count of the frame that
follows. The reader loops: read 2 bytes, read that many, parse one
frame, repeat. Datagrams need none of this.

**Datagram size.** QUIC datagrams are path-MTU bound and typically
smaller than Ethernet UDP — budget `MAX_DATAGRAM_SIZE = 1200` bytes for
the whole Telesthete packet (27-byte header + tag + payload) unless the
session's reported `maxDatagramSize` is larger. Producers fragment via
`FRAGMENT_CONT` (§5.4.1) to fit, exactly as on UDP.

**Loss & ordering.** Datagrams are unreliable and unordered, matching
Stream semantics exactly: §5.2 watermark drop applies unchanged.
Because each Channel is its own QUIC stream, loss on one Channel — or on
the datagram flow — never blocks another. This is the specific defect
§9.3 cannot avoid.

**Congestion control.** QUIC supplies congestion control for the
reliable streams (Channels, Control) for free. The datagram flow
(Streams/video) is paced by QUIC but never retransmitted — so real-time
rate control is the application's job, via the `RATE_HINT` /
`KEYFRAME_REQ` loop (§4.9, §4.10). This split is deliberate: bulk
transfer rides QUIC's loop; latency-critical media rides ours.

Advertise reachability with the `webtransport` capability (§12.5). Not
yet implemented.

---

## 10. Telesthetium Hub — Future

Self-hosted relay/signaling server. Peers connect outbound, identify by
`band_id`. Hub matches peers in the same Band and bridges traffic.
Hub sees only `band_id` + opaque ciphertext. Cannot decrypt.

Connection modes:
- **LAN:** Direct UDP, no hub needed.
- **Tunnel:** Both peers connect to hub, traffic relayed.
- **Hybrid:** Try LAN first, fall back to hub, prefer direct.

The hub is also the default HTTP/3 terminator for WebTransport (§9.6)
and the WSS terminator for the legacy WebSocket fallback (§9.3): a
browser peer reaches the band by connecting to the hub over UDP/443,
and the hub bridges it to UDP/AF_UNIX peers in the same band. Routing
is by `band_id` only; the hub still cannot decrypt.

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
│ (27B header, ChaCha20-Poly1305/AES-GCM)      │
├──────────────────────────────────────────────┤
│ UDP / AF_UNIX / WebTransport / WebSocket     │
└──────────────────────────────────────────────┘
```

The transport row is interchangeable: the application and the
Channel/Stream layer above it are identical whether the bytes ride UDP,
an AF_UNIX `SEQPACKET` socket, WebTransport datagrams+streams, or a
WebSocket. A browser dashboard is a first-class peer — it speaks the
same frame over §9.6 WebTransport and decodes §5.4 codec Streams with
WebCodecs (§5.5). No gateway transcode, no second protocol.

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
2. ChaCha20-Poly1305 (IETF) encrypt/decrypt — the mandatory baseline
   suite (libsodium, `@noble/ciphers`, Go x/crypto, etc.)
3. Derive band_id and key from PSK (SHA256 + HKDF, §3.1)
4. A mandatory HELLO announcing `ciphers: ["chacha20-poly1305"]` and its
   `capabilities` (§4.3, §12.5)
5. A transport (UDP on LAN; WebTransport or WebSocket otherwise)

That's it. No need to implement `aes256-gcm`, Channel reliability, Board
replication, or the full multiplexer. A client that speaks the baseline
cipher, the mandatory handshake, Streams, and Control is a first-class
peer.

### 12.2 Implementation Estimates

| Platform        | Transport          | Est. LOC | Notes                        |
|-----------------|--------------------|----------|------------------------------|
| Python (ref)    | UDP + asyncio      | ~2000    | Full library                 |
| Rust (ref v1.1) | UDP + AF_UNIX + tokio | ~3000 | Full library, dmabuf, fds    |
| Kotlin/Android  | OkHttp WebSocket   | ~500     | Stream + Control only        |
| ESP32/C         | WebSocket          | ~300     | Stream + Control only        |
| Browser/JS      | Native WebSocket   | ~200     | Stream + Control only        |
| Browser/JS      | WebTransport       | ~300     | Stream (datagrams) + Channel (streams) + Control; pairs with WebCodecs for video (§5.5) |

### 12.3 Byte Order

All multi-byte integers are **big-endian** (network byte order), with
one explicit exception: dmabuf descriptor `fourcc` (§5.4.2) is
little-endian to match DRM tooling.

### 12.4 Constants

```
HEADER_SIZE        = 27
NONCE_LEN          = 12      (v1.2; 4 zero bytes || 8-byte BE sequence)
AUTH_TAG_SIZE      = 16      (Poly1305 or GMAC; both suites)
MIN_PACKET_SIZE    = 43      (header + tag)
MAGIC_DISCOVERY    = 0x54454C45
PROTOCOL_VERSION   = 3       (single wire version; maps 1.0->1, 1.1->2, 1.2->3.
                             v1.2 is BREAKING vs 1.0/1.1: new AEAD suites,
                             12-byte nonce, mandatory capability + cipher negotiation.)
BASELINE_CIPHER    = "chacha20-poly1305"   (v1.2; mandatory AEAD, §3.2)
KEEPALIVE_INTERVAL = 5       (seconds)
PEER_TIMEOUT       = 15      (seconds, 3x keepalive)
DEFAULT_WINDOW     = 32      (packets)
DEFAULT_RTO        = 500     (milliseconds)
MAX_CHANNEL_DATA   = 1024    (bytes per Channel packet)
FRAG_HEADER_LEN    = 21      (v1.2; Channel fragmentation envelope, §6.6)
MAX_CHUNK_PAYLOAD  = 1003    (v1.2; MAX_CHANNEL_DATA - FRAG_HEADER_LEN)
MAX_FDS_PER_PACKET = 5       (v1.1; 4 planes + 1 fence)
LOCAL_PSK          = "telesthete-local"   (v1.1 sentinel for §3.4)
STREAM_HEADER_LEN  = 8       (v1.1 §5.4)
WEBTRANSPORT_ALPN  = "h3"    (v1.2 §9.6)
WEBTRANSPORT_PORT  = 443     (v1.2; QUIC/UDP)
WT_STREAM_LEN_PREFIX = 2     (v1.2; uint16 BE per-packet length on reliable WT streams)
MAX_DATAGRAM_SIZE  = 1200    (v1.2; conservative QUIC datagram payload budget)
```

v1.2 is a breaking wire revision: capability announce and cipher
negotiation are mandatory, so there is no v1.0/v1.1 fallback. The only
deployed consumer (Rook) is upgraded in lockstep. Feature compatibility
*within* v1.2 is handled per-peer by capabilities (§12.5), not by version
fallback.

### 12.5 Capability negotiation (foundational, v1.2)

Capability announce is **mandatory**. Every peer's `HELLO`/`HELLO_ACK`
(§4.3, §4.4) MUST carry a `capabilities` array and an ordered `ciphers`
list. This is the foundation for cipher (§3.5), transport, and feature
selection; a peer that omits them is non-conformant.

Defined `capabilities` strings:

| Capability   | Meaning                                                         |
|--------------|-----------------------------------------------------------------|
| `dmabuf-v1`  | Peer can produce and/or consume Stream packets with `DMABUF`.   |
| `af-unix`    | Peer reachable via AF_UNIX (§9.4).                              |
| `sync-file`  | Peer honors `WITH_FENCE`. Implies `dmabuf-v1`.                  |
| `reuse-v1`   | Peer honors `REUSE`. Implies `dmabuf-v1`.                       |
| `webtransport` | Peer reachable via WebTransport (§9.6).                       |
| `keyframe-req` | Producer honors `KEYFRAME_REQ` (§4.9).                       |
| `rate-hint`  | Producer honors `RATE_HINT` (§4.10).                            |

An absent capability = not supported: the sender MUST NOT use that
feature with the peer and falls back to the baseline the feature extends
(e.g. §5.1 Stream payload over UDP). Capability strings are
forward-extensible; unknown ones are ignored.

The `ciphers` list is the AEAD suites the peer supports (§3.2), ordered
by preference; it MUST include the baseline `chacha20-poly1305`.
Selection follows §3.5. Cipher is a separate ordered field, not a
capability string, because preference order is significant.

Codec selection (H.264 / HEVC / AV1) for §5.4 codec Streams is **not** a
capability — it is negotiated by the application (Rook, KVM) via
`METACONTROL` (§4.7) or out of band, so the transport stays codec-
agnostic. `webtransport` advertises reachability only; a peer may speak
WebTransport and UDP simultaneously and the sender picks per §9.5-style
preference (direct UDP > WebTransport > WebSocket).

---

## 13. Compatibility Matrix

v1.2 is a clean breaking revision; v1.0/v1.1 peers do not interoperate
(different AEAD, different nonce, optional-vs-mandatory capabilities) and
were never deployed beyond development. Within v1.2, all feature
compatibility is by capability (§12.5), negotiated per peer — a feature
is used with a peer only if it advertises the matching capability/cipher.

| Feature                       | Required to use it with a peer    |
|-------------------------------|-----------------------------------|
| Baseline AEAD + frame parse   | always (mandatory)                |
| §5.1 Stream payload (UDP)     | always (mandatory baseline)       |
| `aes256-gcm` suite            | `aes256-gcm` in peer's `ciphers`  |
| §5.4 StreamHeader / `DMABUF`  | `dmabuf-v1`                       |
| `WITH_FENCE` / `REUSE`        | `sync-file` / `reuse-v1`          |
| AF_UNIX transport             | `af-unix`                        |
| WebTransport transport        | `webtransport`                   |
| `KEYFRAME_REQ` / `RATE_HINT`  | `keyframe-req` / `rate-hint`     |

Absent capability ⇒ the sender restricts itself to the baseline the
feature extends. Unknown capabilities/ciphers are ignored, so the set is
forward-extensible without a version bump.

---

## 14. Dependencies

Reference implementations:
- **Python** (`telesthete/`) — ChaCha20-Poly1305 IETF baseline via PyNaCl
  bindings (`crypto_aead_chacha20poly1305_ietf_*`) or `cryptography`;
  `aes256-gcm` optional via the same. Python 3.10+
- **Rust** (`rust/telesthete/`) — `chacha20poly1305` (use the
  `ChaCha20Poly1305` IETF type, not `XChaCha20Poly1305`), `aes-gcm`
  (optional), `hkdf`, `sha2`, `nix` (AF_UNIX + SCM_RIGHTS), `tokio`

Cross-language: the baseline needs ChaCha20-Poly1305 (IETF, RFC 8439),
in every libsodium binding, Go `x/crypto`, Node `crypto`,
`@noble/ciphers`, etc. `aes256-gcm` is optional.

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
6. (v1.2) WebTransport endpoint topology: is the hub always the HTTP/3
   terminator, or do individual hosts terminate WebTransport directly?
   Hub-terminated is the default (one public cert, reuses §10 routing).
   Direct host-terminated WT needs the `serverCertificateHashes`
   short-lived-ECDSA dance per browser; deferred unless a use case
   demands hub-less browser access.
7. (v1.2) Should lossy video over WebTransport datagrams rely on QUIC
   pacing alone, or is the §4.10 RATE_HINT loop mandatory? Advisory in
   v1.2; revisit if real deployments show encoder overrun on congested
   paths.
8. (v1.2) WebTransport datagram MTU: fixed `MAX_DATAGRAM_SIZE = 1200`
   or probe the session's `maxDatagramSize` and grow? Start fixed and
   conservative; probing is a later optimization.

---

## 16. Changelog

| Version | Date       | Changes                                                                                                                                                                                                                |
|---------|------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| 1.0     | 2026-04-07 | Initial spec from Python reference implementation.                                                                                                                                                                     |
| 1.1     | 2026-05-11 | Added §3.4 local crypto profile, §5.4 StreamHeader + dmabuf descriptor (capability-gated), §9.4 AF_UNIX transport, §9.5 send-loop priority across transports, §12.5 capability negotiation. `PROTOCOL_VERSION` → 2. Backwards-compatible: v1.0 peers continue to interoperate. |
| 1.2     | 2026-06-30 | **Breaking wire revision** (collapsed into 1.2 — pre-adoption, sole consumer Rook upgraded in lockstep; no version spam). **Browser/edge:** §9.6 WebTransport (Stream→datagrams, Channel/Control→reliable QUIC streams, length-prefixed), §5.5 WebCodecs decode of §5.4 codec Streams, §4.9 `KEYFRAME_REQ` + §4.10 `RATE_HINT`, §9.3 TCP head-of-line caveat. **Crypto remediation:** AEAD is now a negotiated suite — ChaCha20-Poly1305 (IETF) mandatory baseline + AES-256-GCM optional (§3.2); 12-byte nonce (was 24); per-cipher key derivation (§3.1); real AEAD AAD (the pre-v1.2 Python reference wrongly used XSalsa20/`SecretBox` with AAD prepended — non-interoperable with the spec/Rust). **Capability announce is now foundational/mandatory** with an ordered `ciphers` list and end-to-end cipher negotiation (§3.5, §4.3, §12.5). Replay protection §3.3 SHOULD→MUST. Rook's Channel fragmentation envelope promoted to §6.6. Version fields reconciled to one `PROTOCOL_VERSION` (= 3; 1.0→1, 1.1→2, 1.2→3). |
