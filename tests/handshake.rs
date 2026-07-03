//! Handshake correctness: good PSK agrees, wrong PSK rejected, replay rejected, framing.
use telesthete::crypto::{self, DatagramKey, RecordKey};

const PSK_A: [u8; 32] = [7u8; 32];
const PSK_B: [u8; 32] = [9u8; 32];

/// Drive the 3-message handshake in-memory; return both sessions on success.
fn run(client_psk: [u8; 32], host_psk: [u8; 32]) -> Result<(crypto::Session, crypto::Session), crypto::HsError> {
    let hs = crypto::ClientHandshake::new(client_psk);
    let hello1 = hs.hello1(40000);
    let (hello2, host_sess, pending, _client_udp) = crypto::host_respond(&hello1, host_psk, 40001)?;
    let (client_sess, _host_udp, confirm3) = hs.recv_hello2(&hello2)?;
    pending.verify(&confirm3)?;
    Ok((client_sess, host_sess))
}

#[test]
fn good_psk_agrees_on_keys() {
    let (c, h) = run(PSK_A, PSK_A).expect("handshake should succeed");
    // Directional keys must cross: client tx == host rx, and vice versa.
    assert_eq!(c.k_tcp_tx, h.k_tcp_rx);
    assert_eq!(c.k_tcp_rx, h.k_tcp_tx);
    assert_eq!(c.k_udp_tx, h.k_udp_rx);
    assert_eq!(c.session_id, h.session_id);
    // Sanity: keys are not all-zero.
    assert_ne!(c.k_tcp_tx, [0u8; 32]);
}

#[test]
fn wrong_psk_rejected() {
    // Host verifies the client's confirm last; wrong PSK => BadConfirm somewhere.
    let err = run(PSK_A, PSK_B).unwrap_err();
    assert_eq!(err, crypto::HsError::BadConfirm);
}

#[test]
fn client_rejects_wrong_host_psk() {
    // The client checks confirmH before it would ever send confirm3.
    let hs = crypto::ClientHandshake::new(PSK_A);
    let hello1 = hs.hello1(1234);
    let (hello2, _hsess, _pending, _u) = crypto::host_respond(&hello1, PSK_B, 1).unwrap();
    assert_eq!(hs.recv_hello2(&hello2).unwrap_err(), crypto::HsError::BadConfirm);
}

#[test]
fn replayed_handshake_rejected() {
    // Capture a full good exchange, then replay client's messages against a FRESH host.
    let hs = crypto::ClientHandshake::new(PSK_A);
    let hello1 = hs.hello1(5000);
    let (_hello2_a, _sa, _pa, _ua) = crypto::host_respond(&hello1, PSK_A, 1).unwrap();
    // Attacker replays the same hello1 to a new host instance (fresh ephemeral + nonce).
    let (hello2_b, _sb, pending_b, _ub) = crypto::host_respond(&hello1, PSK_A, 1).unwrap();
    // Attacker cannot produce confirm3 for host B's transcript without the client's secret;
    // best it can do is replay nothing valid. A random/blank confirm must fail.
    assert_eq!(pending_b.verify(&[0u8; 32]).unwrap_err(), crypto::HsError::BadConfirm);
    // And host B's hello2 differs from host A's (fresh keys) — proving no replayable key.
    assert_ne!(&hello2_b[0..32], &_hello2_a[0..32]);
}

#[test]
fn tcp_record_roundtrip() {
    let (c, h) = run(PSK_A, PSK_A).unwrap();
    let mut c_tx = RecordKey::new(c.k_tcp_tx);
    let mut h_rx = RecordKey::new(h.k_tcp_rx);
    for i in 0..5u8 {
        let msg = vec![i; 100 + i as usize];
        let ct = c_tx.seal(&msg);
        assert_eq!(h_rx.open(&ct).unwrap(), msg); // in-order counter nonces
    }
    // Tampered ciphertext fails auth.
    let mut ct = c_tx.seal(b"secret");
    ct[0] ^= 0xFF;
    assert!(h_rx.open(&ct).is_none());
}

#[test]
fn udp_datagram_roundtrip_and_forgery() {
    let (c, h) = run(PSK_A, PSK_A).unwrap();
    let tx = DatagramKey::new(c.k_udp_tx, true);
    let rx = DatagramKey::new(h.k_udp_rx, false);
    let ct = tx.seal(0, 3, 42, b"motion-payload");
    assert_eq!(rx.open(0, 3, 42, &ct).unwrap(), b"motion-payload");
    // Wrong seq (nonce) => auth fails (defeats replay/reorder forgery).
    assert!(rx.open(0, 3, 43, &ct).is_none());
    // Wrong channel (AAD) => auth fails.
    assert!(rx.open(0, 4, 42, &ct).is_none());
}
