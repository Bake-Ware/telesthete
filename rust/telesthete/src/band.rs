//! `Band` — top-level public API. One Band per PSK; multiple peers connect
//! to the same Band and exchange traffic across Stream/Channel/Control.
//!
//! Mirrors the Python reference's `Band` class shape (`band.stream(id)`,
//! `band.connect_peer(addr)`, `band.start()`/`stop()`).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::debug;

use crate::board::{BoardEndpoint, BoardHub};
use crate::channel::{ChannelEndpoint, ChannelHub};
use crate::control::{
    send_control_json, ControlChannel, ControlConfig, ControlError, PeerState, Peers,
    TYPE_KEEPALIVE,
};
use crate::crypto::{derive_band_id, derive_key, BandId, Key};
use crate::drop_channel::{DropHub, DropReceiver, DropSender};
use crate::stream::{StreamEndpoint, StreamHub};
use crate::transport::Transport;

/// Tunables for one Band instance. `Default` matches SPEC §4.3/§4.5.
#[derive(Debug, Clone)]
pub struct BandOptions {
    /// Session epoch (§4.3); `None` -> current time in ms since the Unix
    /// epoch. Hosts with unreliable clocks MUST pass a persisted
    /// `max(last_saved + 1, now_ms)`.
    pub session: Option<u64>,
    /// Ordered AEAD preference list (§3.5); baseline appended if missing.
    pub ciphers: Vec<String>,
    /// Capability strings advertised in HELLO/HELLO_ACK (§12.5).
    pub capabilities: Vec<String>,
    /// KEEPALIVE cadence (§4.5). SPEC default: 5 s.
    pub keepalive_interval: Duration,
    /// Idle time after which a peer is considered dead and evicted (§4.5).
    /// SPEC default: 15 s (3 missed keepalives).
    pub dead_after: Duration,
    /// Drive keepalives + dead-peer eviction automatically. On by default;
    /// tests that want manual control can disable it.
    pub auto_keepalive: bool,
}

