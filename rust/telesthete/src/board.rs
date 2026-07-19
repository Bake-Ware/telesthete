//! Board channel — replicated LWW key-value state across a Band. SPEC §7.
//!
//! Fire-and-forget datagrams like Streams; convergence comes from idempotent
//! LWW merges plus digest-driven anti-entropy (§7.4), not per-packet
//! reliability. Payload is JSON `{"type": <u8>, "payload": {...}}` like
//! Control (§7.1); SNAPSHOT always rides the §6.6 fragment envelope, even
//! single-chunk — a Board frame starting with the envelope version byte 0x01
//! is a snapshot chunk, one starting with `{` is direct JSON.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::framing::ChannelType;
use crate::transport::{Inbound, Outbound, Transport, TransportError};
use crate::wire::fragment::{fragment, Reassembler, MAX_CHUNK_PAYLOAD};

/// §7.2 message types.
pub const TYPE_SET: u8 = 0x01;
pub const TYPE_DIGEST: u8 = 0x02;
pub const TYPE_SYNC_REQ: u8 = 0x03;
pub const TYPE_SNAPSHOT: u8 = 0x04;

/// §7.4 reference anti-entropy cadence.
pub const DIGEST_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum BoardError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// JSON envelope, like Control (§7.1).
#[derive(Serialize, Deserialize, Debug)]
struct BoardEnvelope {
    #[serde(rename = "type")]
    type_: u8,
    payload: Value,
}

/// §7.2 SET payload (also the element type of SNAPSHOT's `entries`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SetPayload {
    pub key: String,
    #[serde(default)]
    pub value: Value,
    /// `[lamport, actor]` — Lamport clock + writer hostname tiebreak (§7.3).
    pub ts: (u64, String),
    #[serde(default)]
    pub deleted: bool,
}

/// §7.2 DIGEST payload. Fields optional on parse so a malformed probe still
/// compares (and mismatches) instead of erroring, mirroring the reference.
#[derive(Serialize, Deserialize, Debug)]
struct DigestPayload {
    #[serde(default)]
    count: Option<u64>,
    #[serde(default)]
    hash: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Default)]
struct SnapshotPayload {
    #[serde(default)]
    entries: Vec<SetPayload>,
}

/// One replicated map entry. Tombstones (`deleted`) propagate; GC is out of
/// scope for v1.2 (§7.3).
#[derive(Debug, Clone, PartialEq)]
pub struct BoardEntry {
    pub value: Value,
    pub lamport: u64,
    pub actor: String,
    pub deleted: bool,
}

impl BoardEntry {
    /// The LWW timestamp: tuple-ordered `(lamport, actor)` (§7.3).
    pub fn ts(&self) -> (u64, &str) {
        (self.lamport, &self.actor)
    }

    pub fn to_payload(&self, key: &str) -> SetPayload {
        SetPayload {
            key: key.to_string(),
            value: self.value.clone(),
            ts: (self.lamport, self.actor.clone()),
            deleted: self.deleted,
        }
    }
}

/// A key/value change (local write or remote merge) observed on a board.
#[derive(Debug, Clone)]
pub struct BoardChange {
    pub key: String,
    pub value: Value,
    pub deleted: bool,
}

/// Sans-io LWW map core (SPEC §7.3/§7.4): entries, Lamport clock, merge rule,
/// digest. `actor` is this writer's hostname — the total-order tiebreak for
/// equal Lamport clocks.
#[derive(Debug)]
pub struct BoardCore {
    actor: String,
    /// BTreeMap keeps keys sorted, which is exactly the digest's iteration
    /// order (§7.4). String `Ord` is byte-wise; for UTF-8 that equals the
    /// code-point order Python's `sorted()` uses.
    entries: BTreeMap<String, BoardEntry>,
    lamport: u64,
}

impl BoardCore {
    pub fn new(actor: impl Into<String>) -> Self {
        Self {
            actor: actor.into(),
            entries: BTreeMap::new(),
            lamport: 0,
        }
    }

    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub fn lamport(&self) -> u64 {
        self.lamport
    }

    /// Raw entry access (tombstones included). Mainly for tests/introspection.
    pub fn entry(&self, key: &str) -> Option<&BoardEntry> {
        self.entries.get(key)
    }

