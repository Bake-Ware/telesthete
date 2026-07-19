//! Channel — reliable, ordered byte streams over UDP. SPEC §6.
//!
//! A Channel is TCP-in-userspace: a 3-way SYN/SYN-ACK/ACK handshake (§6.3),
//! a sliding send window with retransmission (§6.4), out-of-order buffering
//! and dedup keyed by an inner per-(channel, direction) sequence (§6.1), and
//! message-level fragmentation via the §6.6 envelope. Every packet handed to
//! [`Transport::send`] draws a fresh outer sequence (the AEAD nonce, §3.3), so
//! a retransmission is just a re-send of the same `(flags, seq, data)` frame —
//! it never reuses a nonce.
//!
//! The wire framing ([`ChannelFrame`]) and the reliability state machine
//! ([`ChannelCore`]) are pure/sans-io so they unit-test without any sockets.
//! Each open connection is driven by one task (`run_connection`) that owns a
//! `ChannelCore`, shuttling frames in from the hub's `ChannelType::Channel`
//! route and commands in from the endpoint, mirroring stream.rs / control.rs.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Instant;
use tracing::debug;

use crate::framing::ChannelType;
use crate::transport::{Outbound, Transport, TransportError};
use crate::wire::fragment::{fragment, Reassembler, MAX_CHUNK_PAYLOAD};

/// §6.2 flag bits.
pub const FLAG_SYN: u8 = 0x01;
pub const FLAG_FIN: u8 = 0x02;
pub const FLAG_ACK: u8 = 0x04;
pub const FLAG_RST: u8 = 0x08;
/// Bits 4-7 are reserved and MUST be zero; frames with any set are dropped.
const RESERVED_MASK: u8 = 0xF0;

/// Fixed §6.1 header: flags(1) + ack_num(8) + window(2) + seq(8).
pub const CHANNEL_HEADER_LEN: usize = 19;

/// Default sliding-window size in frames (§6.4).
pub const DEFAULT_WINDOW: u16 = 32;
/// Default retransmit timeout (§6.4). Configurable for tests.
pub const DEFAULT_RTO: Duration = Duration::from_millis(500);
/// How long [`ChannelEndpoint::connect`] waits for ESTABLISHED before erroring.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("channel connect timed out")]
    ConnectTimeout,
    #[error("channel endpoint closed")]
    Closed,
}

/// Parsed §6.1 Channel plaintext payload. Serialize/parse are pure so they are
/// unit-testable without I/O and must stay byte-identical to the Python side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelFrame {
    pub flags: u8,
    pub ack_num: u64,
    pub window: u16,
    pub seq: u64,
    pub data: Vec<u8>,
}

