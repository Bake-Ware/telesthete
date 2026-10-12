//! The band registry: the transport-agnostic relay core (SPEC §10).
//!
//! Peers of every transport (UDP, WSS, WebTransport, AF_UNIX) live in one
//! registry keyed by `band_id`. The canonical unit of relay is a **whole
//! Telesthete frame** (`Arc<[u8]>`): each transport de-frames on ingress and
//! re-frames on egress, so the registry only ever moves complete frames and
//! never inspects the payload.
//!
//! A peer's egress is a [`Sink`] — an mpsc sender the transport drains. That
//! keeps [`Registry::forward`] fully synchronous (no I/O under lock): it
//! collects sinks under the lock, releases it, then `try_send`s. Bounded
//! channels give backpressure-by-drop so one slow peer can't grow memory
//! without limit (conformance D3). Every peer — UDP included — has its own
//! queue, so a burst to one peer can never crowd out another's traffic.
//!
//! **Addressed routing (SPEC §10.1).** The only cleartext the registry acts on
//! beyond `band_id` is the header's `channel_type`/`channel_id`: a ROUTE
//! (0x05) frame declares its sender's route id and is consumed here, never
//! relayed; a CHANNEL (0x02) frame whose nonzero `channel_id` some peer has
//! declared goes only to those peers. Everything else — and any CHANNEL frame
//! for an id nobody declared — is broadcast exactly as before, so bands with
//! no declaring peer see byte-for-byte the old behaviour.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use telesthete::{BandId, ChannelType};
use tokio::sync::mpsc;

use crate::frame::RouteInfo;

/// Caps and validation policy. `Copy` for cheap threading through config.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Maximum distinct bands held at once (conformance D1).
    pub max_bands: usize,
    /// Maximum peers per band (conformance D2).
    pub max_peers_per_band: usize,
    /// A UDP source must send this many packets before it becomes an eligible
    /// relay *destination*. `<= 1` disables the gate.
    ///
    /// NOTE: this is a mild robustness measure, **not** a return-routability
    /// proof. A determined spoofer can send N forged-source packets as cheaply
    /// as one, so it does not defend against deliberate reflection/amplification
    /// — that would require a cryptographic cookie the source echoes, which a
    /// blind relay (holding no key, injecting no plaintext) cannot issue. It
    /// only raises the bar against a single stray/one-off packet turning the hub
    /// into a reflector for that address.
    pub udp_validation_packets: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bands: 4096,
            max_peers_per_band: 256,
            udp_validation_packets: 2,
        }
    }
}

/// How to hand one whole frame to a peer. Both variants are non-blocking sends
/// into a bounded channel that the owning transport drains.
#[derive(Clone)]
pub enum Sink {
    /// A UDP peer: frames go to that peer's own bounded queue, drained by a
    /// per-peer writer task onto the shared socket. Per-peer (not one shared
    /// queue) so a large burst to one peer can't fill the queue and drop
    /// unrelated peers' frames. `addr` is kept for logging.
    Udp {
        tx: mpsc::Sender<Arc<[u8]>>,
        addr: SocketAddr,
    },
    /// A connection-oriented peer (WSS / WebTransport / AF_UNIX): frames go to
    /// that connection's own writer task, which applies transport framing.
    Conn(mpsc::Sender<Arc<[u8]>>),
}

impl Sink {
    fn try_send(&self, frame: Arc<[u8]>) -> Result<(), SendDrop> {
        match self {
            Sink::Udp { tx, .. } => tx.try_send(frame).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => SendDrop::Full,
                mpsc::error::TrySendError::Closed(_) => SendDrop::Closed,
            }),
            Sink::Conn(tx) => tx.try_send(frame).map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => SendDrop::Full,
                mpsc::error::TrySendError::Closed(_) => SendDrop::Closed,
            }),
        }
    }
}

enum SendDrop {
    Full,
    Closed,
}

/// Transport-tagged peer identity. A UDP `SocketAddr` and a connection id never
/// collide (conformance B5).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum PeerKey {
    Udp(SocketAddr),
    Conn(u64),
}

struct Peer {
    sink: Sink,
    last_seen: Instant,
    /// Eligible as a relay destination. Connection peers are eligible at
    /// registration (the handshake proves routability, D5); UDP peers become
    /// eligible after `udp_validation_packets` (D4).
    eligible: bool,
    /// Packets observed from this peer (drives UDP validation).
    seen: u32,
    /// Route id this peer declared with a ROUTE frame (SPEC §10.1), if any.
    /// One per peer; a later declaration replaces it, `channel_id` 0 clears
    /// it. Lives on the peer so eviction/disconnect forgets it for free.
    route: Option<u16>,
}