    /// Local write. Bumps the Lamport clock and returns the SET payload to
    /// broadcast.
    pub fn set(&mut self, key: &str, value: Value) -> SetPayload {
        self.lamport += 1;
        let entry = BoardEntry {
            value,
            lamport: self.lamport,
            actor: self.actor.clone(),
            deleted: false,
        };
        let payload = entry.to_payload(key);
        self.entries.insert(key.to_string(), entry);
        payload
    }

    /// Local delete: writes a tombstone (§7.3) so the delete propagates.
    pub fn delete(&mut self, key: &str) -> SetPayload {
        self.lamport += 1;
        let entry = BoardEntry {
            value: Value::Null,
            lamport: self.lamport,
            actor: self.actor.clone(),
            deleted: true,
        };
        let payload = entry.to_payload(key);
        self.entries.insert(key.to_string(), entry);
        payload
    }

    /// Live value for `key`; `None` if absent or tombstoned.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries
            .get(key)
            .filter(|e| !e.deleted)
            .map(|e| &e.value)
    }

    /// All live (non-tombstoned) key/value pairs.
    pub fn items(&self) -> HashMap<String, Value> {
        self.entries
            .iter()
            .filter(|(_, e)| !e.deleted)
            .map(|(k, e)| (k.clone(), e.value.clone()))
            .collect()
    }

    /// §7.4 anti-entropy digest: `(count, hash)` where hash is SHA-256 over
    /// the sorted-by-key concatenation of `key || lamport_be8 || actor ||
    /// deleted_byte`, hex-encoded. Values are deliberately excluded —
    /// `(lamport, actor)` uniquely versions an entry.
    pub fn digest(&self) -> (u64, String) {
        let mut h = Sha256::new();
        for (key, e) in &self.entries {
            h.update(key.as_bytes());
            h.update(e.lamport.to_be_bytes());
            h.update(e.actor.as_bytes());
            h.update(if e.deleted { [1u8] } else { [0u8] });
        }
        (self.entries.len() as u64, to_hex(&h.finalize()))
    }

    /// Apply one SET payload per the §7.3 merge rule. Returns `true` if it
    /// changed local state. The clock always advances to
    /// `max(local, incoming)`, even for a stale entry.
    pub fn merge_entry(&mut self, payload: &SetPayload) -> bool {
        let (lamport, actor) = (payload.ts.0, payload.ts.1.as_str());
        self.lamport = self.lamport.max(lamport);
        if let Some(current) = self.entries.get(&payload.key) {
            if (lamport, actor) <= current.ts() {
                return false; // strictly-greater wins; equal ts implies equal value
            }
        }
        self.entries.insert(
            payload.key.clone(),
            BoardEntry {
                value: payload.value.clone(),
                lamport,
                actor: actor.to_string(),
                deleted: payload.deleted,
            },
        );
        true
    }

    /// All entries (tombstones included) as SET payloads, for SNAPSHOT (§7.4).
    pub fn snapshot(&self) -> Vec<SetPayload> {
        self.entries
            .iter()
            .map(|(k, e)| e.to_payload(k))
            .collect()
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct BoardShared {
    core: BoardCore,
    destinations: Vec<SocketAddr>,
}

/// One replicated board endpoint over a [`Transport`]. Local writes broadcast
/// SET to every added destination; the background task merges inbound frames
/// and answers anti-entropy probes (§7.4).
pub struct BoardEndpoint {
    transport: Arc<Transport>,
    board_id: u16,
    shared: Arc<Mutex<BoardShared>>,
    changes: mpsc::UnboundedReceiver<BoardChange>,
    changes_tx: mpsc::UnboundedSender<BoardChange>,
    task: JoinHandle<()>,
}

impl Drop for BoardEndpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl BoardEndpoint {
    fn new(
        transport: Arc<Transport>,
        board_id: u16,
        actor: String,
        inbound: mpsc::UnboundedReceiver<Inbound>,
        rebase_rx: tokio::sync::broadcast::Receiver<SocketAddr>,
    ) -> Self {
        let shared = Arc::new(Mutex::new(BoardShared {
            core: BoardCore::new(actor),
            destinations: Vec::new(),
        }));
        let (changes_tx, changes) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_board_task(
            Arc::clone(&transport),
            board_id,
            Arc::clone(&shared),
            changes_tx.clone(),
            inbound,
            rebase_rx,
        ));
        Self {
            transport,
            board_id,
            shared,
            changes,
            changes_tx,
            task,
        }
    }

    pub fn board_id(&self) -> u16 {
        self.board_id
    }

    /// Add a peer that local SET/DIGEST broadcasts go to.
    pub async fn add_destination(&self, peer: SocketAddr) {
        let mut s = self.shared.lock().await;
        if !s.destinations.contains(&peer) {
            s.destinations.push(peer);
        }
    }

    pub async fn remove_destination(&self, peer: SocketAddr) {
        self.shared.lock().await.destinations.retain(|d| *d != peer);
    }

    /// Write `key` and broadcast the SET to all destinations.
    pub async fn set(&self, key: &str, value: Value) -> Result<(), BoardError> {
        let (payload, dests) = {
            let mut s = self.shared.lock().await;
            (s.core.set(key, value), s.destinations.clone())
        };
        let _ = self.changes_tx.send(BoardChange {
            key: payload.key.clone(),
            value: payload.value.clone(),
            deleted: false,
        });
        self.broadcast_set(&payload, &dests).await
    }

    /// Tombstone `key` (§7.3) and broadcast the delete.
    pub async fn delete(&self, key: &str) -> Result<(), BoardError> {
        let (payload, dests) = {
            let mut s = self.shared.lock().await;
            (s.core.delete(key), s.destinations.clone())
        };
        let _ = self.changes_tx.send(BoardChange {
            key: payload.key.clone(),
            value: Value::Null,
            deleted: true,
        });
        self.broadcast_set(&payload, &dests).await
    }

    async fn broadcast_set(
        &self,
        payload: &SetPayload,
        dests: &[SocketAddr],
    ) -> Result<(), BoardError> {
        let body = serde_json::to_value(payload)?;
        for dest in dests {
            send_board_json(&self.transport, *dest, self.board_id, TYPE_SET, &body).await?;
        }
        Ok(())
    }

    /// Live value for `key`; `None` if absent or tombstoned.
    pub async fn get(&self, key: &str) -> Option<Value> {
        self.shared.lock().await.core.get(key).cloned()
    }

    /// All live key/value pairs.
    pub async fn items(&self) -> HashMap<String, Value> {
        self.shared.lock().await.core.items()
    }

    /// §7.4 digest: `(count, hash)`.
    pub async fn digest(&self) -> (u64, String) {
        self.shared.lock().await.core.digest()
    }

    /// Send a DIGEST probe to `dest`, or to every destination when `None`.
    pub async fn send_digest(&self, dest: Option<SocketAddr>) -> Result<(), BoardError> {
        let (count, hash, dests) = {
            let s = self.shared.lock().await;
            let (count, hash) = s.core.digest();
            (count, hash, s.destinations.clone())
        };
        let body = serde_json::to_value(DigestPayload {
            count: Some(count),
            hash: Some(hash),
        })?;
        let targets = match dest {
            Some(d) => vec![d],
            None => dests,
        };
        for d in targets {
            send_board_json(&self.transport, d, self.board_id, TYPE_DIGEST, &body).await?;
        }
        Ok(())
    }

    /// Send a full SNAPSHOT to `dest`, always inside the §6.6 envelope.
    pub async fn send_snapshot(&self, dest: SocketAddr) -> Result<(), BoardError> {
        let entries = self.shared.lock().await.core.snapshot();
        send_snapshot_to(&self.transport, dest, self.board_id, &entries).await
    }

    /// Next change event (local write or remote merge), like Python's
    /// `on_change` callback.
    pub async fn changed(&mut self) -> Option<BoardChange> {
        self.changes.recv().await
    }
}

