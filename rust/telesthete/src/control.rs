//! Control channel — band management, signaling. SPEC §4.
//!
//! Always reliable. Payload is JSON-encoded `{ "type": <u8>, "payload": {...} }`.
//! M0 supports HELLO / HELLO_ACK / KEEPALIVE / GOODBYE.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use crate::crypto::{select_cipher, Suite};
use crate::framing::ChannelType;
use crate::transport::{Outbound, Transport, TransportError};

/// Live state for one known peer, maintained by the control task (§4.3/§3.5).
#[derive(Debug, Clone)]
pub struct PeerState {
    pub hostname: String,
    pub capabilities: Vec<String>,
    /// Negotiated AEAD suite id committed for this peer (§3.5).
    pub cipher: String,
    /// The peer's session epoch from its latest HELLO/HELLO_ACK (§4.3).
    pub session_epoch: u64,
}

/// Shared peer registry: written by the control task, read by the Band's
/// keepalive/dead-peer loop and the application.
pub type Peers = Arc<Mutex<HashMap<SocketAddr, PeerState>>>;

/// Identity + negotiation preferences this endpoint advertises (§3.5/§12.5).
#[derive(Debug, Clone)]
pub struct ControlConfig {
    pub psk: Vec<u8>,
    /// Our session epoch (§4.3), advertised in HELLO/HELLO_ACK.
    pub session: u64,
    pub hostname: String,
    pub capabilities: Vec<String>,
    /// Ordered AEAD preference list; the mandatory baseline is appended if
    /// missing (§12.5).
    pub ciphers: Vec<String>,
}

pub const TYPE_HELLO: u8 = 0x01;
pub const TYPE_HELLO_ACK: u8 = 0x02;
pub const TYPE_KEEPALIVE: u8 = 0x03;
pub const TYPE_FOCUS_CHANGE: u8 = 0x04;
pub const TYPE_METACONTROL: u8 = 0x05;
pub const TYPE_GOODBYE: u8 = 0x06;
pub const TYPE_KEYFRAME_REQ: u8 = 0x07;
pub const TYPE_RATE_HINT: u8 = 0x08;

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

/// Stream consumer -> producer keyframe request (§4.9).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct KeyframeReq {
    pub stream_id: u16,
}