/// Why a peer could not be admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegError {
    /// The band cap (`max_bands`) is full and this would open a new band.
    BandCap,
    /// This band's peer cap (`max_peers_per_band`) is full.
    PeerCap,
}

/// The relay registry. Cheap to share behind an `Arc`.
pub struct Registry {
    bands: Mutex<HashMap<BandId, HashMap<PeerKey, Peer>>>,
    limits: Limits,
    next_conn: AtomicU64,
    /// Federation egress (SPEC §10 extension): when a link to another hub is
    /// active, locally-sourced frames are also handed here to be relayed across
    /// the link. `None` (the default, and the case for every hub with no
    /// `HUB_FED_*` config) means the relay hot path is byte-for-byte unchanged —
    /// federation adds one `Option` check and nothing else.
    fed: Mutex<Option<mpsc::Sender<(BandId, Arc<[u8]>)>>>,
}

impl Registry {
    pub fn new(limits: Limits) -> Self {
        Self {
            bands: Mutex::new(HashMap::new()),
            limits,
            next_conn: AtomicU64::new(1),
            fed: Mutex::new(None),
        }
    }

    /// Install the federation egress channel. Frames sourced from *local* peers
    /// are copied here after local fan-out; the federation task relays them to
    /// linked hubs that have the band. Idempotent; the last set wins.
    pub fn set_federation(&self, tx: mpsc::Sender<(BandId, Arc<[u8]>)>) {
        *self.fed.lock().unwrap() = Some(tx);
    }

    /// Band ids that currently have at least one **local** (non-link) peer.
    /// Advertised to linked hubs so they only relay frames we can actually
    /// deliver.
    pub fn local_bands(&self) -> Vec<BandId> {
        self.bands.lock().unwrap().keys().copied().collect()
    }

    /// Inject a frame that arrived over a federation link: fan out to **every**
    /// eligible local peer in the band and — crucially — do NOT re-federate it
    /// (one-hop rule; a hub never re-forwards a hub-sourced frame). This is the
    /// link-ingress counterpart to [`Registry::forward`].
    ///
    /// Addressed routing applies here too, against *local* declarations: a
    /// CHANNEL frame for a route id a local peer declared reaches only that
    /// peer. (Declarations themselves never cross a link.)
    pub fn inject_from_link(&self, band: &BandId, frame: Arc<[u8]>) {
        let Some(info) = crate::frame::route_info(&frame) else {
            return;
        };
        if info.channel_type == Some(ChannelType::Route) {
            return; // never deliverable; a well-behaved hub never sends one
        }
        let sinks: Vec<Sink> = {
            let bands = self.bands.lock().unwrap();
            match bands.get(band) {
                None => return,
                Some(peers) => Self::targets(peers, None, &info),
            }
        };
        for sink in sinks {
            let _ = sink.try_send(frame.clone());
        }
    }

