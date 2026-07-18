//! WebTransport / HTTP-3 transport (SPEC §9.6).
//!
//! A peer connects to `https://<hub>/telesthete?band=<band_id_hex>` (ALPN `h3`,
//! UDP/443). The band comes from the query; every frame on the session belongs
//! to it. The hub bridges by `band_id` exactly as the UDP/WS relay does and
//! never holds the PSK.
//!
//! ## Carrier mapping (§9.6)
//! QUIC gives both unreliable datagrams and reliable ordered streams, a near
//! 1:1 fit for Telesthete's channel types. The hub maps each frame by its
//! cleartext `channel_type`:
//!
//! | Telesthete     | WebTransport carrier                     | Framing            |
//! |----------------|------------------------------------------|--------------------|
//! | Stream  (0x01) | datagram                                 | one datagram/frame |
//! | Channel (0x02) | one bidi stream per `channel_id`         | 2-byte BE length   |
//! | Control (0x00) | one dedicated bidi stream                | 2-byte BE length   |
//! | Board/Drop/unk | the dedicated reliable stream (no loss)  | 2-byte BE length   |
//!
//! QUIC streams are byte streams, not message streams, so every frame on a
//! reliable carrier is preceded by a 2-byte big-endian length
//! (`WT_STREAM_LEN_PREFIX`). Datagrams preserve boundaries and need none.
//!
//! Ingress reverses this: datagrams are one frame each; reliable streams are
//! de-framed by the length prefix back into whole frames before relay. Bridging
//! is bidirectional — the recv half of every stream (hub-opened or peer-opened)
//! is read.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use telesthete::{BandId, ChannelType};
use wtransport::endpoint::IncomingSession;
use wtransport::{Connection, Endpoint, Identity, RecvStream, SendStream, ServerConfig, VarInt};

use crate::frame;
use crate::registry::{PeerKey, Registry, Sink};
use crate::tls::HubCert;

/// Endpoint route (the query carries `?band=<hex>`).
pub const DEFAULT_WT_PATH: &str = "/telesthete";
/// Per-packet length prefix on reliable WT streams (§12.4).
const WT_STREAM_LEN_PREFIX: usize = 2;

/// Serve WebTransport until `shutdown` resolves. Requires a TLS identity (QUIC
/// mandates TLS 1.3).
pub async fn serve(
    bind: SocketAddr,
    registry: Arc<Registry>,
    conn_queue: usize,
    cert: HubCert,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = ServerConfig::builder()
        .with_bind_address(bind)
        .with_identity(identity_from(&cert)?)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .build();
    let server = Endpoint::server(config)?;
    tracing::info!(%bind, cert_sha256 = %cert.sha256_hex(), "webtransport transport listening");

    let accept_loop = async {
        loop {
            let incoming = server.accept().await;
            let registry = registry.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_session(incoming, registry, conn_queue).await {
                    tracing::debug!(error = %e, "wt session ended");
                }
            });
        }
    };

    tokio::select! {
        _ = accept_loop => {},
        _ = shutdown => tracing::info!("webtransport transport shutting down"),
    }
    Ok(())
}

fn identity_from(cert: &HubCert) -> anyhow::Result<Identity> {
    use wtransport::tls::{Certificate, CertificateChain, PrivateKey};
    let c = Certificate::from_der(cert.cert_der.clone())?;
    let k = PrivateKey::from_der_pkcs8(cert.key_der.clone());
    Ok(Identity::new(CertificateChain::single(c), k))
}

/// Parse `/telesthete?band=<hex>` into a band id (conformance G1).
fn parse_band(path: &str) -> Option<BandId> {
    let (route, query) = path.split_once('?')?;
    if route != DEFAULT_WT_PATH {
        return None;
    }
    let hex = query.split('&').find_map(|kv| kv.strip_prefix("band="))?;
    frame::band_from_hex(hex)
}

async fn handle_session(
    incoming: IncomingSession,
    registry: Arc<Registry>,
    conn_queue: usize,
) -> anyhow::Result<()> {
    let request = incoming.await?;
    let band = match parse_band(request.path()) {
        Some(b) => b,
        None => {
            request.not_found().await;
            return Ok(());
        }
    };

    let conn = Arc::new(request.accept().await?);
    let key = PeerKey::Conn(registry.next_conn_id());
    let (out_tx, out_rx) = tokio::sync::mpsc::channel::<Arc<[u8]>>(conn_queue.max(64));
    if registry.connect(band, key, Sink::Conn(out_tx)).is_err() {
        conn.close(VarInt::from_u32(1), b"registry full");
        return Ok(());
    }
    tracing::info!(band = %frame::band_hex(&band), "wt peer joined");

    let egress = tokio::spawn(egress_writer(conn.clone(), out_rx, registry.clone(), band, key));
    let datagrams = {
        let (c, r) = (conn.clone(), registry.clone());
        tokio::spawn(async move { ingress_datagrams(c, r, band, key).await })
    };
    let streams = {
        let (c, r) = (conn.clone(), registry.clone());
        tokio::spawn(async move { ingress_streams(c, r, band, key).await })
    };

    let _ = conn.closed().await;
    registry.disconnect(&band, &key);
    egress.abort();
    datagrams.abort();
    streams.abort();
    Ok(())
}

