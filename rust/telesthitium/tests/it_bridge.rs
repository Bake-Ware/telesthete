//! Cross-transport bridge integration test (SPEC §10/§11, conformance H1, H2).
//!
//! One band, one peer on each of UDP, WSS, WebTransport, and AF_UNIX. A frame
//! from any peer reaches all the others, each with its own transport framing —
//! the canonical whole-frame relay de-framed on ingress and re-framed on egress.

mod common;
use common::*;

use std::mem::MaybeUninit;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use socket2::{Domain, SockAddr, Socket, Type};
use telesthitium::{HubCert, Registry};
use tokio::net::UdpSocket;
use tokio_tungstenite::tungstenite::Message;
use wtransport::{ClientConfig, Connection, Endpoint};

async fn wt_connect(port: u16, band: &str, cert: &HubCert) -> Connection {
    let digest = wtransport::tls::Sha256Digest::from(cert.sha256);
    let config = ClientConfig::builder()
        .with_bind_default()
        .with_server_certificate_hashes([digest])
        .build();
    Endpoint::client(config)
        .unwrap()
        .connect(format!("https://127.0.0.1:{port}/telesthete?band={band}"))
        .await
        .unwrap()
}

fn seq_recv_marked(sock: &Socket, marker: u8) -> bool {
    for _ in 0..8 {
        let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
        match sock.recv(&mut buf) {
            Ok(n) if n >= 43 && unsafe { buf[42].assume_init() } == marker => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_four_transports_one_band() {
    let reg: Arc<Registry> = registry_open();
    let band = 8u8;
    let hex = band_hex(band);

    // -- start all four transports on one registry --
    let udp_addr = free_udp_addr().await;
    let wt_addr = free_udp_addr().await;
    let ws_port = free_tcp_port().await;
    let ws_addr = format!("127.0.0.1:{ws_port}").parse().unwrap();
    let cert = telesthitium::tls::self_signed(&["localhost", "127.0.0.1"], 14).unwrap();
    let mut dir = std::env::temp_dir();
    dir.push(format!("telesthitium-bridge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    spawn_serve_udp(reg.clone(), udp_addr);
    spawn_serve_ws(reg.clone(), ws_addr);
    spawn_serve_wt(reg.clone(), wt_addr, cert.clone());
    spawn_serve_unix(reg.clone(), dir.clone());
    tokio::time::sleep(Duration::from_millis(350)).await;

    // -- UDP peer seeds band 8 --
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(udp_addr).await.unwrap();
    udp.send(&frame(band, 1, 0x01)).await.unwrap();

    // -- WSS peer --
    let ws = ws_connect_plain(ws_port).await;
    let (mut ws_tx, mut ws_rx) = ws.split();
    ws_tx.send(Message::Binary(Bytes::from(frame(band, 1, 0x02)))).await.unwrap();

    // -- WebTransport peer --
    let wt = wt_connect(wt_addr.port(), &hex, &cert).await;

    // -- AF_UNIX peer (wait for the band socket to be bound) --
    let sock_path = dir.join(format!("{hex}.sock"));
    for _ in 0..25 {
        if sock_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(sock_path.exists(), "unix band socket must be bound");
    let unix_sock = {
        let p = sock_path.clone();
        tokio::task::spawn_blocking(move || {
            let s = Socket::new(Domain::UNIX, Type::SEQPACKET, None).unwrap();
            s.connect(&SockAddr::unix(&p).unwrap()).unwrap();
            s.set_read_timeout(Some(Duration::from_millis(700))).unwrap();
            s.send(&frame(8, 1, 0x03)).unwrap(); // register
            s
        })
        .await
        .unwrap()
    };

    tokio::time::sleep(Duration::from_millis(200)).await; // let all four register

    // -- H1: a Stream (0x01) frame from UDP fans out to WSS, WT, and AF_UNIX --
    udp.send(&frame(band, 1, 0xF0)).await.unwrap();

    let ws_got = ws_recv_binary_marked(&mut ws_rx, 0xF0).await;
    let wt_got = wt_recv_datagram_marked(&wt, 0xF0).await;
    let unix_got = {
        let s = unix_sock.try_clone().unwrap();
        tokio::task::spawn_blocking(move || seq_recv_marked(&s, 0xF0))
            .await
            .unwrap()
    };

    assert!(ws_got, "H1: UDP frame did not reach the WSS peer");
    assert!(wt_got, "H1: UDP frame did not reach the WebTransport peer");
    assert!(unix_got, "H1: UDP frame did not reach the AF_UNIX peer");

    // -- H2: a Channel (0x02) frame from the WSS peer reaches the WT peer
    //        re-framed onto a length-prefixed reliable stream. --
    ws_tx.send(Message::Binary(Bytes::from(frame(band, 2, 0xF2)))).await.unwrap();
    let got = wt_recv_lenprefixed_marked(&wt, 0xF2).await;
    assert!(got, "H2: WSS Channel frame did not reach the WT peer as a len-prefixed stream");

    let _ = std::fs::remove_dir_all(&dir);
}

// -- transport starters --

fn spawn_serve_udp(reg: Arc<Registry>, addr: std::net::SocketAddr) {
    tokio::spawn(async move {
        let _ = telesthitium::udp::serve(addr, reg, 1024, std::future::pending::<()>()).await;
    });
}
fn spawn_serve_ws(reg: Arc<Registry>, addr: std::net::SocketAddr) {
    tokio::spawn(async move {
        let _ = telesthitium::ws::serve(addr, reg, 1024, "/band".into(), None, std::future::pending::<()>())
            .await;
    });
}
fn spawn_serve_wt(reg: Arc<Registry>, addr: std::net::SocketAddr, cert: HubCert) {
    tokio::spawn(async move {
        let _ = telesthitium::wt::serve(addr, reg, 1024, cert, std::future::pending::<()>()).await;
    });
}
fn spawn_serve_unix(reg: Arc<Registry>, dir: PathBuf) {
    tokio::spawn(async move {
        let _ = telesthitium::unix::serve(dir, reg, 1024, std::future::pending::<()>()).await;
    });
}

// -- clients / receivers --

async fn ws_connect_plain(
    port: u16,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/band"))
        .await
        .unwrap();
    ws
}

async fn ws_recv_binary_marked<S>(rx: &mut S, marker: u8) -> bool
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(500), rx.next()).await {
            Ok(Some(Ok(Message::Binary(d)))) if d.len() >= 43 && d[42] == marker => return true,
            Ok(Some(Ok(_))) => continue,
            _ => return false,
        }
    }
    false
}

async fn wt_recv_datagram_marked(conn: &Connection, marker: u8) -> bool {
    for _ in 0..8 {
        match tokio::time::timeout(Duration::from_millis(700), conn.receive_datagram()).await {
            Ok(Ok(dg)) if dg.len() >= 43 && dg[42] == marker => return true,
            Ok(Ok(_)) => continue,
            _ => return false,
        }
    }
    false
}

async fn wt_recv_lenprefixed_marked(conn: &Connection, marker: u8) -> bool {
    let Ok(Ok((_s, mut recv))) =
        tokio::time::timeout(Duration::from_millis(900), conn.accept_bi()).await
    else {
        return false;
    };
    let mut len_buf = [0u8; 2];
    if recv.read_exact(&mut len_buf).await.is_err() {
        return false;
    }
    let len = u16::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    if recv.read_exact(&mut buf).await.is_err() {
        return false;
    }
    buf.len() >= 43 && buf[42] == marker
}
