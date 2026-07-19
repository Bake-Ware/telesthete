//! Drop channel — chunked, resumable file distribution. SPEC §8.
//!
//! Receiver-driven: the receiver requests exactly the chunk ranges it lacks
//! (§8.2), so resume-after-restart is free and the sender stays stateless
//! beyond the file bytes. First frame byte is the type; OFFER/REQUEST/DONE
//! carry JSON after it, CHUNK is `index u32 BE || bytes`.
//!
//! Module named `drop_channel` because `drop` collides with the keyword; the
//! role types are exported as [`DropSender`] / [`DropReceiver`].

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::framing::ChannelType;
use crate::transport::{Inbound, Outbound, Transport, TransportError};

/// §8.2: the §6.4 frame-data budget.
pub const CHUNK_SIZE: usize = 1024;
/// §8.1: a range spans at most 64 chunks.
pub const MAX_RANGE_CHUNKS: u64 = 64;
/// §8.2 reference: re-request an idle range after 1 s.
pub const REREQUEST_TIMEOUT: Duration = Duration::from_secs(1);

/// §8.1 frame types (first byte).
pub const TYPE_OFFER: u8 = 0x01;
pub const TYPE_REQUEST: u8 = 0x02;
pub const TYPE_CHUNK: u8 = 0x03;
pub const TYPE_DONE: u8 = 0x04;

#[derive(Debug, Error)]
pub enum DropError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// §8.1 OFFER body.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct OfferPayload {
    pub name: String,
    pub size: u64,
    pub chunk_size: u64,
    pub total_chunks: u64,
    pub sha256: String,
}

/// §8.1 REQUEST body. Ranges are `[start, end)` chunk indexes; parsed as i64
/// so hostile negative bounds survive parsing and get clamped, not dropped.
#[derive(Serialize, Deserialize, Debug, Default)]
struct RequestPayload {
    #[serde(default)]
    ranges: Vec<(i64, i64)>,
}

/// §8.1 DONE body — the receiver's verdict digest.
#[derive(Serialize, Deserialize, Debug, Default)]
struct DonePayload {
    #[serde(default)]
    sha256: String,
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn json_frame<T: Serialize>(type_: u8, payload: &T) -> Vec<u8> {
    let body = serde_json::to_vec(payload).expect("drop payload serializes");
    let mut frame = Vec::with_capacity(1 + body.len());
    frame.push(type_);
    frame.extend_from_slice(&body);
    frame
}

/// Sans-io sender core: offers one file and serves whatever ranges receivers
/// request (§8.2). Frames returned by [`Self::handle_frame`] go back to the
/// requesting peer.
#[derive(Debug)]
pub struct DropSenderCore {
    pub name: String,
    pub data: Vec<u8>,
    pub total_chunks: u32,
    pub sha256: String,
    /// peer -> receiver's DONE verdict (true = digest matched).
    pub completed: HashMap<SocketAddr, bool>,
}

impl DropSenderCore {
    pub fn new(name: impl Into<String>, data: Vec<u8>) -> Self {
        let total_chunks = std::cmp::max(1, data.len().div_ceil(CHUNK_SIZE)) as u32;
        let sha256 = sha256_hex(&data);
        Self {
            name: name.into(),
            data,
            total_chunks,
            sha256,
            completed: HashMap::new(),
        }
    }

    /// The OFFER frame announcing this file (§8.2).
    pub fn offer_frame(&self) -> Vec<u8> {
        json_frame(
            TYPE_OFFER,
            &OfferPayload {
                name: self.name.clone(),
                size: self.data.len() as u64,
                chunk_size: CHUNK_SIZE as u64,
                total_chunks: self.total_chunks as u64,
                sha256: self.sha256.clone(),
            },
        )
    }

