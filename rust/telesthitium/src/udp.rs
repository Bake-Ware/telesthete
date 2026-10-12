//! UDP transport (SPEC §9.1).
//!
//! One socket serves the whole hub. Each UDP peer gets its **own** bounded
//! egress queue and a writer task that drains it onto the shared socket, so
//! relay dispatch (`Registry::forward`) stays synchronous — no per-packet task
//! spawning, no `send_to().await` under any lock — and a burst addressed to
//! one peer can only overflow that peer's queue, never drop another peer's
//! frames (previously one hub-wide queue was shared by every UDP peer).
//! Ingress: each datagram is one frame (boundaries preserved); malformed
//! (<43 B) datagrams are dropped without disturbing the band.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::frame;
use crate::registry::{PeerKey, Registry, Sink};

/// Serve UDP until `shutdown` resolves. Binds `bind`, relays datagrams between
/// UDP peers (and, via the shared registry, to peers on other transports).
///
/// `peer_queue` is the depth of **each** UDP peer's egress queue (floored at
/// 64, like the connection transports); see `Config::udp_peer_queue`.
pub async fn serve(
    bind: SocketAddr,
    registry: Arc<Registry>,
    peer_queue: usize,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    let sock = Arc::new(UdpSocket::bind(bind).await?);
    let local = sock.local_addr()?;
    tracing::info!(%local, peer_queue, "udp transport listening");
    let depth = peer_queue.max(64);

    // Per-peer writers live in a JoinSet owned by the recv loop: dropping the
    // loop on shutdown aborts them all, so a clean SIGTERM never waits on a
    // registered peer's queue (#4). A writer also exits on its own once the
    // registry drops the peer's sink (prune / cap), after draining.
    let recv = {
        let sock = sock.clone();
        let registry = registry.clone();
        async move {
            let mut writers: JoinSet<()> = JoinSet::new();
            let mut buf = vec![0u8; 65_535];
            loop {
                let (n, src) = match sock.recv_from(&mut buf).await {
                    Ok(x) => x,
                    Err(e) => {
                        // Back off briefly so a persistent socket error can't
                        // spin the recv loop at 100% CPU.
                        tracing::warn!(error = %e, "recv_from failed");
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        continue;
                    }
                };
                let Some(info) = frame::route_info(&buf[..n]) else {
                    tracing::debug!(n, %src, "malformed udp packet ignored");
                    continue;
                };
                // Reap finished writers so the set doesn't grow without bound.
                while writers.try_join_next().is_some() {}
                let make_sink = || peer_sink(&sock, src, depth, &mut writers);
                if let Err(e) = registry.observe_udp(info.band_id, src, make_sink) {
                    tracing::debug!(%src, error = ?e, "udp peer not admitted");
                    continue;
                }
                let packet: Arc<[u8]> = Arc::from(&buf[..n]);
                registry.forward(&info.band_id, &PeerKey::Udp(src), packet);
            }
        }
    };

    // Dropping `recv` (when shutdown wins) drops its JoinSet, aborting every
    // per-peer writer rather than waiting for their queues to close: registered
    // peers hold `Sink::Udp` senders, so waiting would block until prune evicts
    // every UDP peer (up to peer_ttl) — turning a clean SIGTERM into a hang
    // that systemd escalates to SIGKILL.
    tokio::select! {
        _ = recv => {},
        _ = shutdown => tracing::info!("udp transport shutting down"),
    }
    Ok(())
}

/// Create one UDP peer's sink: its own bounded queue of `depth` frames, plus a
/// writer task (owned by `writers`) that drains it onto the shared socket.
fn peer_sink(
    sock: &Arc<UdpSocket>,
    addr: SocketAddr,
    depth: usize,
    writers: &mut JoinSet<()>,
) -> Sink {
    let (tx, rx) = mpsc::channel::<Arc<[u8]>>(depth);
    writers.spawn(peer_writer(sock.clone(), addr, rx));
    Sink::Udp { tx, addr }
}

/// Drain one UDP peer's egress queue onto the shared socket. Ends when every
/// sender is gone (the registry dropped the peer) or on abort at shutdown.
async fn peer_writer(sock: Arc<UdpSocket>, addr: SocketAddr, mut rx: mpsc::Receiver<Arc<[u8]>>) {
    while let Some(packet) = rx.recv().await {
        if let Err(e) = sock.send_to(&packet, addr).await {
            tracing::warn!(%addr, error = %e, "udp send failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn each_udp_peer_gets_its_own_queue() {
        // D6 — the transport builds a separate bounded queue per peer, of the
        // configured depth, so A's backlog can never be B's (the registry test
        // `udp_peer_queues_are_independent` shows the drop isolation itself).
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut writers = JoinSet::new();
        let a = peer_sink(&sock, "127.0.0.1:9".parse().unwrap(), 4, &mut writers);
        let b = peer_sink(&sock, "127.0.0.1:10".parse().unwrap(), 4, &mut writers);
        let (Sink::Udp { tx: ta, .. }, Sink::Udp { tx: tb, .. }) = (a, b) else {
            panic!("udp transport must build Sink::Udp");
        };
        assert!(!ta.same_channel(&tb), "peers must not share a queue");
        assert_eq!((ta.max_capacity(), tb.max_capacity()), (4, 4));
        assert_eq!(writers.len(), 2, "one writer per peer");
        // Dropping the last sender ends that peer's writer (registry eviction).
        drop(ta);
        let done = tokio::time::timeout(std::time::Duration::from_secs(1), writers.join_next())
            .await
            .expect("writer must exit once its peer's sink is gone");
        assert!(done.is_some_and(|r| r.is_ok()));
    }
}
