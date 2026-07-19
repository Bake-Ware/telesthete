//! Channel routing, UDP datagram framing, coalescing, and fragmentation reassembly.
//! See spec/TELESTHETE.md §2, §4, §5.

use std::collections::HashMap;

// proto channel ids (must match spatial-hostd/spatial-client)
pub const CH_CTL: u8 = 0;
pub const CH_INPUT: u8 = 1;
pub const CH_META: u8 = 201;

// input opcode high bit = coalescible (spatial-proto input::COALESCIBLE_BIT)
pub const COALESCIBLE_BIT: u8 = 0x80;

// UDP datagram types
pub const DT_DATA: u8 = 0;
pub const DT_FRAG: u8 = 1;
pub const DT_HELLO: u8 = 2;
pub const DT_PING: u8 = 3;
pub const DT_PONG: u8 = 4;

pub fn default_mtu() -> usize {
    std::env::var("TELESTHETE_MTU").ok().and_then(|v| v.parse().ok()).unwrap_or(1200)
}

/// A channel rides UDP if it's a tex channel (2..=9), audio (200), or a coalescible input
/// message. Everything else (ctl, meta, reliable input) rides TCP. `first_op` is the proto
/// message's first envelope byte (its opcode).
pub fn is_udp(channel: u8, first_op: u8) -> bool {
    match channel {
        CH_CTL | CH_META => false,
        CH_INPUT => first_op & COALESCIBLE_BIT != 0, // motion/scroll -> UDP
        200 => true,                                 // audio
        c if (2..=9).contains(&c) => true,           // tex.N
        _ => false,                                  // unknown -> reliable (safe default)
    }
}

// ---- TCP record plaintext framing: [u8 channel][u32 le len][bytes] ----

pub fn frame_tcp(channel: u8, msg: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + msg.len());
    v.push(channel);
    v.extend_from_slice(&(msg.len() as u32).to_le_bytes());
    v.extend_from_slice(msg);
    v
}

/// Parse one framed message from the front of `buf`; returns (channel, msg, consumed) or None.
pub fn parse_tcp(buf: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buf.len() < 5 {
        return None;
    }
    let ch = buf[0];
    let len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if buf.len() < 5 + len {
        return None;
    }
    Some((ch, buf[5..5 + len].to_vec(), 5 + len))
}

// ---- UDP datagram plaintext bodies (before encryption) ----
// DATA: [channel][seq u24][msg]                        (seq packed in the 3 low bytes)
// The transport DATA/FRAG headers put channel+seq in the clear-ish AAD/nonce, so the
// *plaintext body* we seal is just the proto message (DATA) or the fragment slice (FRAG).

/// Build a DATA datagram plaintext = the proto message as-is.
pub fn data_body(msg: &[u8]) -> Vec<u8> {
    msg.to_vec()
}

/// FRAG header prepended to each fragment's plaintext: [msg_id u32][frag_idx u16][frag_count u16].
pub fn frag_body(msg_id: u32, idx: u16, count: u16, slice: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + slice.len());
    v.extend_from_slice(&msg_id.to_le_bytes());
    v.extend_from_slice(&idx.to_le_bytes());
    v.extend_from_slice(&count.to_le_bytes());
    v.extend_from_slice(slice);
    v
}

pub fn parse_frag(body: &[u8]) -> Option<(u32, u16, u16, &[u8])> {
    if body.len() < 8 {
        return None;
    }
    let msg_id = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
    let idx = u16::from_le_bytes([body[4], body[5]]);
    let count = u16::from_le_bytes([body[6], body[7]]);
    Some((msg_id, idx, count, &body[8..]))
}

/// Split a large tex message into MTU-sized fragment slices.
pub fn fragment(msg: &[u8], mtu: usize) -> Vec<&[u8]> {
    let payload = mtu.saturating_sub(8 + 16).max(1); // minus frag header + AEAD tag
    if msg.len() <= payload {
        return vec![msg];
    }
    msg.chunks(payload).collect()
}

/// Latest-wins tracking + fragment reassembly, per (channel, window). Window id is the
/// proto message's window_id when applicable; for coalescible input/tex it's the first u32
/// of the proto body (both motion/scroll and tex put window_id first). For audio there's no
/// window; we key on channel alone.
#[derive(Default)]
pub struct UdpReasm {
    last_seq: HashMap<(u8, u32), u32>,
    frags: HashMap<(u8, u32), FragSet>,
}

struct FragSet {
    count: u16,
    parts: Vec<Option<Vec<u8>>>,
    have: u16,
}

impl UdpReasm {
    /// seq_newer with the wrap-aware window from spatial-proto (RFC1982-style).
    fn newer(seq: u32, last: u32) -> bool {
        seq != last && seq.wrapping_sub(last) < 0x8000_0000
    }

    /// Returns the proto message if this DATA datagram is fresh (drops stale latest-wins).
    pub fn on_data(&mut self, channel: u8, seq: u32, msg: Vec<u8>) -> Option<Vec<u8>> {
        let win = window_key(channel, &msg);
        let key = (channel, win);
        // audio (no window / not coalescible-ordered): always deliver.
        if channel == 200 {
            return Some(msg);
        }
        match self.last_seq.get(&key) {
            Some(&last) if !Self::newer(seq, last) => None, // stale -> drop
            _ => {
                self.last_seq.insert(key, seq);
                Some(msg)
            }
        }
    }

    /// Feed a FRAG datagram; returns the reassembled message when the last piece lands.
    pub fn on_frag(&mut self, channel: u8, msg_id: u32, idx: u16, count: u16, slice: &[u8]) -> Option<Vec<u8>> {
        if count == 0 || idx >= count {
            return None;
        }
        let key = (channel, msg_id);
        // A newer msg_id on the same channel supersedes older incomplete sets (tex windows
        // map to distinct channels, so channel identifies the stream). Bounds memory too.
        self.frags
            .retain(|&(c, mid), _| c != channel || mid == msg_id || Self::newer(mid, msg_id));
        let set = self.frags.entry(key).or_insert_with(|| FragSet {
            count,
            parts: vec![None; count as usize],
            have: 0,
        });
        if set.count != count {
            return None; // inconsistent
        }
        if set.parts[idx as usize].is_none() {
            set.parts[idx as usize] = Some(slice.to_vec());
            set.have += 1;
        }
        if set.have == set.count {
            let set = self.frags.remove(&key).unwrap();
            let mut out = Vec::new();
            for p in set.parts {
                out.extend_from_slice(&p.unwrap());
            }
            Some(out)
        } else {
            None
        }
    }
}

/// window_id key for latest-wins: first u32 LE of the proto message body, after the 4-byte
/// envelope (op,flags,reserved u16). Motion/scroll and all tex ops put window_id first.
fn window_key(channel: u8, msg: &[u8]) -> u32 {
    if channel == 200 || msg.len() < 8 {
        return 0;
    }
    u32::from_le_bytes([msg[4], msg[5], msg[6], msg[7]])
}