/// Stream consumer -> producer congestion feedback (§4.10).
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RateHint {
    pub stream_id: u16,
    pub target_bps: u32,
    #[serde(default)]
    pub loss: f32,
}

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
    KeyframeReq {
        from: SocketAddr,
        stream_id: u16,
    },
    RateHint {
        from: SocketAddr,
        stream_id: u16,
        target_bps: u32,
        loss: f32,
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
        config: ControlConfig,
        peers: Peers,
        rebase_tx: tokio::sync::broadcast::Sender<SocketAddr>,
    ) -> Self {
        let mut raw = transport.route(ChannelType::Control).await;
        let (tx, rx) = mpsc::unbounded_channel();
        let task_transport = Arc::clone(&transport);
        let session = config.session;
        let mut cfg = config;
        if !cfg.ciphers.iter().any(|c| c == crate::crypto::BASELINE_CIPHER) {
            cfg.ciphers.push(crate::crypto::BASELINE_CIPHER.to_string());
        }
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
                        // mutates replay/session state. `committed` is the
                        // negotiated suite: chosen by us for a HELLO (we are
                        // the responder), dictated by the responder for an ACK.
                        let is_hello = env.type_ == TYPE_HELLO;
                        let (event, ep, hostname, caps, committed) = if is_hello {
                            match serde_json::from_value::<Hello>(env.payload) {
                                Ok(h) => {
                                    let init = if h.ciphers.is_empty() {
                                        vec![crate::crypto::BASELINE_CIPHER.to_string()]
                                    } else {
                                        h.ciphers.clone()
                                    };
                                    let selected = select_cipher(&init, &cfg.ciphers);
                                    (
                                        ControlEvent::Hello {
                                            from: pkt.from,
                                            hostname: h.hostname.clone(),
                                            capabilities: h.capabilities.clone(),
                                            ciphers: h.ciphers,
                                        },
                                        h.session,
                                        h.hostname,
                                        h.capabilities,
                                        selected,
                                    )
                                }
                                Err(e) => {
                                    debug!("bad HELLO: {e}");
                                    continue;
                                }
                            }
                        } else {
                            match serde_json::from_value::<HelloAck>(env.payload) {
                                Ok(h) => {
                                    let committed = if Suite::from_id(&h.cipher).is_some() {
                                        h.cipher.clone()
                                    } else {
                                        if !h.cipher.is_empty() {
                                            warn!(
                                                "peer {} committed unknown cipher {:?}; \
                                                 falling back to baseline",
                                                pkt.from, h.cipher
                                            );
                                        }
                                        crate::crypto::BASELINE_CIPHER.to_string()
                                    };
                                    (
                                        ControlEvent::HelloAck {
                                            from: pkt.from,
                                            hostname: h.hostname.clone(),
                                            capabilities: h.capabilities.clone(),
                                            ciphers: h.ciphers,
                                            cipher: h.cipher,
                                        },
                                        h.session,
                                        h.hostname,
                                        h.capabilities,
                                        committed,
                                    )
                                }
                                Err(e) => {
                                    debug!("bad HELLO_ACK: {e}");
                                    continue;
                                }
                            }
                        };
                        // Epoch monotonicity (§4.3): an OLDER-epoch HELLO is a
                        // replay from before a restart — acting on it would
                        // downgrade the peer's keys and wedge its live session.
                        // Drop it entirely (no event, no ACK, no key change).
                        if matches!(sessions.get(&pkt.from), Some(&prev) if ep < prev) {
                            debug!("drop stale-epoch control from {} (epoch {ep})", pkt.from);
                            continue;
                        }
                        // Session (re)start (SPEC §3.3/§4.3): a strictly-newer
                        // epoch re-keys the peer, clears its control watermark,
                        // and signals the StreamHub to clear its stream
                        // watermarks — so a restarted peer is accepted and its
                        // old-session packets fail authentication.
                        if sessions.get(&pkt.from).map_or(true, |&prev| ep > prev) {
                            sessions.insert(pkt.from, ep);
                            watermark.remove(&pkt.from);
                            let _ = rebase_tx.send(pkt.from);
                        }
                        if matches!(watermark.get(&pkt.from), Some(&wm) if seq <= wm) {
                            debug!("drop replayed control HELLO seq={seq} from {}", pkt.from);
                            continue;
                        }
                        watermark.insert(pkt.from, seq);

                        // Negotiated data keys (§3.1/§3.5): decrypt the peer's
                        // data under ITS epoch, send ours under OUR epoch, both
                        // bound to the committed suite. Idempotent on repeats.
                        let suite = Suite::from_id(&committed)
                            .unwrap_or(Suite::ChaCha20Poly1305);
                        task_transport
                            .register_peer_key(
                                pkt.from,
                                crate::crypto::derive_session_key(&cfg.psk, &committed, ep),
                                suite,
                            )
                            .await;
                        task_transport
                            .register_send_key(
                                pkt.from,
                                crate::crypto::derive_session_key(
                                    &cfg.psk, &committed, session,
                                ),
                                suite,
                            )
                            .await;
                        peers.lock().await.insert(
                            pkt.from,
                            PeerState {
                                hostname,
                                capabilities: caps,
                                cipher: committed.clone(),
                                session_epoch: ep,
                            },
                        );

                        // Auto-answer a HELLO with our HELLO_ACK committing the
                        // selected suite (§3.5) — the Band drives the handshake
                        // itself, like the Python reference.
                        if is_hello {
                            let ack = HelloAck {
                                hostname: cfg.hostname.clone(),
                                capabilities: cfg.capabilities.clone(),
                                ciphers: cfg.ciphers.clone(),
                                cipher: committed,
                                session,
                            };
                            if let Err(e) = send_control_json(
                                &task_transport,
                                pkt.from,
                                TYPE_HELLO_ACK,
                                serde_json::to_value(&ack).unwrap_or_default(),
                                true,
                            )
                            .await
                            {
                                debug!("auto HELLO_ACK to {} failed: {e}", pkt.from);
                            }
                        }

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
                            TYPE_GOODBYE => {
                                // Peer is leaving: drop its registry entry and
                                // key/liveness state (§4.8).
                                peers.lock().await.remove(&pkt.from);
                                sessions.remove(&pkt.from);
                                watermark.remove(&pkt.from);
                                task_transport.forget_peer(pkt.from).await;
                                ControlEvent::Goodbye { from: pkt.from }
                            }
                            TYPE_KEYFRAME_REQ => {
                                match serde_json::from_value::<KeyframeReq>(env.payload) {
                                    Ok(k) => ControlEvent::KeyframeReq {
                                        from: pkt.from,
                                        stream_id: k.stream_id,
                                    },
                                    Err(e) => {
                                        debug!("bad KEYFRAME_REQ: {e}");
                                        continue;
                                    }
                                }
                            }
                            TYPE_RATE_HINT => match serde_json::from_value::<RateHint>(env.payload) {
                                Ok(r) => ControlEvent::RateHint {
                                    from: pkt.from,
                                    stream_id: r.stream_id,
                                    target_bps: r.target_bps,
                                    loss: r.loss,
                                },
                                Err(e) => {
                                    debug!("bad RATE_HINT: {e}");
                                    continue;
                                }
                            },
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

    /// Stream consumer -> producer: request a fresh keyframe (§4.9).
    pub async fn send_keyframe_req(
        &self,
        peer: SocketAddr,
        stream_id: u16,
    ) -> Result<(), ControlError> {
        self.send_typed(peer, TYPE_KEYFRAME_REQ, &KeyframeReq { stream_id })
            .await
    }

    /// Stream consumer -> producer: bitrate/loss feedback for a lossy Stream
    /// (§4.10). Advisory.
    pub async fn send_rate_hint(
        &self,
        peer: SocketAddr,
        stream_id: u16,
        target_bps: u32,
        loss: f32,
    ) -> Result<(), ControlError> {
        self.send_typed(
            peer,
            TYPE_RATE_HINT,
            &RateHint {
                stream_id,
                target_bps,
                loss,
            },
        )
        .await
    }

    async fn send_typed<T: Serialize>(
        &self,
        peer: SocketAddr,
        type_: u8,
        payload: &T,
    ) -> Result<(), ControlError> {
        send_control_json(
            &self.transport,
            peer,
            type_,
            serde_json::to_value(payload)?,
            // HELLO/HELLO_ACK use the base key so a receiver can bootstrap
            // our epoch; other control messages use the session data key.
            matches!(type_, TYPE_HELLO | TYPE_HELLO_ACK),
        )
        .await
    }

    pub async fn recv(&mut self) -> Option<ControlEvent> {
        self.inbound.recv().await
    }
}

