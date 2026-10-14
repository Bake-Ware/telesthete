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

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

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

/// Reassembly timeout: partial messages older than this are discarded (§6.6).
pub const DEFAULT_REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(30);
/// Concurrent partial messages before the oldest is evicted (§6.6).
pub const DEFAULT_BUFFER_LIMIT: usize = 256;
/// A chunk declaring a `total` above this is dropped (§6.6 hardening): ~16 MB
/// at `MAX_CHUNK_PAYLOAD`, while the wire allows 65535 (~64 MB).
pub const DEFAULT_MAX_FRAGMENTS: u16 = 16384;
/// The most chunks the envelope can describe; [`fragment`] never emits more.
pub const WIRE_MAX_FRAGMENTS: u16 = 0xFFFF;
/// Byte budget for the library's own Channel/Board reassemblers
/// ([`Reassembler::for_chunk_size`]): holds one largest-possible message.
pub const LIBRARY_MAX_BUFFERED_BYTES: usize = 64 * 1024 * 1024;
/// How many evicted `fragment_id`s are remembered (each for the timeout).
pub const DEFAULT_EVICTED_MEMORY: usize = 1024;

struct Partial {
    total: u16,
    parts: HashMap<u16, Vec<u8>>,
    /// Bytes held in `parts`.
    size: usize,
    /// Arrival time, for the timeout.
    first_seen: Instant,
    /// Monotonic arrival order, so the memory caps evict the OLDEST buffer
    /// rather than an arbitrary (possibly currently-assembling) one.
    order: u64,
}

