//! WebTransport transport integration tests (SPEC §9.6, conformance G1–G6).
//!
//! Drives a real `wtransport` client through the hub over QUIC, validating the
//! browser `serverCertificateHashes` path (the client pins the hub's self-signed
//! P-256 cert) and the full channel-type → carrier mapping in both directions,
//! bridged against a real UDP peer.

mod common;
use common::*;

use std::sync::Arc;
use std::time::Duration;

use telesthitium::{HubCert, Registry};
use tokio::net::UdpSocket;
use wtransport::{ClientConfig, Connection, Endpoint};

async fn start_udp_and_wt(reg: Arc<Registry>) -> (std::net::SocketAddr, u16, HubCert) {
    let udp_addr = free_udp_addr().await;
    let wt_addr = free_udp_addr().await;
    let cert = telesthitium::tls::self_signed(&["localhost", "127.0.0.1"], 14).unwrap();
    {
        let (reg, addr) = (reg.clone(), udp_addr);
        tokio::spawn(async move {
            let _ = telesthitium::udp::serve(addr, reg, 1024, std::future::pending::<()>()).await;
        });
    }
    {
        let (reg, cert2) = (reg.clone(), cert.clone());
        tokio::spawn(async move {
            let _ =
                telesthitium::wt::serve(wt_addr, reg, 1024, cert2, std::future::pending::<()>())
                    .await;
        });
    }
    tokio::time::sleep(Duration::from_millis(300)).await; // QUIC endpoint warmup
    (udp_addr, wt_addr.port(), cert)
}

/// A WebTransport client that pins the hub's self-signed cert (§9.6).
async fn wt_connect(
    port: u16,
    band: &str,
    cert: &HubCert,
) -> Result<Connection, wtransport::error::ConnectingError> {
    let digest = wtransport::tls::Sha256Digest::from(cert.sha256);
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([digest])
        .build();
    Endpoint::client(config)
        .unwrap()
        .connect(format!("https://127.0.0.1:{port}/telesthete?band={band}"))
        .await
}

async fn recv_datagram_marked(conn: &Connection, marker: u8) -> Option<Vec<u8>> {
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(600), conn.receive_datagram()).await {
            Ok(Ok(dg)) if dg.len() >= 43 && dg[42] == marker => return Some(dg.to_vec()),
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
    None
}

/// Accept one bidi stream and read a single 2-byte-length-prefixed frame off it.
async fn recv_lenprefixed_frame(conn: &Connection) -> Option<Vec<u8>> {
    let (_send, mut recv) = tokio::time::timeout(Duration::from_millis(800), conn.accept_bi())
        .await
        .ok()?
        .ok()?;
    let mut len_buf = [0u8; 2];
    recv.read_exact(&mut len_buf).await.ok()?;
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    recv.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

#[tokio::test]
async fn session_accept_and_band_from_query() {
    // G1 — a valid /telesthete?band=<hex> session is accepted; a bad route is not.
    let reg = registry_open();
    let (_udp, wt_port, cert) = start_udp_and_wt(reg).await;
    let hex = band_hex(5);

    assert!(wt_connect(wt_port, &hex, &cert).await.is_ok(), "valid session must connect");

    // Wrong route -> hub replies not_found -> client connect fails.
    let digest = wtransport::tls::Sha256Digest::from(cert.sha256);
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([digest])
        .build();
    let bad = Endpoint::client(config)
        .unwrap()
        .connect(format!("https://127.0.0.1:{wt_port}/nope?band={hex}"))
        .await;
    assert!(bad.is_err(), "wrong route must be rejected");
}

#[tokio::test]
async fn carrier_mapping_bridges_udp_bidirectionally() {
    // G2 (Stream->datagram egress), G3 (Channel->per-stream len-prefixed egress),
    // G4 (Control->dedicated stream egress), G5 (ingress datagram + len-prefixed
    // stream de-framing), G6 (bridge to a UDP peer).
    let reg = registry_open();
    let (udp_addr, wt_port, cert) = start_udp_and_wt(reg).await;
    let hex = band_hex(6);

    let conn = wt_connect(wt_port, &hex, &cert).await.unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(udp_addr).await.unwrap();

    // Register the UDP peer in band 6.
    udp.send(&frame(6, 1, 0x0D)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    // G2 + G6: a Stream (0x01) frame from UDP egresses to the WT peer as a datagram.
    udp.send(&frame(6, 1, 0x71)).await.unwrap();
    assert!(recv_datagram_marked(&conn, 0x71).await.is_some(), "G2: UDP Stream -> WT datagram");

    // G5 + G6: a datagram from the WT peer is de-framed and bridged to UDP.
    conn.send_datagram(frame(6, 1, 0x72)).unwrap();
    assert!(udp_recv_marked(&udp, 0x72).await.is_some(), "G5: WT datagram -> UDP");

    // G3: a Channel (0x02) frame from UDP egresses on a bidi stream, len-prefixed.
    udp.send(&frame(6, 2, 0x73)).await.unwrap();
    let got = recv_lenprefixed_frame(&conn).await.expect("G3: Channel -> WT stream");
    assert_eq!(got.len(), 43);
    assert_eq!(got[42], 0x73);

    // G4: a Control (0x00) frame egresses on the dedicated reliable stream.
    udp.send(&frame(6, 0, 0x74)).await.unwrap();
    let got = recv_lenprefixed_frame(&conn).await.expect("G4: Control -> WT stream");
    assert_eq!(got[42], 0x74);

    // G5 (stream ingress): a len-prefixed frame the WT peer writes on a bidi
    // stream is de-framed and bridged whole to UDP.
    let (mut send, _recv) = conn.open_bi().await.unwrap().await.unwrap();
    let f = frame(6, 2, 0x75);
    send.write_all(&(f.len() as u16).to_be_bytes()).await.unwrap();
    send.write_all(&f).await.unwrap();
    assert!(udp_recv_marked(&udp, 0x75).await.is_some(), "G5: WT stream -> UDP");
}