/// Encode + send one control envelope. Shared by [`ControlChannel`], the
/// Band's keepalive loop, and `Band::connect_peer`.
pub(crate) async fn send_control_json(
    transport: &Transport,
    peer: SocketAddr,
    type_: u8,
    payload: serde_json::Value,
    use_base_key: bool,
) -> Result<(), ControlError> {
    let env = ControlEnvelope { type_, payload };
    let bytes = serde_json::to_vec(&env)?;
    transport
        .send(Outbound {
            to: peer,
            channel_type: ChannelType::Control,
            channel_id: 0,
            plaintext: bytes,
            priority: 0,
            use_base_key,
        })
        .await?;
    Ok(())
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
    fn keyframe_req_and_rate_hint_serde() {
        // §4.9/§4.10 round-trip; loss defaults to 0.
        let k: KeyframeReq =
            serde_json::from_str(&serde_json::to_string(&KeyframeReq { stream_id: 9 }).unwrap())
                .unwrap();
        assert_eq!(k.stream_id, 9);
        let r: RateHint =
            serde_json::from_str(r#"{"stream_id":9,"target_bps":2000000,"loss":0.05}"#).unwrap();
        assert_eq!((r.stream_id, r.target_bps), (9, 2_000_000));
        assert!((r.loss - 0.05).abs() < 1e-6);
        let r2: RateHint = serde_json::from_str(r#"{"stream_id":1,"target_bps":1}"#).unwrap();
        assert_eq!(r2.loss, 0.0);
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
        assert_eq!(
            select_cipher(std::slice::from_ref(&aes), std::slice::from_ref(&cha)),
            cha
        );
    }
}