/// Collects fragments and emits full payloads. Single-task; not thread-safe.
///
/// Memory is bounded the same ways as the Python reference
/// (`telesthete.protocol.fragment.Reassembler`, SPEC §6.6), all applied before
/// a chunk is buffered:
///
/// * `buffer_limit` — concurrent partial messages; the oldest is evicted.
/// * `max_fragments` — a chunk declaring `total` above it is dropped, as is a
///   chunk of a multi-chunk message whose data exceeds `max_chunk_payload`.
///   A single-chunk message (`total == 1`) is returned at once, never
///   buffered, so it is exempt from the size cap.
/// * `max_buffered_bytes` — optional budget across all partials; the oldest
///   *other* partials are evicted to make room, and a message that still
///   can't fit is dropped.
/// * `timeout` — partials older than this are discarded.
///
/// An evicted message can never complete, so its `fragment_id` is remembered
/// for `timeout` (up to [`DEFAULT_EVICTED_MEMORY`] ids) and its late chunks
/// are dropped rather than starting a fresh partial that would evict live
/// messages in turn.
pub struct Reassembler {
    buffers: HashMap<[u8; 16], Partial>,
    limit: usize,
    max_fragments: u16,
    max_chunk_payload: usize,
    max_buffered_bytes: Option<usize>,
    timeout: Duration,
    bytes: usize,
    evicted: HashMap<[u8; 16], Instant>,
    evicted_order: VecDeque<([u8; 16], Instant)>,
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
    /// Reference defaults: 256 partials, 30 s timeout, `total` ≤ 16384,
    /// chunks ≤ `MAX_CHUNK_PAYLOAD`, no byte budget. Suits a receiver whose
    /// senders use `MAX_CHUNK_PAYLOAD` chunks; to receive from a sender with a
    /// different chunk size use [`Reassembler::for_chunk_size`].
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            limit: DEFAULT_BUFFER_LIMIT,
            max_fragments: DEFAULT_MAX_FRAGMENTS,
            max_chunk_payload: MAX_CHUNK_PAYLOAD,
            max_buffered_bytes: None,
            timeout: DEFAULT_REASSEMBLY_TIMEOUT,
            bytes: 0,
            evicted: HashMap::new(),
            evicted_order: VecDeque::new(),
            next_order: 0,
        }
    }

    /// A reassembler that accepts everything [`fragment`] can emit at
    /// `chunk_size` (any chunk count up to the wire maximum), bounding memory
    /// with a byte budget instead: at least [`LIBRARY_MAX_BUFFERED_BYTES`],
    /// and always room for one largest message. The library's Channel and
    /// Board use this with `MAX_CHUNK_PAYLOAD`, matching their senders.
    pub fn for_chunk_size(chunk_size: usize) -> Self {
        let chunk_size = chunk_size.max(1);
        Self::new()
            .with_max_chunk_payload(chunk_size)
            .with_max_fragments(WIRE_MAX_FRAGMENTS)
            .with_max_buffered_bytes(Some(
                LIBRARY_MAX_BUFFERED_BYTES.max(chunk_size.saturating_mul(WIRE_MAX_FRAGMENTS as usize)),
            ))
    }

    /// Cap concurrent partial messages (min 1).
    pub fn with_buffer_limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(1);
        self
    }

    /// Drop chunks declaring `total` above `max` (min 1).
    pub fn with_max_fragments(mut self, max: u16) -> Self {
        self.max_fragments = max.max(1);
        self
    }

    /// Drop chunks of multi-chunk messages carrying more than `max` bytes (min 1).
    pub fn with_max_chunk_payload(mut self, max: usize) -> Self {
        self.max_chunk_payload = max.max(1);
        self
    }

    /// Bound bytes buffered across all partials (`None` = unbounded).
    pub fn with_max_buffered_bytes(mut self, max: Option<usize>) -> Self {
        self.max_buffered_bytes = max;
        self
    }

    /// Discard partials older than `timeout`.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Bytes currently held across all partial messages.
    pub fn buffered_bytes(&self) -> usize {
        self.bytes
    }

    /// Number of partial messages currently buffered.
    pub fn pending(&self) -> usize {
        self.buffers.len()
    }

    /// Feed one decrypted Channel payload. Returns the full message when this
    /// chunk completes one, else None. Invalid/duplicate/over-limit chunks
    /// return None.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<Vec<u8>> {
        let c = parse_chunk(chunk)?;
        if c.total > self.max_fragments {
            return None;
        }
        let now = Instant::now();
        self.gc(now);
        if self.evicted.contains_key(&c.fragment_id) {
            return None; // remainder of a message we already gave up on
        }

        let mut existing = self.buffers.get(&c.fragment_id).map(|p| p.total);
        if existing.is_some_and(|t| t != c.total) {
            // Corrupt sender changed total mid-message; reset this buffer.
            self.drop_partial(&c.fragment_id);
            existing = None;
        }
        if existing.is_some()
            && self.buffers[&c.fragment_id].parts.contains_key(&c.seq)
        {
            return None; // duplicate
        }
        // Single-chunk messages never touch the buffers, so they bypass every
        // budget (they can't accumulate) and never evict a partial message.
        if existing.is_none() && c.total == 1 {
            return Some(c.data);
        }
        let n = c.data.len();
        if n > self.max_chunk_payload {
            return None;
        }

        if let Some(max) = self.max_buffered_bytes {
            if n > max {
                return None;
            }
            while self.bytes + n > max {
                match self.oldest(Some(&c.fragment_id)) {
                    Some(victim) => self.evict(victim, now),
                    None => {
                        // Only this message's own parts are left: it can't fit.
                        self.evict(c.fragment_id, now);
                        return None;
                    }
                }
            }
        }

        if !self.buffers.contains_key(&c.fragment_id) {
            if self.buffers.len() >= self.limit {
                if let Some(victim) = self.oldest(None) {
                    self.evict(victim, now);
                }
            }
            let order = self.next_order;
            self.next_order += 1;
            self.buffers.insert(
                c.fragment_id,
                Partial {
                    total: c.total,
                    parts: HashMap::new(),
                    size: 0,
                    first_seen: now,
                    order,
                },
            );
        }
        let entry = self.buffers.get_mut(&c.fragment_id).unwrap();
        entry.parts.insert(c.seq, c.data);
        entry.size += n;
        self.bytes += n;
        if entry.parts.len() != entry.total as usize {
            return None;
        }

        let part = self.buffers.remove(&c.fragment_id).unwrap();
        self.bytes -= part.size;
        let mut out = Vec::with_capacity(part.size);
        for i in 0..part.total {
            out.extend_from_slice(&part.parts[&i]);
        }
        Some(out)
    }

    fn oldest(&self, exclude: Option<&[u8; 16]>) -> Option<[u8; 16]> {
        self.buffers
            .iter()
            .filter(|(k, _)| Some(*k) != exclude)
            .min_by_key(|(_, p)| p.order)
            .map(|(k, _)| *k)
    }

    fn drop_partial(&mut self, fid: &[u8; 16]) {
        if let Some(p) = self.buffers.remove(fid) {
            self.bytes -= p.size;
        }
    }

    /// Drop a partial that can no longer complete and remember its id.
    fn evict(&mut self, fid: [u8; 16], now: Instant) {
        self.drop_partial(&fid);
        self.evicted.insert(fid, now);
        self.evicted_order.push_back((fid, now));
        while self.evicted.len() > DEFAULT_EVICTED_MEMORY {
            let Some((old, at)) = self.evicted_order.pop_front() else { break };
            if self.evicted.get(&old) == Some(&at) {
                self.evicted.remove(&old);
            }
        }
    }

    fn gc(&mut self, now: Instant) {
        let timeout = self.timeout;
        let expired = |at: Instant| now.duration_since(at) > timeout;
        while let Some(&(fid, at)) = self.evicted_order.front() {
            if !expired(at) {
                break;
            }
            self.evicted_order.pop_front();
            if self.evicted.get(&fid) == Some(&at) {
                self.evicted.remove(&fid);
            }
        }
        if self.buffers.is_empty() {
            return;
        }
        let stale: Vec<[u8; 16]> = self
            .buffers
            .iter()
            .filter(|(_, p)| expired(p.first_seen))
            .map(|(k, _)| *k)
            .collect();
        for k in stale {
            self.drop_partial(&k);
        }
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

    fn ck(fid: u8, seq: u16, total: u16, data: &[u8]) -> Vec<u8> {
        pack_chunk(&[fid; 16], seq, total, data)
    }

    fn feed_all(r: &mut Reassembler, chunks: &[Vec<u8>]) -> Option<Vec<u8>> {
        let mut out = None;
        for c in chunks {
            if let Some(m) = r.feed(c) {
                out = Some(m);
            }
        }
        out
    }

    #[test]
    fn rejects_oversize_chunk_but_not_single_chunk_messages() {
        let mut r = Reassembler::new();
        let big = vec![b'x'; MAX_CHUNK_PAYLOAD + 1];
        assert!(r.feed(&ck(2, 0, 2, &big)).is_none());
        assert_eq!((r.pending(), r.buffered_bytes()), (0, 0));
        let ok = vec![b'y'; MAX_CHUNK_PAYLOAD];
        assert!(r.feed(&ck(2, 0, 2, &ok)).is_none());
        let mut want = ok.clone();
        want.push(b'z');
        assert_eq!(r.feed(&ck(2, 1, 2, b"z")).unwrap(), want);
        // total == 1 is never buffered: exempt from the chunk cap and budget
        // (the ESP32 firmware sends whole messages as one oversize chunk).
        let mut r = Reassembler::new().with_max_buffered_bytes(Some(10));
        let big = vec![b'q'; 1400];
        assert_eq!(r.feed(&ck(3, 0, 1, &big)).unwrap(), big);
        assert_eq!((r.pending(), r.buffered_bytes()), (0, 0));
    }

    #[test]
    fn rejects_declared_total_above_max_fragments() {
        let mut r = Reassembler::new();
        assert!(r.feed(&ck(5, 0, DEFAULT_MAX_FRAGMENTS + 1, b"a")).is_none());
        assert_eq!(r.pending(), 0);
        let mut r = Reassembler::new().with_max_fragments(3);
        assert!(r.feed(&ck(6, 0, 4, b"a")).is_none());
        assert_eq!(feed_all(&mut r, &fragment(b"abc", 1, &[6; 16])).unwrap(), b"abc");
    }

    #[test]
    fn byte_budget_evicts_oldest_and_remembers_it() {
        let mut r = Reassembler::new().with_max_buffered_bytes(Some(10));
        assert!(r.feed(&ck(0xa, 0, 2, b"aaaa")).is_none());
        assert!(r.feed(&ck(0xb, 0, 2, b"bbbb")).is_none());
        assert_eq!(r.buffered_bytes(), 8);
        assert!(r.feed(&ck(0xc, 0, 2, b"cccc")).is_none()); // evicts a
        assert_eq!((r.pending(), r.buffered_bytes()), (2, 8));
        assert!(r.feed(&ck(0xa, 1, 2, b"A")).is_none());
        assert_eq!(r.pending(), 2, "an evicted message's tail is not re-buffered");
        assert_eq!(r.feed(&ck(0xb, 1, 2, b"B")).unwrap(), b"bbbbB");
        assert_eq!(r.feed(&ck(0xc, 1, 2, b"C")).unwrap(), b"ccccC");
        assert_eq!((r.pending(), r.buffered_bytes()), (0, 0));
    }

    #[test]
    fn message_that_cannot_fit_is_dropped_and_remembered() {
        let mut r = Reassembler::new().with_max_buffered_bytes(Some(6));
        assert!(r.feed(&ck(0xd, 0, 3, b"1234")).is_none());
        assert!(r.feed(&ck(0xd, 1, 3, b"5678")).is_none());
        assert!(r.feed(&ck(0xd, 2, 3, b"9")).is_none());
        assert_eq!((r.pending(), r.buffered_bytes()), (0, 0));
        // A chunk bigger than the whole budget is refused without evicting.
        assert!(r.feed(&ck(0xe, 0, 2, b"kk")).is_none());
        assert!(r.feed(&ck(0xf, 0, 2, &[b'x'; 7])).is_none());
        assert_eq!((r.pending(), r.buffered_bytes()), (1, 2));
    }

    #[test]
    fn buffer_limit_eviction_is_remembered() {
        let mut r = Reassembler::new().with_buffer_limit(1);
        assert!(r.feed(&ck(0x22, 0, 2, b"a")).is_none());
        assert!(r.feed(&ck(0x23, 0, 2, b"b")).is_none()); // evicts 0x22
        assert!(r.feed(&ck(0x22, 1, 2, b"A")).is_none());
        assert_eq!(r.feed(&ck(0x23, 1, 2, b"B")).unwrap(), b"bB");
    }

    #[test]
    fn timeout_discards_partials_and_forgets_evictions() {
        let mut r = Reassembler::new()
            .with_buffer_limit(1)
            .with_timeout(Duration::from_millis(30));
        assert!(r.feed(&ck(0x24, 0, 2, b"a")).is_none());
        assert!(r.feed(&ck(0x25, 0, 2, b"b")).is_none()); // evicts 0x24
        std::thread::sleep(Duration::from_millis(50));
        assert!(r.feed(&ck(0x24, 0, 2, b"x")).is_none()); // GC: 0x25 gone, 0x24 forgotten
        assert_eq!(r.pending(), 1);
        assert_eq!(r.feed(&ck(0x24, 1, 2, b"y")).unwrap(), b"xy");
        assert_eq!(r.buffered_bytes(), 0);
    }

    #[test]
    fn evicted_memory_is_bounded() {
        let mut r = Reassembler::new().with_buffer_limit(1);
        for i in 0..(DEFAULT_EVICTED_MEMORY as u32 + 50) {
            let mut fid = [0u8; 16];
            fid[..4].copy_from_slice(&i.to_be_bytes());
            r.feed(&pack_chunk(&fid, 0, 2, b"z"));
        }
        assert!(r.evicted.len() <= DEFAULT_EVICTED_MEMORY);
        assert!(r.evicted_order.len() <= DEFAULT_EVICTED_MEMORY + 1);
    }

    #[test]
    fn total_change_resets_accounting() {
        let mut r = Reassembler::new().with_max_buffered_bytes(Some(100));
        assert!(r.feed(&ck(0x10, 0, 3, b"abc")).is_none());
        assert!(r.feed(&ck(0x10, 0, 2, b"de")).is_none());
        assert_eq!((r.pending(), r.buffered_bytes()), (1, 2));
        assert_eq!(r.feed(&ck(0x10, 1, 2, b"f")).unwrap(), b"def");
        assert_eq!(r.buffered_bytes(), 0);
    }

    #[test]
    fn three_interleaved_30mb_messages_under_64mib_two_complete() {
        // Mirrors the Python test: one message must be sacrificed to the
        // budget; remembering it lets the other two complete.
        let size = 30_000_000usize;
        let cs = MAX_CHUNK_PAYLOAD;
        let total = size.div_ceil(cs) as u16;
        let mut r = Reassembler::new()
            .with_max_fragments(WIRE_MAX_FRAGMENTS)
            .with_max_buffered_bytes(Some(64 * 1024 * 1024));
        let payloads: Vec<Vec<u8>> = (0..3u8).map(|k| vec![0x41 + k; size]).collect();
        let mut done = 0;
        for seq in 0..total {
            let lo = seq as usize * cs;
            for (k, p) in payloads.iter().enumerate() {
                let piece = &p[lo..(lo + cs).min(size)];
                if let Some(m) = r.feed(&pack_chunk(&[0x30 + k as u8; 16], seq, total, piece)) {
                    assert_eq!(&m, p);
                    done += 1;
                }
            }
        }
        assert_eq!(done, 2);
        assert_eq!(r.pending(), 0);
    }

    #[test]
    fn for_chunk_size_accepts_what_fragment_emits() {
        let payload: Vec<u8> = (0..5120u32).map(|i| i as u8).collect();
        let chunks = fragment(&payload, 2000, &FID);
        assert!(feed_all(&mut Reassembler::new(), &chunks).is_none(), "default drops 2000 B chunks");
        assert_eq!(feed_all(&mut Reassembler::for_chunk_size(2000), &chunks).unwrap(), payload);
        let payload = vec![b't'; DEFAULT_MAX_FRAGMENTS as usize + 10];
        let chunks = fragment(&payload, 1, &FID);
        assert_eq!(feed_all(&mut Reassembler::for_chunk_size(1), &chunks).unwrap(), payload);
        let r = Reassembler::for_chunk_size(MAX_CHUNK_PAYLOAD);
        assert!(r.max_buffered_bytes.is_some_and(|b| b >= LIBRARY_MAX_BUFFERED_BYTES));
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
