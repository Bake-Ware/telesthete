# telesthitium

The **reference relay hub** for the Telesthete wire protocol — SPEC §10
("Telesthetium Hub").

The hub matches peers by the cleartext 16-byte `band_id` prefix of each frame
and bridges opaque ciphertext between every peer in the same band. It holds no
PSK and **cannot decrypt** — it reads only the cleartext header (`band_id`, and
`channel_type`/`channel_id` for WebTransport carrier selection); everything from
offset 27 on is opaque.

It terminates all four transports §10 names and bridges freely between them:

| Transport | SPEC | Notes |
|-----------|------|-------|
| UDP | §9.1 | primary; one socket, one writer task |
| WebSocket / WSS | §9.3 | native TLS in-hub, or plain WS behind a TLS proxy |
| WebTransport / HTTP-3 | §9.6 | QUIC; channel-type → carrier mapping, self-signed hash pinning |
| AF_UNIX | §9.4 | `SOCK_SEQPACKET`, one `<band_id_hex>.sock` per active band |

The canonical unit of relay is one whole Telesthete frame: each transport
de-frames on ingress and re-frames on egress. See **`CONFORMANCE.md`** for the
claim-by-claim spec inventory and the test that proves each one.

## Build

From the repo's `rust/` workspace:

```bash
cd rust
cargo build --release -p telesthitium
# binary: target/release/telesthete-hub
cargo test -p telesthitium          # 46 tests
```

## Run

```bash
# UDP only (the default)
RUST_LOG=info ./target/release/telesthete-hub

# UDP + plain WS (TLS terminated by a reverse proxy) + AF_UNIX
HUB_WS_BIND=0.0.0.0:7475 HUB_UNIX_DIR=$XDG_RUNTIME_DIR/telesthete ./telesthete-hub

# UDP + native WSS + WebTransport (self-signed identity auto-generated)
HUB_WS_BIND=0.0.0.0:443 HUB_WS_TLS=1 HUB_WT_BIND=0.0.0.0:443 \
  HUB_TLS_SANS=relay.example.com,203.0.113.10 ./telesthete-hub
```

## Configuration

Every tunable is an environment variable; bad values fall back to the default
and are never fatal. Transports other than UDP are opt-in via their bind
address / directory.

| Var | Default | Meaning |
|-----|---------|---------|
| `HUB_BIND` | `0.0.0.0:7474` | UDP bind (§9.1); `off` to disable |
| `HUB_WS_BIND` | *(off)* | WS/WSS bind (§9.3) |
| `HUB_WS_PATH` | `/band` | WS endpoint path |
| `HUB_WS_TLS` | `false` | terminate native TLS on the WS listener |
| `HUB_WT_BIND` | *(off)* | WebTransport/QUIC bind (§9.6); requires a TLS identity |
| `HUB_UNIX_DIR` | *(off)* | AF_UNIX socket directory (§9.4) |
| `HUB_TLS_CERT` / `HUB_TLS_KEY` | *(self-signed)* | operator cert/key PEM paths |
| `HUB_TLS_SANS` | `localhost` | SANs for the auto-generated self-signed cert |
| `HUB_PEER_TTL_SECS` | `15` | idle eviction = `PEER_TIMEOUT` (§12.4) |
| `HUB_PRUNE_SECS` | `5` | eviction sweep interval |
| `HUB_MAX_BANDS` | `4096` | band cap |
| `HUB_MAX_PEERS_PER_BAND` | `256` | per-band peer cap |
| `HUB_CONN_QUEUE` | `1024` | per-connection outbound queue depth |
| `HUB_UDP_VALIDATION_PACKETS` | `2` | UDP packets required before a source is an eligible destination (`1` disables) |
| `RUST_LOG` | `info` | `tracing` env-filter |

### TLS

WebTransport mandates TLS 1.3, and native WSS needs a cert too. Supply an
operator cert with `HUB_TLS_CERT`/`HUB_TLS_KEY`, or let the hub generate a
self-signed **ECDSA P-256** identity (validity ≤ 14 days, per the browser
`serverCertificateHashes` rule, §9.6). The identity's SHA-256 is logged at
startup so a browser can pin it.

## systemd

`systemd/telesthete-hub.service` is a hardened unit (no new privileges, strict
sandbox, syscall filter to `@system-service`) that reads its configuration from
`/etc/telesthete-hub.env`. Install:

```bash
sudo install -m 0755 target/release/telesthete-hub /usr/local/bin/
sudo install -m 0644 systemd/telesthete-hub.service /etc/systemd/system/
printf 'HUB_BIND=0.0.0.0:7474\nRUST_LOG=info\n' | sudo tee /etc/telesthete-hub.env
sudo useradd --system --no-create-home --shell /usr/sbin/nologin telesthete
sudo systemctl daemon-reload
sudo systemctl enable --now telesthete-hub
```

## Security model

- **No keys, no plaintext.** The hub cannot decrypt; a compromised hub leaks
  only traffic patterns (who is on which band, and when), never payloads.
- **UDP validation gate.** A UDP source must send a few packets before the hub
  will relay a band's traffic to it (`HUB_UDP_VALIDATION_PACKETS`, default 2).
  This is a mild robustness measure, **not** a return-routability proof — a
  determined spoofer can send several forged-source packets as cheaply as one,
  so it does not defend against deliberate reflection/amplification (a blind
  relay cannot issue the echoed cookie that would). Connection transports
  (WSS/WebTransport/AF_UNIX) prove routability via their handshake and are bound
  to a single band per connection.
- **Bounded memory.** Band and per-band peer caps, plus bounded per-connection
  queues (drop-on-overflow), cap resource use under flood.

## Protocol notes

- Frame layout per SPEC §1: `band_id (16) || channel_type (1) || channel_id (2)
  || sequence (8) || ciphertext (variable)`. The hub parses only the first 19
  bytes; the rest is opaque.
- Minimum accepted packet length is `MIN_PACKET_SIZE` = 43 (§1) — malformed
  shorter packets are dropped.
- Discovery is implicit: sending a frame for a `band_id` registers the peer for
  that band. The hub never originates frames of its own.
- Eviction: peers unseen for `HUB_PEER_TTL_SECS` are dropped; empty bands are
  reaped.
