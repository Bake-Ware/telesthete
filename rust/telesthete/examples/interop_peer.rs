//! Cross-language interop peer, driven by `tests/test_interop.py`.
//!
//! Binds a Band on an ephemeral loopback port, prints `READY <addr>` to stderr,
//! then acts as a responder: it prints `HELLO` when it receives a peer's HELLO
//! (base-key handshake) and `KEEPALIVE` when it receives a session-keyed
//! keepalive afterward (proving the session data-key path interoperates), then
//! exits 0. Exits 1 on a 5 s idle timeout.

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

    let mut saw_hello = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), band.control().recv()).await {
            Ok(Some(ev)) => match ev {
                ControlEvent::Hello { .. } | ControlEvent::HelloAck { .. } => {
                    println!("HELLO");
                    saw_hello = true;
                }
                ControlEvent::Keepalive { .. } if saw_hello => {
                    // A session-keyed keepalive decrypted under the Python peer's
                    // data key (learned from its HELLO epoch): interop confirmed.
                    println!("KEEPALIVE");
                    std::process::exit(0);
                }
                _ => {}
            },
            _ => {
                eprintln!("timeout");
                std::process::exit(1);
            }
        }
    }
}