impl ChannelFrame {
    /// `flags || ack_num(8 BE) || window(2 BE) || seq(8 BE) || data` (§6.1).
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(CHANNEL_HEADER_LEN + self.data.len());
        out.push(self.flags);
        out.extend_from_slice(&self.ack_num.to_be_bytes());
        out.extend_from_slice(&self.window.to_be_bytes());
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.data);
        out
    }

    /// Parse one frame. Returns `None` on a short frame or one with reserved
    /// bits set (§6.2) — such frames are dropped, not delivered.
    pub fn parse(buf: &[u8]) -> Option<ChannelFrame> {
        if buf.len() < CHANNEL_HEADER_LEN {
            return None;
        }
        let flags = buf[0];
        if flags & RESERVED_MASK != 0 {
            return None;
        }
        let ack_num = u64::from_be_bytes(buf[1..9].try_into().ok()?);
        let window = u16::from_be_bytes(buf[9..11].try_into().ok()?);
        let seq = u64::from_be_bytes(buf[11..19].try_into().ok()?);
        Some(ChannelFrame {
            flags,
            ack_num,
            window,
            seq,
            data: buf[CHANNEL_HEADER_LEN..].to_vec(),
        })
    }

    /// True if this frame consumes an inner seq on receipt: SYN, FIN, or any
    /// data-carrying frame (§6.1). Pure ACKs do not.
    fn consumes_seq(&self) -> bool {
        self.flags & FLAG_SYN != 0 || self.flags & FLAG_FIN != 0 || !self.data.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnState {
    Closed,
    SynSent,
    Established,
    FinSent,
}

/// One unacknowledged outgoing sequenced frame, kept for retransmission (§6.4).
/// Stored as `(flags, seq, data)` and re-serialized with the current
/// `ack_num`/`window` on each send — never re-sent under a stale outer
/// sequence (§6.1).
struct Unacked {
    flags: u8,
    seq: u64,
    data: Vec<u8>,
}

/// Sans-io reliability state machine for one Channel direction pair (§6.3-§6.5).
///
/// Drives CLOSED → SYN_SENT → ESTABLISHED → FIN_SENT → CLOSED for the
/// initiator and CLOSED → ESTABLISHED (on incoming SYN) → … for the responder.
/// All effects are collected in `outbox` (frames to encrypt+send) and
/// `delivered` (reassembled application messages) for the owning task to flush.
struct ChannelCore {
    state: ConnState,

    // Send side.
    send_next: u64,           // next inner seq to assign to a sequenced frame
    unacked: VecDeque<Unacked>, // in-flight sequenced frames, ascending by seq
    send_queue: VecDeque<Vec<u8>>, // fragment chunks awaiting window (§6.4: queue, never drop)
    peer_window: u16,         // peer's advertised window; cap on frames in flight
    closing: bool,            // a FIN is pending once the queue drains

    // Recv side.
    rcv_next: u64,                        // next in-order inner seq expected
    recv_buffer: BTreeMap<u64, ChannelFrame>, // out-of-order frames, keyed by seq
    reassembler: Reassembler,             // §6.6 message reassembly

    // Config.
    window: u16, // our advertised receive window
    rto: Duration,

    // Outputs, drained by the task.
    outbox: Vec<Vec<u8>>,
    delivered: Vec<Vec<u8>>,
    just_established: bool,
    peer_closed: bool,
}

impl ChannelCore {
    fn new(window: u16, rto: Duration) -> Self {
        Self {
            state: ConnState::Closed,
            send_next: 0,
            unacked: VecDeque::new(),
            send_queue: VecDeque::new(),
            peer_window: DEFAULT_WINDOW,
            closing: false,
            rcv_next: 0,
            recv_buffer: BTreeMap::new(),
            reassembler: Reassembler::new(),
            window,
            rto,
            outbox: Vec::new(),
            delivered: Vec::new(),
            just_established: false,
            peer_closed: false,
        }
    }

    fn has_unacked(&self) -> bool {
        !self.unacked.is_empty()
    }

    /// Build + queue a frame for sending, stamping the live ack_num/window.
    fn emit(&mut self, flags: u8, seq: u64, data: Vec<u8>) {
        let frame = ChannelFrame {
            flags,
            ack_num: self.rcv_next,
            window: self.window,
            seq,
            data,
        };
        self.outbox.push(frame.serialize());
    }

    /// Emit a sequenced frame and retain it for retransmission (§6.4).
    fn emit_reliable(&mut self, flags: u8, seq: u64, data: Vec<u8>) {
        self.unacked.push_back(Unacked {
            flags,
            seq,
            data: data.clone(),
        });
        self.emit(flags, seq, data);
    }

    /// Emit a pure ACK: carries `send_next` WITHOUT consuming it (§6.1), never
    /// retransmitted.
    fn emit_pure_ack(&mut self) {
        self.emit(FLAG_ACK, self.send_next, Vec::new());
    }

    /// Initiator: open the connection (§6.3). SYN consumes inner seq 0.
    fn connect(&mut self) {
        if self.state != ConnState::Closed {
            return;
        }
        self.state = ConnState::SynSent;
        let seq = self.send_next;
        self.send_next += 1;
        self.emit_reliable(FLAG_SYN, seq, Vec::new());
    }

    /// Fragment `msg` into §6.6 chunks and queue them as data frames. Sending
    /// waits for ESTABLISHED and for window room (`pump`).
    fn send_message(&mut self, msg: &[u8]) {
        let mut fid = [0u8; 16];
        getrandom::getrandom(&mut fid).expect("CSPRNG unavailable");
        for chunk in fragment(msg, MAX_CHUNK_PAYLOAD, &fid) {
            self.send_queue.push_back(chunk);
        }
        self.pump();
    }

    /// Begin an active close (§6.5): FIN is sent after any queued data drains.
    fn close(&mut self) {
        self.closing = true;
        self.pump();
    }

    /// Turn queued chunks into data frames while the peer's window allows, then
    /// send a pending FIN once the queue is empty. Only runs once ESTABLISHED.
    fn pump(&mut self) {
        if self.state != ConnState::Established {
            return;
        }
        let limit = self.peer_window.max(1) as usize;
        while !self.send_queue.is_empty() && self.unacked.len() < limit {
            let data = self.send_queue.pop_front().unwrap();
            let seq = self.send_next;
            self.send_next += 1;
            self.emit_reliable(FLAG_ACK, seq, data);
        }
        if self.closing && self.send_queue.is_empty() && self.unacked.len() < limit {
            let seq = self.send_next;
            self.send_next += 1;
            self.emit_reliable(FLAG_FIN | FLAG_ACK, seq, Vec::new());
            self.state = ConnState::FinSent;
            self.closing = false;
        }
    }

    /// Cumulative ACK (§6.1): drop every in-flight frame with `seq < ack_num`.
    fn ack(&mut self, ack_num: u64) {
        while let Some(front) = self.unacked.front() {
            if front.seq < ack_num {
                self.unacked.pop_front();
            } else {
                break;
            }
        }
    }

    /// Retransmit every in-flight frame (RTO fired, §6.4). Re-serialized so it
    /// carries a fresh outer sequence and the latest ack_num/window.
    fn on_timeout(&mut self) {
        let pending: Vec<(u8, u64, Vec<u8>)> = self
            .unacked
            .iter()
            .map(|u| (u.flags, u.seq, u.data.clone()))
            .collect();
        for (flags, seq, data) in pending {
            self.emit(flags, seq, data);
        }
    }

    /// Process one received frame through the state machine.
    fn on_frame(&mut self, f: ChannelFrame) {
        // Transport already authenticated the packet; the parser already
        // dropped reserved-bit frames. Cumulative ack + window apply in every
        // state.
        self.ack(f.ack_num);
        self.peer_window = f.window;

        if f.flags & FLAG_RST != 0 {
            self.state = ConnState::Closed;
            self.peer_closed = true;
            return;
        }

        match self.state {
            ConnState::Closed => {
                // Incoming SYN → become the responder (§6.5).
                if f.flags & FLAG_SYN != 0 {
                    self.rcv_next = f.seq + 1; // SYN consumes its seq (0)
                    self.state = ConnState::Established;
                    self.just_established = true;
                    let seq = self.send_next;
                    self.send_next += 1;
                    self.emit_reliable(FLAG_SYN | FLAG_ACK, seq, Vec::new());
                }
            }
            ConnState::SynSent => {
                if f.flags & FLAG_SYN != 0 && f.flags & FLAG_ACK != 0 {
                    // SYN+ACK: our SYN was just acked; finish the handshake.
                    self.rcv_next = f.seq + 1;
                    self.state = ConnState::Established;
                    self.just_established = true;
                    self.emit_pure_ack();
                    self.pump();
                } else if f.flags & FLAG_SYN != 0 {
                    // Simultaneous open: respond like the responder path.
                    self.rcv_next = f.seq + 1;
                    self.state = ConnState::Established;
                    self.just_established = true;
                    let seq = self.send_next;
                    self.send_next += 1;
                    self.emit_reliable(FLAG_SYN | FLAG_ACK, seq, Vec::new());
                }
            }
            ConnState::Established | ConnState::FinSent => {
                self.deliver(f);
                self.pump();
            }
        }
    }

    /// Reorder, dedup, and deliver a data/FIN frame; re-ack duplicates (§6.4).
    fn deliver(&mut self, f: ChannelFrame) {
        if !f.consumes_seq() {
            return; // pure ACK: nothing to deliver, ack already applied
        }
        if f.seq < self.rcv_next || self.recv_buffer.contains_key(&f.seq) {
            // Already delivered or already buffered — dedup, but re-ack so a
            // peer whose ACK was lost stops retransmitting (§6.4).
            self.emit_pure_ack();
            return;
        }
        self.recv_buffer.insert(f.seq, f);
        // Drain contiguously from rcv_next (§6.4 in-order reassembly).
        while let Some(fr) = self.recv_buffer.remove(&self.rcv_next) {
            self.rcv_next += 1;
            if fr.flags & FLAG_FIN != 0 {
                self.peer_closed = true;
                // Passive close (§6.5): acknowledge the peer's FIN with our own.
                if self.state == ConnState::Established {
                    let seq = self.send_next;
                    self.send_next += 1;
                    self.emit_reliable(FLAG_FIN | FLAG_ACK, seq, Vec::new());
                    self.state = ConnState::FinSent;
                }
            } else if !fr.data.is_empty() {
                if let Some(msg) = self.reassembler.feed(&fr.data) {
                    self.delivered.push(msg);
                }
            }
        }
        self.emit_pure_ack();
    }
}

/// Commands from a [`ChannelEndpoint`] to its connection task.
enum Command {
    Connect(oneshot::Sender<()>),
    Send(Vec<u8>),
    Close,
}

/// Everything one connection task needs: transport plumbing plus the
/// endpoint/hub channels it is wired to.
struct ConnTask {
    transport: Arc<Transport>,
    peer: SocketAddr,
    channel_id: u16,
    window: u16,
    rto: Duration,
    cmd_rx: mpsc::UnboundedReceiver<Command>,
    frame_rx: mpsc::UnboundedReceiver<ChannelFrame>,
    deliver_tx: mpsc::UnboundedSender<Vec<u8>>,
}

/// The per-(peer, channel_id) task: owns a [`ChannelCore`], its retransmit
/// timer, and the plumbing between the hub's inbound frames and the endpoint.
async fn run_connection(t: ConnTask) {
    let ConnTask {
        transport,
        peer,
        channel_id,
        window,
        rto,
        mut cmd_rx,
        mut frame_rx,
        deliver_tx,
    } = t;
    let mut core = ChannelCore::new(window, rto);
    let mut deliver_tx = Some(deliver_tx);
    let mut pending_connect: Option<oneshot::Sender<()>> = None;
    // Single retransmit timer, armed while frames are in flight (§6.4).
    let mut deadline: Option<Instant> = None;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(Command::Connect(resp)) => {
                    if core.state == ConnState::Established {
                        let _ = resp.send(());
                    } else {
                        pending_connect = Some(resp);
                        core.connect();
                    }
                }
                Some(Command::Send(data)) => core.send_message(&data),
                Some(Command::Close) => core.close(),
                None => break, // endpoint dropped
            },
            frame = frame_rx.recv() => match frame {
                Some(f) => core.on_frame(f),
                None => break, // hub gone
            },
            _ = sleep_until_opt(deadline), if deadline.is_some() => {
                core.on_timeout();
                deadline = None; // re-armed below
            }
        }

        for bytes in core.outbox.drain(..) {
            if let Err(e) = transport
                .send(Outbound {
                    to: peer,
                    channel_type: ChannelType::Channel,
                    channel_id,
                    plaintext: bytes,
                    priority: 4,
                    use_base_key: false,
                })
                .await
            {
                debug!("channel 0x{channel_id:04x} send to {peer} failed: {e}");
            }
        }

        if let Some(tx) = deliver_tx.as_ref() {
            for msg in core.delivered.drain(..) {
                if tx.send(msg).is_err() {
                    // Receiver dropped; stop trying to deliver.
                    deliver_tx = None;
                    break;
                }
            }
        } else {
            core.delivered.clear();
        }

        if core.just_established {
            core.just_established = false;
            if let Some(resp) = pending_connect.take() {
                let _ = resp.send(());
            }
        }

        // Signal EOF to the endpoint once the peer has closed (§6.5): dropping
        // the sender turns the next `recv_message` into `None`.
        if core.peer_closed {
            deliver_tx = None;
        }

        deadline = match (core.has_unacked(), deadline) {
            (true, Some(d)) => Some(d),           // keep the oldest frame's timer
            (true, None) => Some(Instant::now() + core.rto),
            (false, _) => None,
        };
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Inbound Channel message — payload + channel_id + sender. Retained for API
/// compatibility with callers that want the peer/channel alongside the bytes.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub from: SocketAddr,
    pub channel_id: u16,
    pub data: Vec<u8>,
}

