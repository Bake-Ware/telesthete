//! Channel — reliable, ordered byte streams. SPEC §6.
//!
//! Initial implementation is **best-effort, single-packet**: when
//! application messages all fit in one UDP packet at standard MTU,
//! loss is rare on loopback and reordering within a single packet is
//! nonsensical. This skip-the-window-and-retransmits shortcut is
//! replaced by the full sliding-window / SYN-ACK-FIN state machine
//! once bandwidth + multi-host pressures need it.
//!
//! API shape mirrors what a TCP-like impl will expose so callers
//! don't refactor later.

use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::debug;

use crate::framing::ChannelType;
use crate::transport::{Outbound, Transport, TransportError};

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
}

/// Inbound Channel message — payload + channel_id + sender.
#[derive(Debug, Clone)]
pub struct ChannelMessage {
    pub from: std::net::SocketAddr,
    pub channel_id: u16,
    pub data: Vec<u8>,
}

pub struct ChannelEndpoint {
    transport: Arc<Transport>,
    peer: std::net::SocketAddr,
    channel_id: u16,
    inbound: mpsc::UnboundedReceiver<ChannelMessage>,
}

impl ChannelEndpoint {
    pub async fn send(&self, data: &[u8]) -> Result<(), ChannelError> {
        // Initial framing: send the bytes raw, no reliability header. Fits
        // in one packet (~1400 byte MTU minus Telesthete's 43-byte
        // overhead = ~1357 byte max payload).
        self.transport
            .send(Outbound {
                to: self.peer,
                channel_type: ChannelType::Channel,
                channel_id: self.channel_id,
                plaintext: data.to_vec(),
                priority: 4,
            })
            .await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<ChannelMessage> {
        self.inbound.recv().await
    }

    /// A `Send + Sync + Clone` sender-only handle for this endpoint.
    /// Useful for cockpit-side input dispatch where multiple tasks want
    /// to send into the same channel without coordinating on the
    /// (single-consumer) receiver half.
    pub fn sender(&self) -> ChannelSender {
        ChannelSender {
            transport: Arc::clone(&self.transport),
            peer: self.peer,
            channel_id: self.channel_id,
        }
    }
}

/// Sender-only counterpart to [`ChannelEndpoint`]. Cheap to clone.
#[derive(Clone)]
pub struct ChannelSender {
    transport: Arc<Transport>,
    peer: std::net::SocketAddr,
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
            })
            .await?;
        Ok(())
    }

    pub fn peer(&self) -> std::net::SocketAddr {
        self.peer
    }

    pub fn channel_id(&self) -> u16 {
        self.channel_id
    }
}

pub struct ChannelHub {
    transport: Arc<Transport>,
    senders: Arc<Mutex<std::collections::HashMap<u16, mpsc::UnboundedSender<ChannelMessage>>>>,
}

impl ChannelHub {
    pub async fn new(transport: Arc<Transport>) -> Self {
        let inbound = transport.route(ChannelType::Channel).await;
        let senders: Arc<
            Mutex<std::collections::HashMap<u16, mpsc::UnboundedSender<ChannelMessage>>>,
        > = Arc::new(Mutex::new(std::collections::HashMap::new()));

        let senders_ref = Arc::clone(&senders);
        tokio::spawn(async move {
            let mut rx = inbound;
            while let Some(pkt) = rx.recv().await {
                let msg = ChannelMessage {
                    from: pkt.from,
                    channel_id: pkt.header.channel_id,
                    data: pkt.payload,
                };
                let senders = senders_ref.lock().await;
                if let Some(tx) = senders.get(&pkt.header.channel_id) {
                    let _ = tx.send(msg);
                } else {
                    debug!(
                        "no Channel subscriber for channel_id=0x{:04x}",
                        pkt.header.channel_id
                    );
                }
            }
        });

        Self { transport, senders }
    }

    pub async fn open(&self, peer: std::net::SocketAddr, channel_id: u16) -> ChannelEndpoint {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().await.insert(channel_id, tx);
        ChannelEndpoint {
            transport: Arc::clone(&self.transport),
            peer,
            channel_id,
            inbound: rx,
        }
    }
}
