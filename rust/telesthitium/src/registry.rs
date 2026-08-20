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
//! without limit (conformance D3).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use telesthete::BandId;
use tokio::sync::mpsc;

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
    /// A UDP peer: frames go to the shared UDP writer task with the peer's addr.
    Udp {
        tx: mpsc::Sender<(SocketAddr, Arc<[u8]>)>,
        addr: SocketAddr,
    },
    /// A connection-oriented peer (WSS / WebTransport / AF_UNIX): frames go to
    /// that connection's own writer task, which applies transport framing.
    Conn(mpsc::Sender<Arc<[u8]>>),
}

impl Sink {
    fn try_send(&self, frame: Arc<[u8]>) -> Result<(), SendDrop> {
        match self {
            Sink::Udp { tx, addr } => tx.try_send((*addr, frame)).map_err(|e| match e {
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
    pub fn inject_from_link(&self, band: &BandId, frame: Arc<[u8]>) {
        let sinks: Vec<Sink> = {
            let bands = self.bands.lock().unwrap();
            match bands.get(band) {
                None => return,
                Some(peers) => peers
                    .iter()
                    .filter(|(_, p)| p.eligible)
                    .map(|(_, p)| p.sink.clone())
                    .collect(),
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
        Self::insert(&mut bands, &self.limits, band, key, sink, true)
    }

    /// Register-or-refresh a UDP peer from an inbound datagram. Tracks
    /// validation and flips the peer eligible once it has proven routability
    /// (D4). The `sink` is only used on first sight; refreshes keep the
    /// original.
    pub fn observe_udp(&self, band: BandId, addr: SocketAddr, sink: Sink) -> Result<(), RegError> {
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
        Self::insert(&mut bands, &self.limits, band, key, sink, false)
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
        sink: Sink,
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
                sink,
                last_seen: Instant::now(),
                eligible,
                seen: 1,
            },
        );
        Ok(())
    }

    /// Relay one whole frame to every *other* eligible peer in the band
    /// (conformance B1/B2/B4). Synchronous: collects sinks under the lock, then
    /// dispatches without holding it. Frames to full queues are dropped and
    /// counted (D3); closed queues are ignored (the peer's disconnect path
    /// removes it).
    pub fn forward(&self, band: &BandId, from: &PeerKey, frame: Arc<[u8]>) {
        let sinks: Vec<Sink> = {
            let bands = self.bands.lock().unwrap();
            match bands.get(band) {
                None => return,
                Some(peers) => peers
                    .iter()
                    .filter(|(k, p)| **k != *from && p.eligible)
                    .map(|(_, p)| p.sink.clone())
                    .collect(),
            }
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

    type UdpRx = mpsc::Receiver<(SocketAddr, Arc<[u8]>)>;

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
        r.observe_udp(band(1), addr, us).unwrap();
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap(); // 2nd packet -> eligible
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
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap();
        assert_eq!(r.is_eligible(&band(1), &PeerKey::Udp(addr)), Some(false));
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap();
        assert_eq!(r.is_eligible(&band(1), &PeerKey::Udp(addr)), Some(true));
    }

    #[tokio::test]
    async fn udp_validation_disabled_when_one() {
        let r = Registry::new(Limits {
            udp_validation_packets: 1,
            ..big()
        });
        let addr: SocketAddr = "127.0.0.1:6002".parse().unwrap();
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap();
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
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap();
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
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap();
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
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        r.observe_udp(band(1), addr, udp(8, addr).0).unwrap(); // fresh activity
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

    #[test]
    fn conn_ids_are_unique() {
        let r = Registry::new(big());
        let a = r.next_conn_id();
        let b = r.next_conn_id();
        assert_ne!(a, b);
    }
}