/// A reliable Channel endpoint (§6). Owns the command/deliver plumbing to a
/// per-connection task; cheap fields (peer/channel_id) mirror the old shape.
pub struct ChannelEndpoint {
    transport: Arc<Transport>,
    peer: SocketAddr,
    channel_id: u16,
    cmd_tx: mpsc::UnboundedSender<Command>,
    deliver_rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl ChannelEndpoint {
    /// Open the connection: send SYN and wait for ESTABLISHED (§6.3), erroring
    /// after [`CONNECT_TIMEOUT`]. The responder side auto-establishes on the
    /// incoming SYN and need not call this.
    pub async fn connect(&self) -> Result<(), ChannelError> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Connect(tx))
            .map_err(|_| ChannelError::Closed)?;
        match tokio::time::timeout(CONNECT_TIMEOUT, rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(ChannelError::Closed),
            Err(_) => Err(ChannelError::ConnectTimeout),
        }
    }

    /// Send a message reliably, fragmenting per §6.6 as needed.
    pub async fn send_message(&self, data: &[u8]) -> Result<(), ChannelError> {
        self.cmd_tx
            .send(Command::Send(data.to_vec()))
            .map_err(|_| ChannelError::Closed)
    }

    /// Receive the next fully-reassembled message, or `None` once the channel
    /// is closed (peer FIN or endpoint torn down).
    pub async fn recv_message(&mut self) -> Option<Vec<u8>> {
        self.deliver_rx.recv().await
    }

    /// Alias for [`send_message`](Self::send_message) — the reliable send path.
    pub async fn send(&self, data: &[u8]) -> Result<(), ChannelError> {
        self.send_message(data).await
    }

    /// Receive the next message wrapped with its peer/channel_id.
    pub async fn recv(&mut self) -> Option<ChannelMessage> {
        self.recv_message().await.map(|data| ChannelMessage {
            from: self.peer,
            channel_id: self.channel_id,
            data,
        })
    }

    /// Begin an orderly close (§6.5): flush queued data, then FIN.
    pub async fn close(&self) -> Result<(), ChannelError> {
        self.cmd_tx
            .send(Command::Close)
            .map_err(|_| ChannelError::Closed)
    }

    /// A `Send + Sync + Clone` low-level sender for this channel. Bypasses the
    /// reliability state machine (raw fire-and-forget frames); useful for
    /// cockpit-side input dispatch where multiple tasks share one channel.
    pub fn sender(&self) -> ChannelSender {
        ChannelSender {
            transport: Arc::clone(&self.transport),
            peer: self.peer,
            channel_id: self.channel_id,
        }
    }
}

