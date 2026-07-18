//! Shutdown integration tests (conformance J2): each transport's `serve` future
//! resolves promptly once its shutdown signal fires.

mod common;
use common::*;

use std::time::Duration;

use tokio::sync::oneshot;

#[tokio::test]
async fn udp_serve_stops_on_shutdown() {
    let reg = registry_open();
    let addr = free_udp_addr().await;
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        telesthitium::udp::serve(addr, reg, 1024, async {
            let _ = rx.await;
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    let _ = tx.send(());
    let out = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(out.is_ok(), "udp serve did not stop within 2s of shutdown");
    assert!(out.unwrap().unwrap().is_ok());
}

#[tokio::test]
async fn ws_serve_stops_on_shutdown() {
    let reg = registry_open();
    let port = free_tcp_port().await;
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        telesthitium::ws::serve(addr, reg, 1024, "/band".into(), None, async {
            let _ = rx.await;
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = tx.send(());
    let out = tokio::time::timeout(Duration::from_secs(2), handle).await;
    assert!(out.is_ok(), "ws serve did not stop within 2s of shutdown");
    assert!(out.unwrap().unwrap().is_ok());
}