/// Drain relayed frames and place each on its §9.6 carrier.
// The per-channel stream map is filled by an async open, which the Entry API
// can't express, so an explicit contains/insert is used deliberately.
#[allow(clippy::map_entry)]
async fn egress_writer(
    conn: Arc<Connection>,
    mut out_rx: tokio::sync::mpsc::Receiver<Arc<[u8]>>,
    registry: Arc<Registry>,
    band: BandId,
    key: PeerKey,
) {
    let mut channels: HashMap<u16, SendStream> = HashMap::new();
    let mut control: Option<SendStream> = None;

    while let Some(frame) = out_rx.recv().await {
        let Some(info) = frame::route_info(&frame) else {
            continue;
        };
        match info.channel_type {
            Some(ChannelType::Stream) => {
                // Unreliable datagram, one per frame, no length prefix (G2).
                if let Err(e) = conn.send_datagram(&frame[..]) {
                    tracing::debug!(error = %e, "wt datagram send failed");
                }
            }
            Some(ChannelType::Channel) => {
                // One reliable bidi stream per channel_id (G3). Open lazily; the
                // async open can't use the Entry API, hence the explicit lookup.
                let cid = info.channel_id;
                if !channels.contains_key(&cid) {
                    if let Some(s) = open_egress_stream(&conn, &registry, band, key).await {
                        channels.insert(cid, s);
                    }
                }
                if let Some(s) = channels.get_mut(&cid) {
                    if !write_framed(s, &frame).await {
                        channels.remove(&cid);
                    }
                }
            }
            // Control, or any type without a defined WT carrier (Board/Drop/
            // unknown): the single dedicated reliable stream — no loss (G4).
            _ => {
                if control.is_none() {
                    control = open_egress_stream(&conn, &registry, band, key).await;
                }
                if let Some(s) = control.as_mut() {
                    if !write_framed(s, &frame).await {
                        control = None;
                    }
                }
            }
        }
    }
}

/// Open a hub-initiated bidi stream, spawning a reader on its recv half so a
/// peer replying on the same stream is bridged too.
async fn open_egress_stream(
    conn: &Arc<Connection>,
    registry: &Arc<Registry>,
    band: BandId,
    key: PeerKey,
) -> Option<SendStream> {
    let opening = conn.open_bi().await.ok()?;
    let (send, recv) = opening.await.ok()?;
    let r = registry.clone();
    tokio::spawn(async move { read_framed_stream(recv, r, band, key).await });
    Some(send)
}

/// Write one length-prefixed frame. Returns false if the stream failed (caller
/// drops it). Frames always fit in a u16 (`MAX_CHANNEL_DATA` = 1024, §12.4).
async fn write_framed(stream: &mut SendStream, frame: &[u8]) -> bool {
    if frame.len() > u16::MAX as usize {
        tracing::warn!(len = frame.len(), "frame too large for a WT stream; dropping");
        return true; // "handled" — keep the stream
    }
    let len = (frame.len() as u16).to_be_bytes();
    if stream.write_all(&len).await.is_err() {
        return false;
    }
    stream.write_all(frame).await.is_ok()
}

async fn ingress_datagrams(
    conn: Arc<Connection>,
    registry: Arc<Registry>,
    band: BandId,
    key: PeerKey,
) {
    while let Ok(dg) = conn.receive_datagram().await {
        if let Some(info) = frame::route_info(&dg) {
            registry.touch(&band, &key);
            registry.forward(&info.band_id, &key, Arc::from(&dg[..]));
        }
    }
}

async fn ingress_streams(
    conn: Arc<Connection>,
    registry: Arc<Registry>,
    band: BandId,
    key: PeerKey,
) {
    while let Ok((_send, recv)) = conn.accept_bi().await {
        let r = registry.clone();
        tokio::spawn(async move { read_framed_stream(recv, r, band, key).await });
    }
}

/// Read length-prefixed frames off one reliable stream and relay each (G5).
async fn read_framed_stream(
    mut recv: RecvStream,
    registry: Arc<Registry>,
    band: BandId,
    key: PeerKey,
) {
    loop {
        let mut len_buf = [0u8; WT_STREAM_LEN_PREFIX];
        if recv.read_exact(&mut len_buf).await.is_err() {
            break; // clean FIN or reset
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u8; len];
        if recv.read_exact(&mut buf).await.is_err() {
            break;
        }
        if let Some(info) = frame::route_info(&buf) {
            registry.touch(&band, &key);
            registry.forward(&info.band_id, &key, Arc::from(buf.as_slice()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_band_from_query() {
        // G1 — band comes from the ?band= query on the right route.
        let hex = "0a".repeat(16);
        let path = format!("/telesthete?band={hex}");
        assert_eq!(parse_band(&path), Some([0x0a; 16]));
    }

    #[test]
    fn parse_band_rejects_wrong_route_or_missing() {
        let hex = "0a".repeat(16);
        assert_eq!(parse_band(&format!("/nope?band={hex}")), None);
        assert_eq!(parse_band("/telesthete"), None);
        assert_eq!(parse_band("/telesthete?nope=1"), None);
        assert_eq!(parse_band("/telesthete?band=xyz"), None);
    }
}
