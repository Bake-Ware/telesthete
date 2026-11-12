# Telesthete additions for application hubs (Rook)

Design only. Nothing here is built yet. Base: `main` at `8538e7c`
("Stream messages carry their datagram's receive time (#16)").

Written 12 November 2026 against Rook's locked plan (`core:PLAN.md`), its gap
report (`core:docs/telesthete-surface.md`) and its protocol
(`core:docs/protocol.md`). The reviewer checks it against the plan; Bake signs it
before anything is built.

## Contents

1. [Summary](#1-summary)
2. [Ground rules](#2-ground-rules)
3. [What I verified](#3-what-i-verified)
4. [Requirement 1: per-peer keys on one endpoint](#4-requirement-1-per-peer-keys-on-one-endpoint)
5. [Requirement 2: a hub built on the relay](#5-requirement-2-a-hub-built-on-the-relay)
6. [Requirement 3: WebSocket clients](#6-requirement-3-websocket-clients)
7. [Requirement 4: links that survive restarts](#7-requirement-4-links-that-survive-restarts)
8. [Requirement 5: a minimal client that can answer calls](#8-requirement-5-a-minimal-client-that-can-answer-calls)
9. [Requirement 6: stale text](#9-requirement-6-stale-text)
10. [Appendix to SPEC.md (exact text)](#10-appendix-to-specmd-exact-text)
11. [Decisions needed](#11-decisions-needed)
12. [Implementation plan](#12-implementation-plan)
13. [What I could not verify](#13-what-i-could-not-verify)

---

## 1. Summary

- **No wire change.** The frame, the key derivation, the handshake, every
  channel type and every conformance vector stay as they are.
  `PROTOCOL_VERSION` stays 3. Everything below is new API, new optional
  behaviour, or a new relay listener that is off unless configured.
- **One key is one band.** A per-peer key is an ordinary PSK, so it has its own
  `band_id`. A new `Endpoint` holds many keys on one transport and picks the key
  by the cleartext `band_id`, which every frame already carries. Removing a key
  drops its frames before decryption, at once.
- **Keys from Ed25519.** Both sides convert their Ed25519 keys to X25519, run
  X25519, and HKDF the result into a 64-character hex PSK. Each side derives it
  alone. No shared secret crosses the wire.
- **Hub shape.** The relay (`telesthete-hub`) stays keyless and is the one
  public address for UDP, WebSocket and WebTransport. A new local attachment
  socket lets an application process terminate many bands through one
  connection. The application (Python) holds the keys and decrypts.
- **WebSocket clients** for Python and Rust, matching what the relay already
  serves (`/band`, one frame per binary message, one band per connection).
- **Link lifecycle.** HELLO is retried until answered. Channels are reset when
  the peer's session changes, can be reopened, and answer a dead incarnation
  with RST. All of it is a receiver's local reaction to frames that exist
  today, so nothing is negotiated.
- **Link profile.** The smallest client that can answer requests: baseline
  cipher, Control, one Channel, a small window, reconnection. An ESP32 needs
  roughly 1,000 to 1,300 lines of C (estimate).

## 2. Ground rules

- **Additive only.** Existing behaviour, wire, vectors and public APIs keep
  working exactly as now. Where a fix would change something that exists, it is
  either opt-in or listed under [Decisions needed](#11-decisions-needed).
- **Application-agnostic.** The spec text in section 10 never mentions workers,
  caps, owners or Rook. It says "application", "hub" and "peer". Telesthete is
  the lock. The application decides who gets a key.
- **Nothing extra.** No signatures, no multi-band peers, no negotiated
  extensions (Rook plan, "Explicitly out this round"). The hub being in many
  bands at once is not "multi-band workers": each worker is in exactly one band,
  and the hub was always going to hold every worker's key.
- **Least new surface.** One new class per library (`Endpoint`), one helper
  (`pairwise_psk`), one transport per library (WebSocket client), a few
  callbacks and options, one relay listener.

## 3. What I verified

Tests run in a clone under `/tmp` at `8538e7c`, Python 3.14, Rust stable:

| Suite | Result |
|---|---|
| `pip install -e .[aes] pytest pytest-asyncio`, `python -m pytest -q` | 138 passed |
| `cargo test --workspace --locked`, `telesthete` | 113 passed |
| `cargo test --workspace --locked`, `telesthitium` | 56 unit + 21 integration passed |

These match the gap report.

I also ran a probe over real localhost UDP with two `Band`s (script described in
the pull request) to check the report's link conditions myself:

| Condition | Result |
|---|---|
| 2. SYN to a Band that has not created the channel | Dropped. Worker stays `SYN_SENT`. Silently, in fact: no `CHANNEL` handler is registered on the transport until the Band's first `channel()` call (`band.py:377`, `381-407`), so `udp.py:179` finds no handler. |
| 3. First HELLO lost (receiver not yet bound) | After 6.5 s both sides have 0 peers. |
| 4. Worker restarted on the same port, new epoch, reopens channel 0 | Hub channel stays `ESTABLISHED` with `rcv_next=2`; worker stuck in `SYN_SENT`. |
| 5. Worker silent while the hub has unacked data | Hub channel `CLOSED` after about 5 s; `band.channel(0, …)` keeps returning the same dead object. |

Code facts this design relies on (file and line at `8538e7c`):

- Python `Band` takes one `psk` (`telesthete/band.py:39`), derives one
  `band_id` (`band.py:74-75`, `protocol/crypto.py:47-49`) and builds its own
  `UDPTransport` (`band.py:97`). There is no transport argument.
- Python `UDPTransport` dispatches by `channel_type` only and never looks at
  `band_id` (`transport/udp.py:172-187`). A frame for another band reaches the
  Band's handlers and fails AEAD. `UnixTransport` exposes the same handler
  interface (`transport/unix.py`, class docstring).
- Python `Band.channels` is keyed by `channel_id` alone (`band.py:117`,
  `365-366`) and the handler drops frames from any other peer (`band.py:399-404`).
- Python `connect_peer` sends one HELLO (`band.py:499-514`); the keepalive loop
  never resends it (`band.py:575-595`). `_remove_peer` does not touch channels
  (`band.py:304-316`).
- Python `Channel`: inner sequences start at 0 only in `__init__`
  (`protocol/channel.py:151-152`); `open()` requires `CLOSED` (`channel.py:188`);
  `_teardown` sets `CLOSED` but does not zero the inner sequences
  (`channel.py:469-476`); a SYN whose inner seq is below `rcv_next` is treated as
  a duplicate (`channel.py:377-381`) and one above it is buffered
  (`channel.py:382-392`); give-up after `MAX_RETRIES = 10` (`channel.py:41`,
  `506-511`).
- Python `ControlChannel` rebases on a newer epoch and calls `on_new_session`
  (`protocol/control.py:287-310`); the Band uses that only to reset watermarks
  (`band.py:279-291`).
- Rust `Transport::bind(addr, key, band_id)` owns one socket, one base key and
  one `band_id`, and drops frames for any other `band_id` after decrypting
  (`rust/telesthete/src/transport.rs:185`, `331-334`).
- Rust `ChannelHub` keys connections by `(peer, channel_id)` (`channel.rs:733`)
  and drops frames with no connection (`channel.rs:692-701`). A Closed connection
  accepts a SYN at any inner seq (`channel.rs:347-357`); Established treats a
  low seq as a duplicate (`channel.rs:389-394`). Give-up after 10 retries
  (`channel.rs:51`, `302-310`). `open()` replaces a route, so Rust can reopen
  (`channel.rs:728-733`).
- Rust control answers every HELLO, equal epoch included, with HELLO_ACK
  (`control.rs:380-399`), and broadcasts a rebase on a newer epoch
  (`control.rs:320-324`).
- Neither library sends RST. Both tear down on receiving one
  (`channel.py:364-367`, `channel.rs:340-344`).
- The relay: `Registry::connect(band, PeerKey::Conn(id), Sink::Conn(tx))`
  registers a connection peer (`telesthitium/src/registry.rs:278-281`);
  `forward` relays to every other eligible peer (`registry.rs:430-479`). The
  same `PeerKey` can sit in many bands (the map is
  `HashMap<BandId, HashMap<PeerKey, Peer>>`, `registry.rs:209`). WS connections
  are pinned to the band of their first frame (`ws.rs:195-217`), pinged every
  20 s and closed after 60 s of silence (`ws.rs:38-41`). The federation link
  already speaks `u32 len || u8 type || payload` over a stream
  (`federation.rs:24-29`).
- Rook's installer already downloads `telesthete-hub-<arch>-linux.tar.gz` for
  x86_64 and aarch64 from Telesthete releases (`.github/workflows/release.yml:1-8`).
- Rust public enums `BandError` and `TransportError` and the struct
  `BandOptions` are not `#[non_exhaustive]` (`band.rs:29`, `62`,
  `transport.rs:119`). Adding a variant or a public field would break
  downstream code that matches or builds them exhaustively, so this design adds
  neither.

The report's claims I checked and found correct: conditions 1 to 5, answer b
(keys), answer d (WebSocket), the test counts, and the stale lines in
`PROTOCOL_STATUS.md`.

---

## 4. Requirement 1: per-peer keys on one endpoint

### 4.1 The idea

A Telesthete key is a PSK. `band_id = SHA256(PSK)[:16]` is in cleartext at the
front of every frame (SPEC §1, §3.1). Give each peer its own PSK and each peer
gets its own `band_id`, so the receiver can choose the key before it decrypts
anything. Nothing on the wire changes: to a peer, its pairwise PSK is just a PSK.

So "one endpoint, many keys" is: one transport, a table `band_id → Band`, and a
demultiplexer in front. Each entry is an ordinary `Band` with the existing API
(`channel`, `stream`, `board`, ...). Adding a key adds an entry. Removing a key
removes the entry, and the next frame with that `band_id` is dropped before any
crypto.

Alternatives considered:

- *One band, per-peer keys inside it* (a key id in the frame, or trial
  decryption under every key). A key id changes the wire. Trial decryption costs
  one AEAD attempt per key per frame and still needs the `band_id` to be shared,
  which leaks nothing useful to the relay but buys nothing either. Rejected.
- *One Band and one socket per key* (today's only option, report answer b).
  Needs a port per peer. Rejected; this is the problem.

### 4.2 Python API

```python
from telesthete import Band, Endpoint, pairwise_psk

# Own UDP socket (LAN, tests, a hub without the relay):
ep = Endpoint(hostname="hub", bind_address="0.0.0.0", bind_port=7474)
# Or attached to a relay (section 5):
ep = Endpoint(hostname="hub", attach="/run/telesthete/attach.sock")

await ep.start()
band = ep.add_key(psk)          # -> Band; synchronous; idempotent for the same psk
ep.remove_key(band.band_id)     # -> bool; synchronous
ep.band(band_id)                # -> Optional[Band]
ep.band_ids()                   # -> list[bytes]
await ep.stop()
```

`Endpoint(hostname=None, *, bind_address="0.0.0.0", bind_port=9999,
attach=None, capabilities=None, ciphers=None, session_epoch=None)`.
Exactly one of the UDP bind or `attach` is used (`attach` wins if given).
`capabilities`, `ciphers` and `session_epoch` are passed to every Band the
Endpoint creates, so all of them share one epoch, which MUST increase across
restarts exactly as for a Band (SPEC §4.3).

Rules:

- `add_key(psk)` returns a normal `Band` whose transport is a per-key view of the
  Endpoint's transport. The view's `start`, `run` and `stop` do nothing; the
  Endpoint owns the socket. If the Endpoint is running, the Band is started at
  once. Endpoint Bands are created with `reconnect=True` (section 7).
- `remove_key(band_id)` deletes the table entry first, then stops the Band's
  tasks (keepalive, channel retransmits) without sending GOODBYE. When it
  returns, no frame bearing that `band_id` is decrypted or delivered to the
  application. A callback already running finishes.
- Frames shorter than 43 bytes, or whose `band_id` is not in the table, are
  dropped before decryption and counted (`ep.dropped_unknown`, an int).
- Each Band keeps its own `SequenceSource`. Keys differ per band, so nonce
  uniqueness holds per key exactly as today.

Supporting additions to `Band` (also used by section 6):

- `Band(..., transport=None)`. When given, the Band uses that object instead
  of building a `UDPTransport`, and `bind_address`/`bind_port` are ignored. The
  object must provide what `UDPTransport` and `UnixTransport` already provide:
  `start()`, `async run()`, `async stop()`, `register_handler(channel_type,
  cb(peer_addr, packet_bytes))`, `send(dest, packet_bytes)`, `local_address`.
  The Band drives `start/run/stop` as it does today. Omitting `transport`
  behaves exactly as now.

`pairwise_psk(own_secret: bytes, peer_public: bytes) -> str` (section 4.4),
exported from `telesthete`. `own_secret` is a 32-byte Ed25519 seed or
libsodium's 64-byte secret key; `peer_public` a 32-byte Ed25519 public key.
Raises `ValueError` on a bad or small-order key, an all-zero shared secret, or
`own_public == peer_public`.

### 4.3 Rust API

```rust
use telesthete::{Endpoint, pairwise_psk};

let ep = Endpoint::bind_udp("0.0.0.0:7474".parse()?, "hub").await?;
// #[cfg(unix)] let ep = Endpoint::attach("/run/telesthete/attach.sock", "hub").await?;

let band: Band = ep.add_key(psk.as_bytes(), BandOptions::default()).await?;
let removed: bool = ep.remove_key(&band.band_id()).await;
let ids: Vec<BandId> = ep.band_ids().await;
```

- `Endpoint::bind_udp(addr, hostname) -> io::Result<Endpoint>`,
  `Endpoint::attach(path, hostname) -> io::Result<Endpoint>` (unix only),
  `Endpoint::with_session(self, session: u64) -> Self`.
- `add_key(&self, psk, opts) -> Result<Band, BandError>` returns an owned
  `Band`. Its transport shares the Endpoint's socket. The Band has
  reconnect behaviour on (section 7). Dropping the `Band` removes its key,
  like `remove_key`.
- `remove_key(&self, band_id) -> bool`. After it returns, the receive loop
  drops that `band_id`. The removed `Band`'s sends fail with
  `TransportError::Io` of kind `NotConnected` (no new enum variant).
- `pairwise_psk(own_seed: &[u8; 32], peer_public: &[u8; 32]) -> Result<String,
  PairwiseError>`. `PairwiseError` is a new `#[non_exhaustive]` enum.
- Internals: `Transport` gains a private I/O backend (UDP socket, shared
  Endpoint socket, attachment, WebSocket). `Transport::bind` keeps its
  signature and behaviour. Dependencies: `curve25519-dalek` (Edwards to
  Montgomery, X25519) or `x25519-dalek` plus `ed25519-dalek`, and `sha2`/`hkdf`
  which are already in the tree.

### 4.4 From an Ed25519 keypair to a PSK

The worker generates an Ed25519 keypair and sends the public half in its knock
(HTTPS); the knock reply carries the hub's public key (Rook `docs/protocol.md`,
"knock", "knock_reply"). Each side then holds its own secret key and the other
side's public key. That is enough for a Diffie-Hellman agreement:

```
own_x_secret = clamp(SHA-512(own_seed)[0:32])        RFC 7748 §5 clamping;
                                                      = libsodium crypto_sign_ed25519_sk_to_curve25519
peer_x_public = Montgomery u of the peer's Ed25519    = libsodium crypto_sign_ed25519_pk_to_curve25519
                point (reject non-canonical encodings,
                points not on the curve, small order)
shared = X25519(own_x_secret, peer_x_public)          reject if all 32 bytes are zero
(a, b) = the two Ed25519 public keys, sorted bytewise  reject if equal
okm    = HKDF-SHA256(salt = "telesthete-pairwise-v1",
                     ikm  = shared,
                     info = a || b,                    64 bytes
                     L    = 32)
psk    = lowercase hex of okm                          64 ASCII characters
```

Both sides compute the same `psk`, and it is used as any PSK (SPEC §3.1). Why
each step:

- **Hex string.** The Python API takes `psk: str`, Rust takes `&[u8]`. ASCII
  hex is identical bytes in both, and on an ESP32.
- **Sorted keys in `info`.** Binds the PSK to this pair and makes the
  derivation symmetric without roles.
- **Salt with a version.** `telesthete-pairwise-v1` separates this use from any
  other use of the same X25519 output and versions the derivation.

Security notes, stated plainly:

- This is static-static Diffie-Hellman. The PSK is fixed for the life of the two
  keys. There is no forward secrecy: whoever later steals either secret key can
  derive the PSK and read recorded traffic. That is the same property as any
  Telesthete PSK today. Session epochs still give each session its own data key
  (SPEC §3.1), and rekeying (a new worker keypair) gives a new PSK.
- Using one Ed25519 key for both signing and X25519 is common practice
  (libsodium provides the conversion for exactly this) and I understand it to be
  considered sound in the literature, but I have not re-derived that argument.
  If the reviewer prefers, the application can generate a separate X25519 key
  and send its public half in the knock; the derivation then starts at
  `shared`. That is a Rook protocol change, so it is listed as a decision.
- Changing the hub's key changes every PSK. That is the application's concern.
- The `band_id` is a stable cleartext identifier for the pair, visible to the
  relay and to on-path observers. That is already true of any PSK.

### 4.5 Revoke, rekey, both sides

- **Approve.** The application derives the PSK and calls `add_key`.
- **Revoke at the hub.** `remove_key`. Frames under that key are refused from
  that moment, before decryption.
- **Revoke at the peer.** The peer stops its Band and discards its secret. How
  the other side learns of it is the application's protocol (for Rook, a
  message or the HTTPS knock).
- **Rekey.** The peer makes a new keypair, the application derives the new PSK
  and calls `add_key(new)`. The peer starts a Band under the new PSK. The
  application calls `remove_key(old)` when it chooses. Both keys can be live
  meanwhile; they are different bands.

### 4.6 Existing peers and interop

- No frame, field, key derivation or channel type is added or changed. A peer
  built against today's Telesthete can use a pairwise PSK if the application
  hands it one; it cannot tell the difference.
- The relay routes by `band_id` as today.
- A `Band` created without `transport=` behaves exactly as now.

### 4.7 Versioning

No wire version. `pairwise_psk` is versioned by its salt string; a future
derivation would use `telesthete-pairwise-v2`. Library versions: Python
`0.3.0 → 0.4.0`, Rust crates `0.2.x → 0.3.0` (minor, additive).

### 4.8 Vectors and tests

- `tests/vectors.json` gains `pairwise`: two Ed25519 seeds, their public keys,
  both X25519 conversions, the shared secret, the PSK string and its `band_id`;
  plus negative cases (small-order public key, equal keys, non-canonical
  encoding) that must be rejected. Python `tests/test_pairwise.py` and Rust
  `crypto::tests::pairwise_vectors_match_python` check them byte for byte.
- Endpoint tests, both languages, over loopback UDP: 50 keys on one socket,
  each peer reaches only its own Band; frames with an unknown `band_id` never
  reach a decrypt call; `remove_key` while a Channel is mid-message: no delivery
  after return; rekey with both keys live; add and remove while running under
  load.

---

## 5. Requirement 2: a hub built on the relay

### 5.1 Where we start

`telesthete-hub` (crate `telesthitium`) routes ciphertext by `band_id` over UDP,
WebSocket/WSS, WebTransport and AF_UNIX, and holds no keys
(`rust/telesthitium/README.md`, `CONFORMANCE.md`). The application hub's logic
and plugins are Python (Rook plan, PL3). The hub needs one public address that
terminates every transport and is also the endpoint for every peer's key.

The useful fact: the relay already treats any registered sink as a peer
(`Registry::connect`, `registry.rs:278`), and one `PeerKey` can be in many bands.
Frames carry their `band_id`, so one local connection can carry many bands'
frames without any new framing inside the frame.

### 5.2 The shapes

| | Shape | Who decrypts, who holds keys | Processes | New surface | Main cost |
|---|---|---|---|---|---|
| **H1** | Relay process + application attached over a local socket (new §10.2 "attachment") | The application (Python `Endpoint`). Relay holds no key. | 2 | One relay listener, ~4 message types; Python `Endpoint(attach=)` | Two services to install and supervise |
| H2 | Relay process + application as an ordinary UDP peer on loopback | The application. Relay holds no key. | 2 | None in the relay | Application must send a presence frame into every band every few seconds or the relay evicts it (15 s TTL); each band needs 2 packets before the relay will deliver to it; one relay UDP queue and writer task per band; relay band cap applies |
| H3 | Relay embedded in the Python process (PyO3 module), attached in-process | The application. Relay holds no key. | 1 | Python bindings for the relay; same attachment messages over an in-process channel | New build matrix (maturin wheels for x86_64 and aarch64 Linux at least); a relay fault takes the hub down; a hub restart drops every WebSocket connection |
| H4 | Relay decrypts: Rust Bands inside the relay, plaintext messages to Python over IPC | The relay. | 2 | A new plaintext message IPC | The relay is no longer keyless; a second API for application messages; Python cannot use the library directly |
| H5 | Pure Python hub: UDP, WebSocket and WebTransport servers in Python | The application. | 1 | A second relay implementation | Duplicates `telesthitium` and its 77 conformance tests |

### 5.3 Recommendation: H1

- The relay stays exactly what SPEC §10 says: keyless, routing by `band_id`.
  Its existing behaviour for every existing band is unchanged; the attachment is
  a listener that is off unless `HUB_ATTACH_PATH` is set.
- Keys and decryption live in the Python process, where the application decides
  who gets a key. The library code is the same `Band` used everywhere else.
- A Python hub restart (upgrade, crash) does not drop WebSocket peers or UDP
  NAT mappings: the relay keeps them, the application reattaches, and the links
  recover through section 7. A relay restart is equally isolated.
- The Rook installer already downloads `telesthete-hub` release binaries for
  both architectures, so H1 adds a config line, not a build pipeline.
- H3 can be added later without redesign: the attachment messages are the seam,
  and an in-process attachment is the same messages over a channel.

H2 is the fallback if the relay must not change at all: it works with today's
relay and the same Python `Endpoint` in UDP mode, at the costs in the table.

### 5.4 The attachment (relay side)

New SPEC §10.2, exact text in section 10. In short:

- **Listener.** `HUB_ATTACH_PATH=<path>`: an AF_UNIX `SOCK_STREAM` socket,
  created mode `0660`. Filesystem permissions on the socket and its directory
  are the access control, as for §9.4. Off by default.
- **Messages.** `u32 BE length || u8 type || body`, where `length` counts the
  type byte and body. The federation link uses the same layout
  (`federation.rs:24-29`), so the codec is shared.

| Type | Name | Direction | Body |
|---|---|---|---|
| `0x01` | ATTACH | application → relay | 16-byte `band_id` |
| `0x02` | DETACH | application → relay | 16-byte `band_id` |
| `0x03` | FRAME | both | one whole Telesthete frame (≥ 43 bytes) |
| `0x04` | REFUSED | relay → application | 16-byte `band_id` + 1-byte reason (`0x01` band cap, `0x02` peer cap) |

- **Semantics.** An attachment is one connection peer (`PeerKey::Conn`) that is
  a member of every band it attached: eligible at once, never idle-pruned,
  removed from every band on disconnect. A FRAME from the application is
  relayed exactly as a frame from any connection peer (to every other eligible
  peer in that band, with §10.1 addressed routing and the federation tail); a
  FRAME for a band it has not attached is dropped. The relay sends the
  application every frame delivered to it in any attached band. ATTACH and DETACH
  are idempotent. Unknown types are skipped. A length above 4 MiB + 1 closes the
  attachment. Several attachments may attach the same band; each receives.
- **Code.** A new `attach.rs` beside `unix.rs`, reusing `Registry::connect`,
  `forward` and `disconnect`. Roughly 250 lines plus tests.

### 5.5 The attachment (application side)

`Endpoint(attach=path)` (Python) or `Endpoint::attach(path, …)` (Rust):

- Connects, retrying with backoff (0.5 s doubling to 5 s) while the socket is
  missing. On every (re)connect it sends ATTACH for every key it holds.
- `add_key` sends ATTACH, then one HELLO into the band (the relay forwards it to
  the peer if the peer is connected, which is how a running peer learns the
  application restarted). `remove_key` sends DETACH.
- Every Band's single peer address is the constant `("attach", path)` (Python)
  or a fixed synthetic `SocketAddr` (Rust). A peer that roams, reconnects over a
  different transport, or holds UDP and WebSocket at once is still one peer to
  the Band: the relay fans the application's frames out to every path the peer
  has in the band, and the peer's frames from any path reach the application.
- REFUSED is surfaced as a log line and an `on_refused(band_id, reason)`
  callback. Section 4's guarantees for `remove_key` hold unchanged.

### 5.6 The data path

```
peer (worker)                    relay (telesthete-hub, keyless)           application hub (Python)
Band(pairwise PSK)  --UDP 7474-->  registry[band W]  --attachment FRAME-->  Endpoint: band_id W -> Band(W)
                    --or WSS /band->     |                                   decrypt, Control, Channel
                                         |  <--attachment FRAME------------  reply under the same key
                    <--------------------+
```

Decryption happens only in the peer and in the application. The relay sees
`band_id`, `channel_type`, `channel_id`, sequence and ciphertext, as today.

### 5.7 One-command install on a small VM

The operator runs one command on a fresh VM, for example

```
curl -fsSL https://<hub-release>/install-hub.sh | sudo sh -s -- --domain hub.example.com
```

That script is Rook's, not Telesthete's. What it has to do for the Telesthete
part:

1. Download `telesthete-hub-$(uname -m)-linux.tar.gz` and its `.sha256` from
   the Telesthete release (an existing asset contract), verify, install to
   `/usr/local/bin`.
2. Install `telesthete-hub.service` (the existing hardened unit) with
   `/etc/telesthete-hub.env`:

   ```
   HUB_BIND=0.0.0.0:7474
   HUB_WS_BIND=127.0.0.1:7475
   HUB_ATTACH_PATH=/run/telesthete/attach.sock
   ```

   The unit already has `RuntimeDirectory=telesthete`; the installer adds a
   drop-in that sets `RuntimeDirectoryMode=0750` and `Group=` to a group the
   application's user belongs to, so the application can connect.
3. The application's service starts `After=` and `Wants=` the relay.
4. Whatever terminates TLS on TCP 443 for the application's HTTPS site routes
   WebSocket upgrades on `/band` to `127.0.0.1:7475` and everything else to the
   application. (Caddy does this in a few lines with automatic certificates; the
   relay's native WSS is the alternative if the site lives on another port.)
5. Open TCP 80 and 443 and UDP 7474. On an Oracle VM that means both the VCN
   security list and the instance's own iptables rules.
6. WebTransport stays off unless a client needs it: it needs UDP 443 and a
   certificate the relay can read (`HUB_WT_BIND`, `HUB_TLS_CERT`,
   `HUB_TLS_KEY`). No Rook client uses it today.

Workers then reach the hub at `udp://<host>:7474` or `wss://<host>/band`.

### 5.8 Existing peers, versioning, tests

- **Existing relays and peers.** The attachment is a new, opt-in listener.
  Nothing changes for any band that has no attachment. An attachment looks to
  every other peer like one more peer in the band.
- **Versioning.** The attachment is a local interface between a relay and an
  application installed together. Unknown message types are ignored, so types
  can be added. If an incompatible change is ever needed, the path changes.
- **Tests.** `CONFORMANCE.md` gains section L:
  L1 ATTACH registers a connection peer; L2 frames from band peers reach the
  attachment; L3 FRAME from the attachment reaches band peers on every transport
  (UDP, WS, WT, AF_UNIX); L4 FRAME for an unattached band is dropped; L5 DETACH
  stops delivery both ways; L6 disconnect detaches everything; L7 the socket is
  created `0660`; L8 the attachment is never idle-pruned; L9 unknown types are
  skipped and an oversize length closes; L10 REFUSED on band and peer caps; L11
  one attachment carries 1,000 bands. Python and Rust Endpoint integration tests
  start the built `telesthete-hub` binary with `HUB_ATTACH_PATH` and run a peer
  over UDP and one over WS through it.

---

## 6. Requirement 3: WebSocket clients

### 6.1 What the relay does today

`ws.rs`: path `/band` (configurable with `HUB_WS_PATH`), plain WS behind a TLS
proxy or native WSS; each binary message is one frame, no length prefix; the
connection's band is learned from its first frame and pinned (frames naming
another band are dropped); text, ping and pong messages only reset the idle
timer; the relay pings every 20 s and closes after 60 s of silence; a band or
peer cap closes the connection. Nothing reaches a WS client before its first
frame, because until then the relay does not know its band.

### 6.2 Python client

`telesthete.transport.websocket.WebSocketTransport(url, *, ssl=None,
open_timeout=10.0, reconnect_min=0.5, reconnect_max=30.0)`. Same transport
interface as `UDPTransport`, plus `peer_address`, the `(host, port)` of the URL.

```python
from telesthete import Band
from telesthete.transport.websocket import WebSocketTransport

ws = WebSocketTransport("wss://hub.example.com/band")
band = Band(psk, transport=ws, reconnect=True)
await band.start()
band.connect_peer(*ws.peer_address)
```

- Sends each frame as one binary message. Ignores text messages and anything
  under 43 bytes. Every received frame is attributed to `peer_address`.
- `send(dest, packet)` sends regardless of `dest` (there is one remote); frames
  sent while disconnected are dropped, as UDP loss would drop them.
- Reconnects on close or error with backoff (`reconnect_min` doubling to
  `reconnect_max`, ±20 % jitter). After each (re)connect it calls the Band's
  connected hook, which sends HELLO at once when the Band has `reconnect=True`,
  so the relay learns the band immediately.
- Answers pings (the library does). `ssl=None` verifies against system roots;
  pass an `ssl.SSLContext` for a private CA or a pinned certificate.
- Dependency: the `websockets` package as an optional extra,
  `pip install telesthete[ws]`. The base install is unchanged.

### 6.3 Rust client

Behind a new cargo feature `ws` (off by default, so default dependencies do not
change), using `tokio-tungstenite` 0.30 with rustls, the versions the relay
already uses.

```rust
let band = Band::connect_ws("wss://hub.example.com/band", psk, "worker", BandOptions::default()).await?;
let hub = band.ws_peer().expect("ws band");   // SocketAddr standing for the relay
band.set_reconnect(true);
band.connect_peer(hub).await?;
```

- `Band::connect_ws(url, psk, hostname, opts) -> Result<Band, BandError>`;
  connection failures come back as `BandError::Io` (no new variant).
- `Band::ws_peer(&self) -> Option<SocketAddr>`: the address resolved at first
  connect. It stays the Band's peer identity across reconnects even if DNS later
  resolves elsewhere.
- Same reconnect, ping and drop rules as Python.

### 6.4 Spec alignment

SPEC §9.3 is rewritten to describe what the relay already does and what a client
must do (exact text in section 10). §10's heading loses "Future". §9.6 gets
"served by the reference relay; neither reference library has a client".
WebTransport clients stay out of scope; nothing in Rook needs them yet.

One thing the spec should say and does not: through a relay, a peer cannot tell
other peers apart by address. The relay sends every UDP frame from its own
socket and every WS frame down the one connection, and does not reveal which
peer sent it. A Band keyed by address therefore sees one peer per relay path.
That is fine for two-member bands (the pairwise case) and was already true for
UDP peers behind the relay. The §10 text in section 10 says so.

### 6.5 Existing peers, versioning, tests

- No wire change: §9.3 already defined the framing. Existing UDP Bands are
  untouched; the transport is chosen by the application.
- Tests, both languages: against the built relay, a WS Band and a UDP Band in
  the same band exchange Control and a 100 KB Channel message both ways; the
  relay is killed and restarted and the WS Band reconnects and the Channel
  recovers (section 7); a WS client over native WSS with a self-signed
  certificate trusted by an explicit context; text messages and short binaries
  are ignored.

---

## 7. Requirement 4: links that survive restarts

### 7.1 The five conditions

| # | Condition | Python bug? | Spec gap? | Fix |
|---|---|---|---|---|
| 1 | Channel keyed by id alone (`band.py:117`, `365-366`) | A Python limitation: Rust keys by `(peer, id)` (`channel.rs:733`) and §3.3's watermark is per `(peer, type, id)`. | Yes: §6 never says a Channel's identity is `(peer, channel_id)`. | Spec states the identity and allows an implementation to restrict an id to one peer. Python left as is (decision 5). Moot for pairwise keys: every key has one peer. |
| 2 | Receiver must create the channel before the SYN (`band.py:377`, `395-398`; Rust `channel.rs:692-701`) | Shared by both libraries. | Yes: §6.5 shows `CLOSED → ESTABLISHED` on SYN but not how a channel comes to exist for an unsolicited SYN. | Optional accept hook (§6.7 b). Without it, unchanged. |
| 3 | A lost HELLO is never resent (`band.py:499-514`, `575-595`; Rust `band.rs:296-317`) | Shared by both. | Yes, and §2 calls Control "Always reliable", which neither library does. | §4.11 reconnection: HELLO retry, re-HELLO after silence. Correct the §2 text. |
| 4 | A restarted peer cannot reuse a channel (`channel.py:151-152`, `186-189`, `377-381`) | Yes: after teardown Python neither zeroes its inner sequences (so a reopen sends SYN at seq ≠ 0, against §6.3) nor accepts a SYN whose seq differs from its expected one (Rust does, `channel.rs:350`); and there is no way to drop a cached channel. | Yes: what happens to a Channel when the peer's session changes is undefined; both libraries keep the old state, so the new SYN looks like a duplicate. | §6.7 reset on a newer session and on GOODBYE; §6.7 new-incarnation SYN; Python reopen fixes. |
| 5 | A silent peer tears the channel down for good (`channel.py:41`, `506-511`; peer eviction `peer.py:33`) | Yes: the torn-down Python channel cannot be reopened (as in 4). | Yes: §6.4 has no give-up rule and §6.2 defines RST but never says when to send it, so a side that gave up and a side that did not stay out of step forever. | §6.7: give-up resets the channel (reopenable); RST answers data on a closed channel; §4.11 re-HELLO brings the peer back. |

### 7.2 The new behaviour

All of it is opt-in per Band, through one option, and on by default for the new
`Endpoint`:

- Python: `Band(..., reconnect=False)`. Rust: `Band::set_reconnect(&self, on:
  bool)` (a setter, because adding a field to `BandOptions` would break code that
  builds it without `..Default::default()`).

With `reconnect` on (exact spec text in section 10, §4.11 and §6.7):

**Peers (§4.11).**

1. `connect_peer(addr)` makes `addr` a *target* for the Band's lifetime.
2. Until a HELLO or HELLO_ACK from a target is received, HELLO is resent after
   0.5 s, then 1, 2, 4, then every 5 s (`KEEPALIVE_INTERVAL`). Each resend is a
   new packet with a fresh sequence and the same epoch. A receiver already
   answers a repeated HELLO with HELLO_ACK in both libraries
   (`band.py:228-255`, `control.rs:380-399`), so this needs nothing new from it.
3. When nothing authenticated has arrived from a target for `PEER_TIMEOUT`
   (15 s), the target is lost: evicted as today (§4.5), and step 2 starts again.
4. On a HELLO or HELLO_ACK with a newer epoch (§4.3), or a GOODBYE, from any
   peer, every Channel with that peer is reset (§6.7 c).

**Channels (§6.7).**

- *Accept (b).* Python `Band.on_channel(cb)`, Rust
  `Band::take_incoming_channels(&mut self) ->
  Option<mpsc::UnboundedReceiver<ChannelEndpoint>>`. When registered, a SYN for
  an unknown channel id from a peer whose session is known creates the channel,
  hands it to the application, then processes the SYN. Independent of
  `reconnect`.
- *Reset (c).* State `CLOSED`, both inner sequences back to 0, send queue,
  unacked frames, reorder buffer and partial reassemblies discarded,
  retransmission stopped. The §3.3 outer watermark is left alone (a newer
  session rebases it already). The channel object stays usable. The application
  is told: Python `Channel.on_reset(cb(reason))`, Rust
  `ChannelEndpoint::resets() -> broadcast::Receiver<ResetReason>` with a new
  `#[non_exhaustive] enum ResetReason { PeerRestarted, PeerLeft, PeerReset,
  PeerUnresponsive }`. Python also gets a read-only `Channel.state`.
- *Reopen (d).* A `CLOSED` channel can be opened again; its SYN uses inner seq
  0. A responder in `CLOSED` takes the SYN's seq as the start (`rcv_next = seq +
  1`), as Rust already does. (The second half is always on in Python: it only
  changes a case that today wedges.)
- *New incarnation (e).* In `ESTABLISHED` or `FIN_SENT`, a SYN without ACK that
  arrives after the peer has acknowledged our SYN or SYN+ACK means the peer
  reopened: reset, then handle the SYN as in `CLOSED`. Before that
  acknowledgement it is a retransmission and is answered as today. A replayed
  old SYN cannot trigger this: it fails the §3.3 outer watermark first.
- *RST (f).* An authenticated data or FIN frame on a channel that is `CLOSED`,
  or that does not exist, is answered with RST (`flags=0x08`, `ack=0`,
  `window=0`, `seq=0`, no data, not retransmitted). Never for a SYN (a SYN
  that beats the application's `channel()` call must still succeed on its
  retransmission), never for a pure ACK, never for an RST.
- *Give-up (g).* After `MAX_RETRIES` the channel is reset with reason
  `PeerUnresponsive` instead of becoming unusable. In Rust the connection task
  stays alive in `Closed` instead of exiting.

### 7.3 How the cases play out

- **Peer restarts.** New epoch HELLO (retried until answered) → receiver resets
  its channels → the restarted peer opens channel 0 → the receiver's channel is
  `CLOSED` and accepts the SYN (or the accept hook creates it). The application
  sees `on_reset(PeerRestarted)`, then messages again. No re-pairing: the key
  did not change.
- **Hub application restarts behind the relay.** It reattaches and sends one
  HELLO per band → each connected peer sees a newer epoch → resets → reopens →
  the accept hook creates the channel on the new process.
- **Peer roams or switches UDP to WebSocket.** Behind the relay, the application
  never sees an address change (section 5.5). The peer's own Band talks to one
  fixed relay address. Loss during the switch is retransmission; past ~5 s it
  is give-up, then RST or reopen.
- **Short outage, no restart.** Whichever side gave up resets; if it was the
  initiator it reopens and the other side resets on the new-incarnation SYN; if
  it was the responder, the initiator's next data draws an RST, it resets and
  reopens.
- **Two processes with one key** (for example a new process started before the
  old one exits). The newer epoch wins at the receiver; the older process's
  frames fail authentication there (§4.3 already guarantees this).

### 7.4 Existing peers and interop

Every behaviour above is a local reaction to frames that exist today, so there
is nothing to negotiate:

- With `reconnect` off (the default for `Band`) nothing changes, except the
  always-on Python fix in (d), which changes only a case that today never
  establishes.
- A new peer next to an old one: extra HELLOs are answered as today. An old
  receiver of a reopened channel stays stuck exactly as today (no worse). An
  old peer that receives an RST tears down, which both libraries already do;
  it only receives one when its channel is already dead.
- The only new emitted frame is RST, whose format §6.2 already defines.

### 7.5 Versioning and tests

No wire version. Tests, both languages:

- Over the mock transports: each of the conditions 2 to 5 as a failing test
  first, then passing with `reconnect=True`, and unchanged with it off.
- Over loopback UDP and through the relay: kill and restart each side 20 times
  while messages flow; drop 20 % of datagrams including HELLOs; partition for
  3 s and for 20 s; WS reconnect; two processes with one key.
- Cross-language: a Python peer and a Rust peer restarting each other (no such
  live interop test exists today).

---

## 8. Requirement 5: a minimal client that can answer calls

### 8.1 Why §12.1 is not enough

§12.1 and §12.2 size the minimal and ESP32 clients as Stream and Control only.
Streams are lossy and Control is not retransmitted, so neither carries a
request and its reply reliably. Answering calls needs the §6 Channel.

### 8.2 The link profile

New SPEC §12.6, exact text in section 10. A *link client* implements:

- §1 framing, §3.1 key derivation, the §3.2 baseline cipher only.
- §3.3: one CSPRNG-seeded sequence source; replay watermark per
  `(peer, channel_type, channel_id)`.
- §4.3/§4.4 HELLO and HELLO_ACK with `capabilities: []`,
  `ciphers: ["chacha20-poly1305"]` and a session epoch that increases across
  restarts (persisted when the device has no clock); §4.5 KEEPALIVE; §4.2
  ignore unknown types; §4.8 GOODBYE.
- §4.11 reconnection.
- §6 Channel for at least one channel id, as initiator: inner sequence,
  cumulative ACK, retransmission under fresh outer sequences, deduplication.
  It MAY advertise a window as small as 1 and drop every out-of-order frame
  (a sender may never exceed the advertised window, §6.4, so this is
  conformant). §6.6 envelope on every message; reassembly with a local cap on
  message size, which the application must respect.
- §6.7 reset, reopen, new incarnation and RST.
- One transport: UDP (§9.1) or WebSocket (§9.3).

It may omit AES, Streams, Board, Drop, discovery, AF_UNIX, WebTransport,
StreamHeader/dmabuf, KEYFRAME_REQ/RATE_HINT, addressed routing and the accept
hook.

### 8.3 ESP32 estimate

Not built. Estimates from the Python reference's sizes and typical C ratios.

| Part | C lines (est.) |
|---|---|
| Header pack/unpack, constants | 60 |
| Key derivation, AEAD glue (mbedTLS SHA-256, HKDF, ChaCha20-Poly1305, all in ESP-IDF) | 120 |
| Control: HELLO/ACK/KEEPALIVE/GOODBYE (cJSON), epoch in NVS, §4.11 retry and liveness | 250 |
| Channel: state machine, inner seq, ACK, RTO, window 1-4, §6.7 | 450 |
| §6.6 envelope send and bounded reassembly | 130 |
| Transport: UDP (lwIP sockets) or WebSocket (`esp_websocket_client`) | 100 |
| **Total** | **≈ 1,100 (range 1,000-1,300)** |

- **Flash:** about 20 to 30 KB for this code; mbedTLS is already linked when
  Wi-Fi and TLS are. Pairwise PSK derivation (section 4.4) needs Ed25519 to
  X25519 conversion and X25519, which mbedTLS does not fully provide; the
  libsodium component for ESP-IDF does, at a cost I estimate at 30 to 60 KB of
  flash. It runs once, at approval; the device then stores the 64-character PSK.
- **RAM:** receive and send buffers, 1.5 KB each on UDP (one Channel frame is at
  most 27 + 19 + 1024 + 16 = 1,086 bytes, so a 2 KB WebSocket buffer is
  enough); unacked frames up to window × 1,043 bytes (about 4.2 KB at window 4);
  reassembly up to the local cap (for example 16 KB); a few hundred bytes of
  state. About 10 KB plus the reassembly cap. An ESP32 has about 320 KB of DRAM.
- **Compare** §12.2's ESP32 row: about 300 lines, Stream and Control only.

### 8.4 Existing peers, versioning, tests

A profile is a subset of behaviour, not a wire feature, and needs no capability
bit: the window is advertised per frame and the Channel is baseline. Tests: a
"link profile" test configuration in each library (window 1, baseline only,
reassembly cap 16 KB, no Streams) that must interoperate with the full Band over
UDP and through the relay, restarts included.

---

## 9. Requirement 6: stale text

Proposed edits. None of them changes behaviour.

### 9.1 PROTOCOL_STATUS.md

| Line | Now | Proposed |
|---|---|---|
| 18 | Cipher negotiation, Rust: `◑ structs + select_cipher` | `✅` (per-peer keys and suites: `transport.rs:218-229`, `control.rs:333-350`) |
| 27 | KEYFRAME_REQ / RATE_HINT: `⬜ ⬜` | `✅ ✅` (messages sent and parsed; acting on them is the application's) |
| 29 | StreamHeader + dmabuf, Python: `⬜` | `✅` (`protocol/streamheader.py`) |
| 30 | Channel, Python: `◑ PoC (no real retransmit)` | `✅` |
| 32 | Capability announce, Rust: `✅ structs` | `✅` |
| 33 | Board / Drop: `⬜ ⬜` | `✅ ✅` |
| 40 | LAN discovery, Rust: `⬜` | `✅` (`discovery.rs`) |
| 41 | AF_UNIX, Python: `⬜` | `✅` (`transport/unix.py`, SEQPACKET) |
| 42 | WebTransport: `⬜ ⬜` | `⬜ client ⬜ client — served by the relay` |
| 43 | WebSocket fallback: `⬜ ⬜` | `⬜ client ⬜ client — served by the relay; clients designed in spec/rook-additions.md` |
| 44 | Telesthetium hub: `⬜ ◑ (telesthitium)` | `— ✅ (telesthitium, see CONFORMANCE.md)` |
| 47-58 | "Notable gaps": Rust cipher data path, KEYFRAME wiring, Python Channel retransmit stub | Replace with: "WebSocket and WebTransport clients (none in either library); link lifecycle across restarts (spec/rook-additions.md §7); many keys on one endpoint (§4)." |
| 70 | `109 Rust unit/integration tests` | `113` |
| 71 | `77 hub tests` | unchanged (56 unit + 21 integration) |

Also add one line under the title: "LIBRARY_COMPLETION.md is the verified audit;
this table follows it."

### 9.2 SPEC.md

| Where | Now | Proposed |
|---|---|---|
| §2 table, Control row | "Band management, signaling. Always reliable." | "Band management, signaling. Not retransmitted, except that HELLO is retried under §4.11." |
| §9.3 heading and body (lines 819-834) | "WebSocket (Legacy fallback) — Future … Not yet implemented." | Replaced by the §9.3 text in section 10. |
| §9.6 last line (938-939) | "Not yet implemented." | "The reference relay serves WebTransport; neither reference library has a client." |
| §10 heading (943) | "Telesthetium Hub — Future" | "Telesthetium Hub (relay)", plus the paragraph in section 10. |
| §10.1, line 1005 | "Rook carries one §6.6 chunk per frame." | "An application may carry one §6.6 chunk per frame." |
| §10.1 Compatibility, line 1052-1053 | "'Old' below means the receivers deployed before this addendum (Rook `9f0d6df`)" | Add: "These rows describe Rook 0.4 deployments; later applications use the reference libraries (first row)." |
| §11.1 (1141-1147) | "Rook's agent protocol uses msgpack-encoded messages … Real-time modality data … uses Streams." | "Rook (v1) sends JSON messages, one per Channel message (§6, §6.6), over one Channel per peer, and carries its own streams as messages on that Channel. See the Rook repository's `docs/protocol.md`." |
| §12 intro (1153-1156) | "the Rust crate … is the v1.1 reference implementation (adds AF_UNIX, dmabuf, capability negotiation)" | "The Python package and the Rust crate are both v1.2 reference implementations, pinned to each other by `tests/vectors.json`. `rust/telesthitium` is the reference relay (§10)." |
| §12.2 table | Python/Rust "~3000", ESP32 "WebSocket ~300 Stream + Control only" | Python ≈ 4,300 and Rust ≈ 8,300 lines (`wc -l` of `telesthete/` and `rust/telesthete/src`, in-file tests and comments included); ESP32 row: "UDP or WebSocket, ≈ 1,100, link profile (§12.6)"; keep the browser rows. |
| §13 | — | Rows from section 10. |
| §16 | — | Changelog row from section 10. |

### 9.3 Elsewhere

- `README.md:101-102`: "`Board` and `Drop` are reserved." → "`Board`
  (replicated state) and `Drop` (resumable file transfer)."
- `README.md:105-106`: add "(WebSocket and WebTransport are served by the relay;
  library clients: see PROTOCOL_STATUS.md)".
- `LIBRARY_COMPLETION.md:63-65` (WS/WT clients scoped out of the references):
  add "Superseded for WebSocket by spec/rook-additions.md §6."

---

## 10. Appendix to SPEC.md (exact text)

This is the text to add to SPEC.md as an appendix, unchanged by the
implementation PRs except to fill in vector values. Section numbers are where
each part belongs; the appendix lists them together so the review reads one
block.

````markdown
## Appendix R — Additions for application hubs (v1.2 addendum 2)

Additive and wire-compatible: no frame, field, key derivation or channel type
changes, and `PROTOCOL_VERSION` stays 3. Everything here is either a local
behaviour of a receiver, a library API, or a relay-local interface.

### §3.6 Many keys on one endpoint

A PSK determines its `band_id` (§3.1), and every frame carries its `band_id` in
cleartext. An implementation MAY therefore hold many PSKs on one transport (one
socket, one relay attachment) and choose the key for a frame by its `band_id`
before decrypting it. Such an *endpoint*:

1. MUST drop a frame whose `band_id` it holds no key for, without attempting
   decryption.
2. MUST, once a key has been removed, neither decrypt nor deliver any further
   frame bearing that key's `band_id`.
3. Treats each key as an independent band: its own peers, session state, replay
   watermarks and sequence source (§3.3).

Peers see nothing different: each sees an ordinary band.

### §3.7 Pairwise keys from Ed25519 keypairs

Two parties that hold Ed25519 keypairs and know each other's public key can
derive a PSK for their pair without sending a secret:

```
own_x_secret  = clamp(SHA-512(own_seed)[0:32])         (RFC 7748 §5)
peer_x_public = Montgomery u-coordinate of the peer's Ed25519 point
shared        = X25519(own_x_secret, peer_x_public)
(a, b)        = the two Ed25519 public keys (32 bytes each), sorted bytewise
okm           = HKDF-SHA256(salt = "telesthete-pairwise-v1", ikm = shared,
                            info = a || b, L = 32)
PSK           = lowercase hexadecimal of okm (64 ASCII characters)
```

An implementation MUST reject a peer public key that is not a canonical
encoding of a curve point or that has small order, an all-zero `shared`, and
`a == b`. The PSK is then used as any PSK (§3.1). Pinned by
`tests/vectors.json` `pairwise` (Python `telesthete.pairwise_psk`, Rust
`telesthete::pairwise_psk`).

Static-static agreement gives no forward secrecy: the PSK lasts as long as both
keypairs. A new keypair on either side gives a new PSK and a new `band_id`.

### §4.11 Reconnection (optional)

A peer that implements reconnection:

1. Treats every address it was asked to connect to as a *target* for as long as
   the band runs.
2. Until it has received a HELLO or HELLO_ACK from a target, resends HELLO to it
   after 0.5 s, 1 s, 2 s, 4 s and then every `KEEPALIVE_INTERVAL`. Each resend
   is a new packet under a fresh sequence, carrying the same session epoch.
3. Considers a target lost when nothing authenticated has arrived from it for
   `PEER_TIMEOUT`; evicts it as §4.5 says and resumes step 2.
4. On receiving, from any peer, a HELLO or HELLO_ACK with a newer session epoch
   (§4.3), or a GOODBYE, resets every Channel it has with that peer (§6.7).

Every peer, with or without reconnection, MUST answer a HELLO with a HELLO_ACK,
including a repeated HELLO with an epoch equal to the last one seen (the
reference libraries already do). A HELLO with an older epoch is still ignored
(§4.3).

### §6.7 Channel lifecycle

(a) **Identity.** A Channel is identified by the pair (peer, `channel_id`). An
implementation MAY allow a `channel_id` to be used with only one peer at a time
in a band.

(b) **Accepting.** On receiving a SYN for a `channel_id` it has no Channel for,
from a peer whose session it knows, a peer MAY create the Channel in CLOSED,
hand it to the application, and process the SYN (§6.5). A peer that does not
accept drops the frame.

(c) **Reset.** Resetting a Channel sets it to CLOSED, sets both inner sequences
to 0, discards queued, unacknowledged, out-of-order and partially reassembled
data, and stops retransmission. It leaves the §3.3 outer watermark alone. The
Channel can be opened again (d). A peer resets a Channel when the peer at the
other end starts a new session or says GOODBYE (§4.11 step 4), when it receives
RST, and when it gives up retransmitting (g). The application should be told.

(d) **Reopening.** A CLOSED Channel may be opened again. Its SYN uses inner
sequence 0. A Channel in CLOSED that receives a SYN takes the SYN's sequence as
the start of the peer's sequence space (expects `seq + 1` next).

(e) **New incarnation.** A Channel in ESTABLISHED or FIN_SENT that receives a
SYN without ACK, after the peer has acknowledged this side's SYN or SYN+ACK,
resets (c) and then processes the SYN as in CLOSED. Before that acknowledgement
the SYN is a retransmission and is answered as before.

(f) **RST.** A peer that receives an authenticated frame carrying data or FIN
for a Channel it holds in CLOSED, or holds no Channel for, SHOULD answer with
RST: flags `0x08`, `ack_num` 0, `window` 0, `seq` 0, no data. RST consumes no
sequence and is never retransmitted. A peer MUST NOT send RST in answer to a
SYN, a pure ACK, or an RST.

(g) **Giving up.** A sender that stops retransmitting after a bounded number of
attempts resets the Channel (c) instead of leaving it unusable.

A peer that implements none of (b) to (g) interoperates as before. A peer that
implements them interoperates with one that does not; the old peer simply does
not recover where it did not recover before.

### §9.3 WebSocket (fallback) — replaces the existing §9.3

For networks that block UDP and for clients without WebTransport (§9.6). A
client connects outbound to a relay (§10) over WSS, normally on port 443. The
reference relay serves the path `/band` (configurable).

- Each Telesthete frame is one **binary** WebSocket message. No length prefix:
  the message preserves the boundary. Receivers ignore text messages and binary
  messages shorter than `MIN_PACKET_SIZE`.
- A connection carries one band. The relay learns the band from the first frame
  and drops later frames naming another band. It delivers nothing to the
  connection before that first frame, so a client SHOULD send one (normally
  HELLO) as soon as it connects. A client in several bands opens one connection
  per band.
- The reference relay pings an idle connection every 20 s and closes one that
  has sent nothing, pongs included, for 60 s. Clients answer pings.
- A client that loses its connection SHOULD reconnect with exponential backoff
  and jitter, and SHOULD send HELLO on each new connection. Frames sent while
  disconnected are lost, as on UDP; Channels retransmit.
- All frames on a connection come from, and go to, the relay. The client sees
  every other peer in the band at the relay's address (§10).

**Caveat (the reason §9.6 exists):** WebSocket runs over TCP. A lossy Stream
(§5) carried over TCP is silently turned reliable and in-order — a single lost
segment head-of-line-blocks every Channel and Stream multiplexed on that socket,
and "drop the late frame" (§5.2) becomes "retransmit the frame you wanted
dropped." Acceptable on a LAN with near-zero loss; pathological for real-time
video over a lossy path. Prefer §9.6 WebTransport whenever the client supports
it; fall back to §9.3 only when it does not.

### §10 Telesthetium Hub (relay) — replaces the heading "Telesthetium Hub — Future"

Added after the first paragraph:

The reference relay is `rust/telesthitium` (binary `telesthete-hub`); its
conformance inventory is `rust/telesthitium/CONFORMANCE.md`. A relay forwards
frames without saying which peer sent them: a peer reached through a relay sees
the other peers of the band at the relay's address. Peers that must tell each
other apart through a relay should use one band per pair (§3.6, §3.7).

### §10.2 Local attachment (optional)

A relay MAY offer an *attachment*: a local stream socket through which one
application process takes part in many bands, so that the relay is the public
address for every transport while the application holds the keys (§3.6). The
relay still holds no key.

- **Socket.** The reference relay listens on an AF_UNIX `SOCK_STREAM` socket at
  `HUB_ATTACH_PATH` (no listener when unset), created with mode `0660`.
  Filesystem permissions on the socket and its directory are the access
  control, as in §9.4.
- **Messages.** Each message is `length` (uint32 BE, the number of bytes that
  follow it), `type` (uint8), then the body:

  | Type | Name | Direction | Body |
  |------|------|-----------|------|
  | 0x01 | ATTACH | application → relay | `band_id` (16 B) |
  | 0x02 | DETACH | application → relay | `band_id` (16 B) |
  | 0x03 | FRAME | both | one Telesthete frame (≥ `MIN_PACKET_SIZE`) |
  | 0x04 | REFUSED | relay → application | `band_id` (16 B), reason (uint8): 0x01 band cap, 0x02 peer cap |

  Receivers skip messages of unknown type. A relay closes an attachment that
  announces a `length` above 4 MiB + 1.
- **Membership.** An attachment is one peer in every band it has attached:
  eligible as a destination at once, never removed for idleness, removed from
  all its bands when the connection closes. ATTACH and DETACH are idempotent.
  If a cap prevents ATTACH, the relay sends REFUSED and the band is not
  attached.
- **Relaying.** The relay sends the attachment, as FRAME, every frame it would
  deliver to that peer in any attached band. A FRAME from the attachment is
  relayed exactly like a frame from any connection peer in its band (§10,
  §10.1). A FRAME whose `band_id` the attachment has not attached is dropped.
- Several attachments may attach the same band; each is a separate peer.

### §12.6 Link profile

The smallest peer that can exchange requests and replies reliably. A *link
client* implements: §1; §3.1; the §3.2 baseline suite only; §3.3; §4.3, §4.4,
§4.5 and §4.8 with `capabilities: []` and `ciphers: ["chacha20-poly1305"]`, and
a session epoch that increases across restarts (persisted when the device has
no reliable clock); §4.2's rule to ignore unknown types; §4.11; §6 for at least
one `channel_id` as initiator, with any advertised window from 1 up (it may drop
every frame outside its window); §6.6 for every message, with a local cap on
reassembled size that the application must respect; §6.7 (c) to (g); and one
transport, §9.1 UDP or §9.3 WebSocket. It may omit everything else. An ESP32
link client is estimated at about 1,100 lines of C.

### §13 Compatibility matrix — rows added

| Feature | Required to use it with a peer |
|---------|--------------------------------|
| Many keys on one endpoint (§3.6) | none — local to the endpoint; peers see ordinary bands |
| Pairwise keys (§3.7) | both parties derive the PSK; nothing on the wire |
| Reconnection (§4.11) | none — answering a repeated HELLO is already required |
| Channel lifecycle (§6.7) | none — local reactions; RST is the existing §6.2 flag |
| Local attachment (§10.2) | none — between a relay and a local application |

### §16 Changelog — row added

| 1.2 (addendum 2) | 2026-11-?? | **Additive, wire-compatible; `PROTOCOL_VERSION` stays 3.** §3.6 many keys on one endpoint (choose the key by `band_id`). §3.7 pairwise PSKs from Ed25519 keypairs (`telesthete-pairwise-v1`). §4.11 optional reconnection (HELLO retry, re-HELLO after silence, channel reset on a peer's new session or GOODBYE). §6.7 Channel lifecycle: identity, accepting, reset, reopening, new incarnation, when to send RST, giving up. §9.3 WebSocket specified as the reference relay serves it. §10 relay no longer "Future"; relays hide the sender. §10.2 optional local attachment. §12.6 link profile. §2 Control is not "always reliable". §11.1 corrected (Rook v1 is JSON over a Channel). |
````

---

## 11. Decisions needed

One line each, with my recommendation first.

1. **Hub shape.** H1, relay plus attachment (recommended); H3, relay embedded
   in Python; H2, application as a loopback UDP peer with no relay change.
2. **Where the Ed25519-to-PSK derivation lives.** In Telesthete with vectors,
   so Python, Rust and C agree byte for byte (recommended); or in Rook.
3. **One key for signing and agreement.** Convert the worker's Ed25519 key to
   X25519 (recommended, no Rook protocol change); or a separate X25519 key in
   the knock.
4. **Lifecycle behaviour on by default?** Opt-in per Band, on by default only in
   the new `Endpoint` (recommended, strictly additive); or on everywhere, since
   it only changes cases that today wedge.
5. **Python condition 1** (`Band.channel(id, other_peer)` returns the channel
   bound to the first peer). Leave it and state the rule in §6.7 (a)
   (recommended; pairwise keys never hit it); or fix it, which changes what an
   existing call returns.
6. **Python WebSocket dependency.** `websockets` as an optional extra
   (recommended); or a hand-written RFC 6455 client in the package.
7. **§2 "Control: Always reliable".** Correct the sentence (recommended); or
   make Control reliable, a large change nothing here needs.
8. **WebTransport in the default hub install.** Off until a client needs it
   (recommended); or on, which needs UDP 443 and a certificate the relay can
   read.
9. **Roaming without the relay.** Not supported: an `Endpoint` on its own UDP
   socket sees a roamed peer as a new address (recommended; use the relay); or
   add WireGuard-style "move the peer to the latest authenticated source".
10. **Attachment socket.** AF_UNIX only (recommended; hubs run on Linux); or
    also TCP loopback with a shared secret, for other hosts.

---

## 12. Implementation plan

Each PR carries its own SPEC.md section from section 10, its tests and a
CHANGELOG line. Sizes are new or changed lines including tests: S < 300, M
300-800, L 800-1,500.

| PR | Delivers | Tests | Depends on | Size |
|---|---|---|---|---|
| 0 | Stale text (section 9): PROTOCOL_STATUS.md, SPEC.md §2/§9.6/§10.1/§11.1/§12/§12.2, README, LIBRARY_COMPLETION note | none (docs) | — | S |
| 1 | Pairwise PSK (§3.7): Python `pairwise_psk`, Rust `pairwise_psk`, `vectors.json` `pairwise` | vectors both languages, negative cases | — | S |
| 2a | Python link lifecycle (§4.11, §6.7): `reconnect`, `on_channel`, `on_reset`, `Channel.state`, reopen fixes, RST, give-up reset | mock-transport tests per condition; live UDP restart/loss/partition | — | L |
| 2b | Rust link lifecycle: `set_reconnect`, `take_incoming_channels`, `resets()`, `ResetReason`, connection task survives give-up | as 2a | — | L |
| 3 | Relay attachment (§10.2): `attach.rs`, config, systemd note, CONFORMANCE L1-L11 | relay unit + integration | — | M |
| 4a | Python `Endpoint` (UDP mode) and `Band(transport=)` (§3.6) | 50 keys one socket, removal, rekey, load | 2a | M |
| 4b | Rust `Endpoint` (UDP mode), internal I/O backend in `Transport` | as 4a | 2b | L |
| 5a | Python `Endpoint(attach=)` | against built relay: UDP and WS peers, relay restart, app restart | 3, 4a | S-M |
| 5b | Rust `Endpoint::attach` (parity) | as 5a | 3, 4b | S-M |
| 6a | Python `WebSocketTransport`, `[ws]` extra, §9.3 text | against built relay: WS↔UDP, reconnect, WSS self-signed | 4a (for `transport=`), 2a (HELLO on connect) | M |
| 6b | Rust `Band::connect_ws`, feature `ws` | as 6a | 4b | M |
| 7 | Cross-language interop and link-profile configuration tests; Python↔Rust restart test; version bumps (Python 0.4.0, crates 0.3.0), SPEC changelog row, release | interop matrix | all above | M |

Order and parallelism:

- **Wave 1, in parallel:** 0, 1, 2a, 2b, 3. None depends on another.
- **Wave 2:** 4a after 2a; 4b after 2b; in parallel with each other.
- **Wave 3, in parallel:** 5a, 6a (Python) and 5b, 6b (Rust).
- **Then 7.**

For Rook's critical path (Python hub, Python worker, relay): 1, 2a, 3, 4a, 5a,
6a, then 7. The Rust PRs (2b, 4b, 5b, 6b) are needed for a Rust core and run
alongside. An ESP32 link client is Rook's (or a firmware repo's) work against
§12.6; Telesthete supplies the vectors and the profile test configuration.

---

## 13. What I could not verify

- **ESP32 sizes** are estimates. I did not build a link client or measure
  libsodium on ESP-IDF.
- **Ed25519/X25519 key reuse.** I relied on common practice and my
  understanding of the literature, not a proof I checked.
- **Line counts** in §12.2's proposed table are from `wc -l` on the package
  sources (4,294 Python, 8,258 Rust), rounded; they include comments and
  in-file tests, so they overstate the code a client would need.
- **Probe script.** The condition probe in section 3 is a throwaway script, in
  the pull request description, not in the repository.
- **Python 3.11 and 3.12**, which `setup.py` lists, were not run; only 3.14.
- **The relay's memory and CPU on a small VM** with hundreds of bands on one
  attachment was not measured. The registry is a hash map behind a mutex and
  should be fine at Rook's scale, but L11 is the test that would show it.
- **Caddy routing WebSocket upgrades on one path** is standard practice; I did
  not set up the full Oracle install.