    /// Allocate a process-unique id for a new connection-oriented peer.
    pub fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::Relaxed)
    }

    /// Register a new connection-oriented peer (WSS / WebTransport / AF_UNIX).
    /// Eligible as a destination immediately (D5).
    pub fn connect(&self, band: BandId, key: PeerKey, sink: Sink) -> Result<(), RegError> {
        let mut bands = self.bands.lock().unwrap();
        Self::insert(&mut bands, &self.limits, band, key, || sink, true)
    }

    /// Register-or-refresh a UDP peer from an inbound datagram. Tracks
    /// validation and flips the peer eligible once it has proven routability
    /// (D4). `make_sink` is called only on first sight, and only once the peer
    /// is admitted, so the transport can allocate the peer's own queue (and
    /// writer) lazily; refreshes keep the original sink.
    pub fn observe_udp(
        &self,
        band: BandId,
        addr: SocketAddr,
        make_sink: impl FnOnce() -> Sink,
    ) -> Result<(), RegError> {
        let key = PeerKey::Udp(addr);
        let mut bands = self.bands.lock().unwrap();
        if let Some(peer) = bands.get_mut(&band).and_then(|p| p.get_mut(&key)) {
            peer.last_seen = Instant::now();
            peer.seen = peer.seen.saturating_add(1);
            if !peer.eligible && peer.seen >= self.limits.udp_validation_packets {
                peer.eligible = true;
            }
            return Ok(());
        }
        Self::insert(&mut bands, &self.limits, band, key, make_sink, false)
    }

    /// Refresh an existing peer's liveness on an inbound frame (used by
    /// connection transports, whose sink is already registered).
    pub fn touch(&self, band: &BandId, key: &PeerKey) {
        let mut bands = self.bands.lock().unwrap();
        if let Some(peer) = bands.get_mut(band).and_then(|p| p.get_mut(key)) {
            peer.last_seen = Instant::now();
            peer.seen = peer.seen.saturating_add(1);
        }
    }

    fn insert(
        bands: &mut HashMap<BandId, HashMap<PeerKey, Peer>>,
        limits: &Limits,
        band: BandId,
        key: PeerKey,
        make_sink: impl FnOnce() -> Sink,
        conn: bool,
    ) -> Result<(), RegError> {
        let is_new_band = !bands.contains_key(&band);
        if is_new_band && bands.len() >= limits.max_bands {
            return Err(RegError::BandCap);
        }
        let peers = bands.entry(band).or_default();
        if peers.len() >= limits.max_peers_per_band {
            if is_new_band {
                bands.remove(&band); // don't leave an empty band we just made
            }
            return Err(RegError::PeerCap);
        }
        let eligible = conn || limits.udp_validation_packets <= 1;
        peers.insert(
            key,
            Peer {
                sink: make_sink(),
                last_seen: Instant::now(),
                eligible,
                seen: 1,
                route: None,
            },
        );
        Ok(())
    }

    /// The sinks a frame should reach: eligible peers other than `from`,
    /// narrowed to the declarers of `channel_id` when it is an addressed
    /// CHANNEL frame whose id at least one peer in the band declared (§10.1).
    ///
    /// "Declared" means by some peer *other than the sender*: if the sender is
    /// the only declarer, the addressee must be a peer that never declared
    /// (e.g. an old peer whose derived id collides), so we fall back to
    /// broadcast rather than drop it. A not-yet-eligible declarer does count,
    /// so its traffic isn't sprayed band-wide while it validates. Delivery
    /// still honours eligibility (D4) and never echoes (B1).
    fn targets(
        peers: &HashMap<PeerKey, Peer>,
        from: Option<&PeerKey>,
        info: &RouteInfo,
    ) -> Vec<Sink> {
        let addressed = info.channel_type == Some(ChannelType::Channel)
            && info.channel_id != 0
            && peers
                .iter()
                .any(|(k, p)| Some(k) != from && p.route == Some(info.channel_id));
        peers
            .iter()
            .filter(|(k, p)| Some(*k) != from && p.eligible)
            .filter(|(_, p)| !addressed || p.route == Some(info.channel_id))
            .map(|(_, p)| p.sink.clone())
            .collect()
    }

    /// Relay one whole frame to every *other* eligible peer in the band
    /// (conformance B1/B2/B4), or — for an addressed CHANNEL frame — only to
    /// the peers that declared its route id (K2). A ROUTE frame records the
    /// sender's route id and is consumed, never relayed or federated (K1).
    /// Synchronous: collects sinks under the lock, then dispatches without
    /// holding it. Frames to full queues are dropped and counted (D3); closed
    /// queues are ignored (the peer's disconnect path removes it).
    pub fn forward(&self, band: &BandId, from: &PeerKey, frame: Arc<[u8]>) {
        // Every transport validates the frame before calling us; re-parsing the
        // 19 cleartext bytes here keeps the transports routing-agnostic.
        let Some(info) = crate::frame::route_info(&frame) else {
            return;
        };
        let sinks: Vec<Sink> = {
            let mut bands = self.bands.lock().unwrap();
            let Some(peers) = bands.get_mut(band) else {
                return;
            };
            if info.channel_type == Some(ChannelType::Route) {
                if let Some(p) = peers.get_mut(from) {
                    p.route = (info.channel_id != 0).then_some(info.channel_id);
                }
                return;
            }
            Self::targets(peers, Some(from), &info)
        };
        for sink in sinks {
            match sink.try_send(frame.clone()) {
                Ok(()) => {}
                Err(SendDrop::Full) => {
                    tracing::warn!(band = %crate::frame::band_hex(band), "peer queue full; dropping frame");
                }
                Err(SendDrop::Closed) => {}
            }
        }
        // Federation tail: a locally-sourced frame is also offered to linked
        // hubs (the federation task filters to links that have this band). No-op
        // and near-free when no link is configured (`fed` is `None`).
        let fed = self.fed.lock().unwrap();
        if let Some(tx) = fed.as_ref() {
            let _ = tx.try_send((*band, frame));
        }
    }

    /// Remove a connection-oriented peer on disconnect (conformance C5). Reaps
    /// the band if it becomes empty.
    pub fn disconnect(&self, band: &BandId, key: &PeerKey) {
        let mut bands = self.bands.lock().unwrap();
        if let Some(peers) = bands.get_mut(band) {
            peers.remove(key);
            if peers.is_empty() {
                bands.remove(band);
            }
        }
    }

    /// Evict idle **UDP** peers and reap empty bands (conformance C1/C2).
    /// Returns `(peers_evicted, bands_remaining)`.
    ///
    /// Connection-oriented peers (WSS/WebTransport/AF_UNIX) are **never** evicted
    /// by idle TTL — their liveness is the transport connection itself, and they
    /// are removed explicitly by [`Registry::disconnect`] when it closes. TTL is
    /// a UDP notion (there is no connection to signal a UDP peer's departure);
    /// applying it to connections would silently drop a legitimate receive-only
    /// peer whose socket is still open, with no way for it to re-register.
    pub fn prune(&self, ttl: Duration) -> (usize, usize) {
        let now = Instant::now();
        let mut bands = self.bands.lock().unwrap();
        let mut evicted = 0usize;
        bands.retain(|_band, peers| {
            let before = peers.len();
            peers.retain(|k, p| match k {
                PeerKey::Conn(_) => true,
                PeerKey::Udp(_) => now.duration_since(p.last_seen) <= ttl,
            });
            evicted += before - peers.len();
            !peers.is_empty()
        });
        (evicted, bands.len())
    }

    // -- inspection (used by tests and metrics) ----------------------------

    pub fn band_count(&self) -> usize {
        self.bands.lock().unwrap().len()
    }

    /// Snapshot of the currently-active band ids (used by the AF_UNIX manager to
    /// reconcile its per-band sockets, §9.4).
    pub fn bands(&self) -> Vec<BandId> {
        self.bands.lock().unwrap().keys().copied().collect()
    }

    pub fn peer_count(&self, band: &BandId) -> usize {
        self.bands
            .lock()
            .unwrap()
            .get(band)
            .map_or(0, |p| p.len())
    }

    pub fn contains(&self, band: &BandId, key: &PeerKey) -> bool {
        self.bands
            .lock()
            .unwrap()
            .get(band)
            .is_some_and(|p| p.contains_key(key))
    }

    /// The route id a peer has declared (SPEC §10.1), if it exists and declared one.
    pub fn route_of(&self, band: &BandId, key: &PeerKey) -> Option<u16> {
        self.bands
            .lock()
            .unwrap()
            .get(band)
            .and_then(|p| p.get(key))
            .and_then(|peer| peer.route)
    }

    /// Whether a peer is currently an eligible relay destination, if it exists.
    pub fn is_eligible(&self, band: &BandId, key: &PeerKey) -> Option<bool> {
        self.bands
            .lock()
            .unwrap()
            .get(band)
            .and_then(|p| p.get(key))
            .map(|peer| peer.eligible)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn band(n: u8) -> BandId {
        [n; 16]
    }

    fn frame(b: BandId) -> Arc<[u8]> {
        let mut v = vec![0u8; 43];
        v[..16].copy_from_slice(&b);
        Arc::from(v.as_slice())
    }

    fn conn(cap: usize) -> (Sink, mpsc::Receiver<Arc<[u8]>>) {
        let (tx, rx) = mpsc::channel(cap);
        (Sink::Conn(tx), rx)
    }

    type UdpRx = mpsc::Receiver<Arc<[u8]>>;

    fn udp(cap: usize, addr: SocketAddr) -> (Sink, UdpRx) {
        let (tx, rx) = mpsc::channel(cap);
        (Sink::Udp { tx, addr }, rx)
    }

    fn big() -> Limits {
        Limits {
            max_bands: 1024,
            max_peers_per_band: 1024,
            udp_validation_packets: 2,
        }
    }

    #[tokio::test]
    async fn forward_excludes_sender() {
        // B1
        let r = Registry::new(big());
        let (s1, mut r1) = conn(8);
        let (s2, mut r2) = conn(8);
        let (k1, k2) = (PeerKey::Conn(1), PeerKey::Conn(2));
        r.connect(band(1), k1, s1).unwrap();
        r.connect(band(1), k2, s2).unwrap();
        r.forward(&band(1), &k1, frame(band(1)));
        assert!(r1.try_recv().is_err(), "sender must not receive its own frame");
        assert!(r2.try_recv().is_ok(), "other peer must receive");
    }

    #[tokio::test]
    async fn bands_are_isolated() {
        // B2
        let r = Registry::new(big());
        let (s1, mut r1) = conn(8);
        let (s2, mut r2) = conn(8);
        r.connect(band(1), PeerKey::Conn(1), s1).unwrap();
        r.connect(band(2), PeerKey::Conn(2), s2).unwrap();
        r.forward(&band(1), &PeerKey::Conn(99), frame(band(1)));
        assert!(r1.try_recv().is_ok());
        assert!(r2.try_recv().is_err(), "other band must not see traffic");
    }

    #[tokio::test]
    async fn relay_is_byte_exact() {
        // B4
        let r = Registry::new(big());
        let (s, mut rx) = conn(8);
        r.connect(band(1), PeerKey::Conn(1), s).unwrap();
        let mut raw = vec![7u8; 50];
        raw[..16].copy_from_slice(&band(1));
        let f: Arc<[u8]> = Arc::from(raw.as_slice());
        r.forward(&band(1), &PeerKey::Conn(2), f.clone());
        let got = rx.try_recv().unwrap();
        assert_eq!(&got[..], &f[..], "frame must be relayed byte-for-byte");
    }

    #[tokio::test]
    async fn mixed_transport_keys_distinct() {
        // B5 — a UDP peer and a conn peer coexist in one band, both receive.
        let r = Registry::new(big());
        let addr: SocketAddr = "127.0.0.1:6000".parse().unwrap();
        let (us, mut urx) = udp(8, addr);
        let (cs, mut crx) = conn(8);
        r.observe_udp(band(1), addr, || us).unwrap();
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap(); // 2nd packet -> eligible
        r.connect(band(1), PeerKey::Conn(1), cs).unwrap();
        assert_eq!(r.peer_count(&band(1)), 2);
        r.forward(&band(1), &PeerKey::Conn(99), frame(band(1)));
        assert!(urx.try_recv().is_ok());
        assert!(crx.try_recv().is_ok());
    }

    #[tokio::test]
    async fn conn_peer_eligible_immediately() {
        // D5
        let r = Registry::new(big());
        let (s, _rx) = conn(8);
        r.connect(band(1), PeerKey::Conn(1), s).unwrap();
        assert_eq!(r.is_eligible(&band(1), &PeerKey::Conn(1)), Some(true));
    }

    #[tokio::test]
    async fn udp_dest_requires_validation() {
        // D4 — one packet is not enough to become a destination.
        let r = Registry::new(big()); // udp_validation_packets = 2
        let addr: SocketAddr = "127.0.0.1:6001".parse().unwrap();
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        assert_eq!(r.is_eligible(&band(1), &PeerKey::Udp(addr)), Some(false));
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        assert_eq!(r.is_eligible(&band(1), &PeerKey::Udp(addr)), Some(true));
    }

    #[tokio::test]
    async fn udp_validation_disabled_when_one() {
        let r = Registry::new(Limits {
            udp_validation_packets: 1,
            ..big()
        });
        let addr: SocketAddr = "127.0.0.1:6002".parse().unwrap();
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        assert_eq!(r.is_eligible(&band(1), &PeerKey::Udp(addr)), Some(true));
    }

    #[tokio::test]
    async fn slow_peer_queue_bounded() {
        // D3 — a capacity-2 queue never buffers more than 2 frames.
        let r = Registry::new(big());
        let (s, mut rx) = conn(2);
        r.connect(band(1), PeerKey::Conn(1), s).unwrap();
        for _ in 0..5 {
            r.forward(&band(1), &PeerKey::Conn(99), frame(band(1)));
        }
        let mut got = 0;
        while rx.try_recv().is_ok() {
            got += 1;
        }
        assert!(got <= 2, "bounded queue must drop overflow, got {got}");
    }

    #[tokio::test]
    async fn band_cap_enforced() {
        // D1
        let r = Registry::new(Limits {
            max_bands: 1,
            ..big()
        });
        r.connect(band(1), PeerKey::Conn(1), conn(8).0).unwrap();
        assert_eq!(
            r.connect(band(2), PeerKey::Conn(2), conn(8).0),
            Err(RegError::BandCap)
        );
        assert_eq!(r.band_count(), 1);
    }

    #[tokio::test]
    async fn peer_cap_enforced() {
        // D2
        let r = Registry::new(Limits {
            max_peers_per_band: 1,
            ..big()
        });
        r.connect(band(1), PeerKey::Conn(1), conn(8).0).unwrap();
        assert_eq!(
            r.connect(band(1), PeerKey::Conn(2), conn(8).0),
            Err(RegError::PeerCap)
        );
        assert_eq!(r.peer_count(&band(1)), 1);
    }

    #[tokio::test]
    async fn send_registers_peer() {
        // B3 — sending registers the sender as a band peer (implicit discovery).
        let r = Registry::new(big());
        let addr: SocketAddr = "127.0.0.1:6003".parse().unwrap();
        assert!(!r.contains(&band(1), &PeerKey::Udp(addr)));
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        assert!(r.contains(&band(1), &PeerKey::Udp(addr)));
    }

    #[tokio::test]
    async fn prune_evicts_stale_udp_and_reaps() {
        // C1 + C2 — TTL eviction applies to UDP peers.
        let r = Registry::new(Limits {
            udp_validation_packets: 1,
            ..big()
        });
        let addr: SocketAddr = "127.0.0.1:6100".parse().unwrap();
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        assert_eq!(r.peer_count(&band(1)), 1);
        std::thread::sleep(Duration::from_millis(20));
        let (evicted, remaining) = r.prune(Duration::from_millis(5));
        assert_eq!(evicted, 1);
        assert_eq!(remaining, 0);
        assert_eq!(r.band_count(), 0); // empty band reaped
    }

    #[tokio::test]
    async fn active_udp_peer_survives_prune() {
        // C3 — recent activity keeps a UDP peer alive across a sweep.
        let r = Registry::new(Limits {
            udp_validation_packets: 1,
            ..big()
        });
        let addr: SocketAddr = "127.0.0.1:6101".parse().unwrap();
        let key = PeerKey::Udp(addr);
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap(); // fresh activity
        let (evicted, _) = r.prune(Duration::from_millis(50));
        assert_eq!(evicted, 0);
        assert!(r.contains(&band(1), &key));
    }

    #[tokio::test]
    async fn conn_peer_survives_prune_while_connected() {
        // Regression: connection peers are NOT idle-evicted, even when silent
        // far longer than the TTL — their transport connection is their liveness
        // (removal happens via disconnect). A receive-only listener must not go
        // deaf.
        let r = Registry::new(big());
        let k = PeerKey::Conn(1);
        r.connect(band(1), k, conn(8).0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        let (evicted, remaining) = r.prune(Duration::from_millis(1)); // TTL far exceeded
        assert_eq!(evicted, 0, "connection peer must not be TTL-evicted");
        assert_eq!(remaining, 1);
        assert!(r.contains(&band(1), &k));
    }

    #[tokio::test]
    async fn remove_on_disconnect() {
        // C5
        let r = Registry::new(big());
        let k = PeerKey::Conn(1);
        r.connect(band(1), k, conn(8).0).unwrap();
        r.disconnect(&band(1), &k);
        assert!(!r.contains(&band(1), &k));
        assert_eq!(r.band_count(), 0);
    }

    // -- K: addressed routing (SPEC §10.1) ---------------------------------

    /// A 43-byte frame with an explicit cleartext channel_type / channel_id.
    fn typed(b: BandId, ct: u8, cid: u16) -> Arc<[u8]> {
        let mut v = vec![0u8; 43];
        v[..16].copy_from_slice(&b);
        v[16] = ct;
        v[17..19].copy_from_slice(&cid.to_be_bytes());
        Arc::from(v.as_slice())
    }

    const ROUTE: u8 = 0x05;
    const CHANNEL: u8 = 0x02;

    fn drain(rx: &mut mpsc::Receiver<Arc<[u8]>>) -> usize {
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    /// Band 1 with conn peers 1..=n; returns their receivers.
    fn band_of(r: &Registry, n: u64) -> Vec<mpsc::Receiver<Arc<[u8]>>> {
        (1..=n)
            .map(|i| {
                let (s, rx) = conn(64);
                r.connect(band(1), PeerKey::Conn(i), s).unwrap();
                rx
            })
            .collect()
    }

    #[tokio::test]
    async fn route_frame_is_consumed_and_recorded() {
        // K1 — a ROUTE frame records the sender's id and reaches nobody.
        let r = Registry::new(big());
        let mut rx = band_of(&r, 3);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        assert_eq!(r.route_of(&band(1), &PeerKey::Conn(1)), Some(7));
        assert_eq!(r.route_of(&band(1), &PeerKey::Conn(2)), None);
        for q in rx.iter_mut() {
            assert_eq!(drain(q), 0, "ROUTE frames must never be relayed");
        }
    }

    #[tokio::test]
    async fn route_frame_is_not_federated() {
        // K1 — nor offered to a federation link.
        let r = Registry::new(big());
        let (ftx, mut frx) = mpsc::channel(8);
        r.set_federation(ftx);
        let _rx = band_of(&r, 2);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        assert!(frx.try_recv().is_err(), "ROUTE frames must not cross a link");
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), CHANNEL, 0));
        assert!(frx.try_recv().is_ok(), "data frames still federate");
    }

    #[tokio::test]
    async fn addressed_channel_reaches_only_declarers() {
        // K2 — incl. a collision: two peers declaring the same id both get it.
        let r = Registry::new(big());
        let mut rx = band_of(&r, 4);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        r.forward(&band(1), &PeerKey::Conn(2), typed(band(1), ROUTE, 9));
        r.forward(&band(1), &PeerKey::Conn(3), typed(band(1), ROUTE, 7));
        r.forward(&band(1), &PeerKey::Conn(4), typed(band(1), CHANNEL, 7));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![1, 0, 1, 0]);
        r.forward(&band(1), &PeerKey::Conn(4), typed(band(1), CHANNEL, 9));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![0, 1, 0, 0]);
    }

    #[tokio::test]
    async fn undeclared_or_zero_or_non_channel_broadcasts() {
        // K3 — channel_id 0, an id nobody declared, and every non-CHANNEL type
        // (whose channel_id means something else: stream/board/drop id) all
        // keep the old broadcast behaviour.
        let r = Registry::new(big());
        let mut rx = band_of(&r, 3);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        for f in [
            typed(band(1), CHANNEL, 0),
            typed(band(1), CHANNEL, 42),
            typed(band(1), 0x00, 7),
            typed(band(1), 0x01, 7),
            typed(band(1), 0x03, 7),
            typed(band(1), 0x04, 7),
            typed(band(1), 0x7F, 7), // unknown type
        ] {
            r.forward(&band(1), &PeerKey::Conn(3), f);
            let got: Vec<usize> = rx.iter_mut().map(drain).collect();
            assert_eq!(got, vec![1, 1, 0], "must broadcast to all but the sender");
        }
    }

    #[tokio::test]
    async fn addressed_never_echoes_and_sole_self_declarer_falls_back() {
        // K4 — never echoed. If the sender is the only declarer of the id, the
        // addressee must be an undeclared peer: fall back to broadcast.
        let r = Registry::new(big());
        let mut rx = band_of(&r, 3);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), CHANNEL, 7));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![0, 1, 1]);
        // With another declarer, only that declarer gets it — still no echo.
        r.forward(&band(1), &PeerKey::Conn(2), typed(band(1), ROUTE, 7));
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), CHANNEL, 7));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![0, 1, 0]);
    }

    #[tokio::test]
    async fn addressed_honours_eligibility() {
        // K5 — a still-validating UDP declarer is not a destination (D4), and
        // its id is still "known", so the frame is not sprayed band-wide.
        let r = Registry::new(big()); // udp_validation_packets = 2
        let addr: SocketAddr = "127.0.0.1:6200".parse().unwrap();
        let (us, mut urx) = udp(8, addr);
        r.observe_udp(band(1), addr, || us).unwrap();
        r.forward(&band(1), &PeerKey::Udp(addr), typed(band(1), ROUTE, 7));
        let mut rx = band_of(&r, 2);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), CHANNEL, 7));
        assert_eq!(drain(&mut urx), 0);
        assert_eq!(drain(&mut rx[1]), 0);
        // Once validated it receives its addressed traffic.
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), CHANNEL, 7));
        assert_eq!(drain(&mut urx), 1);
        assert_eq!(drain(&mut rx[1]), 0);
    }

    #[tokio::test]
    async fn redeclare_replaces_and_zero_clears() {
        // K6
        let r = Registry::new(big());
        let mut rx = band_of(&r, 3);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 8));
        assert_eq!(r.route_of(&band(1), &PeerKey::Conn(1)), Some(8));
        r.forward(&band(1), &PeerKey::Conn(3), typed(band(1), CHANNEL, 7));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![1, 1, 0], "old id forgotten -> broadcast");
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 0));
        assert_eq!(r.route_of(&band(1), &PeerKey::Conn(1)), None);
    }

    #[tokio::test]
    async fn route_forgotten_on_disconnect_and_prune() {
        // K7 — routes live on the peer entry, so removal forgets them.
        let r = Registry::new(Limits {
            udp_validation_packets: 1,
            ..big()
        });
        let mut rx = band_of(&r, 3);
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        r.disconnect(&band(1), &PeerKey::Conn(1));
        r.forward(&band(1), &PeerKey::Conn(3), typed(band(1), CHANNEL, 7));
        assert_eq!(drain(&mut rx[1]), 1, "departed declarer -> broadcast again");

        let addr: SocketAddr = "127.0.0.1:6201".parse().unwrap();
        r.observe_udp(band(1), addr, || udp(8, addr).0).unwrap();
        r.forward(&band(1), &PeerKey::Udp(addr), typed(band(1), ROUTE, 9));
        std::thread::sleep(Duration::from_millis(20));
        r.touch(&band(1), &PeerKey::Conn(2)); // conn peers are never pruned anyway
        r.prune(Duration::from_millis(5));
        assert!(!r.contains(&band(1), &PeerKey::Udp(addr)));
        r.forward(&band(1), &PeerKey::Conn(3), typed(band(1), CHANNEL, 9));
        assert_eq!(drain(&mut rx[1]), 1, "pruned declarer -> broadcast again");
    }

    #[tokio::test]
    async fn routes_are_band_scoped() {
        // K8 — a declaration in band 1 has no effect on band 2.
        let r = Registry::new(big());
        let (s1, mut r1) = conn(8);
        let (s2, mut r2) = conn(8);
        let (s3, mut r3) = conn(8);
        r.connect(band(1), PeerKey::Conn(1), s1).unwrap();
        r.connect(band(2), PeerKey::Conn(2), s2).unwrap();
        r.connect(band(2), PeerKey::Conn(3), s3).unwrap();
        r.forward(&band(1), &PeerKey::Conn(1), typed(band(1), ROUTE, 7));
        r.forward(&band(2), &PeerKey::Conn(3), typed(band(2), CHANNEL, 7));
        assert_eq!(drain(&mut r1), 0);
        assert_eq!(drain(&mut r2), 1);
        assert_eq!(drain(&mut r3), 0);
    }

    #[tokio::test]
    async fn link_injection_honours_local_routes() {
        // K9 — frames from a federation link are addressed against local
        // declarations; a ROUTE frame arriving over a link is dropped.
        let r = Registry::new(big());
        let mut rx = band_of(&r, 3);
        r.forward(&band(1), &PeerKey::Conn(2), typed(band(1), ROUTE, 7));
        r.inject_from_link(&band(1), typed(band(1), CHANNEL, 7));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![0, 1, 0]);
        r.inject_from_link(&band(1), typed(band(1), CHANNEL, 0));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![1, 1, 1]);
        r.inject_from_link(&band(1), typed(band(1), ROUTE, 7));
        let got: Vec<usize> = rx.iter_mut().map(drain).collect();
        assert_eq!(got, vec![0, 0, 0]);
    }

    #[tokio::test]
    async fn udp_peer_queues_are_independent() {
        // D6 — one UDP peer's full queue costs other peers nothing: flood the
        // band, A (cap 2) overflows and drops, B (cap 64) still gets every
        // frame. (A single hub-wide UDP queue made A's backlog everyone's.)
        let r = Registry::new(Limits {
            udp_validation_packets: 1,
            ..big()
        });
        let a: SocketAddr = "127.0.0.1:6300".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:6301".parse().unwrap();
        let (sa, mut ra) = udp(2, a);
        let (sb, mut rb) = udp(64, b);
        r.observe_udp(band(1), a, || sa).unwrap();
        r.observe_udp(band(1), b, || sb).unwrap();
        for _ in 0..50 {
            r.forward(&band(1), &PeerKey::Conn(99), frame(band(1)));
        }
        let (mut na, mut nb) = (0, 0);
        while ra.try_recv().is_ok() {
            na += 1;
        }
        while rb.try_recv().is_ok() {
            nb += 1;
        }
        assert_eq!(na, 2, "A's queue is bounded");
        assert_eq!(nb, 50, "B is unaffected by A's overflow");
    }

    #[test]
    fn observe_udp_builds_sink_only_on_admission() {
        // The sink factory (which spawns a writer) runs once, on first sight,
        // and not at all for a refused peer.
        let r = Registry::new(Limits {
            max_peers_per_band: 1,
            ..big()
        });
        let a: SocketAddr = "127.0.0.1:6400".parse().unwrap();
        let b: SocketAddr = "127.0.0.1:6401".parse().unwrap();
        let mut built = 0;
        r.observe_udp(band(1), a, || {
            built += 1;
            udp(2, a).0
        })
        .unwrap();
        r.observe_udp(band(1), a, || {
            built += 1;
            udp(2, a).0
        })
        .unwrap();
        assert_eq!(built, 1, "refresh must not rebuild the sink");
        assert_eq!(
            r.observe_udp(band(1), b, || {
                built += 1;
                udp(2, b).0
            }),
            Err(RegError::PeerCap)
        );
        assert_eq!(built, 1, "refused peer must not build a sink");
    }

    #[test]
    fn conn_ids_are_unique() {
        let r = Registry::new(big());
        let a = r.next_conn_id();
        let b = r.next_conn_id();
        assert_ne!(a, b);
    }
}