async fn send_board_raw(
    transport: &Transport,
    dest: SocketAddr,
    board_id: u16,
    plaintext: Vec<u8>,
) -> Result<(), TransportError> {
    transport
        .send(Outbound {
            to: dest,
            channel_type: ChannelType::Board,
            channel_id: board_id,
            plaintext,
            priority: 3, // §9.1: Board is the lowest data priority above Drop
            use_base_key: false,
        })
        .await
}

async fn send_board_json(
    transport: &Transport,
    dest: SocketAddr,
    board_id: u16,
    type_: u8,
    payload: &Value,
) -> Result<(), BoardError> {
    let body = serde_json::to_vec(&BoardEnvelope {
        type_,
        payload: payload.clone(),
    })?;
    send_board_raw(transport, dest, board_id, body).await?;
    Ok(())
}

async fn send_snapshot_to(
    transport: &Transport,
    dest: SocketAddr,
    board_id: u16,
    entries: &[SetPayload],
) -> Result<(), BoardError> {
    let env = serde_json::to_vec(&BoardEnvelope {
        type_: TYPE_SNAPSHOT,
        payload: serde_json::json!({ "entries": entries }),
    })?;
    // SNAPSHOT always rides the §6.6 envelope, even single-chunk (§7.4) — a
    // Board frame starting with the envelope version byte 0x01 is a snapshot
    // chunk; direct JSON frames start with '{'.
    let mut fid = [0u8; 16];
    getrandom::getrandom(&mut fid).expect("CSPRNG unavailable");
    for chunk in fragment(&env, MAX_CHUNK_PAYLOAD, &fid) {
        send_board_raw(transport, dest, board_id, chunk).await?;
    }
    Ok(())
}

