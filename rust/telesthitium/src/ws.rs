//! WebSocket / WSS transport (SPEC §9.3).
//!
//! A peer connects outbound over WS(S); the URL path (`/band`) selects the
//! endpoint. Each **binary** WS message carries exactly one Telesthete frame —
//! the WS frame preserves the boundary, so no length prefix is added (§9.3).
//! The band is learned from the first frame's cleartext `band_id`, exactly as
//! UDP learns it (§10 implicit discovery).
//!
//! TLS terminates in-hub when an identity is configured (native WSS); with no
//! identity the listener speaks plain WS for deployments where a reverse proxy
//! (nginx / Cloudflare) terminates TLS.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

use crate::frame;
use crate::registry::{PeerKey, Registry, Sink};
use crate::tls::HubCert;

/// Default endpoint path a peer connects to.
pub const DEFAULT_WS_PATH: &str = "/band";

/// Serve WS/WSS until `shutdown` resolves.
///
/// `tls = Some(cert)` terminates native TLS in-hub; `None` speaks plain WS.
pub async fn serve(
    bind: SocketAddr,
    registry: Arc<Registry>,
    conn_queue: usize,
    path: String,
    tls: Option<HubCert>,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    let local = listener.local_addr()?;
    let acceptor = match &tls {
        Some(cert) => {
            tracing::info!(%local, path = %path, "wss transport listening (native TLS)");
            Some(build_acceptor(cert)?)
        }
        None => {
            tracing::info!(%local, path = %path, "ws transport listening (plain; TLS upstream)");
            None
        }
    };

    let accept_loop = async {
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, "ws accept failed");
                    continue;
                }
            };
            let registry = registry.clone();
            let path = path.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                match acceptor {
                    Some(acc) => match acc.accept(tcp).await {
                        Ok(tls_stream) => {
                            serve_conn(tls_stream, registry, conn_queue, &path).await
                        }
                        Err(e) => tracing::debug!(%peer, error = %e, "tls handshake failed"),
                    },
                    None => serve_conn(tcp, registry, conn_queue, &path).await,
                }
            });
        }
    };

    tokio::select! {
        _ = accept_loop => {},
        _ = shutdown => tracing::info!("ws transport shutting down"),
    }
    Ok(())
}

fn build_acceptor(cert: &HubCert) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    // Make sure a process-wide CryptoProvider exists. Harmless if already set.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let chain = vec![CertificateDer::from(cert.cert_der.clone())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der.clone()));
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    // WS upgrade rides HTTP/1.1.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// Drive one accepted (already TLS-wrapped, if applicable) connection.
// The tungstenite handshake callback must return its own large `ErrorResponse`
// type (an `http::Response`); boxing it isn't possible through the trait.
#[allow(clippy::result_large_err)]
async fn serve_conn<S>(stream: S, registry: Arc<Registry>, conn_queue: usize, want_path: &str)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // Handshake, enforcing the endpoint path (F1). A rejected path or failed
    // handshake just closes the connection quietly.
    let ws = {
        let cb = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            if req.uri().path() == want_path {
                Ok(resp)
            } else {
                let mut err = ErrorResponse::new(Some("not found".to_string()));
                *err.status_mut() = StatusCode::NOT_FOUND;
                Err(err)
            }
        };
        match accept_hdr_async(stream, cb).await {
            Ok(ws) => ws,
            Err(_) => return,
        }
    };

    let (mut ws_tx, mut ws_rx) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Arc<[u8]>>(conn_queue.max(64));

    // Writer: one WS binary message per relayed frame (F2 — no length prefix).
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if ws_tx
                .send(Message::Binary(Bytes::copy_from_slice(&frame)))
                .await
                .is_err()
            {
                break;
            }
        }
        let _ = ws_tx.close().await;
    });

    // Reader: learn band from the first frame, relay every frame.
    let mut registered: Option<(telesthete::BandId, PeerKey)> = None;
    while let Some(msg) = ws_rx.next().await {
        let Ok(msg) = msg else { break };
        match msg {
            Message::Binary(data) => {
                let Some(info) = frame::route_info(&data) else {
                    continue; // malformed; drop
                };
                let key = match registered {
                    Some((_, k)) => {
                        registry.touch(&info.band_id, &k);
                        k
                    }
                    None => {
                        let key = PeerKey::Conn(registry.next_conn_id());
                        if registry
                            .connect(info.band_id, key, Sink::Conn(out_tx.clone()))
                            .is_err()
                        {
                            break; // cap hit — refuse the peer
                        }
                        registered = Some((info.band_id, key));
                        key
                    }
                };
                let pkt: Arc<[u8]> = Arc::from(&data[..]);
                registry.forward(&info.band_id, &key, pkt);
            }
            Message::Close(_) => break,
            _ => {} // Ping/Pong handled by the library; Text ignored
        }
    }

    if let Some((band, key)) = registered {
        registry.disconnect(&band, &key);
    }
    drop(out_tx);
    let _ = writer.await;
}