    /// Handle one inbound frame from `from`. Returns the frames to send back
    /// plus, on DONE, the receiver's verified verdict.
    pub fn handle_frame(&mut self, from: SocketAddr, frame: &[u8]) -> (Vec<Vec<u8>>, Option<bool>) {
        let Some((&type_, body)) = frame.split_first() else {
            return (Vec::new(), None);
        };
        match type_ {
            TYPE_REQUEST => {
                let req: RequestPayload = match serde_json::from_slice(body) {
                    Ok(r) => r,
                    Err(e) => {
                        debug!("drop sender: bad REQUEST from {from}: {e}");
                        return (Vec::new(), None);
                    }
                };
                let mut out = Vec::new();
                for (start, end) in req.ranges {
                    out.extend(self.serve_range(start, end));
                }
                (out, None)
            }
            TYPE_DONE => {
                let done: DonePayload = match serde_json::from_slice(body) {
                    Ok(d) => d,
                    Err(e) => {
                        debug!("drop sender: bad DONE from {from}: {e}");
                        return (Vec::new(), None);
                    }
                };
                let ok = done.sha256 == self.sha256;
                if !ok {
                    debug!("drop sender: digest mismatch from {from}");
                }
                self.completed.insert(from, ok);
                (Vec::new(), Some(ok))
            }
            _ => (Vec::new(), None),
        }
    }

    /// Serve one requested range as CHUNK frames, clamping hostile/buggy
    /// bounds to the file and the §8.1 window.
    fn serve_range(&self, start: i64, end: i64) -> Vec<Vec<u8>> {
        let start = start.max(0) as u64;
        let end = (end.max(0) as u64)
            .min(self.total_chunks as u64)
            .min(start + MAX_RANGE_CHUNKS);
        let mut out = Vec::new();
        for index in start..end {
            let lo = index as usize * CHUNK_SIZE;
            let hi = std::cmp::min(lo + CHUNK_SIZE, self.data.len());
            let chunk = if lo >= self.data.len() {
                &[][..]
            } else {
                &self.data[lo..hi]
            };
            let mut frame = Vec::with_capacity(5 + chunk.len());
            frame.push(TYPE_CHUNK);
            frame.extend_from_slice(&(index as u32).to_be_bytes());
            frame.extend_from_slice(chunk);
            out.push(frame);
        }
        out
    }
}

/// Sans-io receiver core: pulls an offered file chunk-window by chunk-window,
/// verifying at the end (§8.2). `have` may be pre-seeded with persisted chunks
/// to resume. Frames returned by handlers go to [`Self::sender_addr`].
#[derive(Debug)]
pub struct DropReceiverCore {
    pub offer: Option<OfferPayload>,
    pub have: HashMap<u32, Vec<u8>>,
    pub sender_addr: Option<SocketAddr>,
    /// `None` until complete, then the sha256 verdict.
    pub verified: Option<bool>,
    /// One outstanding window (§8.2).
    outstanding: Option<(u32, u32)>,
    last_progress: Option<Instant>,
    rerequest_timeout: Duration,
    completion: Option<(Vec<u8>, bool)>,
}

impl DropReceiverCore {
    pub fn new(have: HashMap<u32, Vec<u8>>) -> Self {
        Self {
            offer: None,
            have,
            sender_addr: None,
            verified: None,
            outstanding: None,
            last_progress: None,
            rerequest_timeout: REREQUEST_TIMEOUT,
            completion: None,
        }
    }

    /// Contiguous `[start, end)` runs of chunks we lack, split to §8.1 size.
    pub fn missing_ranges(&self) -> Vec<(u32, u32)> {
        let total = self.offer.as_ref().map_or(0, |o| o.total_chunks as u32);
        let mut out = Vec::new();
        let mut run_start: Option<u32> = None;
        for i in 0..=total {
            let lacking = i < total && !self.have.contains_key(&i);
            if lacking && run_start.is_none() {
                run_start = Some(i);
            } else if !lacking {
                if let Some(rs) = run_start.take() {
                    let mut s = rs;
                    while s < i {
                        out.push((s, std::cmp::min(s + MAX_RANGE_CHUNKS as u32, i)));
                        s += MAX_RANGE_CHUNKS as u32;
                    }
                }
            }
        }
        out
    }