async fn run_board_task(
    transport: Arc<Transport>,
    board_id: u16,
    shared: Arc<Mutex<BoardShared>>,
    changes_tx: mpsc::UnboundedSender<BoardChange>,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    mut rebase_rx: tokio::sync::broadcast::Receiver<SocketAddr>,
) {
    // Replay watermark per peer (SPEC §3.3): accept-first, then strictly
    // increasing outer sequence. Merges are idempotent, but a replayed SET
    // must not re-trigger change events forever.
    let mut watermark: HashMap<SocketAddr, u64> = HashMap::new();
    let mut reassemblers: HashMap<SocketAddr, Reassembler> = HashMap::new();
    loop {
        let pkt = tokio::select! {
            pkt = inbound.recv() => match pkt {
                Some(pkt) => pkt,
                None => break,
            },
            // Peer restarted (SPEC §3.3/§4.3): forget its watermark (Python's
            // reset_peer) so its fresh-session sequences are accepted.
            peer = rebase_rx.recv() => match peer {
                Ok(addr) => {
                    watermark.remove(&addr);
                    reassemblers.remove(&addr);
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            },
        };
        if matches!(watermark.get(&pkt.from), Some(&wm) if pkt.header.sequence <= wm) {
            debug!("board {board_id}: dropping replayed/stale frame from {}", pkt.from);
            continue;
        }
        watermark.insert(pkt.from, pkt.header.sequence);

        let payload = if pkt.payload.first() == Some(&0x01) {
            // §6.6 chunk of a SNAPSHOT (§7.4).
            let r = reassemblers.entry(pkt.from).or_default();
            match r.feed(&pkt.payload) {
                Some(full) => full,
                None => continue,
            }
        } else {
            pkt.payload
        };

        let env: BoardEnvelope = match serde_json::from_slice(&payload) {
            Ok(e) => e,
            Err(e) => {
                debug!("board {board_id}: bad envelope from {}: {e}", pkt.from);
                continue;
            }
        };
        match env.type_ {
            TYPE_SET => {
                let set: SetPayload = match serde_json::from_value(env.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        debug!("board {board_id}: bad SET: {e}");
                        continue;
                    }
                };
                let changed = shared.lock().await.core.merge_entry(&set);
                if changed {
                    let _ = changes_tx.send(BoardChange {
                        key: set.key,
                        value: set.value,
                        deleted: set.deleted,
                    });
                }
            }
            TYPE_DIGEST => {
                let probe: DigestPayload = match serde_json::from_value(env.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        debug!("board {board_id}: bad DIGEST: {e}");
                        continue;
                    }
                };
                let (count, hash) = shared.lock().await.core.digest();
                if probe.count != Some(count) || probe.hash.as_deref() != Some(hash.as_str()) {
                    if let Err(e) = send_board_json(
                        &transport,
                        pkt.from,
                        board_id,
                        TYPE_SYNC_REQ,
                        &serde_json::json!({}),
                    )
                    .await
                    {
                        debug!("board {board_id}: SYNC_REQ to {} failed: {e}", pkt.from);
                    }
                }
            }
            TYPE_SYNC_REQ => {
                let entries = shared.lock().await.core.snapshot();
                if let Err(e) = send_snapshot_to(&transport, pkt.from, board_id, &entries).await {
                    debug!("board {board_id}: SNAPSHOT to {} failed: {e}", pkt.from);
                }
            }
            TYPE_SNAPSHOT => {
                let snap: SnapshotPayload = match serde_json::from_value(env.payload) {
                    Ok(p) => p,
                    Err(e) => {
                        debug!("board {board_id}: bad SNAPSHOT: {e}");
                        continue;
                    }
                };
                let mut s = shared.lock().await;
                for entry in snap.entries {
                    if s.core.merge_entry(&entry) {
                        let _ = changes_tx.send(BoardChange {
                            key: entry.key,
                            value: entry.value,
                            deleted: entry.deleted,
                        });
                    }
                }
            }
            other => debug!("board {board_id}: unknown message type {other}"),
        }
    }
}

