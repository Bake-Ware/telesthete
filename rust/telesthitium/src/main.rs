//! Telesthete Hub — band-id based UDP relay.
//!
//! Per SPEC §10, the hub is a dumb forwarder. It sees the 16-byte cleartext
//! `band_id` prefix of every packet and bridges packets to every other peer
//! that has spoken the same `band_id` recently. No decryption, no PSK, no
//! identity beyond network address.
//!
//! Wire frame parsed: first 16 bytes (band_id). Everything else is opaque.
//! Min legal Telesthete packet is 27 (header) + 16 (auth tag) = 43 bytes,
//! but we accept anything ≥ 27 to stay robust against future header tweaks.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::signal;
use tokio::sync::RwLock;
use tokio::time;
use tracing::{debug, info, warn};

type BandId = [u8; 16];

#[derive(Clone, Copy)]
struct PeerEntry {
    addr: SocketAddr,
    last_seen: Instant,
}

struct State {
    bands: RwLock<HashMap<BandId, Vec<PeerEntry>>>,
    peer_ttl: Duration,
}

fn hex16(b: &BandId) -> String {
    let mut s = String::with_capacity(32);
    for byte in b {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bind = std::env::var("HUB_BIND").unwrap_or_else(|_| "0.0.0.0:7474".to_string());
    let ttl_secs: u64 = std::env::var("HUB_PEER_TTL_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let prune_secs: u64 = std::env::var("HUB_PRUNE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);

    let sock = Arc::new(UdpSocket::bind(&bind).await?);
    let local = sock.local_addr()?;
    info!(%local, peer_ttl_secs = ttl_secs, prune_secs, "telesthete-hub listening");

    let state = Arc::new(State {
        bands: RwLock::new(HashMap::new()),
        peer_ttl: Duration::from_secs(ttl_secs),
    });

    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker = time::interval(Duration::from_secs(prune_secs));
            loop {
                ticker.tick().await;
                prune(&state).await;
            }
        });
    }

    let recv_state = state.clone();
    let recv_sock = sock.clone();
    let recv_task = tokio::spawn(async move {
        let mut buf = vec![0u8; 65_535];
        loop {
            let (n, src) = match recv_sock.recv_from(&mut buf).await {
                Ok(x) => x,
                Err(e) => {
                    warn!(error = %e, "recv_from failed");
                    continue;
                }
            };
            if n < 27 {
                debug!(n, %src, "short packet, ignoring");
                continue;
            }
            let mut band: BandId = [0u8; 16];
            band.copy_from_slice(&buf[0..16]);
            // Cheap clone — typical Telesthete packets are <1500 bytes.
            let packet = buf[..n].to_vec();
            let state = recv_state.clone();
            let sock = recv_sock.clone();
            tokio::spawn(async move {
                relay(&state, &sock, band, src, packet).await;
            });
        }
    });

    tokio::select! {
        _ = signal::ctrl_c() => info!("SIGINT received, shutting down"),
        _ = sigterm() => info!("SIGTERM received, shutting down"),
        _ = recv_task => warn!("recv task exited unexpectedly"),
    }

    Ok(())
}

async fn sigterm() {
    if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
        s.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn relay(state: &State, sock: &UdpSocket, band: BandId, src: SocketAddr, packet: Vec<u8>) {
    let dests: Vec<SocketAddr> = {
        let mut bands = state.bands.write().await;
        let entries = bands.entry(band).or_default();
        let mut found = false;
        let now = Instant::now();
        for e in entries.iter_mut() {
            if e.addr == src {
                e.last_seen = now;
                found = true;
                break;
            }
        }
        if !found {
            info!(band = %hex16(&band), peer = %src, n_peers = entries.len() + 1, "peer joined band");
            entries.push(PeerEntry {
                addr: src,
                last_seen: now,
            });
        }
        entries
            .iter()
            .filter(|e| e.addr != src)
            .map(|e| e.addr)
            .collect()
    };

    for d in dests {
        if let Err(e) = sock.send_to(&packet, d).await {
            warn!(error = %e, dest = %d, "send_to failed");
        }
    }
}

async fn prune(state: &State) {
    let now = Instant::now();
    let mut bands = state.bands.write().await;
    let mut empty: Vec<BandId> = Vec::new();
    let mut evicted = 0usize;
    for (band, entries) in bands.iter_mut() {
        let before = entries.len();
        entries.retain(|e| now.duration_since(e.last_seen) <= state.peer_ttl);
        let removed = before - entries.len();
        if removed > 0 {
            debug!(band = %hex16(band), removed, "pruned stale peers");
            evicted += removed;
        }
        if entries.is_empty() {
            empty.push(*band);
        }
    }
    for b in empty {
        bands.remove(&b);
    }
    if evicted > 0 {
        info!(evicted, n_bands = bands.len(), "prune cycle");
    }
}
