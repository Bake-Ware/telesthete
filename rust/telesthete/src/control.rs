//! Control channel — band management, signaling. SPEC §4.
//!
//! Always reliable. Payload is JSON-encoded `{ "type": <u8>, "payload": {...} }`.
//! M0 supports HELLO / HELLO_ACK / KEEPALIVE / GOODBYE.

use std::net::SocketAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::debug;

use crate::framing::ChannelType;
use crate::transport::{Outbound, Transport, TransportError};

pub const TYPE_HELLO: u8 = 0x01;
pub const TYPE_HELLO_ACK: u8 = 0x02;
pub const TYPE_KEEPALIVE: u8 = 0x03;
pub const TYPE_FOCUS_CHANGE: u8 = 0x04;
pub const TYPE_METACONTROL: u8 = 0x05;
pub const TYPE_GOODBYE: u8 = 0x06;

/// Capability strings advertised in HELLO / HELLO_ACK per Telesthete v1.1
/// §12.5. Forward-compatible: peers ignore unknown capabilities, and a
/// peer that omits the field is treated as an empty list (i.e. v1.0).
pub mod capability {
    pub const DMABUF_V1: &str = "dmabuf-v1";
    pub const AF_UNIX: &str = "af-unix";
    pub const SYNC_FILE: &str = "sync-file";
    pub const REUSE_V1: &str = "reuse-v1";
}

/// Sentinel PSK for the local trust profile (v1.1 §3.4). Cleartext on
/// the filesystem is the access control; this just keeps a single
/// derive path for the AEAD layer.
pub const LOCAL_PSK: &str = "telesthete-local";

