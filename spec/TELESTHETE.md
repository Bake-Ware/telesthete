# Telesthete v1 — Rook Wire Transport

The transport under the spatial thin client. It carries `spatial-proto` messages between
one host daemon (`spatial-hostd`) and one client (`spatial-client`) over a home LAN/WiFi,
with the channel semantics the design (§5) requires: **an input packet must never queue
behind a keyframe.**

Status: **v1**, integrated behind `--transport telesthete` (default `tcp` = the loopback
stand-in). This document also resolves the open items in `spatial-proto/spec/SPEC.md §7`.

---

## 1. Architecture decision: hybrid TCP + UDP

One logical session uses **two sockets** established under a single handshake:

- **TCP** — reliable, ordered. Carries `ctl`, `input` (key/button/focus), and `meta`.
  These are small and must never be lost or reordered. TCP already gives us exactly that.
- **UDP** — unreliable datagrams, one datagram = one channel message. Carries `tex.N`
  (bulk, loss-tolerant via keyframe recovery), `audio` (low-latency lossy), and the
  coalescible `motion`/`scroll` input sub-stream (latest-wins).

### Why not single TCP (what the stand-in does)
A single byte stream serializes *all* channels through one retransmit queue. One lost
segment of an 8 KB keyframe stalls **every** later byte — including the next keystroke —
until it is retransmitted (~1 RTT+). That is head-of-line blocking across channels, and
it is precisely the failure the design forbids. Rejected.

### Why not pure UDP with our own reliability
`ctl`/`input` need exactly-once ordered delivery. Re-implementing retransmission, ordering,
and congestion control over UDP is a TCP re-write with more bugs. The reliable channels are
tiny; TCP is the right tool. We only go to UDP for the channels that *tolerate* loss, so we
never implement reliability we don't need. Rejected for the reliable channels.

### Why not QUIC (the "correct" long answer)
QUIC gives independent streams (no cross-stream HOL) + TLS in one socket, and is the
natural v2 target. But the client is C++ and would need a C QUIC library (quiche/ngtcp2),
the hostd a Rust one (quinn); binding both and threading their async loops through the
existing synchronous session loop is a large integration for v1. The TCP+UDP hybrid gets
the same HOL isolation between the *priority classes we actually have* with two ordinary
sockets and no async runtime. **QUIC is documented as the intended v2 migration.**

### The tradeoff we accept
UDP loss on `tex` is recovered by a `KEYFRAME_REQUEST` over the reliable TCP channel
(§6), not by retransmit — so a dropped keyframe costs one round-trip of staleness (hidden
behind the PAUSED overlay, design §7), not a stall. Audio loss is simply dropped (a
20 ms gap). Motion loss is a non-event (latest-wins). This is the correct loss posture for
each channel class, and it is why the split pays for itself.

---

## 2. Channel classes

| proto channel id | class | socket | semantics |
|---|---|---|---|
| `ctl` (0) | reliable-ordered | TCP | never lost, in order |
| `input` reliable ops (KEY/BUTTON/FOCUS) | reliable-ordered | TCP | never lost, in order |
| `input` coalescible (MOTION 0x81/SCROLL 0x82) | latest-wins | UDP | newest `seq` wins; stale dropped |
| `tex.N` (2..9) | bulk loss-tolerant | UDP | fragmented; keyframe-recovered |
| `audio` (200) | low-latency lossy | UDP | dropped on loss, never buffered |
| `meta` (201) | reliable | TCP | never lost |

The transport routes by the same 1-byte channel id the stand-in used, so `spatial-proto`
message bytes are **unchanged**. Whether a given `input` message goes on TCP or UDP is
decided by its opcode's high bit (`0x80` = coalescible, already in the proto): the sender
peeks the first envelope byte.

### Message-boundary preservation (SPEC §7 resolution)
- **TCP** channels frame each message as `[u8 channel][u32 le len][len bytes]` *inside the
  encrypted record* (§4), so the receiver re-emits exact message boundaries. **Resolves
  SPEC §7 "message-boundary vs byte-stream": the transport preserves boundaries; the
  Envelope does NOT need a `body_len`.**
