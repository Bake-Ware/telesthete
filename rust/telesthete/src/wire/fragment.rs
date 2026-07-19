//! Channel message fragmentation envelope. See SPEC.md §6.6.
//!
//! A logical message larger than one Channel payload is split into chunks,
//! each carried inside its own CHANNEL frame and AEAD-encrypted independently.
//! The envelope is the first bytes of the Channel plaintext:
//!
//! ```text
//! Offset  Size  Field
//! 0       1     version       0x01
//! 1       16    fragment_id   random per logical message
//! 17      2     seq           u16 BE, 0-based chunk index
//! 19      2     total         u16 BE, total chunk count (>= 1)
//! 21      var   data          raw chunk bytes
//! ```
//!
//! Single-chunk (and empty) messages still use the envelope with seq=0 total=1.
//! Promoted to the reference from the Rook worker implementation in v1.2. Must
//! stay byte-identical to the Python reference (telesthete/protocol/fragment.py).

use std::collections::HashMap;

pub const VERSION: u8 = 0x01;
pub const FRAG_HEADER_LEN: usize = 21;
pub const MAX_CHANNEL_DATA: usize = 1024; // SPEC §6.4 / §12.4
pub const MAX_CHUNK_PAYLOAD: usize = MAX_CHANNEL_DATA - FRAG_HEADER_LEN; // 1003

/// A parsed fragmentation chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub fragment_id: [u8; 16],
    pub seq: u16,
    pub total: u16,
    pub data: Vec<u8>,
}

/// Pack one §6.6 chunk: 21-byte header + data.
pub fn pack_chunk(fragment_id: &[u8; 16], seq: u16, total: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(FRAG_HEADER_LEN + data.len());
    out.push(VERSION);
    out.extend_from_slice(fragment_id);
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(data);
    out
}

/// Parse one chunk. Returns None on a short / wrong-version / nonsense chunk.
pub fn parse_chunk(chunk: &[u8]) -> Option<Chunk> {
    if chunk.len() < FRAG_HEADER_LEN || chunk[0] != VERSION {
        return None;
    }
    let mut fid = [0u8; 16];
    fid.copy_from_slice(&chunk[1..17]);
    let seq = u16::from_be_bytes([chunk[17], chunk[18]]);
    let total = u16::from_be_bytes([chunk[19], chunk[20]]);
    if total == 0 || seq >= total {
        return None;
    }
    Some(Chunk {
        fragment_id: fid,
        seq,
        total,
        data: chunk[FRAG_HEADER_LEN..].to_vec(),
    })
}

/// Split `payload` into wire chunks (HEADER + data) under a fixed `fragment_id`.
/// Callers pass a random id per logical message (e.g. from `rand`); a fixed id
/// keeps output deterministic for tests/vectors.
pub fn fragment(payload: &[u8], chunk_size: usize, fragment_id: &[u8; 16]) -> Vec<Vec<u8>> {
    assert!(chunk_size > 0, "chunk_size must be positive");
    if payload.is_empty() {
        return vec![pack_chunk(fragment_id, 0, 1, &[])];
    }
    let pieces: Vec<&[u8]> = payload.chunks(chunk_size).collect();
    assert!(pieces.len() <= 0xFFFF, "payload too large; max 65535 fragments");
    let total = pieces.len() as u16;
    pieces
        .into_iter()
        .enumerate()
        .map(|(seq, piece)| pack_chunk(fragment_id, seq as u16, total, piece))
        .collect()
}

struct Partial {
    total: u16,
    parts: HashMap<u16, Vec<u8>>,
    /// Monotonic arrival order, so the memory cap evicts the OLDEST buffer
    /// rather than an arbitrary (possibly currently-assembling) one.
    order: u64,
}

/// Collects fragments and emits full payloads. Single-task; not thread-safe.
pub struct Reassembler {
    buffers: HashMap<[u8; 16], Partial>,
    limit: usize,
    next_order: u64,
}

impl Default for Reassembler {
    /// Same as [`Reassembler::new`]. (A derived `Default` would zero `limit`,
    /// making the memory cap evict every in-progress buffer.)
    fn default() -> Self {
        Self::new()
    }
}

