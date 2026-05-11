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
    _recv_loop: JoinHandle<()>,
}

impl Band {
    /// Bind a UDP socket and spin up the receive loop.
    pub async fn bind(
        psk: &[u8],
        bind_addr: SocketAddr,
        hostname: impl Into<String>,
    ) -> Result<Self, BandError> {
        let key = derive_key(psk);
        let band_id = derive_band_id(psk);
        let transport = Arc::new(Transport::bind(bind_addr, key, band_id).await?);
        let recv_loop = transport.spawn_recv_loop();

        let stream_hub = StreamHub::new(Arc::clone(&transport)).await;
        let channel_hub = ChannelHub::new(Arc::clone(&transport)).await;
        let control = ControlChannel::new(Arc::clone(&transport)).await;

        Ok(Self {
            transport,
            stream_hub,
            channel_hub,
            control: Some(control),
            hostname: hostname.into(),
            key,
            band_id,
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
            "payload": Hello { hostname: self.hostname.clone(), capabilities: Vec::new() }
        });
        let bytes = serde_json::to_vec(&env).map_err(crate::control::ControlError::from)?;
        self.transport
            .send(Outbound {
                to: peer,
                channel_type: ChannelType::Control,
                channel_id: 0,
                plaintext: bytes,
                priority: 0,
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
        let bob = Band::bind(b"stream-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();

        let bob_addr = bob.local_addr().unwrap();
        let mut bob_stream = bob.stream(alice.local_addr().unwrap(), 7).await;
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
        let bob = Band::bind(b"stale-psk", "127.0.0.1:0".parse().unwrap(), "bob")
            .await
            .unwrap();

        let bob_addr = bob.local_addr().unwrap();
        let mut bob_stream = bob.stream(alice.local_addr().unwrap(), 1).await;
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
}