/// Owns the inbound Board route and demultiplexes per `board_id` into
/// per-endpoint tasks, like [`crate::stream::StreamHub`].
pub struct BoardHub {
    transport: Arc<Transport>,
    actor: String,
    senders: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<Inbound>>>>,
    /// Per-peer session-restart signal (SPEC §3.3): each opened endpoint
    /// subscribes so it can clear that peer's replay watermark.
    rebase_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    /// Demux task; aborted on drop so it releases its `Arc<Transport>`.
    task: tokio::task::JoinHandle<()>,
}

impl Drop for BoardHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl BoardHub {
    pub async fn new(
        transport: Arc<Transport>,
        actor: impl Into<String>,
        rebase_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    ) -> Self {
        let mut inbound = transport.route(ChannelType::Board).await;
        let senders: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<Inbound>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let senders_ref = Arc::clone(&senders);
        let task = tokio::spawn(async move {
            while let Some(pkt) = inbound.recv().await {
                let senders = senders_ref.lock().await;
                if let Some(tx) = senders.get(&pkt.header.channel_id) {
                    let _ = tx.send(pkt);
                } else {
                    debug!("no Board subscriber for board_id={}", pkt.header.channel_id);
                }
            }
        });
        Self {
            transport,
            actor: actor.into(),
            senders,
            rebase_tx,
            task,
        }
    }

