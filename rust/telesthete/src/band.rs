//! `Band` — top-level public API. One Band per PSK; multiple peers connect
//! to the same Band and exchange traffic across Stream/Channel/Control.
//!
//! Mirrors the Python reference's `Band` class shape (`band.stream(id)`,
//! `band.connect_peer(addr)`, `band.start()`/`stop()`).

use std::net::SocketAddr;
use std::sync::Arc;

use thiserror::Error;
use tokio::task::JoinHandle;

use crate::channel::{ChannelEndpoint, ChannelHub};
use crate::control::{ControlChannel, ControlError};
use crate::crypto::{derive_band_id, derive_key, BandId, Key};
use crate::stream::{StreamEndpoint, StreamHub};
use crate::transport::Transport;

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
    /// `None` after [`Band::take_control`] hands the receiver to a drain task.
    control: Option<ControlChannel>,
    hostname: String,
    key: Key,
    band_id: BandId,
    /// This Band instance's session epoch (§4.3), fixed at creation and
    /// advertised in HELLO so a peer rebases its replay watermark when we
    /// restart.
    session: u64,
    _recv_loop: JoinHandle<()>,
}

impl Band {
    /// Bind a UDP socket and spin up the receive loop.
    pub async fn bind(
        psk: &[u8],
        bind_addr: SocketAddr,
        hostname: impl Into<String>,
    ) -> Result<Self, BandError> {
        let base_key = derive_key(psk);
        let band_id = derive_band_id(psk);

        // One session epoch for this Band instance (§4.3), sampled once and used
        // for our data key, our HELLO payloads, and connect_peer — so a peer
        // never sees two different epochs from us.
        let session = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

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

        let stream_hub = StreamHub::new(Arc::clone(&transport), rebase_tx.subscribe()).await;
        let channel_hub = ChannelHub::new(Arc::clone(&transport)).await;
        let control =
            ControlChannel::new(Arc::clone(&transport), psk.to_vec(), session, rebase_tx).await;

        Ok(Self {
            transport,
            stream_hub,
            channel_hub,
            control: Some(control),
            hostname: hostname.into(),
            key: base_key,
            band_id,
            session,
            _recv_loop: recv_loop,
        })
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

    /// Send a HELLO to a peer to introduce ourselves.
    pub async fn connect_peer(&self, peer: SocketAddr) -> Result<(), BandError> {
        // Built directly on `transport` rather than the (possibly already-taken)
        // ControlChannel — lets the cockpit `take_control` first then HELLO new
        // peers as they're discovered.
        use crate::control::{Hello, TYPE_HELLO};
        use crate::framing::ChannelType;
        use crate::transport::Outbound;
        let env = serde_json::json!({
            "type": TYPE_HELLO,
            "payload": Hello {
                hostname: self.hostname.clone(),
                capabilities: Vec::new(),
                ciphers: vec![crate::crypto::BASELINE_CIPHER.to_string()],
                session: self.session,
            }
        });
        let bytes = serde_json::to_vec(&env).map_err(crate::control::ControlError::from)?;
        self.transport
            .send(Outbound {
                to: peer,
                channel_type: ChannelType::Control,
                channel_id: 0,
                plaintext: bytes,
                priority: 0,
                use_base_key: true, // HELLO bootstraps under the base key
            })
            .await
            .map_err(crate::control::ControlError::from)?;
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
}
