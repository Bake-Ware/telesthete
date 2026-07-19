//! UDP transport integration tests (SPEC §9.1, conformance E1–E3).
//!
//! Spins the real hub UDP listener on a loopback ephemeral port and drives
//! genuine UDP client sockets through it.

use std::sync::Arc;
use std::time::Duration;

use telesthitium::{Config, Registry};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;

/// A minimal well-formed frame (>= MIN_PACKET_LEN) for `band`, tagged with a
/// distinguishing byte in the opaque region so tests can assert exact relay.
fn frame(band: u8, marker: u8) -> Vec<u8> {
    let mut v = vec![0u8; 43];
    v[..16].copy_from_slice(&[band; 16]);
    v[16] = 0x01; // Stream
    v[42] = marker;
    v
}

async fn start_hub() -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let mut cfg = Config::default();
    // Route immediately (no validation delay) so a two-packet test is simple;
    // validation itself is covered by registry unit tests (D4).
    cfg.limits.udp_validation_packets = 1;
    let registry = Arc::new(Registry::new(cfg.limits));
    let bind: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    // Bind here to learn the port, then hand the socket's address to serve by
    // binding inside serve — instead we bind a throwaway to grab a free port.
    let probe = UdpSocket::bind(bind).await.unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe); // release; serve() rebinds it (tiny race window, fine on loopback)

    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = telesthitium::udp::serve(addr, registry, 1024, async {
            let _ = rx.await;
        })
        .await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await; // let it bind
    (addr, tx)
}

#[tokio::test]
async fn two_udp_peers_relay() {
    // E1 + E2 — a frame from peer A reaches peer B, byte-exact, boundaries kept.
    let (hub, _stop) = start_hub().await;

    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    a.connect(hub).await.unwrap();
    b.connect(hub).await.unwrap();

    // Both must announce themselves first (implicit discovery), so the hub
    // knows B's address before A's frame arrives.
    b.send(&frame(7, 0xB0)).await.unwrap();
    a.send(&frame(7, 0xA0)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    // A sends a payload frame; B should receive it verbatim.
    let sent = frame(7, 0xCC);
    a.send(&sent).await.unwrap();

    let mut buf = vec![0u8; 2048];
    // B may first receive A's earlier announce frame; read until we see 0xCC.
    let mut saw = None;
    for _ in 0..4 {
        let n = tokio::time::timeout(Duration::from_millis(300), b.recv(&mut buf))
            .await
            .expect("timed out waiting for relayed frame")
            .unwrap();
        if buf[..n] == sent[..] {
            saw = Some(buf[..n].to_vec());
            break;
        }
    }
    assert_eq!(saw.as_deref(), Some(&sent[..]), "B must receive A's frame byte-exact");
}

#[tokio::test]
async fn short_datagram_ignored() {
    // E3 — a sub-minimum datagram must not crash the hub or reach a peer.
    let (hub, _stop) = start_hub().await;

    let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    a.connect(hub).await.unwrap();
    b.connect(hub).await.unwrap();

    b.send(&frame(9, 0xB0)).await.unwrap(); // register B
    a.send(&frame(9, 0xA0)).await.unwrap(); // register A
    tokio::time::sleep(Duration::from_millis(30)).await;

    // A short (malformed) datagram from A: must be dropped, not relayed.
    a.send(&[1, 2, 3]).await.unwrap();
    // A valid frame afterwards proves the hub is still relaying normally.
    let good = frame(9, 0x5A);
    a.send(&good).await.unwrap();

    let mut buf = vec![0u8; 2048];
    let mut saw_good = false;
    for _ in 0..4 {
        let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(300), b.recv(&mut buf)).await
        else {
            break;
        };
        assert_ne!(&buf[..n], &[1, 2, 3], "short datagram must never be relayed");
        if buf[..n] == good[..] {
            saw_good = true;
            break;
        }
    }
    assert!(saw_good, "hub must keep relaying valid frames after a malformed one");
}