    /// Open (or replace) the endpoint for `board_id`. The Band's actor string
    /// (its hostname) is the LWW tiebreak for this writer (§7.3).
    pub async fn open(&self, board_id: u16) -> BoardEndpoint {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().await.insert(board_id, tx);
        BoardEndpoint::new(
            Arc::clone(&self.transport),
            board_id,
            self.actor.clone(),
            rx,
            self.rebase_tx.subscribe(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_band_id, derive_key};
    use crate::framing::encode_packet;
    use serde_json::json;
    use std::time::Duration;

    // -- BoardCore (sans-io) — mirrors tests/test_board.py ------------------

    fn merge_all(dst: &mut BoardCore, payloads: &[SetPayload]) {
        for p in payloads {
            dst.merge_entry(p);
        }
    }

    #[test]
    fn set_replicates_and_get() {
        let mut a = BoardCore::new("alice");
        let mut b = BoardCore::new("bob");
        let p = a.set("cursor", json!({"x": 4, "y": 2}));
        b.merge_entry(&p);
        assert_eq!(b.get("cursor"), Some(&json!({"x": 4, "y": 2})));
        assert_eq!(b.items().len(), 1);
    }

    #[test]
    fn lww_merge_higher_lamport_wins() {
        let mut a = BoardCore::new("alice");
        let mut b = BoardCore::new("bob");
        let p1 = a.set("k", json!("from-alice"));
        b.merge_entry(&p1); // b merged lamport=1, so its next write is lamport=2
        let p2 = b.set("k", json!("from-bob"));
        a.merge_entry(&p2);
        assert_eq!(a.get("k"), Some(&json!("from-bob")));
        assert_eq!(b.get("k"), Some(&json!("from-bob")));
    }

    #[test]
    fn equal_lamport_actor_tiebreak() {
        // Concurrent writes with equal clocks: higher actor string wins everywhere.
        let mut a = BoardCore::new("alice");
        let mut b = BoardCore::new("bob");
        let pa = a.set("k", json!("alice-val")); // (1, "alice")
        let pb = b.set("k", json!("bob-val")); // (1, "bob") -> "bob" > "alice"
        b.merge_entry(&pa);
        a.merge_entry(&pb);
        assert_eq!(a.get("k"), Some(&json!("bob-val")));
        assert_eq!(b.get("k"), Some(&json!("bob-val")));
    }

    #[test]
    fn delete_tombstone_propagates() {
        let mut a = BoardCore::new("alice");
        let mut b = BoardCore::new("bob");
        let p1 = a.set("k", json!(1));
        let p2 = a.delete("k");
        merge_all(&mut b, &[p1, p2]);
        assert_eq!(b.get("k"), None);
        assert!(b.items().is_empty());
        // Tombstone must beat the older live entry even if replayed out of order.
        assert!(b.entry("k").unwrap().deleted);
    }

    #[test]
    fn stale_merge_is_ignored() {
        let mut a = BoardCore::new("alice");
        a.set("k", json!("new")); // lamport 1
        a.set("k", json!("newer")); // lamport 2
        let changed = a.merge_entry(&SetPayload {
            key: "k".into(),
            value: json!("old"),
            ts: (1, "zzz".into()),
            deleted: false,
        });
        assert!(!changed);
        assert_eq!(a.get("k"), Some(&json!("newer")));
    }

    #[test]
    fn digest_equal_iff_converged() {
        let mut a = BoardCore::new("alice");
        let mut b = BoardCore::new("bob");
        let p1 = a.set("x", json!(1));
        let p2 = a.set("y", json!(2));
        assert_ne!(a.digest(), b.digest());
        merge_all(&mut b, &[p1, p2]);
        assert_eq!(a.digest(), b.digest());
        let p3 = b.set("z", json!(3));
        assert_ne!(a.digest(), b.digest());
        a.merge_entry(&p3);
        assert_eq!(a.digest(), b.digest());
    }

    #[test]
    fn lamport_advances_past_merged_clock() {
        let mut a = BoardCore::new("alice");
        let mut b = BoardCore::new("bob");
        let mut payloads = Vec::new();
        for _ in 0..5 {
            payloads.push(a.set("k", json!("spin"))); // a's clock at 5
        }
        merge_all(&mut b, &payloads);
        let p = b.set("k", json!("bob-wins"));
        assert_eq!(b.entry("k").unwrap().lamport, 6, "must be lamport 6, not 1");
        a.merge_entry(&p);
        assert_eq!(a.get("k"), Some(&json!("bob-wins")));
    }

    /// Digest cross-compatibility with the Python implementation — the
    /// anti-entropy hinge (§7.4). Vectors: tests/vectors.json `board_digest`.
    #[test]
    fn board_digest_vectors_match_python() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors.json");
        let raw = std::fs::read_to_string(path).expect("read tests/vectors.json");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let cases = v["board_digest"]
            .as_array()
            .expect("vectors.json must have a board_digest section");
        assert!(cases.len() >= 2);
        for case in cases {
            let mut core = BoardCore::new("vector");
            for e in case["entries"].as_array().unwrap() {
                core.merge_entry(&SetPayload {
                    key: e["key"].as_str().unwrap().to_string(),
                    value: Value::Null,
                    ts: (
                        e["lamport"].as_u64().unwrap(),
                        e["actor"].as_str().unwrap().to_string(),
                    ),
                    deleted: e["deleted"].as_bool().unwrap(),
                });
            }
            let (count, hash) = core.digest();
            assert_eq!(count, case["count"].as_u64().unwrap());
            assert_eq!(hash, case["hash"].as_str().unwrap(), "digest diverged from Python");
        }
    }

    // -- endpoint layer over loopback UDP ------------------------------------

    // Hubs are returned (even though tests only use the endpoints) so their
    // rebase broadcast senders stay alive for the endpoint tasks.
    async fn endpoint_pair(
        psk: &[u8],
    ) -> (
        BoardEndpoint,
        BoardEndpoint,
        SocketAddr,
        SocketAddr,
        (BoardHub, BoardHub),
    ) {
        let key = derive_key(psk);
        let band_id = derive_band_id(psk);
        let ta = Arc::new(
            Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
                .await
                .unwrap(),
        );
        let tb = Arc::new(
            Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
                .await
                .unwrap(),
        );
        let a_addr = ta.local_addr().unwrap();
        let b_addr = tb.local_addr().unwrap();
        // Standalone hubs (no Band): a rebase channel nothing broadcasts on.
        let (rebase_a, _) = tokio::sync::broadcast::channel(8);
        let (rebase_b, _) = tokio::sync::broadcast::channel(8);
        let hub_a = BoardHub::new(Arc::clone(&ta), "alice", rebase_a).await;
        let hub_b = BoardHub::new(Arc::clone(&tb), "bob", rebase_b).await;
        ta.spawn_recv_loop();
        tb.spawn_recv_loop();
        let a = hub_a.open(3).await;
        let b = hub_b.open(3).await;
        (a, b, a_addr, b_addr, (hub_a, hub_b))
    }

    async fn wait_until<F, Fut>(mut cond: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        for _ in 0..100 {
            if cond().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not reached within timeout");
    }

    #[tokio::test]
    async fn endpoint_set_replicates_over_loopback() {
        let (a, mut b, _a_addr, b_addr, _hubs) = endpoint_pair(b"board-endpoint-psk").await;
        a.add_destination(b_addr).await;
        a.set("cursor", json!({"x": 4, "y": 2})).await.unwrap();
        let change = tokio::time::timeout(Duration::from_secs(1), b.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change.key, "cursor");
        assert!(!change.deleted);
        assert_eq!(b.get("cursor").await, Some(json!({"x": 4, "y": 2})));
        assert_eq!(b.items().await.len(), 1);
    }

    #[tokio::test]
    async fn digest_mismatch_triggers_sync_req_then_snapshot_converges() {
        // Anti-entropy round (§7.4): b never saw a's SET (no destination), so
        // a's DIGEST probe makes b answer SYNC_REQ and a reply with SNAPSHOT.
        let (a, b, _a_addr, b_addr, _hubs) = endpoint_pair(b"board-sync-psk").await;
        a.set("only-on-a", json!("v")).await.unwrap(); // no destinations: lossy
        assert_ne!(a.digest().await, b.digest().await);
        a.send_digest(Some(b_addr)).await.unwrap();
        wait_until(|| async { b.get("only-on-a").await == Some(json!("v")) }).await;
        assert_eq!(a.digest().await, b.digest().await);
    }

    #[tokio::test]
    async fn large_snapshot_fragments_and_reassembles() {
        let (a, b, _a_addr, b_addr, _hubs) = endpoint_pair(b"board-frag-psk").await;
        for i in 0..50 {
            a.set(&format!("key-{i}"), json!("v".repeat(100)))
                .await
                .unwrap(); // ~5KB of entries > one chunk
        }
        // The snapshot envelope must exceed one §6.6 chunk so this exercises
        // real fragmentation.
        let env = serde_json::to_vec(&BoardEnvelope {
            type_: TYPE_SNAPSHOT,
            payload: serde_json::json!({ "entries": a.shared.lock().await.core.snapshot() }),
        })
        .unwrap();
        assert!(env.len() > MAX_CHUNK_PAYLOAD, "snapshot must fragment");
        a.send_snapshot(b_addr).await.unwrap();
        let want = a.items().await;
        wait_until(|| {
            let want = want.clone();
            let b = &b;
            async move { b.items().await == want }
        })
        .await;
    }

    #[tokio::test]
    async fn replayed_set_is_dropped_by_watermark() {
        let psk = b"board-replay-psk";
        let (_a, mut b, _a_addr, b_addr, _hubs) = endpoint_pair(psk).await;
        // Forge one SET packet with a fixed sequence and deliver it twice.
        let env = serde_json::to_vec(&BoardEnvelope {
            type_: TYPE_SET,
            payload: json!({"key":"k","value":1,"ts":[1,"alice"],"deleted":false}),
        })
        .unwrap();
        let pkt = encode_packet(
            &derive_key(psk),
            &derive_band_id(psk),
            ChannelType::Board,
            3,
            42,
            &env,
        )
        .unwrap();
        let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.send_to(&pkt, b_addr).await.unwrap();
        sock.send_to(&pkt, b_addr).await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(1), b.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.key, "k");
        let replay = tokio::time::timeout(Duration::from_millis(300), b.changed()).await;
        assert!(replay.is_err(), "replayed SET must be dropped (SPEC §3.3)");
    }
}