- **UDP** channels put one message per datagram (plus a small transport header, §5). The
  datagram *is* the boundary.

### Latest-wins semantic (SPEC §7 resolution)
The coalescible `input` sub-stream and per-window `tex` freshness are handled at the
**transport** for UDP: the receiver tracks the last delivered `seq` per (channel, window)
and drops anything not `seq_newer` (the wrap-aware comparison already in
`spatial-proto::input::seq_newer`). **Resolves SPEC §7 "latest-wins channel semantic":
telesthete provides it for UDP channels; the app layer no longer needs to.**

---

## 3. Handshake, authentication, encryption

Everything after the handshake is encrypted. This link carries keystrokes over home WiFi;
plaintext is not acceptable.

### Trust model
A **pre-shared key (PSK)** gates the band (design §5, §7: "access gated by the band PSK";
"approval is granted by installing the host control app"). Both ends load the same 32-byte
PSK from config. Knowledge of the PSK is the authorization; there is no PKI in v1.

### PSK provisioning
- Path: `$XDG_CONFIG_HOME/telesthete/psk` (fallback `~/.config/telesthete/psk`), or the
  path in `$TELESTHETE_PSK_FILE`.
- Format: a single line, 64 hex chars (32 bytes) or a `base64:` prefix. `0600` perms
  expected; the library warns (does not fail) on looser perms.
- Generation: `telesthete-keygen` (or any 32 random bytes hex-encoded). The "pre-set
  install script" in the design writes this file on both ends.

### Handshake (authenticated key agreement, no hand-rolled primitives)
An X25519 ephemeral exchange bound to the PSK, with explicit key confirmation. Primitives
are all from audited crates (`x25519-dalek`, `hkdf`, `sha2`, `chacha20poly1305`).

```
Client                                            Host (responder)
  eC = X25519 ephemeral                            eH = X25519 ephemeral
  --> Hello1 { version, eC.pub, nC (16B random),
               client_udp_port }
                                                   <-- Hello2 { eH.pub, nH (16B random),
                                                                confirmH }
  (both compute)  dh = X25519(e_self.priv, e_peer.pub)
  transcript = version ‖ eC.pub ‖ eH.pub ‖ nC ‖ nH
  keys = HKDF-SHA256(ikm = dh ‖ PSK, info = "telesthete-v1", salt = transcript)
       -> K_tcp_c2h, K_tcp_h2c, K_udp_c2h, K_udp_h2c, session_id (16B)
  confirmH = HMAC-key(K_tcp_h2c) over ("host-confirm" ‖ transcript)   [checked by client]
  --> Confirm3 { confirmC }
  confirmC = HMAC-key(K_tcp_c2h) over ("client-confirm" ‖ transcript) [checked by host]
```

- **Wrong PSK** → `dh ‖ PSK` differs → derived keys differ → `confirm` MAC check fails on
  the first message → connection dropped. Neither side ever reveals whether PSK or DH was
  the mismatch.
- **Replay** → the responder picks a fresh `eH` and `nH` every attempt, so a captured
  transcript yields different keys on replay and confirmation fails. Ephemeral keys also
  give **forward secrecy**: a later PSK compromise does not decrypt recorded sessions.
- The UDP keys are separate from the TCP keys and used with **explicit per-datagram
  nonces** (§5), because UDP is out-of-order and cannot use a streaming nonce counter.

### Binding the UDP socket to the session
`Hello1` carries the client's UDP port; `Hello2` carries the host's. The first UDP
datagram each way includes the `session_id` (encrypted) so the peer binds the datagram
source address to the authenticated session (defeats blind UDP spoofing — an attacker
would need the UDP key to forge `session_id`).

---

## 4. TCP record format

After the handshake, the TCP socket carries a stream of **encrypted records**:

```
[u32 le ct_len][ct_len bytes: ChaCha20-Poly1305(nonce = counter, ad = "")]
plaintext of a record = [u8 channel][u32 le msg_len][msg_len bytes proto message]
```

