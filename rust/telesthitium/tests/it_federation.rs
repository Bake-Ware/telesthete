//! Hub federation integration tests (SPEC §10 extension).
//!
//! Two hubs linked over TCP must pool a band: a frame sent by a UDP peer on hub
//! A reaches a UDP peer on hub B and vice-versa — but only when the link is
//! active (default-revoked otherwise), and a link-injected frame is delivered to
//! local peers only (one hop).

mod common;
use common::*;

use std::sync::Arc;
use std::time::Duration;

use telesthitium::federation::{self, FedConfig};
use telesthitium::Registry;
use tokio::net::UdpSocket;
use tokio::sync::watch;

async fn start_udp(reg: Arc<Registry>) -> u16 {
    let addr = free_udp_addr().await;
    let port = addr.port();
    tokio::spawn(async move {
        let _ = telesthitium::udp::serve(addr, reg, 1024, std::future::pending::<()>()).await;
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    port
}

/// Bring up two hubs A (dials) and B (listens) with a shared secret, each with a
/// UDP transport. Returns their registries + UDP ports + the link listen port.
async fn two_linked_hubs(
    inbound_active: bool,
) -> (Arc<Registry>, u16, Arc<Registry>, u16, watch::Sender<bool>) {
    let (sd_tx, sd_rx) = watch::channel(false);
    let reg_a = registry_open();
    let reg_b = registry_open();
    let a_udp = start_udp(reg_a.clone()).await;
    let b_udp = start_udp(reg_b.clone()).await;
    let link_port = free_tcp_port().await;

    // Build configs directly (no process-global env, so tests can run parallel).
    // B listens for hub links (the consuming hub); default-revoked unless active.
    let cfg_b = FedConfig {
        listen: Some(format!("127.0.0.1:{link_port}")),
        links: vec![],
        secret: "shared-fed-secret".into(),
        inbound_active,
    };
    federation::spawn(cfg_b, reg_b.clone(), sd_rx.clone());

    // A dials B (the sharing hub); its side is active.
    let cfg_a = FedConfig {
        listen: None,
        links: vec![format!("127.0.0.1:{link_port}")],
        secret: "shared-fed-secret".into(),
        inbound_active: true,
    };
    federation::spawn(cfg_a, reg_a.clone(), sd_rx.clone());

    // Let the link establish + first BANDS exchange happen.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (reg_a, a_udp, reg_b, b_udp, sd_tx)
}

#[tokio::test]
async fn frame_crosses_active_link_both_ways() {
    let (_ra, a_port, _rb, b_port, _sd) = two_linked_hubs(true).await;

    // A peer on hub A and a peer on hub B, same band (7).
    let pa = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    pa.connect(("127.0.0.1", a_port)).await.unwrap();
    let pb = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    pb.connect(("127.0.0.1", b_port)).await.unwrap();

    // Both announce (register + advertise their band to the peers' hub). Send a
    // couple so the band shows up in each hub's local_bands and propagates.
    for _ in 0..2 {
        pa.send(&frame(7, 0, 0xA1)).await.unwrap();
        pb.send(&frame(7, 0, 0xB1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    // Give the (on-change, ~1s poll) BANDS advertisement time to cross the link.
    tokio::time::sleep(Duration::from_millis(1600)).await;

    // A sends a marked frame; B's peer should receive it via the link.
    pa.send(&frame(7, 0, 0xCC)).await.unwrap();
    let got = udp_recv_marked(&pb, 0xCC).await;
    assert!(got.is_some(), "peer on hub B should receive hub A's frame across the link");

    // And the reverse direction.
    pb.send(&frame(7, 0, 0xDD)).await.unwrap();
    let got_rev = udp_recv_marked(&pa, 0xDD).await;
    assert!(got_rev.is_some(), "peer on hub A should receive hub B's frame across the link");
}

#[tokio::test]
async fn revoked_link_relays_nothing() {
    let (_ra, a_port, _rb, b_port, _sd) = two_linked_hubs(false).await;
    let pa = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    pa.connect(("127.0.0.1", a_port)).await.unwrap();
    let pb = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    pb.connect(("127.0.0.1", b_port)).await.unwrap();

    for _ in 0..2 {
        pa.send(&frame(9, 0, 0xA2)).await.unwrap();
        pb.send(&frame(9, 0, 0xB2)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    tokio::time::sleep(Duration::from_millis(1600)).await;

    // B's inbound link is revoked, so B must NOT inject A's frames to its peers.
    pa.send(&frame(9, 0, 0xEE)).await.unwrap();
    let got = udp_recv_marked(&pb, 0xEE).await;
    assert!(got.is_none(), "a revoked inbound link must relay nothing to hub B");
}

#[tokio::test]
async fn unrelated_band_is_not_relayed() {
    let (_ra, a_port, _rb, b_port, _sd) = two_linked_hubs(true).await;
    let pa = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    pa.connect(("127.0.0.1", a_port)).await.unwrap();
    // A peer on B in a DIFFERENT band (5) than A's peer (3).
    let pb = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    pb.connect(("127.0.0.1", b_port)).await.unwrap();
    for _ in 0..2 {
        pa.send(&frame(3, 0, 0xA3)).await.unwrap();
        pb.send(&frame(5, 0, 0xB3)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    tokio::time::sleep(Duration::from_millis(1600)).await;

    // A's band-3 frame must not reach B's band-5 peer.
    pa.send(&frame(3, 0, 0xFA)).await.unwrap();
    assert!(udp_recv_marked(&pb, 0xFA).await.is_none(),
        "a frame must not cross into a band the remote peer isn't in");
}
