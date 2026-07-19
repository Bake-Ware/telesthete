//! Cross-language interop peer, driven by `tests/test_interop.py`.
//!
//! Binds a Band on an ephemeral loopback port, prints `READY <addr>` to stderr,
//! then acts as a responder: prints `HELLO` on the peer's HELLO (base-key
//! handshake), opens a Stream back to that peer, and prints `STREAM <data>` when
//! it receives a session-keyed stream frame (proving the session data-key path
//! AND the §5.1 stream wire format interoperate), then exits 0. Exits 1 on a
//! 5 s idle timeout.

use std::net::SocketAddr;
use std::time::Duration;

use telesthete::{Band, ControlEvent};

#[tokio::main]
async fn main() {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let mut band = Band::bind(b"interop-psk", addr, "rust-peer")
        .await
        .expect("bind");
    eprintln!("READY {}", band.local_addr().unwrap());

    // 1) Handshake: wait for the peer's HELLO (base key) and learn its address.
    let peer = loop {
        match tokio::time::timeout(Duration::from_secs(5), band.control().recv()).await {
            Ok(Some(ControlEvent::Hello { from, .. }))
            | Ok(Some(ControlEvent::HelloAck { from, .. })) => {
                println!("HELLO");
                break from;
            }
            Ok(Some(_)) => continue,
            _ => {
                eprintln!("timeout waiting for HELLO");
                std::process::exit(1);
            }
        }
    };

    // 2) Data: receive a session-keyed stream frame from that peer.
    let mut s = band.stream(peer, 9).await;
    match tokio::time::timeout(Duration::from_secs(5), s.recv()).await {
        Ok(Some(msg)) => {
            println!("STREAM {}", String::from_utf8_lossy(&msg.data));
            std::process::exit(0);
        }
        _ => {
            eprintln!("no stream data");
            std::process::exit(1);
        }
    }
}