- Nonce is a per-direction 96-bit counter (monotonic, starts 0). In-order by TCP, so a
  counter is safe and needs no transmission.
- One record = one proto message (v1; the framing allows batching later).
- 16-byte Poly1305 tag per record. Overhead: 4 (ct_len) + 16 (tag) = 20 B/msg — negligible
  for ctl/input.

---

## 5. UDP datagram format

```
[u8 type][u8 channel][u24 le seq][... type-specific ...][ChaCha20-Poly1305 tag inline]
```

The whole datagram after a 1-byte clear `type` is encrypted with `K_udp_*` using
`nonce = type ‖ channel ‖ seq ‖ dir_bit` (96-bit, unique per datagram per direction).

- `type=DATA` (0): `[channel][seq][msg_len u16][proto message]` — for `audio`, `motion`,
  `scroll`, and small `tex`. Receiver drops if not `seq_newer(seq, last[channel,window])`.
- `type=FRAG` (1): `[channel][seq][msg_id u32][frag_idx u16][frag_count u16][frag bytes]`
  — for `tex` messages larger than the datagram MTU. Reassembled by `(channel, msg_id)`;
  incomplete sets dropped when a newer `msg_id` for that window arrives or after 250 ms.
  **Resolves SPEC §7 "h264 fragment MTU": telesthete fragments bulk datagrams at a
  configurable MTU (default 1200 for WiFi safety; raise via `TELESTHETE_MTU`). proto's
  `frag_idx` (u16) is the fragment index; this transport frag header is transport-internal
  and independent of the proto's own `TEX_FRAME.frag_idx`.**
- `type=HELLO_UDP` (2): first datagram each way, carries encrypted `session_id` for
  source-address binding.

Loss handling: no retransmit. `audio`/`motion` loss is tolerated by class. `tex` loss
leaves the frame incomplete; the client detects the gap (missing frag or stale window) and
issues `KEYFRAME_REQUEST` on the reliable TCP channel.

---

## 6. Liveness, reconnect, session resume

- **Keepalive**: a `PING` control record every 2 s of idle on TCP; `PONG` reply. Three
  missed → peer declared dead, session torn down.
- **Dead-peer detection**: TCP `SO_KEEPALIVE` plus the app-level PING (catches a wedged
  but not-closed peer).
- **Reconnect + resume**: the client reconnects with a fresh handshake. The hostd session
  loop already **reseeds a keyframe for every window to a fresh client** (see
  `session.rs`), so a reconnect transparently repaints the scene. v1 does **not** resume
  the old `session_id` (each connect is a new session); the reseed makes that invisible to
  the user. True 0-RTT resume is a v2 item.

---

## 7. C ABI (mirrors spatial-proto conventions)

`include/telesthete.h`. Borrowed buffers (`tel_bytes` = `{ptr, len}`), explicit error
codes (`TEL_OK=0`, `TEL_ERR_*<0`), no hidden allocation across the boundary. The client
uses the C ABI; hostd uses the Rust API directly.

- `tel_client_connect(host, tcp_port, psk_path) -> *Client | NULL`
- `tel_client_send(client, channel, msg, len)` — routes to TCP/UDP by channel+opcode
- `tel_client_poll(client, cb, user)` — drains all ready messages, calls back per message
- `tel_client_connected(client) -> int`
- `tel_client_free(client)`

The Rust side exposes `TelServer::bind`, `TelServer::accept -> TelSession`, and
`TelSession::{send(channel,&[u8]), poll() -> Vec<(u8, Vec<u8>)>, is_alive()}` shaped to
drop into the hostd session loop next to the `TcpStream` path.

---

## 8. What v1 does NOT do (honest scope)

- No QUIC (v2), no 0-RTT resume (v2), no congestion control on UDP (relies on the fact that
  tex is damage-driven and audio is a fixed ~96 kbps — neither floods a LAN; a real WAN
  path would want pacing).
- No multi-client fan-out (one session per host, matching current hostd).
- Mic channel stays reserved.
- PSK rotation is manual (rewrite the file, reconnect).
