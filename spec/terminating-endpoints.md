# Telesthete additions: terminating endpoints, WebSocket clients, link lifecycle

Design only. Nothing here is built yet. Base: `main` at `8538e7c`
("Stream messages carry their datagram's receive time (#16)").

**Revision 1, 13 November 2026.** Revision 0 (12 November) put a relay process
in front of an application hub and joined them with a new local attachment
socket, and it derived PSKs from Ed25519 keypairs inside Telesthete. On review
the owner ruled otherwise. Telesthete is a wire spec, and the libraries and the
relay are reference implementations of it, so an application's hub may
implement the server side of the spec itself. This revision therefore:

- replaces the relay-plus-attachment shape with a **terminating endpoint**
  (section 5). The stock relay is not changed at all;
- removes key derivation from Telesthete. Telesthete authenticates frames with
  a key the application supplies, and how the application gets that key is the
  application's business;
- keeps everything else (many keys on one endpoint, WebSocket clients, link
  lifecycle, link profile, stale text), adjusted where it mentioned the relay
  or the attachment.

**Revision 2, 13 November 2026.** Review found that a receiver which restarts
accepts a replay of a session recorded before the restart. The gap is in the
protocol on `main`, not something revision 1 introduced. But this design makes
restarts an ordinary event, and its spec text said nothing about them. This
revision:

- checks the edges of the finding and records the results (section 3 and
  section 7.6);
- says the limit in the spec text: a §3.3 note on receiver restarts, and the
  §10.2 restart paragraph (section 10);
- proposes one optional mitigation that changes nothing on the wire. It
  protects an endpoint across its own restarts and does not protect a peer
  (section 7.6);
- names the real fix and asks whether Telesthete should take it on later
  (decision 9);
- carries the result through the conformance list, the compatibility matrix,
  what is lost against a relay, and what I could not verify.

**Revision 3, 13 November 2026.** The decisions are taken. Bake delegated the
sign-off to the reviewer, who took decisions 1 to 8, 10 and 11. Decision 9
stays open for Bake (section 11). The document now describes exactly what is
to be built:

- The epoch floor is not adopted (decision 10). It is removed from the spec
  text and the build plan: the §10.2 "Epoch floor" text, §4.11 step 5, its
  §13 row, T16 and the changelog mention. Section 7.6 keeps the analysis,
  headed as examined and not adopted.
- The §3.3 note now says the protocol has no protection across a receiver's
  restart.
- Section 12 lists, per PR, what is built.

## Contents