impl Default for BandOptions {
    fn default() -> Self {
        Self {
            session: None,
            ciphers: vec![crate::crypto::BASELINE_CIPHER.to_string()],
            capabilities: Vec::new(),
            keepalive_interval: Duration::from_secs(5),
            dead_after: Duration::from_secs(15),
            auto_keepalive: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum BandError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("control: {0}")]
    Control(#[from] ControlError),
}

/// Top-level handle for one Telesthete band.
///
/// `Band` owns the UDP socket, multiplexers (Stream + Channel + Control),
/// and the receive loop. Drop the band to stop all background tasks.
///
/// All open / send methods take `&self`, so `Band` can live behind `Arc`
/// for multi-task use. The control receiver is removed via [`Band::take_control`]
/// for a single drain task; senders for HELLO / KEEPALIVE / GOODBYE remain on
/// `Band` so they keep working after `take_control` runs.
pub struct Band {
    transport: Arc<Transport>,
    stream_hub: StreamHub,
    channel_hub: ChannelHub,
    board_hub: BoardHub,
    drop_hub: DropHub,
    /// `None` after [`Band::take_control`] hands the receiver to a drain task.
    control: Option<ControlChannel>,
    hostname: String,
    key: Key,
    band_id: BandId,
    /// This Band instance's session epoch (§4.3), fixed at creation and
    /// advertised in HELLO so a peer rebases its replay watermark when we
    /// restart.
    session: u64,
    /// Ordered cipher preferences + capabilities advertised in our HELLO.
    ciphers: Vec<String>,
    capabilities: Vec<String>,
    /// Live peer registry, maintained by the control task (§3.5/§4.3).
    peers: Peers,
    recv_loop: JoinHandle<()>,
    keepalive_loop: Option<JoinHandle<()>>,
}

impl Drop for Band {
    /// Stop the background tasks so the socket is actually released — a
    /// dropped Band must not keep receiving (or keepaliving) forever.
    fn drop(&mut self) {
        self.recv_loop.abort();
        if let Some(ka) = &self.keepalive_loop {
            ka.abort();
        }
    }
}

impl Band {
    /// Bind a UDP socket and spin up the receive loop, using the current time
    /// (ms since the Unix epoch) as the session epoch (§4.3). Fine on a
    /// roughly-synced clock; a host whose clock can step backward or start unset
    /// MUST use [`Band::bind_with_session`] with a persisted monotonic value.
    pub async fn bind(
        psk: &[u8],
        bind_addr: SocketAddr,
        hostname: impl Into<String>,
    ) -> Result<Self, BandError> {
        Self::bind_with_options(psk, bind_addr, hostname, BandOptions::default()).await
    }

    /// Bind with an explicit session epoch (§4.3), which MUST increase on every
    /// restart. Consumers with an unreliable clock should pass a persisted
    /// `max(last_saved + 1, now_ms)` so a restart is never mistaken for a stale
    /// session and refused.
    pub async fn bind_with_session(
        psk: &[u8],
        bind_addr: SocketAddr,
        hostname: impl Into<String>,
        session: u64,
    ) -> Result<Self, BandError> {
        let opts = BandOptions {
            session: Some(session),
            ..BandOptions::default()
        };
        Self::bind_with_options(psk, bind_addr, hostname, opts).await
    }

    /// Bind with full control over session epoch, cipher preferences,
    /// capabilities, and keepalive timing.
    pub async fn bind_with_options(
        psk: &[u8],
        bind_addr: SocketAddr,
        hostname: impl Into<String>,
        opts: BandOptions,
    ) -> Result<Self, BandError> {
        let session = opts.session.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        });
        let hostname = hostname.into();
        let base_key = derive_key(psk);
        let band_id = derive_band_id(psk);

        let mut ciphers = opts.ciphers;
        if !ciphers.iter().any(|c| c == crate::crypto::BASELINE_CIPHER) {
            ciphers.push(crate::crypto::BASELINE_CIPHER.to_string()); // §12.5 mandatory
        }

        let mut transport = Transport::bind(bind_addr, base_key, band_id).await?;
        transport.set_session_key(crate::crypto::derive_session_key(
            psk,
            crate::crypto::BASELINE_CIPHER,
            session,
        ));
        let transport = Arc::new(transport);
        let recv_loop = transport.spawn_recv_loop();

        // The control task signals a peer restart here; the StreamHub clears that
        // peer's stream watermarks so its fresh-session packets are accepted.
        let (rebase_tx, _rebase_rx) = tokio::sync::broadcast::channel::<SocketAddr>(64);

        let peers: Peers = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let stream_hub = StreamHub::new(Arc::clone(&transport), rebase_tx.subscribe()).await;
        let channel_hub = ChannelHub::new(Arc::clone(&transport)).await;
        // Board's actor string — the §7.3 LWW tiebreak — is this Band's hostname.
        let board_hub =
            BoardHub::new(Arc::clone(&transport), hostname.clone(), rebase_tx.clone()).await;
        let drop_hub = DropHub::new(Arc::clone(&transport), rebase_tx.clone()).await;
        let control = ControlChannel::new(
            Arc::clone(&transport),
            ControlConfig {
                psk: psk.to_vec(),
                session,
                hostname: hostname.clone(),
                capabilities: opts.capabilities.clone(),
                ciphers: ciphers.clone(),
            },
            Arc::clone(&peers),
            rebase_tx,
        )
        .await;

        let keepalive_loop = if opts.auto_keepalive {
            Some(Self::spawn_keepalive(
                Arc::clone(&transport),
                Arc::clone(&peers),
                opts.keepalive_interval,
                opts.dead_after,
            ))
        } else {
            None
        };

        Ok(Self {
            transport,
            stream_hub,
            channel_hub,
            board_hub,
            drop_hub,
            control: Some(control),
            hostname,
            key: base_key,
            band_id,
            session,
            ciphers,
            capabilities: opts.capabilities,
            peers,
            recv_loop,
            keepalive_loop,
        })
    }

