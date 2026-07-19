//! AF_UNIX transport integration tests (SPEC §9.4, conformance I1–I3).
//!
//! The hub binds one `<band_id_hex>.sock` SEQPACKET listener per active band. A
//! band is seeded by a UDP peer; the local SEQPACKET client then bridges to it.

mod common;
use common::*;

use std::mem::MaybeUninit;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use socket2::{Domain, SockAddr, Socket, Type};
use tokio::net::UdpSocket;

fn tmp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("telesthitium-it-{tag}-{}", std::process::id()));
    p
}

/// Connect a blocking SEQPACKET client with a short read timeout.
fn seqpacket_client(path: &PathBuf) -> Socket {
    let s = Socket::new(Domain::UNIX, Type::SEQPACKET, None).unwrap();
    s.connect(&SockAddr::unix(path).unwrap()).unwrap();
    s.set_read_timeout(Some(Duration::from_millis(600))).unwrap();
    s
}

fn seq_recv(sock: &Socket) -> Option<Vec<u8>> {
    let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
    match sock.recv(&mut buf) {
        Ok(n) if n > 0 => {
            Some(buf[..n].iter().map(|b| unsafe { b.assume_init() }).collect())
        }
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn socket_dir_is_0700() {
    // I3 — the socket directory is created 0700 (directory perms are the ACL).
    let dir = tmp_dir("perms");
    let _ = std::fs::remove_dir_all(&dir);
    let reg = registry_open();
    {
        let (reg, dir) = (reg.clone(), dir.clone());
        tokio::spawn(async move {
            let _ = telesthitium::unix::serve(dir, reg, 1024, std::future::pending::<()>()).await;
        });
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "socket dir must be 0700");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_peer_bridged_with_boundaries() {
    // I1 (local peer bridged to its band) + I2 (one message = one frame).
    let dir = tmp_dir("bridge");
    let _ = std::fs::remove_dir_all(&dir);
    let reg = registry_open();

    // AF_UNIX manager.
    {
        let (reg, dir) = (reg.clone(), dir.clone());
        tokio::spawn(async move {
            let _ = telesthitium::unix::serve(dir, reg, 1024, std::future::pending::<()>()).await;
        });
    }
    // UDP transport, to seed band 4 (so its socket gets bound).
    let udp_addr = free_udp_addr().await;
    {
        let (reg, addr) = (reg.clone(), udp_addr);
        tokio::spawn(async move {
            let _ = telesthitium::udp::serve(addr, reg, 1024, std::future::pending::<()>()).await;
        });
    }
    tokio::time::sleep(Duration::from_millis(120)).await;

    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    udp.connect(udp_addr).await.unwrap();
    udp.send(&frame(4, 1, 0x01)).await.unwrap(); // seed band 4

    // Wait for the reconciliation loop to bind <band4>.sock.
    let sock_path = dir.join(format!("{}.sock", band_hex(4)));
    for _ in 0..20 {
        if sock_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(sock_path.exists(), "hub must bind the band's SEQPACKET socket");

    // Local SEQPACKET client -> hub -> UDP peer (I1).
    let client = tokio::task::spawn_blocking(move || {
        let s = seqpacket_client(&sock_path);
        s.send(&frame(4, 1, 0xA1)).unwrap(); // register + relay to UDP
        s
    })
    .await
    .unwrap();

    assert!(udp_recv_marked(&udp, 0xA1).await.is_some(), "AF_UNIX -> UDP bridge failed");

    // UDP -> hub -> AF_UNIX client, two frames as two distinct messages (I2).
    udp.send(&frame(4, 1, 0xB1)).await.unwrap();
    udp.send(&frame(4, 1, 0xB2)).await.unwrap();
    let (m1, m2) = tokio::task::spawn_blocking(move || {
        let a = seq_recv(&client);
        let b = seq_recv(&client);
        (a, b)
    })
    .await
    .unwrap();

    let mut markers: Vec<u8> = [m1, m2]
        .into_iter()
        .flatten()
        .filter(|f| f.len() == 43)
        .map(|f| f[42])
        .collect();
    markers.sort_unstable();
    assert_eq!(markers, vec![0xB1, 0xB2], "each frame must arrive as its own message");

    let _ = std::fs::remove_dir_all(&dir);
}
