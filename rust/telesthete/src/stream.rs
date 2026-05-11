//! Stream channel — fire-and-forget lossy datagrams. SPEC §5.
//!
//! Each Stream packet payload is `[priority: u8, data: ...]`.
//! Receiver tracks per-(peer, stream_id) high-water mark via the packet
//! sequence number; older packets are dropped silently. This delivers
//! "freshest data" semantics — exactly what live video wants.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::debug;

use crate::framing::ChannelType;
use crate::transport::{Inbound, Outbound, Transport, TransportError};

#[derive(Debug, Error)]
pub enum StreamError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("payload too short")]
    PayloadTooShort,
}

/// Inbound Stream message — payload + sender address + priority.
#[derive(Debug, Clone)]
pub struct StreamMessage {
    pub from: SocketAddr,
    pub stream_id: u16,
    pub priority: u8,
    pub data: Vec<u8>,
}

/// Stream sender + receiver. One handle per `stream_id` per peer.
pub struct StreamEndpoint {
    transport: Arc<Transport>,
    peer: SocketAddr,
    stream_id: u16,
    inbound: mpsc::UnboundedReceiver<StreamMessage>,
}

impl StreamEndpoint {
    pub async fn send(&self, priority: u8, data: &[u8]) -> Result<(), StreamError> {
        let mut payload = Vec::with_capacity(1 + data.len());
        payload.push(priority);
        payload.extend_from_slice(data);
        self.transport
            .send(Outbound {
                to: self.peer,
                channel_type: ChannelType::Stream,
                channel_id: self.stream_id,
                plaintext: payload,
                priority,
            })
            .await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<StreamMessage> {
        self.inbound.recv().await
    }
}

/// Multiplexer that owns the inbound Stream route and demultiplexes per
/// stream_id into per-endpoint queues.
pub struct StreamHub {
    transport: Arc<Transport>,
    /// Per-stream-id senders.
    senders: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<StreamMessage>>>>,
}

impl StreamHub {
    pub async fn new(transport: Arc<Transport>) -> Self {
        let inbound = transport.route(ChannelType::Stream).await;
        let senders: Arc<Mutex<HashMap<u16, mpsc::UnboundedSender<StreamMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let watermarks: Arc<Mutex<HashMap<(SocketAddr, u16), u64>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let senders_ref = Arc::clone(&senders);
        tokio::spawn(async move {
            let mut rx = inbound;
            while let Some(in_pkt) = rx.recv().await {
                if let Some(msg) = handle_stream_inbound(&in_pkt, &watermarks).await {
                    let senders = senders_ref.lock().await;
                    if let Some(tx) = senders.get(&in_pkt.header.channel_id) {
                        let _ = tx.send(msg);
                    } else {
                        debug!(
                            "no Stream subscriber for stream_id={}",
                            in_pkt.header.channel_id
                        );
                    }
                }
            }
        });

        Self { transport, senders }
    }

    pub async fn open(&self, peer: SocketAddr, stream_id: u16) -> StreamEndpoint {
        let (tx, rx) = mpsc::unbounded_channel();
        self.senders.lock().await.insert(stream_id, tx);
        StreamEndpoint {
            transport: Arc::clone(&self.transport),
            peer,
            stream_id,
            inbound: rx,
        }
    }
}

async fn handle_stream_inbound(
    in_pkt: &Inbound,
    watermarks: &Arc<Mutex<HashMap<(SocketAddr, u16), u64>>>,
) -> Option<StreamMessage> {
    if in_pkt.payload.is_empty() {
        debug!("dropping empty Stream packet from {}", in_pkt.from);
        return None;
    }
    let key = (in_pkt.from, in_pkt.header.channel_id);
    {
        let mut wm = watermarks.lock().await;
        let entry = wm.entry(key).or_insert(0);
        if in_pkt.header.sequence <= *entry {
            // Stale — drop.
            return None;
        }
        *entry = in_pkt.header.sequence;
    }
    let priority = in_pkt.payload[0];
    let data = in_pkt.payload[1..].to_vec();
    Some(StreamMessage {
        from: in_pkt.from,
        stream_id: in_pkt.header.channel_id,
        priority,
        data,
    })
}