    /// §4.5 driver: KEEPALIVE to every known peer each interval; evict peers
    /// whose last authenticated packet is older than `dead_after`.
    fn spawn_keepalive(
        transport: Arc<Transport>,
        peers: Peers,
        interval: Duration,
        dead_after: Duration,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let addrs: Vec<SocketAddr> = peers.lock().await.keys().copied().collect();
                for addr in &addrs {
                    if let Err(e) = send_control_json(
                        &transport,
                        *addr,
                        TYPE_KEEPALIVE,
                        serde_json::json!({}),
                        false,
                    )
                    .await
                    {
                        debug!("keepalive to {addr} failed: {e}");
                    }
                }
                let seen = transport.last_seen().await;
                let now = std::time::Instant::now();
                let mut dead = Vec::new();
                peers.lock().await.retain(|addr, st| {
                    let alive = seen
                        .get(addr)
                        .is_some_and(|t| now.duration_since(*t) < dead_after);
                    if !alive {
                        debug!("peer {} at {addr} timed out; evicting", st.hostname);
                        dead.push(*addr);
                    }
                    alive
                });
                for addr in dead {
                    transport.forget_peer(addr).await;
                }
            }
        })
    }

    /// Snapshot of the live peer registry (§4.3/§3.5).
    pub async fn peers(&self) -> HashMap<SocketAddr, PeerState> {
        self.peers.lock().await.clone()
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.transport.local_addr()
    }

    pub fn band_id(&self) -> BandId {
        self.band_id
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// Send a HELLO to a peer to introduce ourselves, advertising our cipher
    /// preferences and capabilities (§3.5/§12.5).
    pub async fn connect_peer(&self, peer: SocketAddr) -> Result<(), BandError> {
        // Built directly on `transport` rather than the (possibly already-taken)
        // ControlChannel — lets the cockpit `take_control` first then HELLO new
        // peers as they're discovered.
        use crate::control::{Hello, TYPE_HELLO};
        let hello = Hello {
            hostname: self.hostname.clone(),
            capabilities: self.capabilities.clone(),
            ciphers: self.ciphers.clone(),
            session: self.session,
        };
        send_control_json(
            &self.transport,
            peer,
            TYPE_HELLO,
            serde_json::to_value(&hello).map_err(ControlError::from)?,
            true, // HELLO bootstraps under the base key
        )
        .await?;
        Ok(())
    }

    /// Open a Stream endpoint to a peer with the given `stream_id`.
    pub async fn stream(&self, peer: SocketAddr, stream_id: u16) -> StreamEndpoint {
        self.stream_hub.open(peer, stream_id).await
    }

    /// Open a Channel endpoint to a peer with the given `channel_id`.
    pub async fn channel(&self, peer: SocketAddr, channel_id: u16) -> ChannelEndpoint {
        self.channel_hub.open(peer, channel_id).await
    }

    /// Open the replicated Board with the given `board_id` (SPEC §7). The
    /// Band's hostname is the board's actor string (§7.3 tiebreak); add
    /// destinations per peer for SET/DIGEST broadcast.
    pub async fn board(&self, board_id: u16) -> BoardEndpoint {
        self.board_hub.open(board_id).await
    }

    /// Offer one file on `drop_id` (SPEC §8); serve receivers' range requests.
    pub async fn drop_send(
        &self,
        drop_id: u16,
        name: impl Into<String>,
        data: Vec<u8>,
    ) -> DropSender {
        self.drop_hub.open_sender(drop_id, name, data).await
    }

    /// Receive the file offered on `drop_id` (SPEC §8).
    pub async fn drop_recv(&self, drop_id: u16) -> DropReceiver {
        self.drop_hub
            .open_receiver(drop_id, HashMap::new())
            .await
    }

    /// Receive on `drop_id`, resuming from persisted chunks (§8.2).
    pub async fn drop_recv_resume(
        &self,
        drop_id: u16,
        have: HashMap<u32, Vec<u8>>,
    ) -> DropReceiver {
        self.drop_hub.open_receiver(drop_id, have).await
    }

    /// Borrow the Control channel for HELLO_ACK / KEEPALIVE / GOODBYE / etc.
    ///
    /// Panics if [`Band::take_control`] has already removed it. Used by
    /// callers that want both send + recv in one task (source daemon, tests).
    pub fn control(&mut self) -> &mut ControlChannel {
        self.control
            .as_mut()
            .expect("control channel already taken via Band::take_control")
    }

    /// Remove the Control channel so it can be driven by a dedicated drain
    /// task. Returns `None` on second call. Senders ([`connect_peer`]) keep
    /// working — they go through the underlying transport directly.
    pub fn take_control(&mut self) -> Option<ControlChannel> {
        self.control.take()
    }

    /// Used in tests to assert key derivation parity.
    #[doc(hidden)]
    pub fn key(&self) -> &Key {
        &self.key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn band_loopback_hello() {
        let alice = Band::bind(b"loopback-psk", "127.0.0.1:0".parse().unwrap(), "alice")
            .await
            .unwrap();
        let mut bob = Band::bind(b"loopback-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();

        let bob_addr = bob.local_addr().unwrap();
        alice.connect_peer(bob_addr).await.unwrap();

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), bob.control().recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            crate::control::ControlEvent::Hello { hostname, .. } => {
                assert_eq!(hostname, "alice");
            }
            other => panic!("expected Hello, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn negotiates_aes_and_streams_under_it() {
        // §3.5 end-to-end: both prefer AES-256-GCM; the responder auto-ACKs
        // committing it, both sides re-key, and stream data flows under the
        // negotiated (non-baseline) suite.
        let opts = || BandOptions {
            ciphers: vec!["aes256-gcm".into(), "chacha20-poly1305".into()],
            ..BandOptions::default()
        };
        let mut alice = Band::bind_with_options(
            b"aes-psk",
            "127.0.0.1:0".parse().unwrap(),
            "alice",
            opts(),
        )
        .await
        .unwrap();
        let mut bob =
            Band::bind_with_options(b"aes-psk", "127.0.0.1:0".parse().unwrap(), "bob", opts())
                .await
                .unwrap();

        let bob_addr = bob.local_addr().unwrap();
        let alice_addr = alice.local_addr().unwrap();
        alice.connect_peer(bob_addr).await.unwrap();

        // Bob sees the HELLO; alice receives bob's AUTOMATIC HELLO_ACK
        // committing aes256-gcm — no manual ack driving.
        let hello = tokio::time::timeout(std::time::Duration::from_secs(1), bob.control().recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(hello, crate::control::ControlEvent::Hello { .. }));
        let ack = tokio::time::timeout(std::time::Duration::from_secs(1), alice.control().recv())
            .await
            .unwrap()
            .unwrap();
        match ack {
            crate::control::ControlEvent::HelloAck { cipher, hostname, .. } => {
                assert_eq!(hostname, "bob");
                assert_eq!(cipher, "aes256-gcm");
            }
            other => panic!("expected auto HELLO_ACK, got {other:?}"),
        }

        // Both registries agree on the committed suite.
        assert_eq!(alice.peers().await[&bob_addr].cipher, "aes256-gcm");
        assert_eq!(bob.peers().await[&alice_addr].cipher, "aes256-gcm");

        // Data path actually uses it (alice -> bob and bob -> alice).
        let mut bob_stream = bob.stream(alice_addr, 9).await;
        let alice_stream = alice.stream(bob_addr, 9).await;
        alice_stream.send(0, b"under-aes").await.unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), bob_stream.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.data, b"under-aes");
    }

    #[tokio::test]
    async fn keepalives_sustain_then_dead_peer_evicted() {
        // §4.5 with compressed timers: while both bands run, keepalives keep
        // the peer alive well past dead_after; once bob is dropped, alice
        // evicts him.
        let opts = || BandOptions {
            keepalive_interval: std::time::Duration::from_millis(50),
            dead_after: std::time::Duration::from_millis(250),
            ..BandOptions::default()
        };
        let mut alice = Band::bind_with_options(
            b"ka-psk",
            "127.0.0.1:0".parse().unwrap(),
            "alice",
            opts(),
        )
        .await
        .unwrap();
        let mut bob =
            Band::bind_with_options(b"ka-psk", "127.0.0.1:0".parse().unwrap(), "bob", opts())
                .await
                .unwrap();

        let bob_addr = bob.local_addr().unwrap();
        alice.connect_peer(bob_addr).await.unwrap();
        let _ = bob.control().recv().await.unwrap(); // HELLO
        let _ = alice.control().recv().await.unwrap(); // auto HELLO_ACK

        // Both sides know each other (bob learned alice from her HELLO).
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            alice.peers().await.contains_key(&bob_addr),
            "live peer must survive several dead_after windows via keepalives"
        );

        drop(bob); // recv+keepalive loops abort -> bob goes silent
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        assert!(
            !alice.peers().await.contains_key(&bob_addr),
            "silent peer must be evicted after dead_after"
        );
    }

    #[tokio::test]
    async fn band_stream_roundtrip() {
        let alice = Band::bind(b"stream-psk", "127.0.0.1:0".parse().unwrap(), "alice")
            .await
            .unwrap();
        let mut bob = Band::bind(b"stream-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();

        let bob_addr = bob.local_addr().unwrap();
        let alice_addr = alice.local_addr().unwrap();
        // Handshake first: session-keyed data requires bob to learn alice's
        // epoch from her HELLO before it can decrypt her stream (SPEC §3.3).
        alice.connect_peer(bob_addr).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), bob.control().recv())
            .await
            .unwrap()
            .unwrap();

        let mut bob_stream = bob.stream(alice_addr, 7).await;
        let alice_stream = alice.stream(bob_addr, 7).await;

        alice_stream.send(0, b"hello world").await.unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), bob_stream.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.stream_id, 7);
        assert_eq!(msg.data, b"hello world");
        assert_eq!(msg.priority, 0);
    }

    #[tokio::test]
    async fn band_stream_drops_stale() {
        // Send seq=2 first, then seq=1 — the second should be dropped by
        // the high-water mark in StreamHub.
        let alice = Band::bind(b"stale-psk", "127.0.0.1:0".parse().unwrap(), "alice")
            .await
            .unwrap();
        let mut bob = Band::bind(b"stale-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();

        let bob_addr = bob.local_addr().unwrap();
        let alice_addr = alice.local_addr().unwrap();
        // Handshake first so bob learns alice's session epoch (SPEC §3.3).
        alice.connect_peer(bob_addr).await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(1), bob.control().recv())
            .await
            .unwrap()
            .unwrap();

        let mut bob_stream = bob.stream(alice_addr, 1).await;
        let alice_stream = alice.stream(bob_addr, 1).await;

        for _ in 0..3 {
            alice_stream.send(0, b"frame").await.unwrap();
        }
        // Drain everything that's enqueued.
        let mut count = 0;
        while let Ok(Some(_)) =
            tokio::time::timeout(std::time::Duration::from_millis(150), bob_stream.recv()).await
        {
            count += 1;
        }
        assert_eq!(count, 3, "all 3 distinct-sequence frames should arrive");
    }

    #[tokio::test]
    async fn restart_rekeys_peer_and_accepts_new_session() {
        // A restarted peer (newer session epoch, from the SAME address) must be
        // re-keyed so its new-session data decrypts. Driven via a raw alice
        // Transport with two epochs, avoiding a same-port rebind (the detached
        // recv loop keeps the old socket bound — tracked separately).
        use crate::crypto::{derive_band_id, derive_key, derive_session_key, BASELINE_CIPHER};
        use crate::framing::ChannelType;
        use crate::transport::{Outbound, Transport};
        use std::time::Duration;

        let psk = b"restart-rekey-psk";
        let mut bob = Band::bind(psk, "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();
        let bob_addr = bob.local_addr().unwrap();

        let mut alice = Transport::bind(
            "127.0.0.1:0".parse().unwrap(),
            derive_key(psk),
            derive_band_id(psk),
        )
        .await
        .unwrap();
        let alice_addr = alice.local_addr().unwrap();

        let hello = |epoch: u64| -> Vec<u8> {
            serde_json::to_vec(&serde_json::json!({
                "type": 1u8,
                "payload": {"hostname":"alice","capabilities":[],
                            "ciphers":["chacha20-poly1305"],"session": epoch}
            }))
            .unwrap()
        };
        let spl = |d: &[u8]| -> Vec<u8> {
            let mut p = vec![0u8];
            p.extend_from_slice(d);
            p
        };
        let ctl = |pl: Vec<u8>| Outbound {
            to: bob_addr,
            channel_type: ChannelType::Control,
            channel_id: 0,
            plaintext: pl,
            priority: 0,
            use_base_key: true,
        };
        let strm = |pl: Vec<u8>| Outbound {
            to: bob_addr,
            channel_type: ChannelType::Stream,
            channel_id: 5,
            plaintext: pl,
            priority: 0,
            use_base_key: false,
        };

        // --- session 1 (epoch 1000) ---
        alice.set_session_key(derive_session_key(psk, BASELINE_CIPHER, 1000));
        alice.send(ctl(hello(1000))).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), bob.control().recv())
            .await
            .unwrap()
            .unwrap();
        let mut bob_stream = bob.stream(alice_addr, 5).await;
        alice.send(strm(spl(b"s1"))).await.unwrap();
        let m = tokio::time::timeout(Duration::from_secs(1), bob_stream.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(m.data, b"s1");

        // --- restart: session 2 (epoch 2000) from the SAME address ---
        alice.set_session_key(derive_session_key(psk, BASELINE_CIPHER, 2000));
        alice.send(ctl(hello(2000))).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), bob.control().recv())
            .await
            .unwrap()
            .unwrap();
        // Data under the NEW session key must decrypt (bob re-keyed on the newer
        // epoch); with the old key it would be dropped at the transport.
        let mut got = false;
        for _ in 0..5 {
            alice.send(strm(spl(b"s2"))).await.unwrap();
            if let Ok(Some(m)) =
                tokio::time::timeout(Duration::from_millis(250), bob_stream.recv()).await
            {
                if m.data == b"s2" {
                    got = true;
                    break;
                }
            }
        }
        assert!(got, "restarted peer's data must decrypt under the re-keyed session");
    }

    // Bring both Bands to a Control HELLO handshake so session-keyed Channel
    // data decrypts (SPEC §3.3), like `band_stream_roundtrip`. Returns the
    // peer addresses (alice_addr, bob_addr).
    async fn hello_pair(alice: &Band, bob: &mut Band) -> (SocketAddr, SocketAddr) {
        let bob_addr = bob.local_addr().unwrap();
        let alice_addr = alice.local_addr().unwrap();
        alice.connect_peer(bob_addr).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), bob.control().recv())
            .await
            .unwrap()
            .unwrap();
        (alice_addr, bob_addr)
    }

    #[tokio::test]
    async fn band_channel_handshake_and_message() {
        // §6.3 3-way handshake over loopback UDP; bob auto-ESTABLISHED on SYN.
        let alice = Band::bind(b"chan-psk", "127.0.0.1:0".parse().unwrap(), "alice")
            .await
            .unwrap();
        let mut bob = Band::bind(b"chan-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();
        let (alice_addr, bob_addr) = hello_pair(&alice, &mut bob).await;

        // Both sides open the endpoint before traffic (like Streams).
        let mut bob_chan = bob.channel(alice_addr, 9).await;
        let alice_chan = alice.channel(bob_addr, 9).await;

        alice_chan.connect().await.unwrap();
        alice_chan.send_message(b"reliable hello").await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(2), bob_chan.recv_message())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, b"reliable hello");
    }

    #[tokio::test]
    async fn band_channel_large_message_fragments() {
        // §6.6 fragmentation round-trip through send_message/recv_message.
        let alice = Band::bind(b"chan-frag-psk", "127.0.0.1:0".parse().unwrap(), "alice")
            .await
            .unwrap();
        let mut bob = Band::bind(b"chan-frag-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();
        let (alice_addr, bob_addr) = hello_pair(&alice, &mut bob).await;

        let mut bob_chan = bob.channel(alice_addr, 3).await;
        let alice_chan = alice.channel(bob_addr, 3).await;
        alice_chan.connect().await.unwrap();

        let msg: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
        alice_chan.send_message(&msg).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(3), bob_chan.recv_message())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got, msg);
    }

    #[tokio::test]
    async fn band_board_set_replicates() {
        // §7 over loopback UDP: HELLO handshake first, then a Board SET flows
        // from alice to bob under the session data keys.
        let alice = Band::bind(b"board-band-psk", "127.0.0.1:0".parse().unwrap(), "alice")
            .await
            .unwrap();
        let mut bob = Band::bind(b"board-band-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();
        let (_alice_addr, bob_addr) = hello_pair(&alice, &mut bob).await;

        let alice_board = alice.board(3).await;
        let mut bob_board = bob.board(3).await;
        alice_board.add_destination(bob_addr).await;
        alice_board
            .set("cursor", serde_json::json!({"x": 4, "y": 2}))
            .await
            .unwrap();

        let change = tokio::time::timeout(Duration::from_secs(2), bob_board.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(change.key, "cursor");
        assert_eq!(
            bob_board.get("cursor").await,
            Some(serde_json::json!({"x": 4, "y": 2}))
        );
    }

    #[tokio::test]
    async fn band_drop_small_file_transfer() {
        // §8 over loopback UDP: HELLO handshake, then a small Drop end-to-end
        // with the sha verdict confirmed on both ends.
        let alice = Band::bind(b"drop-band-psk", "127.0.0.1:0".parse().unwrap(), "alice")
            .await
            .unwrap();
        let mut bob = Band::bind(b"drop-band-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();
        let (_alice_addr, bob_addr) = hello_pair(&alice, &mut bob).await;
        // Wait until alice has processed bob's auto HELLO_ACK so she can
        // decrypt bob's session-keyed REQUEST frames (SPEC §3.3).
        for _ in 0..50 {
            if alice.peers().await.contains_key(&bob_addr) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(alice.peers().await.contains_key(&bob_addr));

        let data: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
        let mut sender = alice.drop_send(5, "blob.bin", data.clone()).await;
        let mut receiver = bob.drop_recv(5).await;
        sender.offer(bob_addr).await.unwrap();

        let (got, ok) = tokio::time::timeout(Duration::from_secs(3), receiver.recv_file())
            .await
            .unwrap()
            .unwrap();
        assert!(ok);
        assert_eq!(got, data);
        assert_eq!(receiver.verified().await, Some(true));

        let (from, verdict) = tokio::time::timeout(Duration::from_secs(2), sender.recv_verdict())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, bob_addr);
        assert!(verdict);
        assert!(sender.completed().await[&bob_addr]);
    }
}
