//! AF_UNIX transport (SPEC §9.4).
//!
//! Same-host peers reach a band over a Unix-domain socket. The socket type is
//! `SOCK_SEQPACKET` (§9.4 preferred): connection-oriented and message-boundary
//! preserving, so one datagram carries exactly one Telesthete frame with no
//! length prefix (conformance I2). Tokio has no native async SEQPACKET, so a
//! `socket2` socket is driven through tokio's [`AsyncFd`].
//!
//! Addressing follows §9.4: one socket per band at
//! `$XDG_RUNTIME_DIR/telesthete/<band_id_hex>.sock`. The hub is the local
//! rendezvous — it binds a band's socket once that band is active on the hub
//! (from any transport) and local peers connect to it. A reconciliation loop
//! keeps the bound sockets in step with the live band set. The directory is the
//! access control and is created `0700` (I3).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use socket2::{Domain, SockAddr, Socket, Type};
use telesthete::BandId;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::frame;
use crate::registry::{PeerKey, Registry, Sink};

/// Default socket directory: `$XDG_RUNTIME_DIR/telesthete`, else `/tmp/telesthete`.
pub fn default_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("telesthete")
}

/// Serve AF_UNIX until `shutdown` resolves. Binds one `<band_id_hex>.sock`
/// listener per active band under `dir`, reconciled on a short interval.
pub async fn serve(
    dir: PathBuf,
    registry: Arc<Registry>,
    conn_queue: usize,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    ensure_dir(&dir)?;
    tracing::info!(dir = %dir.display(), "af_unix transport serving (SOCK_SEQPACKET)");

    let mut active: HashMap<BandId, (JoinHandle<()>, PathBuf)> = HashMap::new();

    let reconcile = async {
        let mut ticker = tokio::time::interval(Duration::from_millis(500));
        loop {
            ticker.tick().await;
            let want: HashSet<BandId> = registry.bands().into_iter().collect();

            for band in &want {
                if active.contains_key(band) {
                    continue;
                }
                let path = dir.join(format!("{}.sock", frame::band_hex(band)));
                match bind_seqpacket(&path) {
                    Ok(sock) => match AsyncFd::new(sock) {
                        Ok(afd) => {
                            let handle = tokio::spawn(accept_loop(
                                afd,
                                registry.clone(),
                                conn_queue,
                                *band,
                            ));
                            active.insert(*band, (handle, path));
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "af_unix AsyncFd registration failed");
                            let _ = std::fs::remove_file(&path);
                        }
                    },
                    Err(e) => {
                        tracing::warn!(band = %frame::band_hex(band), error = %e, "af_unix bind failed")
                    }
                }
            }

            active.retain(|band, (handle, path)| {
                if want.contains(band) {
                    true
                } else {
                    handle.abort();
                    let _ = std::fs::remove_file(path);
                    false
                }
            });
        }
    };

    tokio::select! {
        _ = reconcile => {},
        _ = shutdown => tracing::info!("af_unix transport shutting down"),
    }

    for (_band, (handle, path)) in active {
        handle.abort();
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

fn ensure_dir(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    // Directory permissions are the §9.4 access control (I3).
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

fn bind_seqpacket(path: &Path) -> io::Result<Socket> {
    let _ = std::fs::remove_file(path); // clear any stale socket
    let sock = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
    sock.bind(&SockAddr::unix(path)?)?;
    sock.listen(128)?;
    sock.set_nonblocking(true)?;
    Ok(sock)
}

/// Receive buffer per connection. A datagram larger than this is detected via
/// MSG_TRUNC and dropped (never relayed truncated), rather than silently
/// corrupting the frame.
const RECV_BUF: usize = 128 * 1024;

/// Accept SEQPACKET connections on one band's socket; each becomes a peer. A
/// transient `accept` error must not permanently kill this band's socket, so we
/// back off and retry rather than exiting the loop (#10).
async fn accept_loop(
    afd: AsyncFd<Socket>,
    registry: Arc<Registry>,
    conn_queue: usize,
    band: BandId,
) {
    loop {
        match accept(&afd).await {
            Ok(sock) => {
                if sock.set_nonblocking(true).is_err() {
                    continue;
                }
                match AsyncFd::new(sock) {
                    Ok(conn) => {
                        tokio::spawn(serve_peer(conn, registry.clone(), conn_queue, band));
                    }
                    Err(e) => tracing::debug!(error = %e, "af_unix conn registration failed"),
                }
            }
            Err(e) => {
                tracing::debug!(error = %e, "af_unix accept error; backing off and retrying");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn serve_peer(afd: AsyncFd<Socket>, registry: Arc<Registry>, conn_queue: usize, band: BandId) {
    let key = PeerKey::Conn(registry.next_conn_id());
    let (out_tx, mut out_rx) = mpsc::channel::<Arc<[u8]>>(conn_queue.max(64));
    if registry.connect(band, key, Sink::Conn(out_tx)).is_err() {
        return; // cap hit
    }
    tracing::info!(band = %frame::band_hex(&band), "af_unix peer joined");

    // One task drives both directions. A failed send (e.g. EMSGSIZE on an
    // oversized frame) or a closed socket breaks the loop and tears the peer
    // down — so a write-dead peer never lingers registered but undeliverable
    // (#9). SEQPACKET preserves message boundaries: one recv = one frame (I2).
    let mut buf = vec![0u8; RECV_BUF];
    loop {
        tokio::select! {
            r = recv_msg(&afd, &mut buf) => match r {
                // 0 = orderly close, or an invalid empty datagram; either way,
                // close the connection (an empty message is never a valid frame).
                Ok(0) => break,
                // MSG_TRUNC reports the true datagram length; if it exceeds the
                // buffer the frame is incomplete (missing its AEAD tag) — drop
                // it rather than relay corruption (#8), and keep serving.
                Ok(n) if n > buf.len() => {
                    tracing::warn!(bytes = n, cap = buf.len(), "af_unix frame exceeds buffer; dropping");
                }
                Ok(n) => {
                    if let Some(info) = frame::route_info(&buf[..n]) {
                        // Pin the connection to its band (#2): a frame for a
                        // different band_id is dropped, not relayed cross-band.
                        if info.band_id == band {
                            registry.touch(&band, &key);
                            registry.forward(&band, &key, Arc::from(&buf[..n]));
                        } else {
                            tracing::debug!("af_unix frame band_id != socket band; dropping");
                        }
                    }
                }
                Err(_) => break,
            },
            out = out_rx.recv() => match out {
                Some(f) => {
                    if send_msg(&afd, &f).await.is_err() {
                        break; // send failed -> tear down (do not linger deaf)
                    }
                }
                None => break, // registry dropped our sink
            },
        }
    }

    registry.disconnect(&band, &key);
}

// -- async SEQPACKET primitives over AsyncFd --------------------------------

async fn accept(afd: &AsyncFd<Socket>) -> io::Result<Socket> {
    loop {
        let mut guard = afd.readable().await?;
        match guard.try_io(|inner| inner.get_ref().accept()) {
            Ok(Ok((sock, _addr))) => return Ok(sock),
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => continue,
        }
    }
}

async fn send_msg(afd: &AsyncFd<Socket>, data: &[u8]) -> io::Result<usize> {
    loop {
        let mut guard = afd.writable().await?;
        match guard.try_io(|inner| inner.get_ref().send(data)) {
            Ok(res) => return res,
            Err(_would_block) => continue,
        }
    }
}

/// Receive one SEQPACKET message. Returns the datagram's **true** length (via
/// `MSG_TRUNC`) even when it exceeds `buf`, so the caller can detect and drop a
/// truncated frame rather than relay one missing its tail.
async fn recv_msg(afd: &AsyncFd<Socket>, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        let mut guard = afd.readable().await?;
        // socket2's recv wants `[MaybeUninit<u8>]`; `u8` and `MaybeUninit<u8>`
        // share layout, and we only ever read back the bytes recv actually
        // wrote, so viewing the initialized buffer as uninit is sound.
        let uninit: &mut [MaybeUninit<u8>] = unsafe {
            std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<MaybeUninit<u8>>(), buf.len())
        };
        match guard.try_io(|inner| inner.get_ref().recv_with_flags(uninit, libc::MSG_TRUNC)) {
            Ok(res) => return res,
            Err(_would_block) => continue,
        }
    }
}