/// Low-level, fire-and-forget sender counterpart to [`ChannelEndpoint`]. Cheap
/// to clone. Sends raw bytes as the Channel plaintext with no §6 reliability.
#[derive(Clone)]
pub struct ChannelSender {
    transport: Arc<Transport>,
    peer: SocketAddr,
    channel_id: u16,
}

impl ChannelSender {
    pub async fn send(&self, data: &[u8]) -> Result<(), ChannelError> {
        self.transport
            .send(Outbound {
                to: self.peer,
                channel_type: ChannelType::Channel,
                channel_id: self.channel_id,
                plaintext: data.to_vec(),
                priority: 4,
                use_base_key: false,
            })
            .await?;
        Ok(())
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }
}

type Routes = Arc<Mutex<HashMap<(SocketAddr, u16), mpsc::UnboundedSender<ChannelFrame>>>>;

/// Demultiplexer that owns the inbound `ChannelType::Channel` route and fans
/// frames out to per-(peer, channel_id) connection tasks.
pub struct ChannelHub {
    transport: Arc<Transport>,
    routes: Routes,
    window: u16,
    rto: Duration,
}

impl ChannelHub {
    pub async fn new(transport: Arc<Transport>) -> Self {
        Self::with_options(transport, DEFAULT_WINDOW, DEFAULT_RTO).await
    }

