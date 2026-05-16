# telesthitium

Discovery + relay hub for the Telesthete wire protocol — SPEC §10
("Telesthetium Hub").

The hub is a dumb forwarder. It reads the cleartext 16-byte `band_id`
prefix of each Telesthete frame and bridges packets to every other peer
that has spoken the same `band_id` recently. It has no PSK, no
identity beyond network address, and cannot decrypt any payload.

## Build

From the repo's `rust/` workspace:

```bash
cd rust
cargo build --release -p telesthitium
# binary: target/release/telesthete-hub
```

## Run

```bash
RUST_LOG=info HUB_BIND=0.0.0.0:7474 ./target/release/telesthete-hub
```

Environment:

| Var                 | Default        | Meaning                          |
|---------------------|----------------|----------------------------------|
| `HUB_BIND`          | `0.0.0.0:7474` | UDP bind address                 |
| `HUB_PEER_TTL_SECS` | `60`           | Idle eviction threshold          |
| `HUB_PRUNE_SECS`    | `10`           | Eviction sweep interval          |
| `RUST_LOG`          | `info`         | `tracing` env-filter             |

## systemd

`systemd/telesthete-hub.service` is a hardened unit (no new privs,
strict sandbox, syscall filter to `@system-service`). Install:

```bash
sudo install -m 0755 target/release/telesthete-hub /usr/local/bin/
sudo install -m 0644 systemd/telesthete-hub.service /etc/systemd/system/
sudo useradd --system --no-create-home --shell /usr/sbin/nologin telesthete
sudo systemctl daemon-reload
sudo systemctl enable --now telesthete-hub
```

## Protocol notes

- Frame layout per SPEC §1: `band_id (16) || channel_type (1) ||
  channel_id (2) || sequence (8) || ciphertext (variable)`.
- The hub parses only the first 16 bytes (`band_id`). Everything else
  is opaque bytes that get copied to the destination peers as-is.
- Minimum accepted packet length is 27 (the cleartext header); the spec
  baseline 43 (header + 16-byte AEAD tag) is enforced by peers, not the
  hub.
- Discovery is implicit: the act of sending a packet with a given
  `band_id` registers the peer for that band. The hub never originates
  packets of its own.
- Eviction: peers that haven't been seen for `HUB_PEER_TTL_SECS` are
  dropped from the registry; bands with no peers are reaped.

## Not yet implemented

- WSS (SPEC §9.3) — for hostile NAT / firewall.
- Per-band auth / allowlist.
- Metrics endpoint.
