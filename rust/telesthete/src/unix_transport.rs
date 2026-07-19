//! AF_UNIX transport for Telesthete v1.1 §9.4.
//!
//! Same wire frame as the UDP transport (§1) and same AEAD (§3, with the
//! local profile of §3.4). The reason this lives in a separate module is
//! that we need raw `recvmsg` / `sendmsg` with `SCM_RIGHTS` ancillary
//! messages so that dmabuf fds can ride alongside Stream packets.
//!
//! ## Socket type
//!
//! v1.1 §9.4 prefers `SOCK_SEQPACKET`. Tokio's stable async UDS API only
//! covers `SOCK_DGRAM` first-class, so this module uses `SOCK_DGRAM` and
//! relies on the kernel's per-message-boundary behaviour for datagram
//! sockets. Spec permits this; v1.1 postmortem can move to SEQPACKET.
//!
//! ## Ownership of fds
//!
//! - Inbound: parsed `SCM_RIGHTS` fds are wrapped in [`OwnedFd`] before any
//!   fallible code runs, so a parse error on the rest of the packet still
//!   closes them.
//! - Outbound: callers borrow fds; the kernel duplicates them on `sendmsg`
//!   so the caller retains ownership.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram as StdUnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nix::sys::socket::{
    recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr,
};
use thiserror::Error;
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use crate::framing::{decode_packet, encode_packet, ChannelType, FramingError, Header};
use crate::transport::SequenceCounter;

/// v1.1 §12.4: 4 planes + 1 fence fd per Stream packet. Single definition lives
/// in `wire::dmabuf`; re-exported here so the two never drift.
pub use crate::wire::dmabuf::MAX_FDS_PER_PACKET;

/// XDG runtime dir env var (per the spec convention).
pub const SOCKET_DIR_ENV: &str = "XDG_RUNTIME_DIR";
/// Fallback when XDG_RUNTIME_DIR is unset (e.g. during test or sudo).
pub const SOCKET_DIR_FALLBACK: &str = "/tmp";

/// Inbound packet from an AF_UNIX peer. Carries any `SCM_RIGHTS` fds that
/// arrived in the same `recvmsg`.
#[derive(Debug)]
pub struct UnixInbound {
    pub from: PathBuf,
    pub header: Header,
    pub payload: Vec<u8>,
    pub fds: Vec<OwnedFd>,
}

/// Outbound packet for AF_UNIX. `fds` are borrowed; the kernel duplicates
/// them on `sendmsg`.
pub struct UnixOutbound<'a> {
    pub to: PathBuf,
    pub channel_type: ChannelType,
    pub channel_id: u16,
    pub plaintext: Vec<u8>,
    pub priority: u8,
    pub fds: &'a [BorrowedFd<'a>],
}