    /// Build a hub with a custom window / RTO (tests use a short RTO).
    pub async fn with_options(transport: Arc<Transport>, window: u16, rto: Duration) -> Self {
        let inbound = transport.route(ChannelType::Channel).await;
        let routes: Routes = Arc::new(Mutex::new(HashMap::new()));

        let routes_ref = Arc::clone(&routes);
        tokio::spawn(async move {
            let mut rx = inbound;
            while let Some(pkt) = rx.recv().await {
                let Some(frame) = ChannelFrame::parse(&pkt.payload) else {
                    debug!(
                        "dropping malformed Channel frame from {} (channel_id=0x{:04x})",
                        pkt.from, pkt.header.channel_id
                    );
                    continue;
                };
                let key = (pkt.from, pkt.header.channel_id);
                let routes = routes_ref.lock().await;
                if let Some(tx) = routes.get(&key) {
                    if tx.send(frame).is_err() {
                        debug!("channel task for {key:?} gone");
                    }
                } else {
                    debug!(
                        "no Channel connection for {} channel_id=0x{:04x}",
                        pkt.from, pkt.header.channel_id
                    );
                }
            }
        });

        Self {
            transport,
            routes,
            window,
            rto,
        }
    }

    /// Open (or re-open) a Channel to `peer` on `channel_id`. Spawns the
    /// connection task and returns its endpoint. Call
    /// [`ChannelEndpoint::connect`] on the initiating side.
    pub async fn open(&self, peer: SocketAddr, channel_id: u16) -> ChannelEndpoint {
        let (frame_tx, frame_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (deliver_tx, deliver_rx) = mpsc::unbounded_channel();

        self.routes.lock().await.insert((peer, channel_id), frame_tx);

        tokio::spawn(run_connection(ConnTask {
            transport: Arc::clone(&self.transport),
            peer,
            channel_id,
            window: self.window,
            rto: self.rto,
            cmd_rx,
            frame_rx,
            deliver_tx,
        }));

        ChannelEndpoint {
            transport: Arc::clone(&self.transport),
            peer,
            channel_id,
            cmd_tx,
            deliver_rx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn frame_round_trip() {
        let f = ChannelFrame {
            flags: FLAG_ACK,
            ack_num: 42,
            window: 32,
            seq: 7,
            data: b"payload".to_vec(),
        };
        let bytes = f.serialize();
        assert_eq!(bytes.len(), CHANNEL_HEADER_LEN + 7);
        assert_eq!(ChannelFrame::parse(&bytes).unwrap(), f);
    }

    #[test]
    fn parse_rejects_reserved_bits_and_short() {
        // Any reserved bit (4-7) set → dropped (§6.2).
        let mut f = ChannelFrame {
            flags: FLAG_ACK | 0x10,
            ack_num: 1,
            window: 32,
            seq: 0,
            data: vec![],
        };
        assert!(ChannelFrame::parse(&f.serialize()).is_none());
        f.flags = 0x80;
        assert!(ChannelFrame::parse(&f.serialize()).is_none());
        // Short buffer → dropped.
        assert!(ChannelFrame::parse(&[0u8; 5]).is_none());
    }

    #[test]
    fn hardcoded_vector() {
        // Standalone parity anchor for the Python side (§6.1).
        let f = ChannelFrame {
            flags: 4,
            ack_num: 1,
            window: 32,
            seq: 5,
            data: b"hello".to_vec(),
        };
        let expect = "04000000000000000100200000000000000005".to_string() + &to_hex(b"hello");
        assert_eq!(to_hex(&f.serialize()), expect);
        assert_eq!(ChannelFrame::parse(&from_hex(&expect)).unwrap(), f);
    }

    /// Byte-identical with the Python reference: see tests/vectors.json
    /// `channel_frame`. Optional section — return if absent, like
    /// wire/fragment.rs::conformance_vectors_match_python.
    #[test]
    fn conformance_vectors_match_python() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors.json");
        let raw = std::fs::read_to_string(path).expect("read tests/vectors.json");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let Some(cases) = v.get("channel_frame").and_then(|f| f.as_array()) else {
            return; // section optional
        };
        for case in cases {
            let f = ChannelFrame {
                flags: case["flags"].as_u64().unwrap() as u8,
                ack_num: case["ack_num"].as_u64().unwrap(),
                window: case["window"].as_u64().unwrap() as u16,
                seq: case["seq"].as_u64().unwrap(),
                data: from_hex(case["data_hex"].as_str().unwrap()),
            };
            let want = case["packed_hex"].as_str().unwrap();
            assert_eq!(to_hex(&f.serialize()), want, "channel_frame bytes diverged");
            assert_eq!(ChannelFrame::parse(&from_hex(want)).unwrap(), f);
        }
    }

    // ---- state-machine (sans-io) tests ----

    fn core() -> ChannelCore {
        ChannelCore::new(DEFAULT_WINDOW, DEFAULT_RTO)
    }

    /// Move every queued frame from `src` into `dst` via serialize→parse→
    /// on_frame, exercising the wire codec. Returns how many frames moved.
    fn shuttle(src: &mut ChannelCore, dst: &mut ChannelCore) -> usize {
        let frames: Vec<Vec<u8>> = src.outbox.drain(..).collect();
        let n = frames.len();
        for bytes in frames {
            let f = ChannelFrame::parse(&bytes).expect("valid frame");
            dst.on_frame(f);
        }
        n
    }

    /// Drive a full handshake between a fresh initiator and responder.
    fn handshake() -> (ChannelCore, ChannelCore) {
        let mut init = core();
        let mut resp = core();
        init.connect(); // SYN
        shuttle(&mut init, &mut resp); // SYN → responder, emits SYN|ACK
        shuttle(&mut resp, &mut init); // SYN|ACK → initiator, emits pure ACK
        shuttle(&mut init, &mut resp); // pure ACK → responder
        assert_eq!(init.state, ConnState::Established);
        assert_eq!(resp.state, ConnState::Established);
        (init, resp)
    }

    #[test]
    fn three_way_handshake_seq_and_ack() {
        let mut init = core();
        let mut resp = core();

        init.connect();
        assert_eq!(init.state, ConnState::SynSent);
        // Initiator SYN: flags=0x01, seq=0.
        let syn = ChannelFrame::parse(&init.outbox[0]).unwrap();
        assert_eq!((syn.flags, syn.seq, syn.ack_num), (FLAG_SYN, 0, 0));
        shuttle(&mut init, &mut resp);

        // Responder SYN|ACK: flags=0x05, seq=0, ack=1.
        assert_eq!(resp.state, ConnState::Established);
        let synack = ChannelFrame::parse(&resp.outbox[0]).unwrap();
        assert_eq!(
            (synack.flags, synack.seq, synack.ack_num),
            (FLAG_SYN | FLAG_ACK, 0, 1)
        );
        shuttle(&mut resp, &mut init);
        assert_eq!(init.state, ConnState::Established);
        assert!(init.just_established);

        // Initiator pure ACK: flags=0x04, ack=1, carries seq=1 but unconsumed.
        let ack = ChannelFrame::parse(&init.outbox[0]).unwrap();
        assert_eq!((ack.flags, ack.ack_num), (FLAG_ACK, 1));
        shuttle(&mut init, &mut resp);
        assert!(resp.unacked.is_empty(), "SYN|ACK acked by final ACK");
    }

    #[test]
    fn reliable_in_order_delivery() {
        let (mut init, mut resp) = handshake();
        for i in 0..5u8 {
            init.send_message(&[i, i, i]);
        }
        // Deliver all data frames, plus the ACKs coming back.
        for _ in 0..4 {
            shuttle(&mut init, &mut resp);
            shuttle(&mut resp, &mut init);
        }
        assert_eq!(resp.delivered, (0..5u8).map(|i| vec![i, i, i]).collect::<Vec<_>>());
    }

    #[test]
    fn large_message_fragments_and_reassembles() {
        let (mut init, mut resp) = handshake();
        let msg: Vec<u8> = (0..6000u32).map(|i| (i % 251) as u8).collect();
        init.send_message(&msg);
        // > 1 chunk, so > 1 data frame in flight.
        assert!(init.unacked.len() > 1, "message must fragment");
        for _ in 0..4 {
            shuttle(&mut init, &mut resp);
            shuttle(&mut resp, &mut init);
        }
        assert_eq!(resp.delivered, vec![msg]);
    }

    #[test]
    fn retransmission_recovers_lost_frame() {
        let (mut init, mut resp) = handshake();
        init.send_message(b"needs-retransmit");
        // Simulate total loss of the first copy: drop the outbox.
        init.outbox.clear();
        assert!(init.has_unacked());
        // RTO fires → retransmit the same (flags, seq, data).
        init.on_timeout();
        shuttle(&mut init, &mut resp);
        assert_eq!(resp.delivered, vec![b"needs-retransmit".to_vec()]);
    }

    #[test]
    fn duplicate_frame_delivered_once() {
        let (mut init, mut resp) = handshake();
        init.send_message(b"dup");
        let data_frame = init.outbox[0].clone();
        let f = ChannelFrame::parse(&data_frame).unwrap();
        resp.on_frame(f.clone());
        resp.on_frame(f); // duplicate inner seq → dropped
        assert_eq!(resp.delivered, vec![b"dup".to_vec()]);
    }

    #[test]
    fn cumulative_ack_clears_send_buffer() {
        let (mut init, mut resp) = handshake();
        for i in 0..3u8 {
            init.send_message(&[i]);
        }
        assert_eq!(init.unacked.len(), 3);
        // Responder receives all + acks cumulatively.
        shuttle(&mut init, &mut resp);
        shuttle(&mut resp, &mut init);
        assert!(init.unacked.is_empty(), "cumulative ACK drains send buffer");
    }

    #[test]
    fn out_of_order_frames_reordered() {
        let (mut init, mut resp) = handshake();
        init.send_message(&[10]);
        init.send_message(&[20]);
        let f0 = ChannelFrame::parse(&init.outbox[0]).unwrap();
        let f1 = ChannelFrame::parse(&init.outbox[1]).unwrap();
        // Deliver seq+1 before seq: buffered, then released in order.
        resp.on_frame(f1);
        assert!(resp.delivered.is_empty());
        resp.on_frame(f0);
        assert_eq!(resp.delivered, vec![vec![10], vec![20]]);
    }

    #[test]
    fn close_sends_fin_and_marks_peer_closed() {
        let (mut init, mut resp) = handshake();
        init.close();
        assert_eq!(init.state, ConnState::FinSent);
        shuttle(&mut init, &mut resp);
        assert!(resp.peer_closed, "responder sees the FIN");
    }
}
