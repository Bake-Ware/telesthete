# Telesthetium Hub — Conformance Inventory

The reference hub implements **SPEC §10** in full: it matches peers by `band_id`
and bridges opaque ciphertext between them across every transport §10 names —
UDP (§9.1), WebSocket/WSS (§9.3), WebTransport/HTTP-3 (§9.6), and AF_UNIX (§9.4).

**Status: complete.** Every claim below is covered by a test — 33 unit tests
plus 13 integration tests (46 total), all green, zero warnings. Run them with
`cargo test -p telesthitium`.

**What the hub is NOT.** The hub *cannot decrypt* (§10): it holds no PSK and
never derives a `Key`. It therefore does not implement crypto (§3), the control
channel semantics (§4), Channel reliability (§6), Stream watermarking (§5),
capability/cipher negotiation (§12.5), or LAN discovery (§9.2 — explicitly a
peer, no-hub mechanism). The hub reads only the **cleartext** header fields
(§1): `band_id` for routing, and `channel_type`/`channel_id` for selecting the
correct WebTransport carrier (§9.6). Everything from offset 27 on is opaque.

## A. Frame routing (§1, §12.4) — `src/frame.rs`

| ID | Claim | Test |
|----|-------|------|
| A1 | `band_id` = bytes 0..16 (cleartext, raw). | `frame::band_id_is_first_16` |
| A2 | `channel_type` = byte 16; `channel_id` = bytes 17..19 (uint16 BE). | `frame::reads_channel_type_and_id_be` |
| A3 | `MIN_PACKET_SIZE` = 43; shorter packets are malformed and dropped. | `frame::rejects_below_min_packet`, `frame::accepts_exactly_min` |
| A4 | Unknown `channel_type` (0x05+) still yields `band_id`+`channel_id` for routing. | `frame::unknown_channel_type_still_routable` |
| A5 | Routing agrees byte-for-byte with the library (`telesthete::{Header, ChannelType}`). | `frame::matches_library_header` |

## B. Band registry & relay semantics (§10) — `src/registry.rs`

| ID | Claim | Test |
|----|-------|------|
| B1 | Delivered to every *other* peer in the band; never echoed to the sender. | `registry::forward_excludes_sender` |
| B2 | Different bands never see each other's traffic. | `registry::bands_are_isolated` |
| B3 | Sending registers the sender (implicit discovery, §10). | `registry::send_registers_peer` |
| B4 | Frames are relayed verbatim to message transports. | `registry::relay_is_byte_exact` |
| B5 | UDP `SocketAddr` keys and connection-id keys never collide. | `registry::mixed_transport_keys_distinct` |

## C. Lifecycle: TTL & pruning (§10, §12.4)

| ID | Claim | Test |
|----|-------|------|
| C1+C2 | Idle **UDP** peers are evicted; empty bands are reaped. | `registry::prune_evicts_stale_udp_and_reaps` |
| C3 | Activity within TTL keeps a UDP peer alive across sweeps. | `registry::active_udp_peer_survives_prune` |
| C4 | Default TTL = `PEER_TIMEOUT` (15 s, §12.4); env-overridable. | `config::default_ttl_is_peer_timeout` |
| C5 | Connection peers are removed on disconnect, and are **never** idle-pruned while connected (a receive-only listener stays reachable). | `registry::remove_on_disconnect`, `registry::conn_peer_survives_prune_while_connected` |

## D. Robustness / anti-abuse (hardening required of a reference)

| ID | Claim | Test |
|----|-------|------|
| D1 | Bands are capped; a new band beyond the cap is refused. | `registry::band_cap_enforced` |
| D2 | Peers-per-band are capped; overflow refused. | `registry::peer_cap_enforced` |
| D3 | A slow peer's outbound queue is bounded; overflow drops (logged). | `registry::slow_peer_queue_bounded` |
| D4 | A UDP source is not a relay destination until it has sent `udp_validation_packets` (default 2). This is a mild robustness gate, **not** a return-routability proof — see the caveat under Deviations. | `registry::udp_dest_requires_validation`, `registry::udp_validation_disabled_when_one` |
| D5 | Connection peers (WSS/WT/AF_UNIX) are eligible destinations immediately. | `registry::conn_peer_eligible_immediately` |

## E. UDP transport (§9.1) — `src/udp.rs`, `tests/it_udp.rs`

| ID | Claim | Test |
|----|-------|------|
| E1+E2 | Relays whole datagrams between UDP peers; boundaries preserved, byte-exact. | `two_udp_peers_relay` |
| E3 | Malformed (<43 B) datagrams are ignored without disturbing the band. | `short_datagram_ignored` |

## F. WebSocket / WSS transport (§9.3) — `src/ws.rs`, `tests/it_ws.rs`

| ID | Claim | Test |
|----|-------|------|
| F1 | The URL path (`/band`) selects the endpoint; wrong paths are rejected. | `connects_on_band_path` |
| F2 | One WS binary frame = one Telesthete packet, no length prefix. | `binary_frame_is_one_packet` |
| F3 | A WS peer and a UDP peer bridge both directions. | `ws_udp_bridge` |
| F4 | Native TLS (WSS) terminates in-hub; plain WS is the default for a reverse proxy. | `wss_native_tls` (+ `connects_on_band_path`/`binary_frame_is_one_packet` cover plain) |