#[derive(Debug, Error)]
pub enum UnixTransportError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("nix: {0}")]
    Nix(#[from] nix::Error),
    #[error("framing: {0}")]
    Framing(#[from] FramingError),
    #[error("too many fds: {0} (max {MAX_FDS_PER_PACKET})")]
    TooManyFds(usize),
    #[error("invalid socket path: {0}")]
    InvalidPath(String),
    #[error("transport closed")]
    Closed,
}

/// AF_UNIX transport runtime. Owns the bound socket and the recv loop.
pub struct UnixTransport {
    socket: Arc<AsyncFd<StdUnixDatagram>>,
    bind_path: PathBuf,
    key: crate::crypto::Key,
    band_id: [u8; 16],
    seq: Arc<SequenceCounter>,
    routes: Arc<Mutex<HashMap<ChannelType, mpsc::UnboundedSender<UnixInbound>>>>,
}

impl UnixTransport {
    /// Bind a SOCK_DGRAM socket at `path`. The path's parent directory must
    /// already exist; permissions on it are the access control.
    pub async fn bind(
        path: impl AsRef<Path>,
        key: crate::crypto::Key,
        band_id: [u8; 16],
    ) -> Result<Self, UnixTransportError> {
        let path = path.as_ref().to_path_buf();
        // Bind synchronously — std stops at the bind syscall, which is cheap.
        // Remove a stale leftover (common when a previous run crashed).
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(UnixTransportError::Io(e)),
        }
        let std_sock = StdUnixDatagram::bind(&path)?;
        std_sock.set_nonblocking(true)?;
        let socket = Arc::new(AsyncFd::new(std_sock)?);
        debug!(?path, "telesthete unix transport bound");
        Ok(Self {
            socket,
            bind_path: path,
            key,
            band_id,
            seq: Arc::new(SequenceCounter::default()),
            routes: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn bind_path(&self) -> &Path {
        &self.bind_path
    }

    pub fn band_id(&self) -> [u8; 16] {
        self.band_id
    }

    pub async fn route(&self, ty: ChannelType) -> mpsc::UnboundedReceiver<UnixInbound> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.lock().await.insert(ty, tx);
        rx
    }

    /// Encrypt + send one packet, with optional fds piggybacking via
    /// `SCM_RIGHTS`.
    pub async fn send(&self, out: UnixOutbound<'_>) -> Result<(), UnixTransportError> {
        if out.fds.len() > MAX_FDS_PER_PACKET {
            return Err(UnixTransportError::TooManyFds(out.fds.len()));
        }
        let seq = self.seq.next();
        let pkt = encode_packet(
            &self.key,
            &self.band_id,
            out.channel_type,
            out.channel_id,
            seq,
            &out.plaintext,
        )?;
        let dest = UnixAddr::new(out.to.as_path())?;
        let raw_fds: Vec<RawFd> = out.fds.iter().map(|f| f.as_raw_fd()).collect();

        loop {
            let mut guard = self.socket.writable().await?;
            let result = guard.try_io(|inner| {
                let fd = inner.get_ref().as_raw_fd();
                let iov = [io::IoSlice::new(&pkt)];
                let scm = ControlMessage::ScmRights(&raw_fds);
                let cmsgs: &[ControlMessage] = if raw_fds.is_empty() {
                    &[]
                } else {
                    std::slice::from_ref(&scm)
                };
                // SAFETY: nix wraps the raw `fd` with a non-owning helper; the
                // socket itself remains owned by `inner`.
                let res = sendmsg::<UnixAddr>(fd, &iov, cmsgs, MsgFlags::empty(), Some(&dest));
                match res {
                    Ok(_) => Ok(()),
                    Err(nix::errno::Errno::EAGAIN) => Err(io::Error::from(io::ErrorKind::WouldBlock)),
                    Err(e) => Err(io::Error::from(e)),
                }
            });
            match result {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(e)) => return Err(UnixTransportError::Io(e)),
                Err(_would_block) => continue,
            }
        }
    }

    /// Spawn the recv loop. Decoded packets dispatch to subscribers
    /// registered via [`Self::route`].
    pub fn spawn_recv_loop(&self) -> tokio::task::JoinHandle<()> {
        let socket = Arc::clone(&self.socket);
        let routes = Arc::clone(&self.routes);
        let key = self.key;
        let band_id = self.band_id;

        tokio::spawn(async move {
            // Datagrams from a 1500-byte UDP world fit easily, but dmabuf
            // descriptors are tiny (~32 B). 64 KiB buffer is generous.
            let mut buf = vec![0u8; 65_535];

            loop {
                let triple = loop {
                    let mut guard = match socket.readable().await {
                        Ok(g) => g,
                        Err(e) => {
                            warn!("unix readable() error: {e}");
                            return;
                        }
                    };
                    let r = guard.try_io(|inner| {
                        let fd = inner.get_ref().as_raw_fd();
                        let mut iov = [io::IoSliceMut::new(&mut buf)];
                        // SCM_RIGHTS for up to MAX_FDS_PER_PACKET file
                        // descriptors. `cmsg_space!` returns a fresh Vec
                        // sized to fit; do it per-iter so we never reuse
                        // a buffer that may have residual cmsg state.
                        let mut cmsg_buf = nix::cmsg_space!([RawFd; MAX_FDS_PER_PACKET]);
                        let msg = recvmsg::<UnixAddr>(
                            fd,
                            &mut iov,
                            Some(&mut cmsg_buf),
                            MsgFlags::empty(),
                        );
                        match msg {
                            Ok(m) => {
                                let from = parse_addr(m.address);
                                let fds = match m.cmsgs() {
                                    Ok(it) => collect_fds(it),
                                    Err(e) => {
                                        warn!("cmsgs parse failed: {e}");
                                        Vec::new()
                                    }
                                };
                                Ok((m.bytes, from, fds))
                            }
                            Err(nix::errno::Errno::EAGAIN) => {
                                Err(io::Error::from(io::ErrorKind::WouldBlock))
                            }
                            Err(e) => Err(io::Error::from(e)),
                        }
                    });
                    match r {
                        Ok(Ok(triple)) => break triple,
                        Ok(Err(e)) => {
                            warn!("recvmsg failed: {e}");
                            // Skip this iteration, retry readable.
                            continue;
                        }
                        Err(_would_block) => continue,
                    }
                };

                let (n, from, fds) = triple;

                let (header, payload) = match decode_packet(&key, &buf[..n]) {
                    Ok(v) => v,
                    Err(e) => {
                        debug!("decode_packet from {from:?} failed: {e}");
                        // fds dropped here close themselves.
                        continue;
                    }
                };
                if header.band_id != band_id {
                    debug!("dropping unix packet from {from:?}: foreign band_id");
                    continue;
                }
                let inbound = UnixInbound {
                    from,
                    header,
                    payload,
                    fds,
                };
                tracing::trace!(
                    ?header.channel_type,
                    channel_id = header.channel_id,
                    seq = header.sequence,
                    payload_len = inbound.payload.len(),
                    fd_count = inbound.fds.len(),
                    "telesthete unix rx"
                );
                let routes = routes.lock().await;
                if let Some(tx) = routes.get(&header.channel_type) {
                    if tx.send(inbound).is_err() {
                        debug!("unix route for {:?} closed", header.channel_type);
                    }
                } else {
                    debug!("unix: no route for {:?}", header.channel_type);
                }
            }
        })
    }
}

impl Drop for UnixTransport {
    fn drop(&mut self) {
        // Best-effort: clean up the socket file. Ignore errors (path may
        // already be gone if another bind reused the slot, or if /tmp got
        // wiped under us).
        let _ = std::fs::remove_file(&self.bind_path);
    }
}

fn parse_addr(addr: Option<UnixAddr>) -> PathBuf {
    addr.and_then(|a| a.path().map(|p| p.to_path_buf()))
        .unwrap_or_else(PathBuf::new)
}

fn collect_fds<'a, I>(cmsgs: I) -> Vec<OwnedFd>
where
    I: IntoIterator<Item = ControlMessageOwned>,
{
    let mut out = Vec::new();
    for cmsg in cmsgs {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            for fd in fds {
                // SAFETY: kernel transferred ownership of the fd to us via
                // SCM_RIGHTS; wrapping in OwnedFd makes the close()
                // automatic on drop.
                out.push(unsafe { OwnedFd::from_raw_fd(fd) });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_band_id, derive_key};
    use std::os::fd::AsFd;
    use std::time::Duration;
    use tempfile::tempdir;

    fn psk_key_band(psk: &[u8]) -> (crate::crypto::Key, [u8; 16]) {
        (derive_key(psk), derive_band_id(psk))
    }

    #[tokio::test]
    async fn unix_loopback_no_fds() {
        let dir = tempdir().unwrap();
        let alice_path = dir.path().join("alice.sock");
        let bob_path = dir.path().join("bob.sock");
        let (key, band_id) = psk_key_band(b"local-test");

        let alice = UnixTransport::bind(&alice_path, key, band_id).await.unwrap();
        let bob = UnixTransport::bind(&bob_path, key, band_id).await.unwrap();
        let mut bob_in = bob.route(ChannelType::Stream).await;
        bob.spawn_recv_loop();

        alice
            .send(UnixOutbound {
                to: bob_path.clone(),
                channel_type: ChannelType::Stream,
                channel_id: 7,
                plaintext: b"hello unix".to_vec(),
                priority: 0,
                fds: &[],
            })
            .await
            .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(1), bob_in.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"hello unix");
        assert_eq!(got.header.channel_id, 7);
        assert_eq!(got.header.channel_type, ChannelType::Stream);
        assert!(got.fds.is_empty());
    }

    #[tokio::test]
    async fn unix_loopback_with_fd() {
        let dir = tempdir().unwrap();
        let alice_path = dir.path().join("alice.sock");
        let bob_path = dir.path().join("bob.sock");
        let (key, band_id) = psk_key_band(b"local-test-fd");

        let alice = UnixTransport::bind(&alice_path, key, band_id).await.unwrap();
        let bob = UnixTransport::bind(&bob_path, key, band_id).await.unwrap();
        let mut bob_in = bob.route(ChannelType::Stream).await;
        bob.spawn_recv_loop();

        // Send a pipe write-end as the fd. Receiver should be able to
        // write to it and the read-end (kept on alice's side) should see
        // the bytes.
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        alice
            .send(UnixOutbound {
                to: bob_path.clone(),
                channel_type: ChannelType::Stream,
                channel_id: 1,
                plaintext: b"with fd".to_vec(),
                priority: 0,
                fds: &[write_fd.as_fd()],
            })
            .await
            .unwrap();

        let got = tokio::time::timeout(Duration::from_secs(1), bob_in.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"with fd");
        assert_eq!(got.fds.len(), 1);

        // Drop alice's copy so the only writer left is bob's received fd.
        drop(write_fd);

        // Write through the fd that arrived on bob's side.
        let received_fd = got.fds.into_iter().next().unwrap();
        let n = nix::unistd::write(&received_fd, b"hello").unwrap();
        assert_eq!(n, 5);
        drop(received_fd);

        // Read on the original read-end.
        let mut buf = [0u8; 5];
        let n = nix::unistd::read(&read_fd, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");
    }

    #[tokio::test]
    async fn unix_foreign_band_dropped() {
        let dir = tempdir().unwrap();
        let alice_path = dir.path().join("alice.sock");
        let bob_path = dir.path().join("bob.sock");
        let (key_a, band_a) = psk_key_band(b"band-a");
        let (key_b, band_b) = psk_key_band(b"band-b");

        let alice = UnixTransport::bind(&alice_path, key_a, band_a).await.unwrap();
        let bob = UnixTransport::bind(&bob_path, key_b, band_b).await.unwrap();
        let mut bob_in = bob.route(ChannelType::Stream).await;
        bob.spawn_recv_loop();

        alice
            .send(UnixOutbound {
                to: bob_path.clone(),
                channel_type: ChannelType::Stream,
                channel_id: 1,
                plaintext: b"foreign".to_vec(),
                priority: 0,
                fds: &[],
            })
            .await
            .unwrap();

        let r = tokio::time::timeout(Duration::from_millis(200), bob_in.recv()).await;
        assert!(r.is_err(), "expected timeout (foreign-band drop)");
    }

    #[tokio::test]
    async fn unix_too_many_fds_rejected() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("a.sock");
        let (key, band_id) = psk_key_band(b"x");
        let t = UnixTransport::bind(&path, key, band_id).await.unwrap();

        // Make MAX_FDS_PER_PACKET + 1 pipe ends.
        let mut keepalive = Vec::new();
        let mut borrowed: Vec<BorrowedFd> = Vec::new();
        for _ in 0..(MAX_FDS_PER_PACKET + 1) {
            let (r, _w) = nix::unistd::pipe().unwrap();
            keepalive.push(r);
        }
        for fd in &keepalive {
            borrowed.push(fd.as_fd());
        }

        let dest = dir.path().join("does-not-exist.sock");
        let err = t
            .send(UnixOutbound {
                to: dest,
                channel_type: ChannelType::Stream,
                channel_id: 0,
                plaintext: vec![],
                priority: 0,
                fds: &borrowed,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, UnixTransportError::TooManyFds(n) if n == MAX_FDS_PER_PACKET + 1));
    }
}
