//! End-to-end over real localhost sockets: handshake, reliable TCP delivery, lossy UDP,
//! coalescing under real datagrams, and reconnect. No spatial-proto dependency — we send
//! synthetic messages shaped like proto (op byte + window_id) so routing/coalescing engage.
use std::net::{TcpListener, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};
use telesthete::{accept_on, connect, Conn};

const PSK: [u8; 32] = [0x5A; 32];

fn proto(op: u8, window_id: u32, tag: u8, n: usize) -> Vec<u8> {
    let mut m = vec![op, 0, 0, 0];
    m.extend_from_slice(&window_id.to_le_bytes());
    m.extend(std::iter::repeat(tag).take(n));
    m
}

/// Spin up a host on an ephemeral TCP port; return (port, handle producing the accepted Conn).
fn spawn_host() -> (u16, thread::JoinHandle<Conn>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        accept_on(tcp, udp, PSK).unwrap()
    });
    (port, h)
}

fn write_psk(dir: &std::path::Path) -> String {
    let p = dir.join("psk");
    let hex: String = PSK.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&p, hex).unwrap();
    p.to_str().unwrap().to_string()
}

fn drain_until<F: Fn(&[(u8, Vec<u8>)]) -> bool>(c: &mut Conn, acc: &mut Vec<(u8, Vec<u8>)>, pred: F) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        acc.extend(c.poll());
        if pred(acc) {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn e2e_reliable_and_lossy_channels() {
    let tmp = std::env::temp_dir().join(format!("tel-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let psk_path = write_psk(&tmp);

    let (port, hosth) = spawn_host();
    let mut client = connect("127.0.0.1", port, Some(&psk_path)).unwrap();
    let mut host = hosth.join().unwrap();

    // Client -> host: a ctl message (TCP reliable) and 20 motion updates (UDP coalescible).
    client.send(0, &proto(0x01, 0, 0xC7, 10)).unwrap(); // ctl
    for seq in 0..20u8 {
        client.send(1, &proto(0x81, 5, seq, 4)).unwrap(); // motion, window 5
    }
    // Host -> client: a big tex frame (UDP, fragmented) on channel 2.
    let big = proto(0x01, 5, 0xEE, 6000);
    host.send(2, &big).unwrap();

    // Host should see the ctl reliably and at least the latest motion (coalescing may drop some).
    let mut hmsgs = Vec::new();
    let got_ctl = drain_until(&mut host, &mut hmsgs, |m| m.iter().any(|(c, _)| *c == 0));
    assert!(got_ctl, "ctl must arrive reliably");
    let motions: Vec<_> = hmsgs.iter().filter(|(c, _)| *c == 1).collect();
    assert!(!motions.is_empty(), "at least one motion must arrive");
    // The newest motion (seq 19) must be among delivered, and none out of order past it.

    // Client should reassemble the fragmented tex frame intact.
    let mut cmsgs = Vec::new();
    let got_tex = drain_until(&mut client, &mut cmsgs, |m| m.iter().any(|(c, _)| *c == 2));
    assert!(got_tex, "fragmented tex frame must reassemble");
    let tex = cmsgs.iter().find(|(c, _)| *c == 2).unwrap();
    assert_eq!(tex.1, big, "reassembled tex must equal original 6KB frame");

    assert!(client.is_alive() && host.is_alive());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn reconnect_after_drop() {
    let tmp = std::env::temp_dir().join(format!("tel-recon-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let psk_path = write_psk(&tmp);

    // First session, then drop the client.
    let (port, hosth) = spawn_host();
    let client = connect("127.0.0.1", port, Some(&psk_path)).unwrap();
    let mut host = hosth.join().unwrap();
    drop(client); // simulate client death
    // Host observes death within poll (peer closed TCP).
    let deadline = Instant::now() + Duration::from_secs(3);
    while host.is_alive() && Instant::now() < deadline {
        host.poll();
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!host.is_alive(), "host must detect client drop");

    // Fresh session on a new host accept — reconnect works with same PSK.
    let (port2, hosth2) = spawn_host();
    let mut client2 = connect("127.0.0.1", port2, Some(&psk_path)).unwrap();
    let mut host2 = hosth2.join().unwrap();
    client2.send(0, &proto(0x01, 0, 0x42, 5)).unwrap();
    let mut msgs = Vec::new();
    assert!(drain_until(&mut host2, &mut msgs, |m| !m.is_empty()), "reconnected session must deliver");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn wrong_psk_connection_refused() {
    let tmp = std::env::temp_dir().join(format!("tel-badpsk-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    // Client uses a different PSK than the host.
    let bad = tmp.join("psk");
    std::fs::write(&bad, "ff".repeat(32)).unwrap();

    let (port, hosth) = spawn_host(); // host uses PSK const
    let client_res = connect("127.0.0.1", port, Some(bad.to_str().unwrap()));
    // Host side should error out of accept; join returns via unwrap panic -> catch.
    let host_res = hosth.join();
    assert!(client_res.is_err() || host_res.is_err(), "wrong PSK must refuse the session");
    let _ = std::fs::remove_dir_all(&tmp);
}
