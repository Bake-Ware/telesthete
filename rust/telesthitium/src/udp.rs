//! UDP transport (SPEC §9.1).
//!
//! One socket serves the whole hub. Each UDP peer gets its **own** bounded
//! egress queue and a writer task that drains it onto the shared socket, so
//! relay dispatch (`Registry::forward`) stays synchronous — no per-packet task
//! spawning, no `send_to().await` under any lock — and a burst addressed to
//! one peer can only overflow that peer's queue, never drop another peer's
//! frames (previously one hub-wide queue was shared by every UDP peer).
//! All UDP queues together are capped in bytes by one [`ByteBudget`]
//! (`HUB_UDP_QUEUE_BYTES`, D7), and a queue + writer exist only for a source
//! that has passed validation (D8), so spoofed or one-off sources cost the hub
//! a registry entry and nothing else.
//! Ingress: each datagram is one frame (boundaries preserved); malformed
//! (<43 B) datagrams are dropped without disturbing the band.
//!
//! An evicted peer's writer keeps draining what was already queued, then
//! exits. If the same address is re-admitted meanwhile, two writers briefly
//! send to it; the old one only flushes frames queued before eviction, so at
//! worst a few frames reach the peer out of order — which UDP permits and
//! every receiver already tolerates (replay windows, §6.6 reassembly).

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::config::DEFAULT_UDP_QUEUE_BYTES;
use crate::frame;
use crate::registry::{ByteBudget, PeerKey, Registry, Sink};

/// Serve UDP until `shutdown` resolves, with the default hub-wide queue byte
/// budget (`DEFAULT_UDP_QUEUE_BYTES`). See [`serve_with_budget`].
pub async fn serve(
    bind: SocketAddr,
    registry: Arc<Registry>,
    peer_queue: usize,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    let budget = ByteBudget::new(DEFAULT_UDP_QUEUE_BYTES);
    serve_with_budget(bind, registry, peer_queue, budget, shutdown).await
}

/// Serve UDP until `shutdown` resolves. Binds `bind`, relays datagrams between
/// UDP peers (and, via the shared registry, to peers on other transports).
///
/// `peer_queue` is the depth of **each** UDP peer's egress queue (floored at
/// 64, like the connection transports); see `Config::udp_peer_queue`.
/// `budget` caps the bytes queued across all UDP peers (D7); see
/// `Config::udp_queue_bytes`.
pub async fn serve_with_budget(
    bind: SocketAddr,
    registry: Arc<Registry>,
    peer_queue: usize,
    budget: Arc<ByteBudget>,
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
                let make_sink = || peer_sink(&sock, src, depth, &budget, &mut writers);
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

/// Create one UDP peer's sink: its own bounded queue of `depth` frames, drawing
/// on the hub-wide `budget`, plus a writer task (owned by `writers`) that
/// drains it onto the shared socket.
fn peer_sink(
    sock: &Arc<UdpSocket>,
    addr: SocketAddr,
    depth: usize,
    budget: &Arc<ByteBudget>,
    writers: &mut JoinSet<()>,
) -> Sink {
    let (tx, rx) = mpsc::channel::<Arc<[u8]>>(depth);
    writers.spawn(peer_writer(sock.clone(), addr, rx, budget.clone()));
    Sink::Udp {
        tx,
        addr,
        budget: budget.clone(),
    }
}

/// Drain one UDP peer's egress queue onto the shared socket, refunding each
/// frame's bytes to the budget as it leaves the queue. Ends when every sender
/// is gone (the registry dropped the peer) or on abort at shutdown.
async fn peer_writer(
    sock: Arc<UdpSocket>,
    addr: SocketAddr,
    mut rx: mpsc::Receiver<Arc<[u8]>>,
    budget: Arc<ByteBudget>,
) {
    while let Some(packet) = rx.recv().await {
        budget.release(packet.len());
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
        let budget = ByteBudget::unlimited();
        let a = peer_sink(
            &sock,
            "127.0.0.1:9".parse().unwrap(),
            4,
            &budget,
            &mut writers,
        );
        let b = peer_sink(
            &sock,
            "127.0.0.1:10".parse().unwrap(),
            4,
            &budget,
            &mut writers,
        );
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

    #[tokio::test]
    async fn writer_refunds_budget_as_frames_leave() {
        // D7 — bytes are charged on enqueue and refunded once the writer has
        // taken the frame, so a draining queue frees budget for other peers.
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let sink_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dst = sink_sock.local_addr().unwrap();
        let mut writers = JoinSet::new();
        let budget = ByteBudget::new(1000);
        let Sink::Udp { tx, budget: b, .. } = peer_sink(&sock, dst, 8, &budget, &mut writers)
        else {
            panic!("udp sink");
        };
        assert!(b.try_reserve(100));
        tx.try_send(Arc::from(vec![0u8; 100].as_slice())).unwrap();
        let mut buf = [0u8; 256];
        let n = tokio::time::timeout(std::time::Duration::from_secs(1), sink_sock.recv(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(n, 100);
        assert_eq!(budget.used(), 0, "writer must refund the frame's bytes");
    }
}
