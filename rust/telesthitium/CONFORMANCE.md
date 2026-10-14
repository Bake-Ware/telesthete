# Telesthetium Hub — Conformance Inventory

The reference hub implements **SPEC §10** in full: it matches peers by `band_id`
and bridges opaque ciphertext between them across every transport §10 names —
UDP (§9.1), WebSocket/WSS (§9.3), WebTransport/HTTP-3 (§9.6), and AF_UNIX (§9.4).

**Status: complete.** Every claim below is covered by a test — 56 unit tests
plus 21 integration tests (77 total), all green. Run them with
`cargo test -p telesthitium`.

**What the hub is NOT.** The hub *cannot decrypt* (§10): it holds no PSK and
never derives a `Key`. It therefore does not implement crypto (§3), the control
channel semantics (§4), Channel reliability (§6), Stream watermarking (§5),
capability/cipher negotiation (§12.5), or LAN discovery (§9.2 — explicitly a
peer, no-hub mechanism). The hub reads only the **cleartext** header fields
(§1): `band_id` for routing, `channel_type`/`channel_id` for addressed routing
(§10.1) and for selecting the correct WebTransport carrier (§9.6). Everything
from offset 27 on is opaque.

## A. Frame routing (§1, §12.4) — `src/frame.rs`

| ID | Claim | Test |
|----|-------|------|
| A1 | `band_id` = bytes 0..16 (cleartext, raw). | `frame::band_id_is_first_16` |
| A2 | `channel_type` = byte 16; `channel_id` = bytes 17..19 (uint16 BE). | `frame::reads_channel_type_and_id_be` |
| A3 | `MIN_PACKET_SIZE` = 43; shorter packets are malformed and dropped. | `frame::rejects_below_min_packet`, `frame::accepts_exactly_min` |
| A4 | Unknown `channel_type` (0x07+) still yields `band_id`+`channel_id` for routing. | `frame::unknown_channel_type_still_routable` |
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
| D6 | Every UDP peer has its own bounded egress queue (`HUB_UDP_PEER_QUEUE`, default 256), so one peer's overflow never drops another peer's frames. | `registry::udp_peer_queues_are_independent`, `udp::each_udp_peer_gets_its_own_queue`, `it_route::burst_to_one_udp_peer_does_not_starve_another` |
| D7 | Bytes queued across **all** UDP peers are capped hub-wide (`HUB_UDP_QUEUE_BYTES`, default 32 MiB); a frame that would exceed it is dropped like a full queue, and bytes are refunded as writers dequeue. Connection queues are bounded per connection (D3) and not budgeted. | `registry::udp_byte_budget_is_hub_wide`, `registry::byte_budget_refunds_refused_frames`, `udp::writer_refunds_budget_as_frames_leave`, `config::defaults_are_sane` |
| D8 | A UDP peer's queue and writer task are created only once it is validated (D4): a one-packet (possibly spoofed) source costs a registry entry, not a queue. | `registry::observe_udp_builds_sink_only_on_validation` |

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

## K. Addressed routing (§10.1) — `src/registry.rs`, `tests/it_route.rs`

| ID | Claim | Test |
|----|-------|------|
| K0 | `channel_type` 0x05 parses as Route and 0x06 as Addressed; `channel_id` is the declared / destination route id. | `frame::route_type_is_parsed` |
| K1 | A Route frame records the sender's route id and is never relayed or federated. | `registry::route_frame_is_consumed_and_recorded`, `registry::route_frame_is_not_federated` |
| K2 | An Addressed (0x06) frame with a nonzero `channel_id` that an eligible peer other than the sender declared reaches only the declarer(s) — all of them on a collision. | `registry::addressed_frame_reaches_only_declarers` |
| K3 | Addressed with `channel_id` 0 or an id with no eligible declarer, and **every other type — Channel (0x02) included** — are broadcast unchanged; library Channel connection ids are never treated as addresses. | `registry::undeclared_or_zero_or_non_addressed_broadcasts`, `registry::library_channel_ids_are_never_addressed` |
| K4 | Addressed frames are never echoed; if the sender is the only declarer, the frame is broadcast. | `registry::addressed_never_echoes_and_sole_self_declarer_falls_back` |
| K5 | Only eligible declarers count: a still-validating declarer neither narrows delivery nor receives; its declaration takes effect once it validates. | `registry::unvalidated_declarer_is_inert` |
| K6 | A re-declaration replaces the id; declaring 0 withdraws it. | `registry::redeclare_replaces_and_zero_clears` |
| K7 | Route ids are forgotten on disconnect and on idle eviction. | `registry::route_forgotten_on_disconnect_and_prune` |
| K8 | Declarations are band-scoped. | `registry::routes_are_band_scoped` |
| K9 | Link-injected frames are addressed against local declarations (Channel still broadcast); a Route frame from a link is dropped. | `registry::link_injection_honours_local_routes` |
| K10 | End to end over UDP sockets: addressed, broadcast, undeclared, and Channel frames reach the right peers. | `it_route::addressed_frames_reach_only_the_declarer` |
| K11 | Route squatting: a spoofed or unvalidated declaration of a victim's id can neither divert nor black-hole the victim's traffic; a validated squatter only becomes a co-declarer. | `registry::spoofed_declaration_cannot_black_hole_an_id`, `it_route::one_packet_spoofed_declaration_cannot_divert_traffic` |
| K12 | NAT rebinding: when an eligible UDP peer holds an id, a same-IP entry holding that id and silent since the new entry appeared loses it; live same-IP peers and other IPs keep theirs. | `registry::nat_rebind_moves_the_route_to_the_new_mapping`, `registry::live_peers_behind_one_nat_keep_sharing_an_id` |

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
- **Addressed routing (§10.1)** reads `channel_type`/`channel_id`, which are
  not authenticated to the hub (it has no key). Anyone who knows a band's
  `band_id` can forge a Route frame: forging one from a victim's address can
  withdraw or change the victim's id until its next re-declaration (targeted
  delivery denial, bounded by the re-declaration interval). Spoofed or
  unvalidated declarations are inert (K11), and a declaration can never route
  a frame to nobody. Payload AEAD is unchanged. See SPEC §10.1 Security.
- **UDP egress memory.** Worst case is bounded by `HUB_UDP_QUEUE_BYTES`
  (D7) regardless of how many UDP sources an attacker mints; per-peer depth
  (`HUB_UDP_PEER_QUEUE`) only shapes fairness below that. The budget charges
  a broadcast frame once per destination queue although the bytes are
  `Arc`-shared, so it over-counts (conservative). Validated peers only (D8).
- **Writer overlap after re-admission.** An evicted UDP peer's writer drains
  frames already queued before it exits; if the same address is re-admitted
  meanwhile, two writers briefly send to it. Only frames queued before the
  eviction can be reordered against new ones — harmless for UDP, whose
  receivers already tolerate reordering.
- **NAT rebind window.** Without a same-IP declaration from the new mapping
  (e.g. a peer that never re-declares, or one behind a NAT that also changes
  its IP), the old entry keeps its id until idle eviction (`HUB_PEER_TTL_SECS`,
  15 s); the addressee misses addressed frames for that window.
- **Connections are pinned to one band.** Although §10 routes purely by
  `band_id`, each WSS/WebTransport/AF_UNIX connection is bound to the band of its
  first frame (or, for WebTransport, its `?band=` query); frames naming a
  different `band_id` are dropped rather than relayed cross-band (defense in
  depth — `band_id` is still the routing capability, so this removes no ability
  a peer couldn't get by connecting to that band directly).