## G. WebTransport transport (§9.6) — `src/wt.rs`, `tests/it_wt.rs`, `src/tls.rs`

| ID | Claim | Test |
|----|-------|------|
| G1 | Accepts sessions at `/telesthete?band=<hex>`; wrong route rejected. | `session_accept_and_band_from_query`, `wt::parse_band_*` |
| G2 | Stream (0x01) frames egress as datagrams, one per packet, no prefix. | `carrier_mapping_bridges_udp_bidirectionally` |
| G3 | Channel (0x02) frames egress on a per-`channel_id` bidi stream, 2-byte BE length-prefixed. | `carrier_mapping_bridges_udp_bidirectionally` |
| G4 | Control (0x00) frames egress on a dedicated reliable stream, length-prefixed. | `carrier_mapping_bridges_udp_bidirectionally` |
| G5 | Ingress: datagrams are one frame each; reliable streams are de-framed by the length prefix. | `carrier_mapping_bridges_udp_bidirectionally` |
| G6 | A WT peer bridges to UDP/WSS/AF_UNIX peers in the same band. | `carrier_mapping_bridges_udp_bidirectionally`, `it_bridge` |
| G7 | Self-signed identity is ECDSA P-256, validity ≤ 14 days; SHA-256 DER hash exposed for `serverCertificateHashes`. | `tls::self_signed_is_p256`, `tls::self_signed_rejects_long_validity`, `tls::exposes_cert_hash` (end-to-end: the WT tests pin by hash) |

## H. Cross-transport bridging (§10, §11) — `tests/it_bridge.rs`

| ID | Claim | Test |
|----|-------|------|
| H1 | One band, one peer each on UDP/WSS/WT/AF_UNIX — a frame reaches all others with correct per-transport framing. | `all_four_transports_one_band` |
| H2 | Reframing is source-agnostic: a Channel frame from the WSS peer reaches the WT peer length-prefixed on a reliable stream. | `all_four_transports_one_band` |

## I. AF_UNIX transport (§9.4) — `src/unix.rs`, `tests/it_unix.rs`

| ID | Claim | Test |
|----|-------|------|
| I1 | Binds `<band_id_hex>.sock` under the socket dir; local SEQPACKET peers are bridged into their band. | `local_peer_bridged_with_boundaries` |
| I2 | One SEQPACKET message = one Telesthete packet (boundaries preserved). | `local_peer_bridged_with_boundaries` |
| I3 | The socket directory is created 0700 (directory perms are the access control). | `socket_dir_is_0700` |

## J. Operability

| ID | Claim | Test |
|----|-------|------|
| J1 | Env tunables have documented defaults; bad values fall back, never panic. | `config::env_parsing_is_lenient` |
| J2 | Each transport's `serve` resolves promptly on its shutdown signal. | `it_shutdown::{udp,ws}_serve_stops_on_shutdown` |
| J3 | The hub never logs payload bytes (it can't read them); logs are band_id/peer/counters only. | code review — no frame contents in any `tracing` call |

## Deviations & interpretations (for reviewer awareness)

- **AF_UNIX addressing (§9.4).** §9.4 describes a peer-to-peer local model (a peer
  binds `<band_id_hex>.sock`; others connect). A relay hub has no fixed peer, so
  the hub itself is the local rendezvous: it binds a band's socket once that band
  is active on the hub (seeded from any transport) via a reconciliation loop, and
  local peers connect to it. Consequence: a purely-local band with no non-unix
  peer has no socket until something else creates the band. This matches §10's
  "bridges … to AF_UNIX peers in the same band" while staying practical.
- **Board (0x03) / Drop (0x04) over WebTransport.** §9.6 defines carriers only for
  Stream/Channel/Control. These future channel types (and any unknown type) egress
  on the dedicated reliable stream — the no-loss default — until §9.6 specifies a
  carrier for them.
- **UDP min-packet.** The hub enforces `MIN_PACKET_SIZE` = 43 (§1), rejecting the
  27..42-byte range the earlier prototype accepted, since such packets are
  malformed per §1.
- **UDP validation gate (D4)** is a minor hardening measure the spec does not
  mandate; it defaults on (`udp_validation_packets = 2`) and is disableable via
  env (`HUB_UDP_VALIDATION_PACKETS=1`). It is **not** a true return-routability
  check: a determined spoofer can send N forged-source packets as cheaply as
  one, so it does not defend against deliberate reflection/amplification — real
  return-routability needs an echoed cryptographic cookie, which a blind relay
  cannot issue. It only stops a single stray packet from making the hub a
  reflector for that address.
- **Connection peers are not idle-pruned.** TTL eviction (C1) applies only to UDP
  peers; a WSS/WebTransport/AF_UNIX peer's liveness is its transport connection
  (removed on disconnect, plus a WS ping/idle-timeout for dead-but-unclosed
  sockets), so a legitimate receive-only listener is never silently dropped.
- **Connections are pinned to one band.** Although §10 routes purely by
  `band_id`, each WSS/WebTransport/AF_UNIX connection is bound to the band of its
  first frame (or, for WebTransport, its `?band=` query); frames naming a
  different `band_id` are dropped rather than relayed cross-band (defense in
  depth — `band_id` is still the routing capability, so this removes no ability
  a peer couldn't get by connecting to that band directly).
