//! Shared helpers for the transport integration tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;

use telesthitium::{Limits, Registry};
use tokio::net::{TcpListener, UdpSocket};

/// A minimal well-formed frame (`MIN_PACKET_LEN` = 43) for band `band` with the
/// given cleartext `channel_type`, tagged with `marker` in the opaque region so
/// tests can assert exact, whole-frame relay.
pub fn frame(band: u8, channel_type: u8, marker: u8) -> Vec<u8> {
    let mut v = vec![0u8; 43];
    v[..16].copy_from_slice(&[band; 16]);
    v[16] = channel_type;
    v[42] = marker;
    v
}

/// Lowercase hex of the all-`band` band id.
pub fn band_hex(band: u8) -> String {
    [band; 16].iter().map(|b| format!("{b:02x}")).collect()
}

/// A registry with UDP return-routability disabled, so tests need only a single
/// announce packet to make a UDP peer an eligible destination. (The validation
/// gate itself is covered by registry unit tests, D4.)
pub fn registry_open() -> Arc<Registry> {
    Arc::new(Registry::new(Limits {
        udp_validation_packets: 1,
        ..Limits::default()
    }))
}

/// Grab a free loopback TCP port (for WS).
pub async fn free_tcp_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

/// Grab a free loopback UDP address (for UDP / WebTransport QUIC).
pub async fn free_udp_addr() -> SocketAddr {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let a = s.local_addr().unwrap();
    drop(s);
    a
}

/// Read from a UDP socket until a frame with `marker` at byte 42 arrives, or
/// give up after a handful of attempts. Returns the exact bytes seen.
pub async fn udp_recv_marked(sock: &UdpSocket, marker: u8) -> Option<Vec<u8>> {
    let mut buf = vec![0u8; 4096];
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_millis(400), sock.recv(&mut buf)).await
        {
            Ok(Ok(n)) if n >= 43 && buf[42] == marker => return Some(buf[..n].to_vec()),
            Ok(Ok(_)) => continue,
            _ => return None,
        }
    }
    None
}