#[derive(Debug, Error)]
pub enum ControlError {
    #[error("transport: {0}")]
    Transport(#[from] TransportError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Hello {
    pub hostname: String,
    /// Telesthete v1.1 §12.5. Omitted on the wire when empty so v1.0
    /// peers parse without complaint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HelloAck {
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Keepalive {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Goodbye {}

#[derive(Debug, Clone)]
pub enum ControlEvent {
    Hello {
        from: SocketAddr,
        hostname: String,
        capabilities: Vec<String>,
    },
    HelloAck {
        from: SocketAddr,
        hostname: String,
        capabilities: Vec<String>,
    },
    Keepalive {
        from: SocketAddr,
    },
    Goodbye {
        from: SocketAddr,
    },
    Other {
        from: SocketAddr,
        type_: u8,
        payload: serde_json::Value,
    },
}

#[derive(Serialize, Deserialize, Debug)]
struct ControlEnvelope {
    #[serde(rename = "type")]
    type_: u8,
    payload: serde_json::Value,
}

pub struct ControlChannel {
    transport: Arc<Transport>,
    inbound: mpsc::UnboundedReceiver<ControlEvent>,
}

impl ControlChannel {
    pub async fn new(transport: Arc<Transport>) -> Self {
        let mut raw = transport.route(ChannelType::Control).await;
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(pkt) = raw.recv().await {
                match serde_json::from_slice::<ControlEnvelope>(&pkt.payload) {
                    Ok(env) => {
                        let event = match env.type_ {
                            TYPE_HELLO => match serde_json::from_value::<Hello>(env.payload) {
                                Ok(h) => ControlEvent::Hello {
                                    from: pkt.from,
                                    hostname: h.hostname,
                                    capabilities: h.capabilities,
                                },
                                Err(e) => {
                                    debug!("bad HELLO: {e}");
                                    continue;
                                }
                            },
                            TYPE_HELLO_ACK => match serde_json::from_value::<HelloAck>(env.payload)
                            {
                                Ok(h) => ControlEvent::HelloAck {
                                    from: pkt.from,
                                    hostname: h.hostname,
                                    capabilities: h.capabilities,
                                },
                                Err(e) => {
                                    debug!("bad HELLO_ACK: {e}");
                                    continue;
                                }
                            },
                            TYPE_KEEPALIVE => ControlEvent::Keepalive { from: pkt.from },
                            TYPE_GOODBYE => ControlEvent::Goodbye { from: pkt.from },
                            _ => ControlEvent::Other {
                                from: pkt.from,
                                type_: env.type_,
                                payload: env.payload,
                            },
                        };
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Err(e) => debug!("bad control envelope from {}: {e}", pkt.from),
                }
            }
        });

        Self {
            transport,
            inbound: rx,
        }
    }

    pub async fn send_hello(&self, peer: SocketAddr, hostname: &str) -> Result<(), ControlError> {
        self.send_hello_with_caps(peer, hostname, &[]).await
    }

    /// v1.1 §12.5: send a HELLO advertising capability strings.
    pub async fn send_hello_with_caps(
        &self,
        peer: SocketAddr,
        hostname: &str,
        capabilities: &[&str],
    ) -> Result<(), ControlError> {
        self.send_typed(
            peer,
            TYPE_HELLO,
            &Hello {
                hostname: hostname.into(),
                capabilities: capabilities.iter().map(|s| (*s).to_string()).collect(),
            },
        )
        .await
    }

    pub async fn send_hello_ack(
        &self,
        peer: SocketAddr,
        hostname: &str,
    ) -> Result<(), ControlError> {
        self.send_hello_ack_with_caps(peer, hostname, &[]).await
    }

    pub async fn send_hello_ack_with_caps(
        &self,
        peer: SocketAddr,
        hostname: &str,
        capabilities: &[&str],
    ) -> Result<(), ControlError> {
        self.send_typed(
            peer,
            TYPE_HELLO_ACK,
            &HelloAck {
                hostname: hostname.into(),
                capabilities: capabilities.iter().map(|s| (*s).to_string()).collect(),
            },
        )
        .await
    }

    pub async fn send_keepalive(&self, peer: SocketAddr) -> Result<(), ControlError> {
        self.send_typed(peer, TYPE_KEEPALIVE, &Keepalive {}).await
    }

    pub async fn send_goodbye(&self, peer: SocketAddr) -> Result<(), ControlError> {
        self.send_typed(peer, TYPE_GOODBYE, &Goodbye {}).await
    }

    async fn send_typed<T: Serialize>(
        &self,
        peer: SocketAddr,
        type_: u8,
        payload: &T,
    ) -> Result<(), ControlError> {
        let env = ControlEnvelope {
            type_,
            payload: serde_json::to_value(payload)?,
        };
        let bytes = serde_json::to_vec(&env)?;
        self.transport
            .send(Outbound {
                to: peer,
                channel_type: ChannelType::Control,
                channel_id: 0,
                plaintext: bytes,
                priority: 0,
            })
            .await?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Option<ControlEvent> {
        self.inbound.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_omits_capabilities_when_empty() {
        // v1.0 wire compat: a v1.0 peer parses {"hostname": "x"} only.
        let h = Hello {
            hostname: "alice".into(),
            capabilities: Vec::new(),
        };
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, r#"{"hostname":"alice"}"#);
    }

    #[test]
    fn hello_includes_capabilities_when_set() {
        let h = Hello {
            hostname: "alice".into(),
            capabilities: vec![capability::DMABUF_V1.into(), capability::AF_UNIX.into()],
        };
        let json = serde_json::to_string(&h).unwrap();
        let parsed: Hello = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.hostname, "alice");
        assert_eq!(parsed.capabilities, h.capabilities);
    }

    #[test]
    fn hello_parses_v1_0_payload() {
        // v1.1 peer must accept v1.0 HELLO (no `capabilities` field).
        let v1_0_json = r#"{"hostname":"bob"}"#;
        let parsed: Hello = serde_json::from_str(v1_0_json).unwrap();
        assert_eq!(parsed.hostname, "bob");
        assert!(parsed.capabilities.is_empty());
    }

    #[test]
    fn hello_ignores_unknown_capabilities() {
        let json = r#"{"hostname":"x","capabilities":["something-weird","dmabuf-v1"]}"#;
        let parsed: Hello = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.capabilities.len(), 2);
        assert!(parsed
            .capabilities
            .iter()
            .any(|c| c == capability::DMABUF_V1));
    }
}