impl Reassembler {
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            limit: 256,
            next_order: 0,
        }
    }

    /// Feed one decrypted Channel payload. Returns the full message when this
    /// chunk completes one, else None. Invalid/duplicate chunks return None.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        let c = parse_chunk(chunk)?;

        let order = self.next_order;
        let entry = self.buffers.entry(c.fragment_id).or_insert_with(|| Partial {
            total: c.total,
            parts: HashMap::new(),
            order,
        });
        if entry.parts.is_empty() {
            self.next_order += 1; // a fresh buffer consumed this order tick
        }
        if entry.total != c.total {
            // Corrupt sender changed total mid-message; reset this buffer.
            *entry = Partial {
                total: c.total,
                parts: HashMap::new(),
                order: entry.order,
            };
        }
        if entry.parts.contains_key(&c.seq) {
            return None; // duplicate
        }
        entry.parts.insert(c.seq, c.data);

        if entry.parts.len() as u16 != c.total {
            // Bound memory: past the cap, evict the OLDEST buffer — but never the
            // one we just fed (which would drop the message a chunk just advanced).
            if self.buffers.len() > self.limit {
                let victim = self
                    .buffers
                    .iter()
                    .filter(|(k, _)| **k != c.fragment_id)
                    .min_by_key(|(_, p)| p.order)
                    .map(|(k, _)| *k);
                if let Some(k) = victim {
                    self.buffers.remove(&k);
                }
            }
            return None;
        }

        let part = self.buffers.remove(&c.fragment_id).unwrap();
        let mut out = Vec::new();
        for i in 0..part.total {
            out.extend_from_slice(&part.parts[&i]);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FID: [u8; 16] = [7u8; 16];

    #[test]
    fn single_chunk_round_trip() {
        let chunks = fragment(b"hello", MAX_CHUNK_PAYLOAD, &FID);
        assert_eq!(chunks.len(), 1);
        let mut r = Reassembler::new();
        assert_eq!(r.feed(&chunks[0]).unwrap(), b"hello");
    }

    #[test]
    fn empty_message_uses_envelope() {
        let chunks = fragment(b"", MAX_CHUNK_PAYLOAD, &FID);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), FRAG_HEADER_LEN);
        let mut r = Reassembler::new();
        assert_eq!(r.feed(&chunks[0]).unwrap(), b"");
    }

    #[test]
    fn multi_chunk_round_trip_any_order() {
        let payload: Vec<u8> = (0..(MAX_CHUNK_PAYLOAD * 2 + 5)).map(|i| i as u8).collect();
        let chunks = fragment(&payload, MAX_CHUNK_PAYLOAD, &FID);
        assert_eq!(chunks.len(), 3);
        let mut r = Reassembler::new();
        // out of order: 2, 0, 1
        assert!(r.feed(&chunks[2]).is_none());
        assert!(r.feed(&chunks[0]).is_none());
        assert_eq!(r.feed(&chunks[1]).unwrap(), payload);
    }

    #[test]
    fn rejects_garbage_and_duplicates() {
        let mut r = Reassembler::new();
        assert!(r.feed(b"short").is_none());
        let chunks = fragment(&[1, 2, 3, 4], 2, &FID); // 2 chunks
        assert!(r.feed(&chunks[0]).is_none());
        assert!(r.feed(&chunks[0]).is_none()); // duplicate
        assert_eq!(r.feed(&chunks[1]).unwrap(), vec![1, 2, 3, 4]);
    }

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Byte-identical with the Python reference: see tests/vectors.json `fragment`.
    #[test]
    fn conformance_vectors_match_python() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors.json");
        let raw = std::fs::read_to_string(path).expect("read tests/vectors.json");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let Some(frags) = v.get("fragment").and_then(|f| f.as_array()) else {
            return; // section optional
        };
        for case in frags {
            let fid_v = from_hex(case["fragment_id_hex"].as_str().unwrap());
            let mut fid = [0u8; 16];
            fid.copy_from_slice(&fid_v);
            let payload = from_hex(case["payload_hex"].as_str().unwrap());
            let chunk_size = case["chunk_size"].as_u64().unwrap() as usize;
            let want: Vec<String> = case["chunks_hex"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap().to_string())
                .collect();
            let got = fragment(&payload, chunk_size, &fid);
            let got_hex: Vec<String> = got.iter().map(|c| to_hex(c)).collect();
            assert_eq!(got_hex, want, "fragment bytes diverged");
            // round-trip
            let mut r = Reassembler::new();
            let mut out = None;
            for c in &got {
                if let Some(full) = r.feed(c) {
                    out = Some(full);
                }
            }
            assert_eq!(out.unwrap(), payload);
        }
    }
}