1. [Summary](#1-summary)
2. [Ground rules](#2-ground-rules)
3. [What I verified](#3-what-i-verified)
4. [Many keys on one endpoint](#4-many-keys-on-one-endpoint)
5. [An endpoint that terminates many keys](#5-an-endpoint-that-terminates-many-keys)
6. [WebSocket clients](#6-websocket-clients)
7. [Links that survive restarts](#7-links-that-survive-restarts)
8. [A minimal client that can answer requests](#8-a-minimal-client-that-can-answer-requests)
9. [Stale text](#9-stale-text)
10. [Addendum to SPEC.md (exact text)](#10-addendum-to-specmd-exact-text)
11. [Decisions](#11-decisions)
12. [Implementation plan](#12-implementation-plan)
13. [What I could not verify](#13-what-i-could-not-verify)

---

## 1. Summary

- **No wire change.** The frame, the key derivation from a PSK, the handshake,
  every channel type and every conformance vector stay as they are.
  `PROTOCOL_VERSION` stays 3. Everything below is new API or new optional
  behaviour.
- **One key is one band.** A per-peer key is an ordinary PSK with its own
  `band_id`. A new `Endpoint` holds many keys on one set of transports and
  picks the key by the cleartext `band_id` that every frame already carries.
  Removing a key drops its frames before decryption, at once. Each key has one
  remote peer.
- **Terminating endpoint.** A server may be a member of every band it serves
  instead of a relay between other members. It accepts frames over UDP and
  WebSocket, picks the key by `band_id`, and answers on the path the frame
  came in on. It never forwards. A client cannot tell it from the relay. The
  libraries offer a small seam so an application can feed the `Endpoint` from
  its own UDP socket and its own HTTP server: frames in with an opaque path
  token, frames out for a path token.
- **The stock relay is unchanged.** No new listener, no new configuration.
- **WebSocket clients** for Python and Rust, matching what the relay already
  serves and what a terminating endpoint must serve (`/band`, one frame per
  binary message, one band per connection).
- **Link lifecycle.** HELLO is retried until answered. Channels are reset when
  the peer's session changes, can be reopened, and answer a dead incarnation
  with RST. All of it is a receiver's local reaction to frames that exist
  today, so nothing is negotiated. This is what brings peers back after a
  terminating endpoint restarts.
- **Link profile.** The smallest client that can answer requests: baseline
  cipher, Control, one Channel, a small window, reconnection. An ESP32 needs
  roughly 1,000 to 1,300 lines of C (estimate).
- **Replay after a restart is not prevented.** A receiver that restarts
  accepts a replay of a session recorded before the restart, in both
  directions. Neither library prevents it today, and nothing this addendum
  requires does either. The spec text now says so. An application whose
  messages cause actions must use a fresh PSK for each session, or carry
  its own freshness. An epoch floor that would protect an endpoint across its
  own restarts was examined and not adopted. Only a change to key derivation
  protects both sides, and whether to make it is left open (decision 9).

## 2. Ground rules

- **Additive only.** Existing behaviour, wire, vectors and public APIs keep
  working exactly as now. Where a fix would change something that exists, it is
  either opt-in or recorded under [Decisions](#11-decisions).
- **Application-agnostic.** This document and the spec text in section 10 say
  "application", "hub" and "peer". Telesthete authenticates frames with the key
  the application supplies; the application decides who gets a key and how.
- **Nothing extra.** No signatures, no peers in several bands, no negotiated
  extensions, no key agreement. An endpoint holding many keys is not a peer in
  several bands: each remote peer is in exactly one band, and the endpoint is
  the other member of each.
- **Least new surface.** One new class per library (`Endpoint`, with a small
  path seam), one transport per library (WebSocket client), a few callbacks and
  options. Nothing in the relay.

## 3. What I verified

Tests run in a clone under `/tmp` at `8538e7c`, Python 3.14, Rust stable
(revision 0; the base has not moved since):

| Suite | Result |
|---|---|
| `pip install -e .[aes] pytest pytest-asyncio`, `python -m pytest -q` | 138 passed |
| `cargo test --workspace --locked`, `telesthete` | 113 passed |
| `cargo test --workspace --locked`, `telesthitium` | 56 unit + 21 integration passed |

A probe over real localhost UDP with two `Band`s (script described in the pull
request) checked the link conditions:

| Condition | Result |
|---|---|
| 2. SYN to a Band that has not created the channel | Dropped. Initiator stays `SYN_SENT`. Silently, in fact: no `CHANNEL` handler is registered on the transport until the Band's first `channel()` call (`band.py:377`, `381-407`), so `udp.py:179` finds no handler. |
| 3. First HELLO lost (receiver not yet bound) | After 6.5 s both sides have 0 peers. |
| 4. Peer restarted on the same port, new epoch, reopens channel 0 | Other side's channel stays `ESTABLISHED` with `rcv_next=2`; restarted peer stuck in `SYN_SENT`. |
| 5. Peer silent while the other side has unacked data | Channel `CLOSED` after about 5 s; `band.channel(0, …)` keeps returning the same dead object. |

For revision 1, a second probe drove the built relay's WebSocket listener
(`HUB_WS_BIND`, `HUB_MAX_BANDS=1`) with the `websockets` client (17.4), to pin
down what a client can rely on from a server:

| Check | Result |
|---|---|
| Upgrade on `/band?x=1` | Accepted. The query string is ignored (`ws.rs:130` compares `uri().path()` only). No subprotocol is selected; the response carries only `Connection`, `Upgrade`, `Sec-WebSocket-Accept`. |
| Upgrade on `/other` | HTTP 404 (`ws.rs:130-135`). |
| Second connection in the band, before it sends a frame | Receives nothing. After its first frame it receives the next frame from the other connection, as one binary message. |
| A frame naming another band, a 5-byte binary message and a text message on an open connection | All dropped; the connection stays open. |
| First frame of a band beyond the band cap | Connection closed with a Close frame that carries no status code (client sees 1005) (`ws.rs:212`, `174`). |

For revision 2, a replay probe (the reviewer's script, extended; described in
the pull request) recorded one session through a proxy on loopback and
re-sent the recorded datagrams without the key. Each session carried one
Channel message and one Stream message. Python receivers ran on `8538e7c`. The
Rust receiver was a small throwaway example built in the same clone and driven
by the Python sender:

| Case | Receiver | Result |
|---|---|---|
| Receiver restarts (new process, same PSK and port, newer epoch); the session the other side sent it is replayed from that side's address | Python, the side that initiated (HELLO, then SYN) | **Accepted.** The Channel message and the Stream message are delivered again. |
| The same | Rust, Stream only | **Accepted.** |
| The other direction: the passive side restarts; the initiator's HELLO, SYN, ACK, data and Stream frames are replayed from the initiator's address. The Channel is created up front, as the accept hook would | Python | **Accepted.** Both messages delivered again. The restarted receiver had sent nothing. |
| No restart. The sender leaves with GOODBYE; the session is replayed from its address | Python | Rejected. |
| The same | Rust | **Accepted.** |
| No restart. The session is replayed from a new source address | Python | Channel message rejected; **Stream message accepted.** |

Code facts this design relies on (file and line at `8538e7c`):

- Python `Band` takes one `psk` (`telesthete/band.py:39`), derives one
  `band_id` (`band.py:74-75`, `protocol/crypto.py:47-49`) and builds its own
  `UDPTransport` (`band.py:97`). There is no transport argument.
- Python `UDPTransport` dispatches by `channel_type` only and never looks at
  `band_id` (`transport/udp.py:172-187`); handlers may be sync or async
  (`udp.py:181-183`). `send(destination, packet)` is synchronous and enqueues,
  dropping when the queue is full (`udp.py:124-135`). `UnixTransport` exposes
  the same handler interface (`transport/unix.py`, class docstring).
- Python `Band.channels` is keyed by `channel_id` alone (`band.py:117`,
  `365-366`) and the handler drops frames from any other peer address
  (`band.py:399-404`). A reply from an address other than the one a Channel was
  opened to never reaches that Channel.
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
- A data frame from a sender whose epoch is unknown (no HELLO yet) is dropped
  (SPEC §3.1). So after a receiver restarts, nothing but a HELLO from the peer
  restores the session. HELLO and HELLO_ACK are under the base key, so a
  recorded one is still accepted by a receiver that has no state for that
  peer.
- The data key depends on the PSK, the suite and the *sender's* epoch only
  (SPEC §3.1; `band.py:150-162`). Nothing the receiver contributes goes into
  it.
- Replay state is in memory and keyed by the peer's address: Python
  `ControlChannel._peer_session` and `_recv_watermark`
  (`protocol/control.py:92-98`, `281-320`) and the per-address watermarks of
  Stream and Channel (`stream.py:178-202`, `channel.py:335-353`); Rust
  `sessions` and `watermark` in the control task (`control.rs:219-220`) and the
  Stream watermarks keyed by `(addr, stream_id)` (`stream.rs:99`).
- Python never forgets a departed peer's epoch or control watermark:
  `ControlChannel.reset_peer` has no caller, and `_remove_peer` only drops
  destinations (`band.py:298-316`). Rust forgets both on GOODBYE
  (`control.rs:413-419`), and also drops them for evicted peers once its maps
  pass `PEER_MAP_SOFT_CAP` (1,024) entries (`control.rs:181`, `222-229`).
- A receiver accepts a HELLO whose epoch *equals* the last one seen, subject
  to the control watermark (`control.py:287-320`, `control.rs:311-327`).
- The relay's UDP side: one socket receives (`telesthitium/src/udp.rs:76`) and
  sends (`udp.rs:145`), so a peer always hears it from the address it sent to.
  A UDP source becomes a destination only after `udp_validation_packets`
  (default 2) packets (`registry.rs:289-301`, `340`; CONFORMANCE D4).
- The relay's WS side: the band is pinned to the first frame
  (`ws.rs:195-217`); frames naming another band are dropped (`ws.rs:199`);
  pings every 20 s and close after 60 s with nothing received, pongs included
  (`ws.rs:38-41`, `168`, `182`); text, ping and pong only reset the idle timer
  (`ws.rs:222`).
- Rust public enums `BandError` and `TransportError` and the struct
  `BandOptions` are not `#[non_exhaustive]` (`band.rs:29`, `62`,
  `transport.rs:119`). Adding a variant or a public field would break
  downstream code that matches or builds them exhaustively, so this design adds
  neither.

---

## 4. Many keys on one endpoint

### 4.1 The idea

A Telesthete key is a PSK. `band_id = SHA256(PSK)[:16]` is in cleartext at the
front of every frame (SPEC §1, §3.1). Give each peer its own PSK and each peer
gets its own `band_id`, so the receiver can choose the key before it decrypts
anything. Nothing on the wire changes: to the peer, its PSK is just a PSK.

So "one endpoint, many keys" is: one set of transports, a table
`band_id → Band`, and a demultiplexer in front. Each entry is an ordinary `Band`
with the existing API (`channel`, `stream`, `board`, ...). Adding a key adds an
entry. Removing a key removes the entry, and the next frame with that `band_id`
is dropped before any crypto.

Each entry has **one remote peer**. The Band sees that peer at one constant
address, `Endpoint.PEER`, whatever path its frames arrive on; the Endpoint
tracks which path that is (section 5.4). This is what lets a peer reconnect
over a new WebSocket connection, or come back from a new NAT mapping, without
its Channels noticing. A band that needs several remote peers on one key uses
a plain `Band`, as today.

Alternatives considered:

- *One band, per-peer keys inside it* (a key id in the frame, or trial
  decryption under every key). A key id changes the wire. Trial decryption
  costs one AEAD attempt per key per frame. Rejected.
- *One Band and one socket per key* (today's only option). Needs a port per
  peer. Rejected; this is the problem.

### 4.2 Python API

```python
from telesthete import Band, Endpoint

# Own UDP socket (LAN, tests):
ep = Endpoint(hostname="hub", bind_address="0.0.0.0", bind_port=7474)
# Or fed by the application's own transports (section 5.3):
ep = Endpoint(hostname="hub", send=send_to_path)

await ep.start()
band = ep.add_key(psk)          # -> Band; synchronous; idempotent for the same psk
ep.remove_key(band.band_id)     # -> bool; synchronous
ep.band(band_id)                # -> Optional[Band]
ep.band_ids()                   # -> list[bytes]
ep.connect(band.band_id, path)  # start a session with the key's peer at path
await ep.stop()
```

`Endpoint(hostname=None, *, bind_address="0.0.0.0", bind_port=9999,
send=None, capabilities=None, ciphers=None, session_epoch=None)`. With `send`,
the Endpoint opens no socket (section 5.3); without it, it binds UDP.
`capabilities`, `ciphers` and `session_epoch` are passed to every Band the
Endpoint creates, so all of them share one epoch, which MUST increase across
restarts exactly as for a Band (SPEC §4.3).

Rules:

- `add_key(psk)` returns a normal `Band` whose transport is a per-key view of
  the Endpoint. The view's `start`, `run` and `stop` do nothing; the Endpoint
  owns the I/O. If the Endpoint is running, the Band is started at once.
  Endpoint Bands are created with `reconnect=True` (section 7).
- The Band's one peer is at `Endpoint.PEER` (the constant `("peer", 0)`).
  Use it for `band.channel(id, Endpoint.PEER)` and the like. Frames the Band
  sends to it go to the key's current path (section 5.4); with no current path
  they are dropped, as UDP loss would drop them.
- `connect(band_id, path)` makes `path` the key's current path and starts the
  §4.11 HELLO retry towards it. In UDP mode a path is a `(host, port)` tuple.
  Use it instead of `band.connect_peer`.
- `remove_key(band_id)` deletes the table entry first, then stops the Band's
  tasks (keepalive, channel retransmits) without sending GOODBYE. When it
  returns, no frame bearing that `band_id` is decrypted or delivered to the
  application. A callback already running finishes.
- Frames shorter than 43 bytes, or whose `band_id` is not in the table, are
  dropped before decryption and counted (`ep.dropped_unknown`, an int).
- Each Band keeps its own `SequenceSource`. Keys differ per band, so nonce
  uniqueness holds per key exactly as today.

Supporting addition to `Band` (also used by section 6):

- `Band(..., transport=None)`. When given, the Band uses that object instead
  of building a `UDPTransport`, and `bind_address`/`bind_port` are ignored. The
  object must provide what `UDPTransport` and `UnixTransport` already provide:
  `start()`, `async run()`, `async stop()`, `register_handler(channel_type,
  cb(peer_addr, packet_bytes))`, `send(dest, packet_bytes)`, `local_address`.
  The Band drives `start/run/stop` as it does today. Omitting `transport`
  behaves exactly as now.

### 4.3 Rust API

```rust
use telesthete::{Endpoint, Path};

let ep = Endpoint::bind_udp("0.0.0.0:7474".parse()?, "hub").await?;
// or fed by the application's own transports (section 5.3):
// let ep = Endpoint::with_sender("hub", move |path: Path, frame: Bytes| { /* enqueue */ });

let band: Band = ep.add_key(psk, BandOptions::default()).await?;
ep.connect(&band.band_id(), Path::Udp(addr)).await?;
let removed: bool = ep.remove_key(&band.band_id()).await;
let ids: Vec<BandId> = ep.band_ids().await;
```

- `Endpoint::bind_udp(addr, hostname) -> io::Result<Endpoint>`,
  `Endpoint::with_sender(hostname, send) -> Endpoint`,
  `Endpoint::with_session(self, session: u64) -> Self`.
- `add_key(&self, psk, opts) -> Result<Band, BandError>` returns an owned
  `Band`. The Band has reconnect behaviour on (section 7) and sees its one peer
  at `Endpoint::PEER` (the unspecified IPv6 address, port 0, which no real
  source can have). Dropping the `Band` removes its key, like `remove_key`.
- `remove_key(&self, band_id) -> bool`. After it returns, the receive path
  drops that `band_id`. The removed `Band`'s sends fail with
  `TransportError::Io` of kind `NotConnected` (no new enum variant).
- `connect(&self, band_id, path) -> Result<(), BandError>`.
- `Path` is a new `#[non_exhaustive] enum Path { Udp(SocketAddr), Conn(u64) }`.
  `Conn` is any connection the application numbers itself.
- Internals: `Transport` gains a private I/O backend (own UDP socket, Endpoint
  view, WebSocket). `Transport::bind` keeps its signature and behaviour. No new
  dependencies.

### 4.4 Revoke and rekey

How a PSK is made is the application's. Given one:

- **Approve.** `add_key(psk)`.
- **Revoke at the endpoint.** `remove_key(band_id)`. Frames under that key are
  refused from that moment, before decryption.
- **Revoke at the peer.** The peer stops its Band and discards its PSK. How the
  endpoint learns of it is the application's protocol.
- **Rekey.** `add_key(new)`; the peer starts a Band under the new PSK;
  `remove_key(old)` when the application chooses. Both keys can be live
  meanwhile; they are different bands.

### 4.5 Existing peers and interop

- No frame, field, key derivation or channel type is added or changed. A peer
  built against today's Telesthete reaches an `Endpoint` with the PSK the
  application gave it; it cannot tell the difference.
- The relay routes by `band_id` as today.
- A `Band` created without `transport=` behaves exactly as now.

### 4.6 Versioning

No wire version. Library versions: Python `0.3.0 → 0.4.0`, Rust crates
`0.2.x → 0.3.0` (minor, additive).

### 4.7 Tests

Both languages, in UDP mode over loopback and in path mode over an in-memory
sender: 50 keys on one Endpoint, each peer reaches only its own Band; frames
with an unknown `band_id` never reach a decrypt call; `remove_key` while a
Channel is mid-message: no delivery after return; rekey with both keys live;
add and remove while running under load.

---

## 5. An endpoint that terminates many keys

### 5.1 What it is

When every peer has its own key, every band has exactly two members: the peer
and the server. The server then never passes ciphertext between two other
parties. It **terminates**: it accepts frames on UDP and on WebSocket, picks
the key by `band_id`, decrypts, and answers on the path the frame came in on,
as the band's other member. That is far less than a relay: no registry of
peers to forward to, no eligibility, no fan-out, no federation, no route ids.

An application builds it from the `Endpoint` of section 4 plus its own UDP
socket and its own HTTP server. The stock relay (`telesthete-hub`) is not
involved and not changed. The spec text that makes this legal is SPEC §10.2 in
section 10.

### 5.2 What the reference client relies on

To be reachable by an unmodified client, a server must behave as below. Each
row says where the client's dependence comes from. "Client" means today's
`Band` over UDP, and the WebSocket clients of section 6.

**UDP (§9.1).**

| Behaviour | Why the client needs it | Relay today | Terminating endpoint |
|---|---|---|---|
| Replies come from the address and port the client sent to | Both libraries key peers by source address; a Channel drops frames from any other address (`band.py:399-404`, `channel.rs:692-701`) | One socket sends and receives (`udp.rs:76`, `145`) | MUST. A server bound to a wildcard address on a multi-homed host must send from the address the datagram arrived on (bind per address, or `IP_PKTINFO`) |
| One frame per datagram, ≥ 43 bytes; shorter is ignored | §1 | Yes (CONFORMANCE A3, E3) | Same |
| Something answers the client's HELLO | Without it the client never has a peer (today one HELLO, §4.11 retries it) | The other peers answer, through the relay | The endpoint answers, as a peer |
| Presence | The client keeps alive every 5 s (§4.5) | Evicts a UDP source after 15 s silent (C1, C4) | No transport presence; the band evicts the peer after `PEER_TIMEOUT` (§4.5), as any peer does |
| Eligibility gate | Nothing in the client depends on it; it only delays delivery to the client until its second packet (D4) | 2 packets | Not needed: the endpoint sends nothing in answer to a frame that does not authenticate, so an unauthenticated source cannot make it send |

**WebSocket (§9.3).** All of these were observed against the built relay
(section 3) or read in `ws.rs`.

| Behaviour | Why the client needs it | Terminating endpoint |
|---|---|---|
| URL: the client uses the full URL it is given; the relay serves `/band` and answers other paths with 404 | The URL is configuration | Serve the route the application chooses; SHOULD be `/band` so one URL form fits every server |
| Query string ignored | A client may carry one (for example a cache-buster from a proxy) | Ignore it |
| No subprotocol, extension or Origin required | The clients offer none | Require none |
| One binary message = one frame, no length prefix; text and binaries under 43 bytes ignored | §9.3 | Same, both directions |
| Nothing is sent before the connection's first frame; the band is learned from it | The client sends HELLO first, as soon as the connection opens | Same; the endpoint has nothing to say before it knows the key |
| Frames naming another band are dropped (the connection is pinned) | The client sends one band per connection | MAY pin; each frame is authenticated under its own key anyway |
| Server pings every 20 s; closes after 60 s with nothing received, pongs included | The client answers pings (the libraries do this) | Answer pings (RFC 6455 requires it); SHOULD close a connection silent for 60 s; MAY ping |
| Close: no status code; on a cap the connection just closes | The client treats every close alike: reconnect with backoff, HELLO again | Close codes carry no meaning. In particular, do not signal "no such key": that would tell an unauthenticated prober which `band_id`s are served (decision 6) |
| Presence | Relay: a connection peer is never idle-pruned while connected (C5) | The band's §4.5 timeout decides; a closed connection only ends that path |

Nothing else in either client depends on the server: no HELLO field names an
address, Route frames are never sent unless the application declares a route
(and then a terminating endpoint drops them, §2), and WebTransport and AF_UNIX
are not used by these clients.

### 5.3 The seam: bring your own transports

The application already runs one HTTP server, which will serve the WebSocket
route, and it owns its UDP socket. The libraries need only three calls and one
callback. A *path* is an opaque token the application chooses for "where this
frame came from and where its answer goes": for UDP the source address, for
WebSocket the connection.

**Python.**

```python
def send_to_path(path, frame: bytes) -> None:
    """Called by the Endpoint. Must not block and must not raise."""
    kind, where = path
    if kind == "udp":
        udp_transport.sendto(frame, where)          # asyncio DatagramTransport
    else:
        queues[where].put_nowait(frame)             # per-connection queue; drop when full

ep = Endpoint(hostname="hub", send=send_to_path)
await ep.start()

# UDP: asyncio DatagramProtocol
def datagram_received(self, data, addr):
    ep.receive(("udp", addr), data)

# WebSocket route on the application's HTTP server, any framework
async def band_route(ws):
    path = ("ws", id(ws)); queues[id(ws)] = asyncio.Queue(256)
    writer = asyncio.create_task(pump(queues[id(ws)], ws))   # sends each frame as one binary message
    try:
        async for msg in ws:
            if isinstance(msg, bytes):
                ep.receive(path, msg)
    finally:
        ep.close_path(path); writer.cancel(); del queues[id(ws)]
```

- `ep.receive(path, frame)`: synchronous; enqueues the frame on the Endpoint's
  bounded receive queue and returns (drops and counts when full, as a full UDP
  socket buffer would). Paths must be hashable.
- `ep.close_path(path)`: the connection behind `path` is gone. If it was a
  key's current path, that key has no current path until its peer's next
  accepted frame. The Band is not told; its §4.5 timeout and the peer's
  reconnection handle the rest.
- `send(path, frame)`: the application's callback. It must not block (one slow
  connection must not stall the others) and must not raise. A frame it cannot
  send at once it drops, as UDP would.
- `ep.connect(band_id, path)` (section 4.2): for an endpoint that starts the
  session itself, for example to a remembered UDP address after a restart.

**Rust.** The same shape: `Endpoint::with_sender(hostname, send)` where
`send: impl Fn(Path, Bytes) + Send + Sync + 'static`; `ep.receive(path,
&frame)` (synchronous, non-blocking, drops when full); `ep.close_path(path)`;
`ep.connect(band_id, path)`.

The UDP-socket mode of section 4 is this seam with a built-in socket.

### 5.4 One peer per key: following the path

Each key's band has one remote peer (section 4.1). The Endpoint keeps, per key,
a *current path*: the path of the most recent frame that band **accepted**,
meaning it authenticated, passed the §3.3 replay check, and was not ignored
under §4.3 (a HELLO with an older epoch is ignored). Frames the Band sends to
`Endpoint.PEER` go to the current path.

Why accepted frames only:

- A frame that does not authenticate never moves the path, so nobody without
  the key can redirect the endpoint's answers.
- A replayed frame fails §3.3 first, so replaying a captured frame from another
  address does not move the path either. This holds only while the Endpoint
  still has the key's session state. The Endpoint therefore keeps a key's
  epoch and watermarks for as long as it holds the key, including after a
  GOODBYE and after the peer is evicted (T15). A plain `Band` keys this state
  by address, so it accepts a replayed Stream from a new address (section 3).
  An Endpoint with one peer per key does not, because the state is per key
  and the path plays no part in it. After the Endpoint restarts, or after
  `remove_key` and then `add_key` with the same PSK, that state is gone. A
  replay is then accepted from any path, and the path follows it (section
  7.6).
- During a restart overlap (two processes with one key), the newer epoch wins
  at the endpoint and the older process's frames stop authenticating (§4.3), so
  the path follows the newer process.

What it buys: a peer whose WebSocket connection drops reconnects, sends HELLO
on the new connection, and its session and Channels carry on. The same holds
for a UDP peer whose NAT mapping changes, and for one that switches between
UDP and WebSocket. Without it, every new path would be a new peer to the Band,
and every Channel would be reset by RST and reopened.

What it costs: an attacker on the path who can see a fresh frame and deliver
it from elsewhere before the original arrives moves the path until the peer's
next frame (at most one keepalive interval, 5 s). An attacker on the path can
drop frames anyway. This is the same trade WireGuard makes for roaming.

Implementation note (not public surface): the per-key view learns that a frame
was accepted from the Band's receive path, after decryption and the replay
check. In Python, handlers run synchronously per frame, so the view can read a
flag the Band sets. In Rust it is a private callback in `Transport`.

### 5.5 Where the spec assumes a relay

| Where | Assumption today | How a terminating endpoint fits | Change (exact text in section 10) |
|---|---|---|---|
| §1, `band_id` row | "Relay routing identifier" | Also how an endpoint picks the key | Add "and key selection (§3.6)" |
| §3.1, last paragraph | "The relay sees `band_id` … never possesses the PSK" | True of a relay. A terminating endpoint is a member and holds the PSK | Add one sentence |
| §3.5 | Cipher negotiated end to end because the hub relays without decrypting | An endpoint negotiates as a peer; the rule holds unchanged | None |
| §9.3 | "Connect outbound to Telesthetium hub"; "Not yet implemented" | A server is a relay or a terminating endpoint; the server's and client's duties are what section 5.2 lists | §9.3 rewritten |
| §9.6 | "The server matches the `band_id` exactly as the UDP/WS relay does … never holds the PSK" | No WebTransport client exists; an endpoint does not serve it in this design | One sentence: a terminating endpoint may serve it with the same carrier mapping; not specified further here |
| §10 | The hub is a relay; connection modes LAN, Tunnel, Hybrid | A fourth mode, terminating | Heading loses "Future"; one paragraph; §10.2 new |
| §10.1 | Addressed routing by a routing hub | A terminating endpoint is not a routing hub: it is a peer and drops Route frames (§2). In a two-member band addressing means nothing | None (§10.2 says so) |
| §12.1 | Transports: "UDP on LAN; WebTransport or WebSocket otherwise" | Unchanged; the server may be either kind | None |

### 5.6 Conformance for a terminating endpoint

The relay's inventory (`rust/telesthitium/CONFORMANCE.md`), claim by claim:

| Relay claims | For an endpoint that never forwards | Why |
|---|---|---|
| A1, A2 (header offsets), A3 (under 43 bytes dropped) | Apply | Same frame |
| A4 (unknown `channel_type` still routable) | Does not apply | An endpoint is a peer: it drops a type it does not handle (§2) |
| A5 (agrees with the library) | Applies | Pinned by `tests/vectors.json`, which the endpoint uses through the library |
| B1 (never echoed), B4 (relayed verbatim), B5 (UDP and connection keys distinct) | Do not apply | Nothing is forwarded |
| B2 (bands isolated) | Applies, as key isolation | §3.6 rule 3 |
| B3 (sending registers the sender) | Replaced | Nothing is registered until a frame authenticates under a held key |
| C1 to C4 (UDP TTL, band reaping) | Replaced | Peer liveness is §4.5 in the band; keys are added and removed by the application, not reaped |
| C5 (connection peer removed on disconnect) | Applies, as `close_path` | — |
| D1, D2 (band and peer caps) | Replaced | Keys are the application's; one peer per key |
| D3, D6, D7 (bounded queues per peer, byte budget) | Apply in spirit | Per-path bounded send queues are the application's transport code; the Endpoint's receive queue is bounded |
| D4, D5, D8 (UDP validation) | Do not apply | The endpoint never answers an unauthenticated frame |
| E1, E2 (UDP relay) | Replaced | Answers from the same address and port |
| E3 (short datagram ignored) | Applies | — |
| F1 (path selects the endpoint), F2 (one message = one frame) | Apply | — |
| F3 (WS to UDP bridge) | Replaced | A WS peer and a UDP peer, each on its own key, both served |
| F4 (native TLS) | The application's HTTP server | — |
| G (WebTransport), H (bridging), I (AF_UNIX), K (addressed routing) | Do not apply | Not served, or nothing forwarded |
| J1 (config), J2 (shutdown) | Apply | — |
| J3 (no payload in logs) | Replaced | An endpoint can decrypt: it must not log keys or plaintext by default |

Proposed list for a terminating endpoint, tested in both libraries against the
`Endpoint` in path mode (UDP mode where marked):

| ID | Claim |
|---|---|
| T1 | Frames under 43 bytes, and frames whose `band_id` has no key, are dropped without a decrypt call. |
| T2 | No frame is sent in answer to a frame that fails authentication. |
| T3 | Keys are isolated: a peer reaches only its own Band; a frame under key A arriving on key B's path is handled under key A. |
| T4 | After `remove_key` returns, no frame for that `band_id` is decrypted or delivered. |
| T5 | Answers go out on the path of the key's latest accepted frame (UDP mode: from the address and port the peer sent to). |
| T6 | A frame that fails authentication, a replayed frame, or an older-epoch HELLO does not move the path (while the endpoint holds the key's session state; see T15 and the non-claim below). |
| T7 | A peer that reconnects on a new path, or changes UDP source port, keeps its session and its open Channels. |
| T8 | `close_path` ends the path; the peer's next accepted frame on a new path resumes delivery. |
| T9 | A blocked `send` for one path does not delay frames on other paths (the callback contract; checked with a test sender). |
| T10 | WebSocket (test server built on the seam, Python and Rust): upgrade on `/band` with a query string, no subprotocol; text and short binaries ignored; nothing sent before the first frame; pings answered; closed after 60 s silent. |
| T11 | Route (0x05) frames are never delivered to the application. |
| T12 | After the endpoint restarts with a newer epoch, a peer with §4.11 and §6.7 recovers: over WebSocket on reconnect, over UDP within `PEER_TIMEOUT`, or at once after `connect`. |
| T13 | Keys and plaintext do not appear in logs at default levels. |
| T14 | `stop` returns promptly with peers connected. |
| T15 | While the endpoint holds a key, a recorded session of that key replayed on any path is not delivered and does not move the path, including after the peer sent GOODBYE or was evicted. |

**Stated non-claim.** A terminating endpoint does *not* protect itself
against a replay after its own restart. It also does nothing for a peer that
restarts: that peer accepts a replay of what the endpoint sent it before. The
list says so, so that no one reads T15 as more than it is.

These go in a new `spec/terminating-endpoint-conformance.md` (decision 8),
together with the non-claim, so that an application that writes its own
endpoint has the list.

### 5.7 What is lost against a relay in front

Revision 0 put the relay between peers and the application (shape H1). Against
that, the terminating endpoint loses:

- **A hub restart drops every connection and every session.** With a relay in
  front, the relay held WebSocket connections and UDP mappings while the
  application restarted, and one HELLO per band brought peers back. Now
  WebSocket peers reconnect (their first retry after 0.5 s) and send HELLO;
  UDP peers send HELLO once they declare the endpoint lost, up to
  `PEER_TIMEOUT` (15 s) later, unless the application remembers each key's
  last UDP address and calls `connect` for it at start. Either way every
  Channel resets (section 7), so messages in flight at the restart are lost and
  the application must handle `on_reset`.
- **The relay's hardening is not free.** Per-peer bounded queues, a hub-wide
  byte budget, caps and the WebSocket idle timeout are now the application's
  transport code (T9, T10). The authentication rule (T2) replaces the UDP
  validation gate, which is a gain.
- **Throughput is not measured.** UDP and WebSocket I/O now run in the
  application's process (Python, for the expected first user) instead of in the
  Rust relay. Decryption was already in the application under H1, so the new
  cost is one system call per datagram. I expect it to be small at tens to
  hundreds of peers; I have not measured it.
- **No WebTransport, AF_UNIX rendezvous, federation or peer-to-peer relaying**
  through the hub. An application that later needs any of them runs the stock
  relay beside the endpoint (different port or path); clients cannot tell
  which they reached.
- **One process per key set.** Under H1 several application processes could
  attach the same band. Now one process holds each key. Spreading keys across
  processes means one address per process, or a front that routes by
  `band_id`.
- **Path changes are visible to the endpoint**, which section 5.4 handles. The
  relay hid them.
- **Not lost, but now in plain view: every restart is a replay window.** A
  restarted receiver accepts a replay of a session recorded before the restart
  (section 7.6). Under H1 the application behind the relay held the session
  state too, so its restarts had the same gap. A relay holds no session state
  and cannot close it. It is listed here because this design makes a restart
  the ordinary way the hub recovers. The spec text now says so (§3.3, §10.2).
  Nothing in this addendum closes the gap, on either side (decisions 9 and
  10).

What is gained: one service instead of two, no local socket protocol to
specify and keep, no relay change, no eligibility delay on a client's first
reply, and nothing for the endpoint to forward.

### 5.8 Existing peers, versioning, tests

- **Existing relays and peers.** Untouched. A terminating endpoint looks to
  every client like a relay with one other peer in the band.
- **Versioning.** No wire version. The seam is library API, versioned with the
  libraries (section 4.6).
- **Tests.** T1 to T15 above, plus section 6's
  WebSocket tests run against a path-mode Endpoint as well as against the
  relay. The replay probe of section 3 becomes a test: as a regression test
  for T15, and as a test that documents the non-claim (the restarted
  peer accepts the replay) so a later change in behaviour is noticed.

---

## 6. WebSocket clients

### 6.1 What a server does

What the relay serves today, and what section 5.2 requires of a terminating
endpoint: one frame per binary message, no length prefix; the band learned from
the first frame and pinned (relay) or checked per frame (endpoint); text and
short binaries ignored; pings answered; a quiet connection pinged every 20 s and
closed after 60 s; any close without meaning. Nothing reaches a client before
its first frame.

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
  so the server learns the band immediately.
- Offers no subprotocol. Answers pings (the library does). `ssl=None` verifies
  against system roots; pass an `ssl.SSLContext` for a private CA or a pinned
  certificate.
- Dependency: the `websockets` package as an optional extra,
  `pip install telesthete[ws]`. The base install is unchanged.

### 6.3 Rust client

Behind a new cargo feature `ws` (off by default, so default dependencies do not
change), using `tokio-tungstenite` 0.30 with rustls, the versions the relay
already uses.

```rust
let band = Band::connect_ws("wss://hub.example.com/band", psk, "peer", BandOptions::default()).await?;
let server = band.ws_peer().expect("ws band");   // SocketAddr standing for the server
band.set_reconnect(true);
band.connect_peer(server).await?;
```

- `Band::connect_ws(url, psk, hostname, opts) -> Result<Band, BandError>`;
  connection failures come back as `BandError::Io` (no new variant).
- `Band::ws_peer(&self) -> Option<SocketAddr>`: the address resolved at first
  connect. It stays the Band's peer identity across reconnects even if DNS later
  resolves elsewhere.
- Same reconnect, ping and drop rules as Python.

### 6.4 Spec alignment

SPEC §9.3 is rewritten to say what a server (relay or terminating endpoint) and
a client must do (exact text in section 10). §10's heading loses "Future". §9.6
gets "served by the reference relay; neither reference library has a client".
WebTransport clients stay out of scope.

One thing the spec should say and does not: through a relay, a peer cannot tell
other peers apart by address. The relay sends every UDP frame from its own
socket and every WS frame down the one connection, and does not reveal which
peer sent it. A Band keyed by address therefore sees one peer per relay path.
That is fine for two-member bands and was already true for UDP peers behind the
relay. The §10 text in section 10 says so.

### 6.5 Existing peers, versioning, tests

- No wire change: §9.3 already defined the framing. Existing UDP Bands are
  untouched; the transport is chosen by the application.
- Tests, both languages, each against the built relay **and** against a
  path-mode `Endpoint` behind a minimal test WebSocket server: Control and a
  100 KB Channel message both ways (relay: a WS Band and a UDP Band in one band;
  endpoint: a WS peer and a UDP peer on two keys); the server is killed and
  restarted and the WS Band reconnects and the Channel recovers (section 7); a
  client over native WSS with a self-signed certificate trusted by an explicit
  context; text messages and short binaries are ignored.

---

## 7. Links that survive restarts

### 7.1 The five conditions

| # | Condition | Library bug? | Spec gap? | Fix |
|---|---|---|---|---|
| 1 | Channel keyed by id alone (`band.py:117`, `365-366`) | A Python limitation: Rust keys by `(peer, id)` (`channel.rs:733`) and §3.3's watermark is per `(peer, type, id)`. | Yes: §6 never says a Channel's identity is `(peer, channel_id)`. | Spec states the identity and allows an implementation to restrict an id to one peer. Python left as is (decision 3). Moot for an `Endpoint`: every key has one peer. |
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

1. `connect_peer(addr)` (or `Endpoint.connect`) makes `addr` a *target* for the
   Band's lifetime.
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
  old SYN cannot trigger this while the receiver holds the peer's session: it
  fails the §3.3 outer watermark first. A receiver that has restarted holds no
  session, and there the replay succeeds without needing (e) (section 7.6).
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
  sees `on_reset(PeerRestarted)`, then messages again. The key did not change,
  so nothing else is needed.
- **Terminating endpoint restarts.** Every connection drops and the new process
  has no sessions. WebSocket peers reconnect and send HELLO; UDP peers' frames
  are dropped (epoch unknown, §3.1) until they declare the endpoint lost after
  `PEER_TIMEOUT` and send HELLO, or until the endpoint calls `connect` to a
  remembered address. Either way the peer sees a newer epoch in the endpoint's
  HELLO or HELLO_ACK → resets → reopens → the accept hook creates the channel
  in the new process. The new process would accept a replayed old session in
  the same way. Nothing in this addendum prevents that (section 7.6).
- **Relay restarts** (peers using the stock relay). Peers' WebSocket clients
  reconnect and send HELLO; UDP peers re-HELLO after silence. Sessions survive
  (no epoch changed), so Channels continue after retransmission or, past give-up,
  reset and reopen.
- **Peer roams or switches UDP to WebSocket.** At an `Endpoint`, the path
  follows the peer's latest accepted frame (section 5.4), so the Band sees no
  change. Behind a relay, the peer's own Band talks to one fixed relay address.
  Loss during the switch is retransmission; past about 5 s it is give-up, then
  RST or reopen.
- **Short outage, no restart.** Whichever side gave up resets; if it was the
  initiator it reopens and the other side resets on the new-incarnation SYN; if
  it was the responder, the initiator's next data draws an RST, it resets and
  reopens.
- **Two processes with one key** (for example a new process started before the
  old one exits). The newer epoch wins at the receiver; the older process's
  frames fail authentication there (§4.3 already guarantees this), and at an
  `Endpoint` they do not move the path.

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
- Over loopback UDP, through the relay, and against a path-mode `Endpoint`:
  kill and restart each side 20 times while messages flow; drop 20 % of
  datagrams including HELLOs; partition for 3 s and for 20 s; WS reconnect;
  two processes with one key.
- Cross-language: a Python peer and a Rust peer restarting each other (no such
  live interop test exists today).

### 7.6 Replay after a restart

**The gap.** A receiver that restarts accepts a replay of a session recorded
before the restart, and delivers the old messages to the application again.
Several things combine:

- The data key depends on the PSK, the suite and the sender's epoch, and on
  nothing the receiver contributes (§3.1).
- The defences of §3.3 and §4.3 live in the receiver's memory: the high-water
  marks, and the rule that an older or equal epoch does not rebase.
- After a restart the receiver has none of that memory. A recorded HELLO or
  HELLO_ACK, which is under the base key, teaches it the old epoch.
- Recorded Channel frames start at inner sequence 0, like any new session's,
  so they fit a new session.

The attacker needs no key. It needs a recording and a restart, or the means to
hold back the real peer's frames until a restart happens. The gap is in the
protocol on `main`. This design does not create it, but it makes restarts
ordinary, so its spec text has to say so.

**Where it reaches.** These cases were run unless marked "by reading". The
results are in section 3.

| Case | Result |
|---|---|
| A peer restarts; what the other side sent it is replayed | Accepted (Python, Channel and Stream; Rust, Stream). |
| The passive side restarts (an endpoint, in this design); what the peer sent it is replayed | Accepted (Python, Channel and Stream). The restarted side had sent nothing at all. |
| No restart; the sender left with GOODBYE; replayed from its address | Python rejected (it keeps the epoch and watermark). **Rust accepted** (it forgets them on GOODBYE; fixed in PR 1b, decision 11). |
| No restart; replayed to a plain `Band` from a new address | Channel rejected (bound to its address). **Stream accepted**: a new address is a new peer with fresh watermarks. Rust is the same by reading. |
| No restart; replayed to an `Endpoint` (one peer per key) on any path | By reading: rejected. The epoch and watermarks belong to the key, so the replay fails §4.3 or §3.3, and the path does not move. This holds only if the Endpoint keeps a key's state across GOODBYE and eviction (T15). |
| The `Endpoint` restarts, or `remove_key` then `add_key` with the same PSK | By reading: accepted, on any path, and the path follows the replay, so the endpoint's answers go to the replayer. The replayer does not even need the peer's source address. |

**What an application that acts on messages must do.** It must do one of two
things for every message whose repetition matters:

- *A fresh PSK for each session.* The key is agreed anew whenever either
  side starts, and a receiver never accepts an earlier key after it restarts.
  A key that outlives a restart gives no protection. That includes a static
  PSK and a key derived only from long-term keys. A new key also gives a new
  `band_id`, so recorded frames do not even reach the band.
- *Its own freshness.* For example, each such message carries a unique id and
  an expiry. The receiver refuses expired messages and remembers the ids it
  has acted on until they expire, across its own restarts. Or the receiver
  issues a challenge that each request must carry.

Messages whose repetition is harmless need neither.

**Examined and not adopted: the epoch floor.** This would change nothing on
the wire. I examined the candidate in the review brief. It is sound for the
endpoint's side, under the conditions below, and it is small. Revision 2
proposed it as optional text. Decision 10 does not adopt it now: it is not
prototyped, it protects one side only, and it brings a lock-out mode and
slower recovery. The first application will use a fresh key per session. It
is to be revisited together with decision 9. Nothing below is built or goes
into SPEC.md. The analysis is kept so the reasoning is not lost. The rules
were:

- *F1.* For each `band_id`, the endpoint keeps the highest session epoch it
  has accepted from that band's peer in durable storage. This is the *floor*.
  When it accepts a newer epoch, it writes the new floor before it handles any
  other frame of that session.
- *F2.* It accepts a HELLO or HELLO_ACK for that band only in two cases: the
  epoch is greater than the floor, or the epoch equals the floor and this
  process already holds that session (then §3.3 applies as usual). Anything
  else is ignored: no session, no answer, no change of path.
- *F3.* A floor outlives `remove_key`.
- *F4.* While it holds no accepted session for a band, the endpoint sends no
  HELLO for it. So `Endpoint.connect` at start is not used with floors.
- *Peer side (proposed as §4.11 step 5).* When a peer with reconnection
  declares a target lost, it starts a new session before it sends HELLO
  again. That means a new, larger epoch and every Channel reset.

Why it works. A replayed HELLO carries an epoch the endpoint has already
accepted, so it is at or below the floor and is ignored. With no accepted
epoch, the replayed data frames have no key and are dropped (§3.1). The real
peer, if it did not restart, is at the floor too, so its HELLOs are ignored
as well. It hears nothing authenticated, declares the endpoint lost after
`PEER_TIMEOUT`, and comes back with a newer epoch.

Cases checked:

- *A crash between the write and acting on the session.* The write comes
  first, so the worst case is a floor above a session the endpoint never acted
  on. The peer's retried HELLO is then ignored, and the peer starts a new
  session after `PEER_TIMEOUT`.
- *A replay while running.* Unchanged: §3.3 applies.
- *F4.* Without F4 the endpoint's own retried HELLOs would keep the peer from
  ever declaring it lost.

Costs:

- After an endpoint restart every peer is back after up to `PEER_TIMEOUT`
  (15 s) plus its backoff. That includes WebSocket peers, which revision 1
  brought back at once.
- One durable write per peer session per key.
- A peer without step 5 is locked out until it restarts. So an endpoint
  enables floors only when it knows its peers implement step 5.
- An outage longer than `PEER_TIMEOUT` becomes a new session. By then the
  Channels have usually reset through give-up anyway.

What it does not do:

- It does nothing for a peer that restarts (next paragraph).
- It does nothing if the store is lost. If the store is rolled back, for
  example by restoring a VM snapshot, the old floors return, and every session
  accepted since the snapshot can be replayed.
- It does nothing against frames held back and delivered late, once. Those
  are accepted, as today.

**Why there is no small fix for the peer's side.** A restarted peer receives
frames the endpoint sent under the endpoint's epoch. The real endpoint did not
restart and still sends under that same epoch, so a floor at the peer cannot
tell its frames from the recording. Closing this without a wire change would
need two things:

- the endpoint takes a new epoch for a key whenever it accepts a new peer
  session (per key, not per process);
- every peer, link clients included, keeps a durable floor for the endpoint's
  epoch and writes it before acting on the session.

That is a new rule on every peer, plus storage and a write per session on
small devices. It still fails on lost or rolled-back storage. For the peer's
side, a fresh PSK per session or the application's own freshness does the same
job with less. I do not propose it.

**The real fix, not designed here.** Each direction's data key would have to
depend on something fresh from the receiver as well as the sender. That could
be both epochs, taken from HELLO and HELLO_ACK, or a random nonce from each
side carried in them and mixed into the key derivation. A replayed session is
then bound to the receiver's old contribution, and it fails authentication in
a new process. That covers both directions, the first session, and lost
storage, with nothing stored.

It changes key derivation (§3.1), so a peer that does not implement it cannot
derive the keys. It would have to be negotiated: a capability in HELLO, with
today's derivation as the fallback. It needs new vectors in both libraries and
in the link profile. It also means an initiator cannot send data until it has
the responder's contribution. It would not give forward secrecy, which needs
ephemeral key agreement.

It is outside this additive addendum. Decision 9 leaves open, for Bake,
whether Telesthete takes it on later. Nothing in this design waits on it.

---

## 8. A minimal client that can answer requests

### 8.1 Why §12.1 is not enough

§12.1 and §12.2 size the minimal and ESP32 clients as Stream and Control only.
Streams are lossy and Control is not retransmitted, so neither carries a
request and its reply reliably. Answering requests needs the §6 Channel.

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
- One transport: UDP (§9.1) or WebSocket (§9.3), to a relay or a terminating
  endpoint.

It may omit AES, Streams, Board, Drop, discovery, AF_UNIX, WebTransport,
StreamHeader/dmabuf, KEYFRAME_REQ/RATE_HINT, addressed routing and the accept
hook. The PSK is given to it; how is the application's.

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
  Wi-Fi and TLS are. Anything the application needs to obtain its PSK is extra.
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
UDP, through the relay, and against a path-mode `Endpoint`, restarts included.

---

## 9. Stale text

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
| 43 | WebSocket fallback: `⬜ ⬜` | `⬜ client ⬜ client — served by the relay; clients designed in spec/terminating-endpoints.md` |
| 44 | Telesthetium hub: `⬜ ◑ (telesthitium)` | `— ✅ (telesthitium, see CONFORMANCE.md)` |
| 47-58 | "Notable gaps": Rust cipher data path, KEYFRAME wiring, Python Channel retransmit stub | Replace with: "WebSocket and WebTransport clients (none in either library); link lifecycle across restarts and many keys on one endpoint (spec/terminating-endpoints.md §4, §7)." |
| 70 | `109 Rust unit/integration tests` | `113` |
| 71 | `77 hub tests` | unchanged (56 unit + 21 integration) |

Also add one line under the title: "LIBRARY_COMPLETION.md is the verified audit;
this table follows it."

### 9.2 SPEC.md

Stale statements:

| Where | Now | Proposed |
|---|---|---|
| §2 table, Control row (70) | "Band management, signaling. Always reliable." | "Band management, signaling. Not retransmitted, except that HELLO is retried under §4.11." |
| §9.3 heading and body (819-834) | "WebSocket (Legacy fallback) — Future … Not yet implemented." | Replaced by the §9.3 text in section 10. |
| §9.6 last line (938-939) | "Not yet implemented." | "The reference relay serves WebTransport; neither reference library has a client." |
| §10 heading (943) | "Telesthetium Hub — Future" | "Telesthetium Hub (relay)", plus the paragraph in section 10. |
| §11.1 (1141-1147) | "‹name› Protocol … msgpack-encoded messages … Real-time modality data … uses Streams." | Heading "11.1 Application protocols". Body: "An application defines its own messages and carries them in Telesthete payloads. Requests, replies and anything else that must arrive belong on a Channel (§6), one application message per Channel message (§6.6 keeps the boundary). Real-time data that may be dropped belongs on Streams (§5)." |
| §12 intro (1153-1156) | "the Rust crate … is the v1.1 reference implementation (adds AF_UNIX, dmabuf, capability negotiation)" | "The Python package and the Rust crate are both v1.2 reference implementations, pinned to each other by `tests/vectors.json`. `rust/telesthitium` is the reference relay (§10)." |
| §12.2 table | Python/Rust "~3000", ESP32 "WebSocket ~300 Stream + Control only" | Python ≈ 4,300 and Rust ≈ 8,300 lines (`wc -l` of `telesthete/` and `rust/telesthete/src`, in-file tests and comments included); ESP32 row: "UDP or WebSocket, ≈ 1,100, link profile (§12.6)"; keep the browser rows. |
| §1, §3.1, §9.6 | — | The sentences from section 10. |
| §13 | — | Rows from section 10. |
| §16 | — | Changelog row from section 10. |

An application's name, taken out (text only; no rule changes). Below, ‹name› stands for that name as the current text spells it; the line numbers locate it.

| Where | Now | Proposed |
|---|---|---|
| §10.1 (1004) | "‹name› carries one §6.6 chunk per frame." | "An application may carry one §6.6 chunk per frame." |
| §10.1 (1047-1048) | "a deployment whose receivers authenticate with empty AAD (the ‹name› profile) seals them with empty AAD instead" | Drop "(the ‹name› profile)". |
| §10.1 Compatibility (1052-1053) | "'Old' below means the receivers deployed before this addendum (‹name› `9f0d6df`)" | "'Old' below means receivers deployed before this addendum". |
| §10.1 table (1059) | "‹name› worker transport (Python, empty AAD)" | "Application transport (Python, empty AAD)". |
| §10.1 table (1061) | "‹name› TS/Rust example ports" | "Application example ports (TypeScript, Rust)". |
| §10.1 (1066-1067) | "library Channels and the ‹name› transport (which sends `channel_id = 0`)" | "library Channels and application transports that send `channel_id = 0`". |
| §10.1 Security (1103) | "(the ‹name› transport: every keepalive)" | "(for example with every keepalive)". |
| §11 diagram (1122) | "Application (‹name›, KVM, GPU surface stream)" | "Application (KVM, GPU surface stream, …)". |
| §11.1 (1141-1147) | as above | as above (the stale-statement fix also removes the name). |
| §12.4 (1224) | "The only deployed consumer (‹name›) is upgraded in lockstep." | "The only deployed consumer at the time was upgraded in lockstep." |
| §12.5 (1260) | "negotiated by the application (‹name›, KVM)" | "negotiated by the application". |
| §16, row 1.2 (1352) | "sole consumer ‹name› upgraded in lockstep"; "‹name›'s Channel fragmentation envelope promoted to §6.6" | "sole consumer upgraded in lockstep"; "An application's Channel fragmentation envelope promoted to §6.6". |

### 9.3 Elsewhere

- `README.md:86`: "raise it to 60 for ‹name›, whose peers keep alive every 20 s"
  → "raise it to about three keepalive intervals if your peers keep alive less
  often than every 5 s".
- `README.md:89-90`: "[‹name›](…)'s hub installer and Docker image use these same
  assets." → delete.
- `README.md:101-102`: "`Board` and `Drop` are reserved." → "`Board`
  (replicated state) and `Drop` (resumable file transfer)."
- `README.md:105-106`: add "(WebSocket and WebTransport are served by the relay;
  library clients: see PROTOCOL_STATUS.md)".
- `LIBRARY_COMPLETION.md:16`: "Generalizes ‹name›'s build-31 fix." → "Generalizes
  an earlier application-side fix."
- `LIBRARY_COMPLETION.md:63-65` (WS/WT clients scoped out of the references):
  add "Superseded for WebSocket by spec/terminating-endpoints.md §6."

---

## 10. Addendum to SPEC.md (exact text)

This is the text to add to SPEC.md, unchanged by the implementation PRs except
to fill in the date. Section numbers are where each part belongs; the addendum
lists them together so the review reads one block.

````markdown
## Addendum 2 — Terminating endpoints and link lifecycle (v1.2)

Additive and wire-compatible: no frame, field, key derivation or channel type
changes, and `PROTOCOL_VERSION` stays 3. Everything here is a local behaviour of
a receiver or a server.

### §1 — `band_id` row, "Notes" column, replaced

Cleartext. Relay routing identifier, and the key selector at an endpoint that
holds many keys (§3.6).

### §3.1 — added after the last paragraph

A terminating endpoint (§10.2) is not a relay: it is a member of the band and
holds its PSK.

### §3.3 — added at the end of the list

- **Receiver restart.** Replay protection holds only while the receiver
  remembers the peer's session: its epoch (§4.3) and its high-water marks.
  The data key depends on the sender's epoch alone (§3.1), and HELLO and
  HELLO_ACK are under the base key. So a receiver that has restarted accepts a
  replay of a session recorded before the restart, and delivers its messages
  again. In both directions, and on every channel type. The same is true of a
  receiver that has forgotten a peer in any other way. Examples: it dropped
  the peer's state on GOODBYE, or it keys peers by address and the replay
  comes from a new one. A receiver SHOULD keep a peer's epoch and high-water
  marks after a GOODBYE or an eviction for as long as it can. Telesthete does
  not otherwise prevent such a replay. An application whose messages cause
  actions must therefore do one of two things. It uses a fresh PSK for each
  session, one that no receiver accepts after it restarts. Or it carries its
  own freshness, for example unique message ids with an expiry, remembered by
  the receiver across restarts. This version of the protocol has no
  protection across a receiver's restart. Closing the gap needs a key
  derivation to which the receiver contributes, which this version does not
  have.

### §3.6 Many keys on one endpoint

A PSK determines its `band_id` (§3.1), and every frame carries its `band_id` in
cleartext. An implementation MAY therefore hold many PSKs on one set of
transports and choose the key for a frame by its `band_id` before decrypting
it. Such an *endpoint*:

1. MUST drop a frame whose `band_id` it holds no key for, without attempting
   decryption.
2. MUST, once a key has been removed, neither decrypt nor deliver any further
   frame bearing that key's `band_id`.
3. Treats each key as an independent band: its own peers, session state, replay
   watermarks and sequence source (§3.3). It keeps a key's session epochs and
   watermarks for as long as it holds the key, also after a GOODBYE or an
   eviction.

Peers see nothing different: each sees an ordinary band.

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
client connects outbound over WSS, normally on TCP 443, to a *server*: a relay
(§10) or a terminating endpoint (§10.2). The client takes the whole URL from
its configuration. The reference relay serves the path `/band` (configurable);
other servers SHOULD use it too.

Both directions:

- Each Telesthete frame is one **binary** WebSocket message. No length prefix:
  the message preserves the boundary. Receivers ignore text messages and binary
  messages shorter than `MIN_PACKET_SIZE`.
- A connection carries one band. A client in several bands opens one connection
  per band.

A server:

- accepts the upgrade on its path whatever the query string, and requires no
  subprotocol, extension or particular `Origin`; it answers other paths with an
  HTTP error (the reference relay: 404);
- learns the connection's band from its first frame and sends nothing to the
  connection before that frame. It MAY drop later frames naming another band
  (the reference relay does);
- answers pings. It SHOULD close a connection that has sent nothing, pongs
  included, for 60 s, and MAY ping a quiet one (the reference relay pings every
  20 s);
- may close a connection at any time. Close status codes carry no meaning in
  this protocol; the reference relay sends none.

A client:

- sends a frame, normally HELLO, as soon as the connection opens;
- answers pings;
- on any close or error, reconnects with exponential backoff and jitter, and
  sends HELLO on each new connection. Frames sent while disconnected are lost,
  as on UDP; Channels retransmit;
- treats every frame on the connection as coming from one peer, the server's
  address. Through a relay that address stands for every other member of the
  band (§10); at a terminating endpoint it is the endpoint.

**Caveat (the reason §9.6 exists):** WebSocket runs over TCP. A lossy Stream
(§5) carried over TCP is silently turned reliable and in-order — a single lost
segment head-of-line-blocks every Channel and Stream multiplexed on that socket,
and "drop the late frame" (§5.2) becomes "retransmit the frame you wanted
dropped." Acceptable on a LAN with near-zero loss; pathological for real-time
video over a lossy path. Prefer §9.6 WebTransport whenever the client supports
it; fall back to §9.3 only when it does not.

### §9.6 — added after "Connection."

A terminating endpoint (§10.2) may serve WebTransport with the same carrier
mapping; this addendum does not specify it further.

### §10 Telesthetium Hub (relay) — replaces the heading "Telesthetium Hub — Future"

Added after the first paragraph:

The reference relay is `rust/telesthitium` (binary `telesthete-hub`); its
conformance inventory is `rust/telesthitium/CONFORMANCE.md`. A relay forwards
frames without saying which peer sent them: a peer reached through a relay sees
the other peers of the band at the relay's address. Peers that must tell each
other apart through a relay should use one band per pair (§3.6).

Added to "Connection modes":

- **Terminating:** the peer connects to a server that is itself the band's
  other member (§10.2). Nothing is relayed.

### §10.2 Terminating endpoints (optional)

A *terminating endpoint* is a server that is a member of every band it serves.
It holds those bands' keys (§3.6), accepts frames over §9.1 UDP, §9.3
WebSocket or both, and answers them as a peer. It never forwards a frame to
anyone else. A client reaches it exactly as it reaches a relay, with the same
addresses, URLs, frames and handshake, and cannot tell the two apart. In a band
served this way the endpoint is normally the client's only other member.

A terminating endpoint:

1. chooses the key by `band_id` and drops frames for bands it holds no key for,
   without decrypting (§3.6). It sends nothing in answer to a frame that does
   not authenticate, so it needs no source validation of the kind a relay may
   apply;
2. is a peer in each band: §3, §4 and §6 apply to it as to any peer. It drops
   Route frames (§2); §10.1 does not apply to it;
3. answers on the path a peer's frames arrive on: over UDP from the local
   address and port the peer sent to, over WebSocket on the same connection;
4. MAY treat each band as having one remote peer, reachable over whichever path
   most recently carried a frame that the band accepted (authenticated, not a
   replay under §3.3, and not ignored under §4.3). Then a peer that reconnects,
   or whose NAT mapping changes, keeps its session and its Channels. Only such
   frames move the path;
5. over WebSocket, behaves as a §9.3 server;
6. detects a lost peer by §4.5, not by the state of a connection.

When a terminating endpoint restarts, its sessions and connections are lost and
its session epoch increases (§4.3). Peers that implement §4.11 and §6.7
recover: over WebSocket when they reconnect, over UDP when they declare the
endpoint lost, or sooner if the endpoint sends HELLO to addresses it remembers.

For a terminating endpoint a restart is the ordinary case, and the limit in
§3.3 applies in full. The restarted endpoint accepts a replay of any session
recorded before the restart, on any path. Under rule 4 the replay also takes
over the path. A peer that restarts likewise accepts a replay of what the
endpoint sent it. An application that acts on messages uses a fresh PSK for
each session, or carries its own freshness (§3.3).

A relay and a terminating endpoint may run side by side on one host, on
different ports or paths.

### §12.6 Link profile

The smallest peer that can exchange requests and replies reliably. A *link
client* implements: §1; §3.1; the §3.2 baseline suite only; §3.3; §4.3, §4.4,
§4.5 and §4.8 with `capabilities: []` and `ciphers: ["chacha20-poly1305"]`, and
a session epoch that increases across restarts (persisted when the device has
no reliable clock); §4.2's rule to ignore unknown types; §4.11; §6 for at least
one `channel_id` as initiator, with any advertised window from 1 up (it may drop
every frame outside its window); §6.6 for every message, with a local cap on
reassembled size that the application must respect; §6.7 (c) to (g); and one
transport, §9.1 UDP or §9.3 WebSocket, to a relay or a terminating endpoint. It
may omit everything else. An ESP32 link client is estimated at about 1,100
lines of C.

### §13 Compatibility matrix — rows added

| Feature | Required to use it with a peer |
|---------|--------------------------------|
| Many keys on one endpoint (§3.6) | none — local to the endpoint; peers see ordinary bands |
| Reconnection (§4.11) | none — answering a repeated HELLO is already required |
| Channel lifecycle (§6.7) | none — local reactions; RST is the existing §6.2 flag |
| Terminating endpoint (§10.2) | none — clients reach it as they reach a relay; recovery after its restart needs §4.11 and §6.7 in the client |
| Receiver-restart note (§3.3) | none — it states a limit; nothing is sent differently |

### §16 Changelog — row added

| 1.2 (addendum 2) | 2026-11-?? | **Additive, wire-compatible; `PROTOCOL_VERSION` stays 3.** §3.6 many keys on one endpoint (choose the key by `band_id`). §4.11 optional reconnection (HELLO retry, re-HELLO after silence, channel reset on a peer's new session or GOODBYE). §6.7 Channel lifecycle: identity, accepting, reset, reopening, new incarnation, when to send RST, giving up. §9.3 WebSocket specified for servers and clients. §10 relay no longer "Future"; relays hide the sender; §10.2 optional terminating endpoints. §3.3 receiver restart: replay protection is lost with the receiver's memory, and what applications must do. §12.6 link profile. §2 Control is not "always reliable". §11.1 rewritten for any application. |
````

---

## 11. Decisions

Bake delegated the sign-off to the reviewer on 13 November 2026 ("Review and
approve it your self"). The reviewer took decisions 1 to 8, 10 and 11 that day.
Decision 9 is open, for Bake. Each entry gives the decision as taken, then the
alternative that was not chosen.

1. **Peers per key at an `Endpoint`: one remote peer per key, following the
   latest accepted path** (section 5.4). Reconnects and NAT changes keep
   their Channels. Taken by the reviewer, 13 Nov 2026.
   *Not chosen:* one peer per path, as a `Band` on a socket does today.
2. **Lifecycle behaviour is opt-in per Band and on by default only in
   `Endpoint`** (section 7.2). Taken by the reviewer, 13 Nov 2026.
   *Not chosen:* on everywhere.
3. **Python condition 1 is left as is, and the rule is stated in §6.7 (a)**
   (`Band.channel(id, other_peer)` keeps returning the channel bound to the
   first peer). Taken by the reviewer, 13 Nov 2026.
   *Not chosen:* fix it, which changes what an existing call returns.
4. **`websockets` as an optional extra, `telesthete[ws]`** (section 6.2).
   Taken by the reviewer, 13 Nov 2026.
   *Not chosen:* a hand-written RFC 6455 client in the package.
5. **Correct the SPEC §2 sentence "Control: Always reliable"** (section 9.2).
   Taken by the reviewer, 13 Nov 2026.
   *Not chosen:* make Control reliable.
6. **WebSocket close codes carry no meaning** (§9.3). Taken by the reviewer,
   13 Nov 2026.
   *Not chosen:* define codes, for example "no such key", which would tell an
   unauthenticated prober which `band_id`s a server holds.
7. **Servers SHOULD serve the WebSocket path `/band`** (§9.3). Taken by the
   reviewer, 13 Nov 2026.
   *Not chosen:* leave the path free.
8. **The terminating-endpoint conformance list goes in a new
   `spec/terminating-endpoint-conformance.md`** (T1 to T15 and the stated
   non-claim, section 5.6). Taken by the reviewer, 13 Nov 2026.
   *Not chosen:* only in the libraries' tests.
9. **Open, for Bake, and outside this addendum: whether Telesthete later makes
   the data key depend on the receiver too.** This is the real fix for replay
   after a restart (section 7.6): both epochs, or a nonce from each side,
   mixed into key derivation, negotiated, with new vectors. Nothing in this
   design waits on it. Until it is decided and built, applications use a fresh
   PSK per session or their own freshness (§3.3).
   *Alternatives on the table:* take it on later as its own negotiated
   addendum, or never, in which case the §3.3 note is the permanent statement
   of the limit.
10. **The epoch floor is not adopted now.** It is not prototyped, it protects
    one side only, it brings a lock-out mode and slower recovery, and the first
    application will use a fresh key per session. Revisit it together with
    decision 9. The analysis stays in section 7.6. Taken by the reviewer,
    13 Nov 2026.
    *Not chosen:* optional §10.2 text and §4.11 step 5, off by default in
    `Endpoint`.
11. **Fix Rust to keep a peer's epoch and control watermark after GOODBYE, in
    PR 1b.** Today it forgets both (`control.rs:413-419`), so a replay from
    the same address is accepted after a peer leaves cleanly (section 3).
    Python already keeps them. Only a replay changes outcome, because a
    conformant returning peer's real frames carry higher sequences or a newer
    epoch (§4.3). Taken by the reviewer, 13 Nov 2026.
    *Not chosen:* leave it, and Rust does not meet the §3.3 SHOULD.

---

## 12. Implementation plan

Each PR carries the SPEC.md text from section 10 that it implements, its tests
and a CHANGELOG line. Sizes are new or changed lines including tests: S < 300,
M 300-800, L 800-1,500. Where a Python PR and a Rust PR implement the same
section, the Python PR carries the SPEC.md text and the Rust PR refers to it.

| PR | Builds | SPEC.md text it carries | Tests | Depends on | Size |
|---|---|---|---|---|---|
| 0 | Stale text (section 9): PROTOCOL_STATUS.md, README, LIBRARY_COMPLETION; SPEC.md §2 Control row corrected (decision 5), §9.6, §10.1, §11, §11.1 (application's name removed), §12, §12.2, §12.4, §12.5, §16 | Section 9's SPEC.md edits; the §3.3 "Receiver restart" note and its §13 row (the limit exists on `main` today) | none (docs) | — | S |
| 1a | Python link lifecycle, §4.11 steps 1 to 4 and §6.7 (a) to (g): `Band(..., reconnect=False)` (off by default, decision 2), `on_channel`, `Channel.on_reset`, `Channel.state`, reopen fixes (always on), RST, give-up reset. Condition 1 unchanged (decision 3) | §4.11, §6.7, their §13 rows | mock-transport test per condition, failing first; live UDP restart, loss, partition | — | L |
| 1b | Rust link lifecycle: `set_reconnect`, `take_incoming_channels`, `resets()`, `ResetReason`, connection task survives give-up. Also keep a peer's epoch and control watermark after GOODBYE (decision 11) | refers to 1a's | as 1a; plus a replay from the same address after GOODBYE is rejected, and a conformant peer that returns after GOODBYE is accepted | — | L |
| 2 | Python `Band(transport=)` and `WebSocketTransport`, with `websockets` as the optional `[ws]` extra (decision 4) | §9.3 (servers SHOULD serve `/band`, decision 7; close codes carry no meaning, decision 6), §9.6 sentence | against the built relay: WS↔UDP, reconnect, WSS self-signed | — | M |
| 3a | Python `Endpoint`: UDP mode and path seam (`receive`, `close_path`, `send` callback), `add_key`, `remove_key`, one remote peer per key with path following (decision 1), `connect`, lifecycle on by default (decision 2). A key's epoch and watermarks are kept across GOODBYE and eviction while the key is held. New `spec/terminating-endpoint-conformance.md` with T1 to T15 and the stated non-claim (decision 8) | §1 row, §3.1 sentence, §3.6, §10 heading, paragraph and Terminating mode, §10.2, their §13 rows | T1 to T15; WS and UDP peers against a test server on the seam; restart; the replay probe as a test for T15 and for the non-claim | 1a, 2 | M-L |
| 3b | Rust `Endpoint` (same as 3a), internal I/O backend in `Transport`, `Path` | refers to 3a's | as 3a | 1b | L |
| 4 | Rust `Band::connect_ws`, feature `ws` | refers to 2's | as 2, plus against a path-mode Endpoint | 3b | M |
| 5 | Cross-language interop and link-profile configuration tests; Python↔Rust restart test; version bumps (Python 0.4.0, crates 0.3.0); release | §12.6, §16 changelog row | interop matrix | all above | M |

Not built, by decision: the epoch floor, §4.11 step 5 and the T16 claim
(decision 10); any change to key derivation (decision 9, open). None of them
appears in SPEC.md.

Eight PRs, against twelve in revision 0: the pairwise key PR and the two
attachment PRs are gone, and the relay is untouched.

Order and parallelism:

- **Wave 1, in parallel:** 0, 1a, 1b, 2. None depends on another.
- **Wave 2:** 3a after 1a and 2; 3b after 1b; in parallel with each other.
- **Wave 3:** 4 after 3b.
- **Then 5.**

A Python-only user needs 1a, 2 and 3a; the Rust PRs run alongside.

---

## 13. What I could not verify

- **ESP32 sizes** are estimates. I did not build a link client.
- **Throughput of a Python terminating endpoint** (UDP and WebSocket I/O plus
  decryption in one process) was not measured.
- **WebSocket close on the wire**: I saw the relay close without a status code
  through the `websockets` client (it reports 1005); I did not capture the
  frames.
- **Path following** (section 5.4) is designed, not prototyped. The claim that
  the per-key view can learn "accepted" from the Band without new public API
  rests on reading the receive paths, not on trying it.
- **Line counts** in §12.2's proposed table are from `wc -l` on the package
  sources (4,294 Python, 8,258 Rust), rounded; they include comments and
  in-file tests, so they overstate the code a client would need.
- **Probe scripts.** The condition probe, the WebSocket probe and the replay
  probe (with its small Rust receiver) in section 3 are throwaway scripts,
  described in the pull request, not in the repository.
- **Python 3.11 and 3.12**, which `setup.py` lists, were not run; only 3.14.
- **Multi-homed UDP replies.** That a wildcard-bound socket can answer from the
  wrong source address is standard behaviour; I did not reproduce it.
- **Replay checks (section 3, section 7.6).**
  - The `Endpoint` rows are by reading, since nothing is built.
  - Rust was run for the restart and GOODBYE cases with Stream only. The
    Rust Channel, the passive-side restart and the new-address case are by
    reading.
  - Board and Drop were not tried. They use the same per-address state, so I
    expect the same results.
  - The Rust map-cap path (`PEER_MAP_SOFT_CAP`) was not exercised.
  - All runs were on loopback.
- **Epoch floor** (section 7.6) is reasoned, not prototyped, and not adopted
  (decision 10). I did not model
  it formally. The cases I checked are a crash between the write and the first
  frame, a retried HELLO, a peer without step 5, and an endpoint that sends
  HELLO first (hence F4).
- **The real fix** (decision 9) is named, not designed. Its interaction with
  the existing order of HELLO, HELLO_ACK and the first data frame is not
  worked out.
