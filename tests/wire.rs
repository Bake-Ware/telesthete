//! Framing, latest-wins coalescing, and fragment reassembly (transport layer, no sockets).
use telesthete::wire::{self, UdpReasm};

// Build a fake proto message: [op][flags][reserved u16][window_id u32 le][rest...]
fn proto_msg(op: u8, window_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut m = vec![op, 0, 0, 0];
    m.extend_from_slice(&window_id.to_le_bytes());
    m.extend_from_slice(payload);
    m
}

#[test]
fn routing_matches_channel_plan() {
    // ctl/meta reliable; audio + tex UDP; input coalescible-by-opcode.
    assert!(!wire::is_udp(0, 0x01)); // ctl
    assert!(!wire::is_udp(201, 0x01)); // meta
    assert!(wire::is_udp(200, 0x01)); // audio
    assert!(wire::is_udp(2, 0x01)); // tex.0
    assert!(wire::is_udp(9, 0x02)); // tex.7
    assert!(!wire::is_udp(1, 0x01)); // input KEY (reliable)
    assert!(!wire::is_udp(1, 0x03)); // input FOCUS (reliable)
    assert!(wire::is_udp(1, 0x81)); // input MOTION (coalescible)
    assert!(wire::is_udp(1, 0x82)); // input SCROLL (coalescible)
}

#[test]
fn tcp_framing_roundtrip() {
    let msg = vec![1, 2, 3, 4, 5];
    let framed = wire::frame_tcp(7, &msg);
    let (ch, got, consumed) = wire::parse_tcp(&framed).unwrap();
    assert_eq!(ch, 7);
    assert_eq!(got, msg);
    assert_eq!(consumed, framed.len());
    // partial buffer returns None
    assert!(wire::parse_tcp(&framed[..3]).is_none());
}

#[test]
fn latest_wins_drops_stale_motion() {
    let mut r = UdpReasm::default();
    // motion for window 5, seq increasing then a stale one
    let m10 = proto_msg(0x81, 5, b"a");
    let m11 = proto_msg(0x81, 5, b"b");
    let m9 = proto_msg(0x81, 5, b"old");
    assert!(r.on_data(1, 10, m10).is_some());
    assert!(r.on_data(1, 11, m11).is_some());
    assert!(r.on_data(1, 9, m9).is_none(), "stale seq 9 after 11 must drop");
    // a different window is independent
    let w6 = proto_msg(0x81, 6, b"c");
    assert!(r.on_data(1, 3, w6).is_some());
}

#[test]
fn audio_never_coalesced() {
    let mut r = UdpReasm::default();
    // audio has no window; even out-of-order seq must all deliver (jitter buffer decides).
    assert!(r.on_data(200, 100, vec![1, 2, 3]).is_some());
    assert!(r.on_data(200, 99, vec![4, 5, 6]).is_some());
    assert!(r.on_data(200, 101, vec![7, 8, 9]).is_some());
}

#[test]
fn fragmentation_reassembles_in_and_out_of_order() {
    let big = proto_msg(0x01, 3, &vec![0xAB; 5000]);
    let frags = wire::fragment(&big, 1200);
    assert!(frags.len() > 1, "5KB must fragment at 1200 MTU");

    let mut r = UdpReasm::default();
    // deliver out of order; only the final piece completes it
    let n = frags.len();
    let mut done = None;
    for i in (0..n).rev() {
        let out = r.on_frag(2, 77, i as u16, n as u16, frags[i]);
        if out.is_some() {
            done = out;
        }
    }
    assert_eq!(done.unwrap(), big, "reassembled message must equal original");
}

#[test]
fn newer_msg_id_supersedes_incomplete() {
    let mut r = UdpReasm::default();
    let a = proto_msg(0x01, 3, &vec![1u8; 3000]);
    let b = proto_msg(0x01, 3, &vec![2u8; 3000]);
    let fa = wire::fragment(&a, 1200);
    let fb = wire::fragment(&b, 1200);
    // start msg 10 (incomplete: only frag 0), then msg 11 fully -> 11 completes, 10 dropped
    assert!(r.on_frag(2, 10, 0, fa.len() as u16, fa[0]).is_none());
    let mut out = None;
    for (i, f) in fb.iter().enumerate() {
        if let Some(m) = r.on_frag(2, 11, i as u16, fb.len() as u16, f) {
            out = Some(m);
        }
    }
    assert_eq!(out.unwrap(), b);
}
