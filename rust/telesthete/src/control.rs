//! Control channel — band management, signaling. SPEC §4.
//!
//! Always reliable. Payload is JSON-encoded `{ "type": <u8>, "payload": {...} }`.
//! M0 supports HELLO / HELLO_ACK / KEEPALIVE / GOODBYE.

use std::collections::HashMap;
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

/// Capability strings advertised in HELLO / HELLO_ACK per Telesthete §12.5.
/// As of v1.2 capability announce is mandatory; unknown capabilities are
/// still ignored (forward-compatible).
pub mod capability {
    pub const DMABUF_V1: &str = "dmabuf-v1";
    pub const AF_UNIX: &str = "af-unix";
    pub const SYNC_FILE: &str = "sync-file";
    pub const REUSE_V1: &str = "reuse-v1";
    pub const WEBTRANSPORT: &str = "webtransport";
    pub const KEYFRAME_REQ: &str = "keyframe-req";
    pub const RATE_HINT: &str = "rate-hint";
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
    /// Mandatory as of v1.2 (§12.5). `default` on parse for robustness.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Ordered AEAD preference list (§3.5). MUST include the baseline.
    #[serde(default)]
    pub ciphers: Vec<String>,
    /// Monotonic session epoch (§4.3). A newer value rebases the receiver's
    /// replay watermark after a restart; `default` -> 0 for older peers.
    #[serde(default)]
    pub session: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HelloAck {
    pub hostname: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub ciphers: Vec<String>,
    /// Committed negotiated suite, chosen by the responder (§3.5).
    #[serde(default)]
    pub cipher: String,
    /// Monotonic session epoch (§4.3), as in [`Hello`].
    #[serde(default)]
    pub session: u64,
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
        ciphers: Vec<String>,
    },
    HelloAck {
        from: SocketAddr,
        hostname: String,
        capabilities: Vec<String>,
        ciphers: Vec<String>,
        cipher: String,
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
    /// This sender's session epoch (§4.3), advertised in HELLO/HELLO_ACK so a
    /// peer rebases its replay watermark when we restart.
    session: u64,
}

impl ControlChannel {
    pub async fn new(
        transport: Arc<Transport>,
        psk: Vec<u8>,
        session: u64,
        rebase_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    ) -> Self {
        let mut raw = transport.route(ChannelType::Control).await;
        let (tx, rx) = mpsc::unbounded_channel();
        let task_transport = Arc::clone(&transport);
        tokio::spawn(async move {
            // Replay protection (SPEC §3.3): highest accepted sequence per peer;
            // `sessions` tracks each peer's epoch for restart rebase. Transport
            // already authenticated the packet, so this runs on trusted plaintext.
            let mut watermark: HashMap<SocketAddr, u64> = HashMap::new();
            let mut sessions: HashMap<SocketAddr, u64> = HashMap::new();
            while let Some(pkt) = raw.recv().await {
                let seq = pkt.header.sequence;
                let env = match serde_json::from_slice::<ControlEnvelope>(&pkt.payload) {
                    Ok(e) => e,
                    Err(e) => {
                        debug!("bad control envelope from {}: {e}", pkt.from);
                        continue;
                    }
                };
                match env.type_ {
                    TYPE_HELLO | TYPE_HELLO_ACK => {
                        // Parse the typed body FIRST so a bad-body packet never
                        // mutates replay/session state.
                        let (event, ep) = if env.type_ == TYPE_HELLO {
                            match serde_json::from_value::<Hello>(env.payload) {
                                Ok(h) => (
                                    ControlEvent::Hello {
                                        from: pkt.from,
                                        hostname: h.hostname,
                                        capabilities: h.capabilities,
                                        ciphers: h.ciphers,
                                    },
                                    h.session,
                                ),
                                Err(e) => {
                                    debug!("bad HELLO: {e}");
                                    continue;
                                }
                            }
                        } else {
                            match serde_json::from_value::<HelloAck>(env.payload) {
                                Ok(h) => (
                                    ControlEvent::HelloAck {
                                        from: pkt.from,
                                        hostname: h.hostname,
                                        capabilities: h.capabilities,
                                        ciphers: h.ciphers,
                                        cipher: h.cipher,
                                    },
                                    h.session,
                                ),
                                Err(e) => {
                                    debug!("bad HELLO_ACK: {e}");
                                    continue;
                                }
                            }
                        };
                        // Session (re)start (SPEC §3.3/§4.3): a strictly-newer
                        // epoch re-keys the peer (new data key via
                        // register_peer_key), clears its control watermark, and
                        // signals the StreamHub to clear its stream watermarks —
                        // so a restarted peer is accepted and its old-session
                        // packets fail authentication under the new key.
                        if sessions.get(&pkt.from).map_or(true, |&prev| ep > prev) {
                            sessions.insert(pkt.from, ep);
                            let peer_key = crate::crypto::derive_session_key(
                                &psk,
                                crate::crypto::BASELINE_CIPHER,
                                ep,
                            );
                            task_transport.register_peer_key(pkt.from, peer_key).await;
                            watermark.remove(&pkt.from);
                            let _ = rebase_tx.send(pkt.from);
                        }
                        if matches!(watermark.get(&pkt.from), Some(&wm) if seq <= wm) {
                            debug!("drop replayed control HELLO seq={seq} from {}", pkt.from);
                            continue;
                        }
                        watermark.insert(pkt.from, seq);
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    _ => {
                        if matches!(watermark.get(&pkt.from), Some(&wm) if seq <= wm) {
                            debug!("drop replayed/stale control seq={seq} from {}", pkt.from);
                            continue;
                        }
                        watermark.insert(pkt.from, seq);
                        let event = match env.type_ {
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
                }
            }
        });

        Self {
            transport,
            inbound: rx,
            session,
        }
    }

    /// Send a HELLO with baseline-only ciphers and no extra capabilities.
    pub async fn send_hello(&self, peer: SocketAddr, hostname: &str) -> Result<(), ControlError> {
        self.send_hello_full(peer, hostname, &[], &[crate::crypto::BASELINE_CIPHER])
            .await
    }

    /// v1.2 §3.5/§12.5: HELLO advertising capabilities + ordered ciphers
    /// (which MUST include the baseline).
    pub async fn send_hello_full(
        &self,
        peer: SocketAddr,
        hostname: &str,
        capabilities: &[&str],
        ciphers: &[&str],
    ) -> Result<(), ControlError> {
        self.send_typed(
            peer,
            TYPE_HELLO,
            &Hello {
                hostname: hostname.into(),
                capabilities: capabilities.iter().map(|s| (*s).to_string()).collect(),
                ciphers: ciphers.iter().map(|s| (*s).to_string()).collect(),
                session: self.session,
            },
        )
        .await
    }

    /// Send a HELLO_ACK committing the baseline suite.
    pub async fn send_hello_ack(
        &self,
        peer: SocketAddr,
        hostname: &str,
    ) -> Result<(), ControlError> {
        self.send_hello_ack_full(
            peer,
            hostname,
            &[],
            &[crate::crypto::BASELINE_CIPHER],
            crate::crypto::BASELINE_CIPHER,
        )
        .await
    }

    /// v1.2 §3.5: HELLO_ACK committing the negotiated `cipher`.
    pub async fn send_hello_ack_full(
        &self,
        peer: SocketAddr,
        hostname: &str,
        capabilities: &[&str],
        ciphers: &[&str],
        cipher: &str,
    ) -> Result<(), ControlError> {
        self.send_typed(
            peer,
            TYPE_HELLO_ACK,
            &HelloAck {
                hostname: hostname.into(),
                capabilities: capabilities.iter().map(|s| (*s).to_string()).collect(),
                ciphers: ciphers.iter().map(|s| (*s).to_string()).collect(),
                cipher: cipher.into(),
                session: self.session,
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
                // HELLO/HELLO_ACK use the base key so a receiver can bootstrap
                // our epoch; other control messages use the session data key.
                use_base_key: matches!(type_, TYPE_HELLO | TYPE_HELLO_ACK),
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

    use crate::crypto::select_cipher;

    #[test]
    fn hello_serializes_capabilities_and_ciphers() {
        let h = Hello {
            hostname: "alice".into(),
            capabilities: vec![capability::AF_UNIX.into(), capability::WEBTRANSPORT.into()],
            ciphers: vec!["aes256-gcm".into(), "chacha20-poly1305".into()],
            session: 0,
        };
        let parsed: Hello = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(parsed.hostname, "alice");
        assert_eq!(parsed.capabilities, h.capabilities);
        assert_eq!(parsed.ciphers, h.ciphers);
    }

    #[test]
    fn hello_round_trips_session_epoch() {
        // §4.3 restart-safety field survives serialize/parse; absent -> 0.
        let h = Hello {
            hostname: "a".into(),
            capabilities: vec![],
            ciphers: vec!["chacha20-poly1305".into()],
            session: 1_737_000_000_000,
        };
        let parsed: Hello = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(parsed.session, 1_737_000_000_000);
        let minimal: Hello = serde_json::from_str(r#"{"hostname":"b"}"#).unwrap();
        assert_eq!(minimal.session, 0);
    }

    #[test]
    fn select_cipher_normalizes_missing_baseline() {
        // §3.5/§12.5: a responder list without the baseline still resolves.
        assert_eq!(
            select_cipher(&["chacha20-poly1305".into()], &["aes256-gcm".into()]),
            "chacha20-poly1305"
        );
        assert_eq!(
            select_cipher(
                &["aes256-gcm".into()],
                &["aes256-gcm".into(), "chacha20-poly1305".into()]
            ),
            "aes256-gcm"
        );
    }

    #[test]
    fn hello_parse_tolerates_missing_fields() {
        // serde(default) keeps parsing robust to a minimal payload.
        let parsed: Hello = serde_json::from_str(r#"{"hostname":"bob"}"#).unwrap();
        assert_eq!(parsed.hostname, "bob");
        assert!(parsed.capabilities.is_empty());
        assert!(parsed.ciphers.is_empty());
    }

    #[test]
    fn hello_ignores_unknown_capabilities() {
        let json =
            r#"{"hostname":"x","capabilities":["weird","dmabuf-v1"],"ciphers":["chacha20-poly1305"]}"#;
        let parsed: Hello = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.capabilities.len(), 2);
        assert!(parsed.capabilities.iter().any(|c| c == capability::DMABUF_V1));
    }

    #[test]
    fn hello_ack_carries_committed_cipher() {
        let a = HelloAck {
            hostname: "b".into(),
            capabilities: vec![],
            ciphers: vec!["chacha20-poly1305".into()],
            cipher: "aes256-gcm".into(),
            session: 0,
        };
        let parsed: HelloAck = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
        assert_eq!(parsed.cipher, "aes256-gcm");
    }

    #[test]
    fn select_cipher_matches_spec() {
        let aes = "aes256-gcm".to_string();
        let cha = "chacha20-poly1305".to_string();
        // initiator's top mutually-supported choice
        assert_eq!(select_cipher(&[aes.clone(), cha.clone()], &[cha.clone(), aes.clone()]), aes);
        assert_eq!(select_cipher(&[cha.clone(), aes.clone()], &[cha.clone(), aes.clone()]), cha);
        // no overlap -> baseline
        assert_eq!(select_cipher(&[aes.clone()], &[cha.clone()]), cha);
    }
}
