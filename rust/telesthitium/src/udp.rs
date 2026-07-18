//! UDP transport (SPEC §9.1).
//!
//! One socket serves the whole hub. A dedicated writer task owns the send half
//! so relay dispatch (`Registry::forward`) stays synchronous — no per-packet
//! task spawning, no `send_to().await` under any lock. Ingress: each datagram
//! is one frame (boundaries preserved); malformed (<43 B) datagrams are dropped
//! without disturbing the band.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::frame;
use crate::registry::{PeerKey, Registry, Sink};

/// Serve UDP until `shutdown` resolves. Binds `bind`, relays datagrams between
/// UDP peers (and, via the shared registry, to peers on other transports).
pub async fn serve(
    bind: SocketAddr,
    registry: Arc<Registry>,
    conn_queue: usize,
    shutdown: impl Future<Output = ()>,
) -> std::io::Result<()> {
    let sock = Arc::new(UdpSocket::bind(bind).await?);
    let local = sock.local_addr()?;
    tracing::info!(%local, "udp transport listening");

    // Writer task: drains the send queue and writes to the socket. Bounding the
    // queue caps memory if a destination is slow to flush.
    let (tx, mut rx) = mpsc::channel::<(SocketAddr, Arc<[u8]>)>(conn_queue.max(64));
    let wsock = sock.clone();
    let writer = tokio::spawn(async move {
        while let Some((addr, packet)) = rx.recv().await {
            if let Err(e) = wsock.send_to(&packet, addr).await {
                tracing::warn!(%addr, error = %e, "udp send failed");
            }
        }
    });

    let recv = {
        let sock = sock.clone();
        let tx = tx.clone();
        let registry = registry.clone();
        async move {
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
                let sink = Sink::Udp {
                    tx: tx.clone(),
                    addr: src,
                };
                if let Err(e) = registry.observe_udp(info.band_id, src, sink) {
                    tracing::debug!(%src, error = ?e, "udp peer not admitted");
                    continue;
                }
                let packet: Arc<[u8]> = Arc::from(&buf[..n]);
                registry.forward(&info.band_id, &PeerKey::Udp(src), packet);
            }
        }
    };

    tokio::select! {
        _ = recv => {},
        _ = shutdown => tracing::info!("udp transport shutting down"),
    }

    // Abort the writer rather than waiting for the channel to close: registered
    // peers hold `Sink::Udp` clones of `tx`, so `writer.await` would otherwise
    // block until prune evicts every UDP peer (up to peer_ttl) — turning a clean
    // SIGTERM into a hang that systemd escalates to SIGKILL.
    writer.abort();
    let _ = writer.await;
    Ok(())
}
