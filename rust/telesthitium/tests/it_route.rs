//! Addressed routing over real UDP sockets (SPEC §10.1, conformance K10).
//!
//! Three UDP peers on one band. B declares a route id with a ROUTE frame; an
//! ADDRESSED (0x06) frame for that id reaches B only, a `channel_id` 0 frame
//! still reaches everyone, a CHANNEL (0x02) frame is broadcast whatever its
//! `channel_id`, and the ROUTE frame itself is never relayed.

mod common;
use common::*;

use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::oneshot;

fn typed(band: u8, ct: u8, cid: u16, marker: u8) -> Vec<u8> {
    let mut v = frame(band, ct, marker);
    v[17..19].copy_from_slice(&cid.to_be_bytes());
    v
}

/// Everything a socket receives within `window`, as byte-42 markers.
async fn markers(sock: &UdpSocket, window: Duration) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 2048];
    let deadline = tokio::time::Instant::now() + window;
    while let Ok(Ok(n)) = tokio::time::timeout_at(deadline, sock.recv(&mut buf)).await {
        if n >= 43 {
            out.push(buf[42]);
        }
    }
    out
}

async fn peer(hub: std::net::SocketAddr) -> UdpSocket {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    s.connect(hub).await.unwrap();
    s
}

#[tokio::test]
async fn addressed_frames_reach_only_the_declarer() {
    let reg = registry_open();
    let hub = free_udp_addr().await;
    let (stop, rx) = oneshot::channel::<()>();
    tokio::spawn(telesthitium::udp::serve(hub, reg, 64, async {
        let _ = rx.await;
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (a, b, c) = (peer(hub).await, peer(hub).await, peer(hub).await);
    // Register all three (marker 0x01), then B declares route 0x0B0B.
    for s in [&a, &b, &c] {
        s.send(&typed(4, 0x02, 0, 0x01)).await.unwrap();
    }
    b.send(&typed(4, 0x05, 0x0B0B, 0xEE)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    for s in [&a, &b, &c] {
        markers(s, Duration::from_millis(50)).await; // discard registration noise
    }

    a.send(&typed(4, 0x06, 0x0B0B, 0xB1)).await.unwrap(); // addressed to B
    a.send(&typed(4, 0x06, 0x0000, 0xB2)).await.unwrap(); // broadcast
    a.send(&typed(4, 0x06, 0x0C0C, 0xB3)).await.unwrap(); // undeclared -> broadcast
    a.send(&typed(4, 0x02, 0x0B0B, 0xB4)).await.unwrap(); // library Channel -> broadcast

    let w = Duration::from_millis(300);
    let (ga, gb, gc) = tokio::join!(markers(&a, w), markers(&b, w), markers(&c, w));
    assert_eq!(ga, Vec::<u8>::new(), "sender never gets its own frames");
    assert_eq!(gb, vec![0xB1, 0xB2, 0xB3, 0xB4]);
    assert_eq!(gc, vec![0xB2, 0xB3, 0xB4], "C must not see B's addressed frame");
    assert!(!gb.contains(&0xEE) && !gc.contains(&0xEE), "ROUTE never relayed");
    let _ = stop.send(());
}

#[tokio::test]
async fn burst_to_one_udp_peer_does_not_starve_another() {
    // D6 smoke test over real sockets: a burst addressed to B (far more than
    // its 64-deep queue) never reaches C, and C still gets the broadcast that
    // follows. Loopback drains too fast to fill a queue reliably, so the
    // isolation guarantee itself is proven deterministically by the registry
    // unit test `udp_peer_queues_are_independent`.
    let reg = registry_open();
    let hub = free_udp_addr().await;
    let (stop, rx) = oneshot::channel::<()>();
    tokio::spawn(telesthitium::udp::serve(hub, reg, 64, async {
        let _ = rx.await;
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (a, b, c) = (peer(hub).await, peer(hub).await, peer(hub).await);
    for s in [&a, &b, &c] {
        s.send(&typed(5, 0x02, 0, 0x01)).await.unwrap();
    }
    b.send(&typed(5, 0x05, 0x0B0B, 0xEE)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    for s in [&a, &b, &c] {
        markers(s, Duration::from_millis(50)).await;
    }

    for _ in 0..2000 {
        a.send(&typed(5, 0x06, 0x0B0B, 0xB1)).await.unwrap();
    }
    a.send(&typed(5, 0x06, 0, 0xC2)).await.unwrap();
    let gc = markers(&c, Duration::from_millis(500)).await;
    assert_eq!(gc, vec![0xC2], "C gets exactly the broadcast, none of B's burst");
    let _ = stop.send(());
}

#[tokio::test]
async fn one_packet_spoofed_declaration_cannot_divert_traffic() {
    // K11 over real sockets, with the default validation gate (2 packets). A
    // source that sends a single ROUTE frame claiming B's id — e.g. a forged
    // source address — is neither a declarer nor a destination: B keeps
    // receiving its addressed frames, and the spoofer receives nothing.
    let reg = std::sync::Arc::new(telesthitium::Registry::new(telesthitium::Limits::default()));
    let hub = free_udp_addr().await;
    let (stop, rx) = oneshot::channel::<()>();
    tokio::spawn(telesthitium::udp::serve(hub, reg, 64, async {
        let _ = rx.await;
    }));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (a, b, spoof) = (peer(hub).await, peer(hub).await, peer(hub).await);
    for s in [&a, &b] {
        s.send(&typed(6, 0x02, 0, 0x01)).await.unwrap();
        s.send(&typed(6, 0x02, 0, 0x01)).await.unwrap(); // validated
    }
    b.send(&typed(6, 0x05, 0x0B0B, 0xEE)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    spoof.send(&typed(6, 0x05, 0x0B0B, 0xEE)).await.unwrap(); // one packet only
    tokio::time::sleep(Duration::from_millis(50)).await;
    for s in [&a, &b, &spoof] {
        markers(s, Duration::from_millis(50)).await;
    }

    a.send(&typed(6, 0x06, 0x0B0B, 0xB1)).await.unwrap();
    let w = Duration::from_millis(300);
    let (gb, gs) = tokio::join!(markers(&b, w), markers(&spoof, w));
    assert_eq!(gb, vec![0xB1], "victim still gets its traffic");
    assert_eq!(gs, Vec::<u8>::new(), "unvalidated spoofer gets nothing");
    let _ = stop.send(());
}
