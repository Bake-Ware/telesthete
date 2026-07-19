//! UDP transport — single socket per peer, packets routed by channel_type.
//!
//! Initial scope: best-effort delivery for all channel types. Per SPEC
//! §6 the `Channel` type is supposed to be reliable (TCP-like); when
//! all application messages fit in one packet, order + reliability come
//! for free at this scale. Sliding-window + retransmission is future
//! work.

use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::crypto::Key;
use crate::framing::{decode_packet, encode_packet, ChannelType, FramingError, Header};

/// Inbound packet decoded into header + cleartext payload + sender address.
#[derive(Debug, Clone)]
pub struct Inbound {
    pub from: SocketAddr,
    pub header: Header,
    pub payload: Vec<u8>,
}

/// Outbound packet to be encrypted + sent.
#[derive(Debug, Clone)]
pub struct Outbound {
    pub to: SocketAddr,
    pub channel_type: ChannelType,
    pub channel_id: u16,
    pub plaintext: Vec<u8>,
    /// Priority hint (lower = higher priority). Currently informational only.
    pub priority: u8,
}

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("framing: {0}")]
    Framing(#[from] FramingError),
    #[error("transport closed")]
    Closed,
}

/// One monotonic sequence counter per sender (shared across this sender's
/// channels/streams). The AEAD nonce is the sequence and the key is band-wide
/// (SPEC §3.1), so the sequence MUST NOT repeat under one key across senders or
/// restarts. It is therefore CSPRNG-initialized (SPEC §3.3) with ~2^63 headroom
/// before wrap, making a cross-sender collision negligible rather than
/// guaranteed (two senders both starting at 0/1 would collide immediately).
#[derive(Debug)]
pub struct SequenceCounter(std::sync::atomic::AtomicU64);

impl SequenceCounter {
    /// CSPRNG-seeded start (SPEC §3.3).
    pub fn random() -> Self {
        let mut b = [0u8; 8];
        getrandom::getrandom(&mut b).expect("CSPRNG unavailable");
        Self(std::sync::atomic::AtomicU64::new(u64::from_be_bytes(b) >> 1))
    }

    /// Return the current value, then advance by one.
    pub fn next(&self) -> u64 {
        self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for SequenceCounter {
    fn default() -> Self {
        Self::random()
    }
}

/// UDP transport runtime. Owns the socket and the send/recv loops.
pub struct Transport {
    socket: Arc<UdpSocket>,
    key: Key,
    band_id: [u8; 16],
    seq: Arc<SequenceCounter>,
    /// Per-channel-type inbound dispatch (control, stream, channel).
    routes: Arc<Mutex<HashMap<ChannelType, mpsc::UnboundedSender<Inbound>>>>,
}

impl Transport {
    pub async fn bind(addr: SocketAddr, key: Key, band_id: [u8; 16]) -> io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(addr).await?);
        debug!("telesthete transport bound on {}", socket.local_addr()?);
        Ok(Self {
            socket,
            key,
            band_id,
            seq: Arc::new(SequenceCounter::default()),
            routes: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn band_id(&self) -> [u8; 16] {
        self.band_id
    }

    /// Subscribe to inbound packets for a channel type. Drops any existing
    /// subscription for that type.
    pub async fn route(&self, ty: ChannelType) -> mpsc::UnboundedReceiver<Inbound> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.lock().await.insert(ty, tx);
        rx
    }

    /// Encrypt + send one packet.
    pub async fn send(&self, out: Outbound) -> Result<(), TransportError> {
        let seq = self.seq.next();
        let pkt = encode_packet(
            &self.key,
            &self.band_id,
            out.channel_type,
            out.channel_id,
            seq,
            &out.plaintext,
        )?;
        self.socket.send_to(&pkt, out.to).await?;
        Ok(())
    }

    /// Spawn the receive loop. Decoded packets are dispatched to subscribers
    /// registered via [`Self::route`].
    pub fn spawn_recv_loop(&self) -> tokio::task::JoinHandle<()> {
        let socket = Arc::clone(&self.socket);
        let routes = Arc::clone(&self.routes);
        let key = self.key;
        let band_id = self.band_id;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65_535];
            loop {
                let (n, from) = match socket.recv_from(&mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("recv_from error: {e}");
                        continue;
                    }
                };
                let (header, payload) = match decode_packet(&key, &buf[..n]) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("decode_packet from {from} failed: {e}");
                        continue;
                    }
                };
                if header.band_id != band_id {
                    debug!("dropping packet from {from}: foreign band_id");
                    continue;
                }
                let inbound = Inbound {
                    from,
                    header,
                    payload,
                };
                tracing::trace!(
                    ?header.channel_type,
                    channel_id = header.channel_id,
                    seq = header.sequence,
                    payload_len = inbound.payload.len(),
                    "telesthete rx"
                );
                let routes = routes.lock().await;
                if let Some(tx) = routes.get(&header.channel_type) {
                    if tx.send(inbound).is_err() {
                        debug!("route for {:?} closed", header.channel_type);
                    }
                } else {
                    debug!("no route for {:?}", header.channel_type);
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_band_id, derive_key};

    #[tokio::test]
    async fn loopback_send_recv() {
        let psk = b"loop-test";
        let key = derive_key(psk);
        let band_id = derive_band_id(psk);

        let alice = Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
            .await
            .unwrap();
        let bob = Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
            .await
            .unwrap();
        let bob_addr = bob.local_addr().unwrap();

        let mut bob_in = bob.route(ChannelType::Stream).await;
        bob.spawn_recv_loop();

        alice
            .send(Outbound {
                to: bob_addr,
                channel_type: ChannelType::Stream,
                channel_id: 1,
                plaintext: b"hello bob".to_vec(),
                priority: 0,
            })
            .await
            .unwrap();

        let got = tokio::time::timeout(std::time::Duration::from_secs(1), bob_in.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"hello bob");
        assert_eq!(got.header.channel_id, 1);
        assert_eq!(got.header.channel_type, ChannelType::Stream);
    }

    #[tokio::test]
    async fn foreign_band_dropped() {
        let key_a = derive_key(b"band-a");
        let id_a = derive_band_id(b"band-a");
        let key_b = derive_key(b"band-b");
        let id_b = derive_band_id(b"band-b");

        let alice = Transport::bind("127.0.0.1:0".parse().unwrap(), key_a, id_a)
            .await
            .unwrap();
        let bob = Transport::bind("127.0.0.1:0".parse().unwrap(), key_b, id_b)
            .await
            .unwrap();
        let bob_addr = bob.local_addr().unwrap();
        let mut bob_in = bob.route(ChannelType::Stream).await;
        bob.spawn_recv_loop();

        alice
            .send(Outbound {
                to: bob_addr,
                channel_type: ChannelType::Stream,
                channel_id: 1,
                plaintext: b"foreign".to_vec(),
                priority: 0,
            })
            .await
            .unwrap();

        // Should NOT arrive — bob is in a different band.
        let r = tokio::time::timeout(std::time::Duration::from_millis(200), bob_in.recv()).await;
        assert!(r.is_err(), "expected timeout (foreign-band drop)");
    }
}