    /// Ask for the next missing window (one outstanding window, §8.2).
    pub fn request_missing(&mut self) -> Vec<Vec<u8>> {
        if self.offer.is_none() || self.sender_addr.is_none() || self.verified.is_some() {
            return Vec::new();
        }
        match self.missing_ranges().first() {
            Some(&range) => {
                self.outstanding = Some(range);
                vec![json_frame(
                    TYPE_REQUEST,
                    &RequestPayload {
                        ranges: vec![(range.0 as i64, range.1 as i64)],
                    },
                )]
            }
            None => Vec::new(),
        }
    }

    /// Drive re-requests: if nothing arrived for the re-request timeout, ask
    /// again (§8.2 lost-CHUNK recovery; the sender keeps no state).
    pub fn tick(&mut self, now: Instant) -> Vec<Vec<u8>> {
        if self.offer.is_some()
            && self.verified.is_none()
            && self
                .last_progress
                .map_or(true, |t| now.duration_since(t) > self.rerequest_timeout)
        {
            self.last_progress = Some(now);
            self.request_missing()
        } else {
            Vec::new()
        }
    }

    /// Handle one inbound frame. Returned frames go to [`Self::sender_addr`].
    pub fn handle_frame(&mut self, from: SocketAddr, frame: &[u8], now: Instant) -> Vec<Vec<u8>> {
        let Some((&type_, body)) = frame.split_first() else {
            return Vec::new();
        };
        match type_ {
            TYPE_OFFER => {
                let offer: OfferPayload = match serde_json::from_slice(body) {
                    Ok(o) => o,
                    Err(e) => {
                        debug!("drop receiver: bad OFFER from {from}: {e}");
                        return Vec::new();
                    }
                };
                if let Some(prev) = &self.offer {
                    if offer.sha256 != prev.sha256 || offer.size != prev.size {
                        // A different file on the same drop_id: start over
                        // (§8.2 resume only applies to a recognized
                        // (name, size, sha256)).
                        self.have.clear();
                        self.verified = None;
                    }
                }
                self.offer = Some(offer);
                self.sender_addr = Some(from);
                self.last_progress = Some(now);
                self.request_missing()
            }
            TYPE_CHUNK => {
                let Some(offer) = self.offer.as_ref() else {
                    return Vec::new();
                };
                if body.len() < 4 {
                    return Vec::new();
                }
                let index = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
                let total = offer.total_chunks as u32;
                if index >= total {
                    return Vec::new();
                }
                if let std::collections::hash_map::Entry::Vacant(slot) = self.have.entry(index) {
                    slot.insert(body[4..].to_vec());
                    self.last_progress = Some(now);
                }
                if self.have.len() as u32 == total {
                    self.finish()
                } else if self
                    .outstanding
                    .is_some_and(|(s, e)| (s..e).all(|i| self.have.contains_key(&i)))
                {
                    self.request_missing() // window drained -> pull the next
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Take the completion event `(data, sha_ok)` once every chunk arrived.
    pub fn take_completion(&mut self) -> Option<(Vec<u8>, bool)> {
        self.completion.take()
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        let (total, size, want) = {
            let offer = self.offer.as_ref().expect("finish requires an offer");
            (
                offer.total_chunks as u32,
                offer.size as usize,
                offer.sha256.clone(),
            )
        };
        let mut data = Vec::with_capacity(size);
        for i in 0..total {
            data.extend_from_slice(&self.have[&i]);
        }
        data.truncate(size);
        let hex = sha256_hex(&data);
        let ok = hex == want;
        self.verified = Some(ok);
        let frame = json_frame(TYPE_DONE, &DonePayload { sha256: hex });
        self.completion = Some((data, ok));
        vec![frame]
    }
}

/// Replay watermark per §3.3: accept-first, then strictly increasing outer
/// sequence per peer.
fn replay_stale(watermark: &mut HashMap<SocketAddr, u64>, from: SocketAddr, seq: u64) -> bool {
    if matches!(watermark.get(&from), Some(&wm) if seq <= wm) {
        return true;
    }
    watermark.insert(from, seq);
    false
}

async fn send_drop_raw(
    transport: &Transport,
    dest: SocketAddr,
    drop_id: u16,
    plaintext: Vec<u8>,
) -> Result<(), TransportError> {
    transport
        .send(Outbound {
            to: dest,
            channel_type: ChannelType::Drop,
            channel_id: drop_id,
            plaintext,
            priority: 4, // §9.1: Drop yields to everything
            use_base_key: false,
        })
        .await
}

/// Sending side of one drop over a [`Transport`]: offers the file and serves
/// requested ranges in a background task.
pub struct DropSender {
    transport: Arc<Transport>,
    drop_id: u16,
    core: Arc<Mutex<DropSenderCore>>,
    verdicts: mpsc::UnboundedReceiver<(SocketAddr, bool)>,
    task: JoinHandle<()>,
}

impl Drop for DropSender {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DropSender {
    fn new(
        transport: Arc<Transport>,
        drop_id: u16,
        name: String,
        data: Vec<u8>,
        mut inbound: mpsc::UnboundedReceiver<Inbound>,
        mut rebase_rx: tokio::sync::broadcast::Receiver<SocketAddr>,
    ) -> Self {
        let core = Arc::new(Mutex::new(DropSenderCore::new(name, data)));
        let (verdict_tx, verdicts) = mpsc::unbounded_channel();
        let task_core = Arc::clone(&core);
        let task_transport = Arc::clone(&transport);
        let task = tokio::spawn(async move {
            let mut watermark: HashMap<SocketAddr, u64> = HashMap::new();
            loop {
                let pkt = tokio::select! {
                    pkt = inbound.recv() => match pkt {
                        Some(pkt) => pkt,
                        None => break,
                    },
                    // Peer restarted (SPEC §3.3): forget its watermark.
                    peer = rebase_rx.recv() => match peer {
                        Ok(addr) => {
                            watermark.remove(&addr);
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    },
                };
                if replay_stale(&mut watermark, pkt.from, pkt.header.sequence) {
                    continue;
                }
                let (frames, verdict) =
                    task_core.lock().await.handle_frame(pkt.from, &pkt.payload);
                for frame in frames {
                    if let Err(e) =
                        send_drop_raw(&task_transport, pkt.from, drop_id, frame).await
                    {
                        debug!("drop {drop_id}: send to {} failed: {e}", pkt.from);
                    }
                }
                if let Some(ok) = verdict {
                    let _ = verdict_tx.send((pkt.from, ok));
                }
            }
        });
        Self {
            transport,
            drop_id,
            core,
            verdicts,
            task,
        }
    }

    pub fn drop_id(&self) -> u16 {
        self.drop_id
    }

    /// Announce the file to `dest` (§8.2). Re-send with the same drop_id to
    /// let a resuming receiver re-request only what it lacks.
    pub async fn offer(&self, dest: SocketAddr) -> Result<(), DropError> {
        let frame = self.core.lock().await.offer_frame();
        send_drop_raw(&self.transport, dest, self.drop_id, frame).await?;
        Ok(())
    }

    /// Next receiver verdict: `(peer, sha_ok)` from its DONE (§8.2).
    pub async fn recv_verdict(&mut self) -> Option<(SocketAddr, bool)> {
        self.verdicts.recv().await
    }

    /// Snapshot of all verdicts received so far.
    pub async fn completed(&self) -> HashMap<SocketAddr, bool> {
        self.core.lock().await.completed.clone()
    }
}

/// Receiving side of one drop over a [`Transport`]: pulls windows, re-requests
/// losses on a timer, verifies, and reports DONE.
pub struct DropReceiver {
    core: Arc<Mutex<DropReceiverCore>>,
    done: mpsc::UnboundedReceiver<(Vec<u8>, bool)>,
    task: JoinHandle<()>,
}

impl Drop for DropReceiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl DropReceiver {
    fn new(
        transport: Arc<Transport>,
        drop_id: u16,
        have: HashMap<u32, Vec<u8>>,
        mut inbound: mpsc::UnboundedReceiver<Inbound>,
        mut rebase_rx: tokio::sync::broadcast::Receiver<SocketAddr>,
    ) -> Self {
        let core = Arc::new(Mutex::new(DropReceiverCore::new(have)));
        let (done_tx, done) = mpsc::unbounded_channel();
        let task_core = Arc::clone(&core);
        let task = tokio::spawn(async move {
            let mut watermark: HashMap<SocketAddr, u64> = HashMap::new();
            // Poll faster than the timeout so a stall is noticed promptly
            // (the core's own elapsed-time check gates actual re-requests).
            let mut ticker = tokio::time::interval(REREQUEST_TIMEOUT / 4);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                let frames = tokio::select! {
                    pkt = inbound.recv() => match pkt {
                        Some(pkt) => {
                            if replay_stale(&mut watermark, pkt.from, pkt.header.sequence) {
                                continue;
                            }
                            task_core
                                .lock()
                                .await
                                .handle_frame(pkt.from, &pkt.payload, Instant::now())
                        }
                        None => break,
                    },
                    _ = ticker.tick() => task_core.lock().await.tick(Instant::now()),
                    // Peer restarted (SPEC §3.3): forget its watermark.
                    peer = rebase_rx.recv() => match peer {
                        Ok(addr) => {
                            watermark.remove(&addr);
                            continue;
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    },
                };
                let (dest, completion) = {
                    let mut c = task_core.lock().await;
                    (c.sender_addr, c.take_completion())
                };
                if let Some(dest) = dest {
                    for frame in frames {
                        if let Err(e) = send_drop_raw(&transport, dest, drop_id, frame).await {
                            debug!("drop {drop_id}: send to {dest} failed: {e}");
                        }
                    }
                }
                if let Some(result) = completion {
                    let _ = done_tx.send(result);
                }
            }
        });
        Self { core, done, task }
    }

    /// Wait for the transfer to complete: `(data, sha_ok)`.
    pub async fn recv_file(&mut self) -> Option<(Vec<u8>, bool)> {
        self.done.recv().await
    }

    /// `None` until complete, then the sha256 verdict.
    pub async fn verified(&self) -> Option<bool> {
        self.core.lock().await.verified
    }

    /// Chunks held so far — persist these to resume after a restart.
    pub async fn chunks(&self) -> HashMap<u32, Vec<u8>> {
        self.core.lock().await.have.clone()
    }
}

/// Owns the inbound Drop route and demultiplexes per `drop_id` into sender /
/// receiver endpoints, like [`crate::stream::StreamHub`].
pub struct DropHub {
    transport: Arc<Transport>,
    senders: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<Inbound>>>>,
    /// Per-peer session-restart signal (SPEC §3.3): each opened endpoint
    /// subscribes so it can clear that peer's replay watermark.
    rebase_tx: tokio::sync::broadcast::Sender<SocketAddr>,
}

impl DropHub {
    pub async fn new(
        transport: Arc<Transport>,
        rebase_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    ) -> Self {
        let mut inbound = transport.route(ChannelType::Drop).await;
        let senders: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<Inbound>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let senders_ref = Arc::clone(&senders);
        tokio::spawn(async move {
            while let Some(pkt) = inbound.recv().await {
                let senders = senders_ref.lock().await;
                if let Some(tx) = senders.get(&pkt.header.channel_id) {
                    let _ = tx.send(pkt);
                } else {
                    debug!("no Drop subscriber for drop_id={}", pkt.header.channel_id);
                }
            }
        });
        Self {
            transport,
            senders,
            rebase_tx,
        }
    }

    /// Open the sending side of `drop_id` for one file.
    pub async fn open_sender(
        &self,
        drop_id: u16,
        name: impl Into<String>,
        data: Vec<u8>,
    ) -> DropSender {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().await.insert(drop_id, tx);
        DropSender::new(
            Arc::clone(&self.transport),
            drop_id,
            name.into(),
            data,
            rx,
            self.rebase_tx.subscribe(),
        )
    }

    /// Open the receiving side of `drop_id`. `have` may be pre-seeded with
    /// persisted chunks to resume (§8.2).
    pub async fn open_receiver(&self, drop_id: u16, have: HashMap<u32, Vec<u8>>) -> DropReceiver {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().await.insert(drop_id, tx);
        DropReceiver::new(
            Arc::clone(&self.transport),
            drop_id,
            have,
            rx,
            self.rebase_tx.subscribe(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saddr() -> SocketAddr {
        "127.0.0.1:1".parse().unwrap()
    }
    fn raddr() -> SocketAddr {
        "127.0.0.2:1".parse().unwrap()
    }

    /// Deterministic pseudo-random bytes (no rand dep in tests).
    fn test_bytes(n: usize, seed: u32) -> Vec<u8> {
        let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
        (0..n)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                (state >> 24) as u8
            })
            .collect()
    }

    /// Deliver frames both ways until both queues drain (mirrors the Python
    /// `_pump`). Returns the sender verdicts observed.
    fn pump(
        sender: &mut DropSenderCore,
        receiver: &mut DropReceiverCore,
        mut to_receiver: Vec<Vec<u8>>,
    ) -> Vec<bool> {
        let now = Instant::now();
        let mut verdicts = Vec::new();
        let mut to_sender: Vec<Vec<u8>> = Vec::new();
        for _ in 0..300 {
            if to_receiver.is_empty() && to_sender.is_empty() {
                break;
            }
            for frame in std::mem::take(&mut to_receiver) {
                to_sender.extend(receiver.handle_frame(saddr(), &frame, now));
            }
            for frame in std::mem::take(&mut to_sender) {
                let (frames, verdict) = sender.handle_frame(raddr(), &frame);
                to_receiver.extend(frames);
                if let Some(ok) = verdict {
                    verdicts.push(ok);
                }
            }
        }
        verdicts
    }

    #[test]
    fn small_file_transfers_and_verifies() {
        let data = b"hello drop".to_vec();
        let mut sender = DropSenderCore::new("blob.bin", data.clone());
        let mut receiver = DropReceiverCore::new(HashMap::new());
        let offer = sender.offer_frame();
        let verdicts = pump(&mut sender, &mut receiver, vec![offer]);
        assert_eq!(receiver.verified, Some(true));
        assert_eq!(receiver.take_completion(), Some((data, true)));
        assert!(sender.completed[&raddr()]);
        assert_eq!(verdicts, vec![true]);
    }

    #[test]
    fn multi_window_transfer() {
        // > 64 chunks forces several REQUEST windows (§8.2 one-outstanding-window).
        let data = test_bytes(CHUNK_SIZE * (MAX_RANGE_CHUNKS as usize * 2 + 3) + 17, 1);
        let mut sender = DropSenderCore::new("blob.bin", data.clone());
        let mut receiver = DropReceiverCore::new(HashMap::new());
        let offer = sender.offer_frame();
        pump(&mut sender, &mut receiver, vec![offer]);
        assert_eq!(receiver.verified, Some(true));
        let mut got = Vec::new();
        for i in 0..sender.total_chunks {
            got.extend_from_slice(&receiver.have[&i]);
        }
        got.truncate(data.len());
        assert_eq!(sha256_hex(&got), sha256_hex(&data));
    }

    #[test]
    fn resume_requests_only_missing_chunks() {
        let data = test_bytes(CHUNK_SIZE * 10, 2);
        let mut have = HashMap::new();
        for i in [0u32, 1, 2, 5, 9] {
            let lo = i as usize * CHUNK_SIZE;
            have.insert(i, data[lo..lo + CHUNK_SIZE].to_vec()); // persisted run
        }
        let mut sender = DropSenderCore::new("blob.bin", data);
        let mut receiver = DropReceiverCore::new(have);
        let offer = sender.offer_frame();
        pump(&mut sender, &mut receiver, vec![offer]);
        assert_eq!(receiver.verified, Some(true));
        assert!(receiver.missing_ranges().is_empty());
    }

    #[test]
    fn lost_chunks_rerequested_on_tick() {
        let data = test_bytes(CHUNK_SIZE * 4, 3);
        let mut sender = DropSenderCore::new("blob.bin", data);
        let mut receiver = DropReceiverCore::new(HashMap::new());
        let now = Instant::now();
        // Deliver OFFER, then LOSE the sender's chunk replies entirely.
        let request = receiver.handle_frame(saddr(), &sender.offer_frame(), now);
        for frame in request {
            let _lost = sender.handle_frame(raddr(), &frame); // chunks dropped in flight
        }
        assert_eq!(receiver.verified, None);

        // tick() past the timeout re-requests; this time let everything through.
        let rerequest = receiver.tick(now + Duration::from_secs(2));
        assert!(!rerequest.is_empty(), "stalled receiver must re-request");
        let mut to_receiver = Vec::new();
        for frame in rerequest {
            let (frames, _) = sender.handle_frame(raddr(), &frame);
            to_receiver.extend(frames);
        }
        pump(&mut sender, &mut receiver, to_receiver);
        assert_eq!(receiver.verified, Some(true));
    }

    #[test]
    fn corrupted_transfer_reports_mismatch() {
        let data = test_bytes(CHUNK_SIZE * 2, 4);
        let mut sender = DropSenderCore::new("blob.bin", data.clone());
        let mut receiver = DropReceiverCore::new(HashMap::new());
        // Deliver offer + request, then corrupt the sender's data before serving.
        let request = receiver.handle_frame(saddr(), &sender.offer_frame(), Instant::now());
        sender.data = test_bytes(data.len(), 999); // sender's bytes changed mid-flight
        let mut to_receiver = Vec::new();
        for frame in request {
            let (frames, _) = sender.handle_frame(raddr(), &frame);
            to_receiver.extend(frames);
        }
        let verdicts = pump(&mut sender, &mut receiver, to_receiver);
        assert_eq!(receiver.verified, Some(false));
        assert_eq!(verdicts, vec![false]);
        assert!(!sender.completed[&raddr()]);
    }

    #[test]
    fn hostile_request_is_clamped() {
        let data = test_bytes(CHUNK_SIZE * 3, 5);
        let mut sender = DropSenderCore::new("blob.bin", data);
        // Absurd range: must serve at most total_chunks frames, not 10M.
        let forged = json_frame(
            TYPE_REQUEST,
            &RequestPayload {
                ranges: vec![(0, 10_000_000)],
            },
        );
        let (frames, _) = sender.handle_frame(raddr(), &forged);
        assert!(frames.len() <= sender.total_chunks as usize);
        // Negative start clamps to 0 rather than panicking or over-serving.
        let negative = json_frame(
            TYPE_REQUEST,
            &RequestPayload {
                ranges: vec![(-5, 2)],
            },
        );
        let (frames, _) = sender.handle_frame(raddr(), &negative);
        assert_eq!(frames.len(), 2);
    }

    #[test]
    fn empty_file() {
        let mut sender = DropSenderCore::new("empty.bin", Vec::new());
        let mut receiver = DropReceiverCore::new(HashMap::new());
        assert_eq!(sender.total_chunks, 1);
        let offer = sender.offer_frame();
        pump(&mut sender, &mut receiver, vec![offer]);
        assert_eq!(receiver.verified, Some(true));
        assert_eq!(receiver.take_completion(), Some((Vec::new(), true)));
    }
}
