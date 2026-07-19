//! WebSocket / WSS transport integration tests (SPEC §9.3, conformance F1–F4).

mod common;
use common::*;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use telesthitium::{HubCert, Registry};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;

async fn start_ws(reg: Arc<Registry>, tls: Option<HubCert>) -> (u16, oneshot::Sender<()>) {
    let port = free_tcp_port().await;
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let _ = telesthitium::ws::serve(addr, reg, 1024, "/band".into(), tls, async {
            let _ = rx.await;
        })
        .await;
    });
    tokio::time::sleep(Duration::from_millis(120)).await;
    (port, tx)
}

async fn ws_connect(port: u16) -> tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
> {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/band"))
        .await
        .unwrap();
    ws
}

/// Read binary messages from a split WS stream until one carries `marker`.
async fn ws_recv_marked<S>(rx: &mut S, marker: u8) -> Option<Vec<u8>>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(400), rx.next()).await {
            Ok(Some(Ok(Message::Binary(d)))) if d.len() >= 43 && d[42] == marker => {
                return Some(d.to_vec())
            }
            Ok(Some(Ok(_))) => continue,
            _ => return None,
        }
    }
    None
}

#[tokio::test]
async fn connects_on_band_path() {
    // F1 — the endpoint path selects the band route.
    let (port, _stop) = start_ws(registry_open(), None).await;
    assert!(tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/band"))
        .await
        .is_ok());
    assert!(tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/wrong"))
        .await
        .is_err());
}

#[tokio::test]
async fn binary_frame_is_one_packet() {
    // F2 — one binary WS message carries exactly one frame; relayed byte-exact.
    let (port, _stop) = start_ws(registry_open(), None).await;
    let mut a = ws_connect(port).await;
    let (mut b_tx, mut b_rx) = ws_connect(port).await.split();

    b_tx.send(Message::Binary(Bytes::from(frame(1, 1, 0xB0)))).await.unwrap();
    a.send(Message::Binary(Bytes::from(frame(1, 1, 0xA0)))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;

    let sent = frame(1, 1, 0xCC);
    a.send(Message::Binary(Bytes::from(sent.clone()))).await.unwrap();
    assert_eq!(ws_recv_marked(&mut b_rx, 0xCC).await.as_deref(), Some(&sent[..]));
}

#[tokio::test]
async fn ws_udp_bridge() {
    // F3 — a WS peer and a UDP peer in one band bridge both directions.
    let reg = registry_open();
    let (ws_port, _s1) = start_ws(reg.clone(), None).await;

    let udp_addr = free_udp_addr().await;
    {
        let (reg, addr) = (reg.clone(), udp_addr);
        tokio::spawn(async move {
            let _ = telesthitium::udp::serve(addr, reg, 1024, std::future::pending::<()>()).await;
        });
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut ws = ws_connect(ws_port).await;
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(udp_addr).await.unwrap();

    // Register both in band 2.
    udp.send(&frame(2, 1, 0x0D)).await.unwrap();
    ws.send(Message::Binary(Bytes::from(frame(2, 1, 0x0E)))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    // WS -> UDP
    ws.send(Message::Binary(Bytes::from(frame(2, 1, 0x51)))).await.unwrap();
    assert!(udp_recv_marked(&udp, 0x51).await.is_some(), "WS->UDP failed");

    // UDP -> WS
    udp.send(&frame(2, 1, 0x52)).await.unwrap();
    let (_tx, mut rx) = ws.split();
    assert!(ws_recv_marked(&mut rx, 0x52).await.is_some(), "UDP->WS failed");
}

#[tokio::test]
async fn band_pinning_drops_cross_band() {
    // #2 regression — a peer registered in band A cannot inject into band B.
    let (port, _stop) = start_ws(registry_open(), None).await;
    let mut a = ws_connect(port).await; // will pin to band 10
    let mut c = ws_connect(port).await; // legitimate band-11 sender
    let (mut b_tx, mut b_rx) = ws_connect(port).await.split(); // band-11 receiver

    a.send(Message::Binary(Bytes::from(frame(10, 1, 0x01)))).await.unwrap(); // A -> band 10
    b_tx.send(Message::Binary(Bytes::from(frame(11, 1, 0x02)))).await.unwrap(); // B -> band 11
    c.send(Message::Binary(Bytes::from(frame(11, 1, 0x03)))).await.unwrap(); // C -> band 11
    tokio::time::sleep(Duration::from_millis(60)).await;

    // A (pinned to band 10) tries to inject into band 11 — must be dropped.
    a.send(Message::Binary(Bytes::from(frame(11, 1, 0xAA)))).await.unwrap();
    // C legitimately sends into band 11 — must arrive.
    c.send(Message::Binary(Bytes::from(frame(11, 1, 0xCC)))).await.unwrap();

    // Collect B's inbound markers until the legitimate one; the injected one
    // must never appear.
    let mut seen = Vec::new();
    for _ in 0..12 {
        match tokio::time::timeout(Duration::from_millis(400), b_rx.next()).await {
            Ok(Some(Ok(Message::Binary(d)))) if d.len() >= 43 => {
                seen.push(d[42]);
                if d[42] == 0xCC {
                    break;
                }
            }
            Ok(Some(Ok(_))) => continue,
            _ => break,
        }
    }
    assert!(seen.contains(&0xCC), "legitimate same-band frame must arrive");
    assert!(!seen.contains(&0xAA), "cross-band injected frame must be dropped");
}

#[tokio::test]
async fn wss_native_tls() {
    // F4 — native TLS terminates in-hub and relays.
    let cert = telesthitium::tls::self_signed(&["localhost"], 14).unwrap();
    let (port, _stop) = start_ws(registry_open(), Some(cert.clone())).await;

    let a = wss_connect(port, &cert).await;
    let b = wss_connect(port, &cert).await;
    let mut a = a;
    let (mut b_tx, mut b_rx) = b.split();

    b_tx.send(Message::Binary(Bytes::from(frame(3, 1, 0xB0)))).await.unwrap();
    a.send(Message::Binary(Bytes::from(frame(3, 1, 0xA0)))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(60)).await;

    let sent = frame(3, 1, 0xDD);
    a.send(Message::Binary(Bytes::from(sent.clone()))).await.unwrap();
    assert_eq!(ws_recv_marked(&mut b_rx, 0xDD).await.as_deref(), Some(&sent[..]));
}

/// Connect a WSS client that trusts the hub's self-signed cert (SAN `localhost`).
async fn wss_connect(
    port: u16,
    cert: &HubCert,
) -> tokio_tungstenite::WebSocketStream<
    tokio_rustls::client::TlsStream<tokio::net::TcpStream>,
> {
    use rustls_pki_types::{CertificateDer, ServerName};
    use std::convert::TryFrom;

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from(cert.cert_der.clone())).unwrap();
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));

    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let server_name = ServerName::try_from("localhost").unwrap();
    let tls = connector.connect(server_name, tcp).await.unwrap();
    let (ws, _) = tokio_tungstenite::client_async("wss://localhost/band", tls)
        .await
        .unwrap();
    ws
}
