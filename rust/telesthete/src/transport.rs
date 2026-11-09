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
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::crypto::{Key, Suite};
use crate::framing::{decode_packet_suite, encode_packet_suite, ChannelType, FramingError, Header};

/// Inbound packet decoded into header + cleartext payload + sender address.
#[derive(Debug, Clone)]
pub struct Inbound {
    pub from: SocketAddr,
    pub header: Header,
    pub payload: Vec<u8>,
    /// When the datagram arrived (see [`ReceiveTime`]).
    pub received: ReceiveTime,
}

/// When a datagram arrived at this host. Local information only: nothing of
/// it is on the wire.
///
/// A packet is handed to the application through several tasks (the receive
/// loop, a hub, the endpoint's queue), each of which a busy host can delay;
/// an application that times arrivals (a delay-based congestion controller,
/// a jitter buffer) needs the time the packet actually arrived, not the time
/// it got to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveTime {
    /// The arrival, on the monotonic clock.
    pub at: Instant,
    /// Where `at` comes from.
    pub source: ReceiveTimeSource,
}

/// How a [`ReceiveTime`] was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveTimeSource {
    /// The kernel's software receive timestamp (`SO_TIMESTAMPNS`, Linux and
    /// Android): when the network stack received the datagram, however late
    /// this process read it.
    Kernel,
    /// The time the receive loop read the datagram from the socket: the
    /// fallback where the kernel gives no timestamp (other platforms, a
    /// kernel that refuses `SO_TIMESTAMPNS`, a datagram without one, or a
    /// wall clock stepped between arrival and read). Time the datagram
    /// waited in the socket buffer counts as arrival delay.
    Read,
}

/// Longest a kernel timestamp may precede the read before it is distrusted
/// (the wall clock it is taken on was stepped forward in between).
const MAX_SOCKET_WAIT: Duration = Duration::from_secs(10);

impl ReceiveTime {
    /// The time a datagram was read, now.
    pub fn read_now() -> Self {
        Self {
            at: Instant::now(),
            source: ReceiveTimeSource::Read,
        }
    }

    /// Map a kernel timestamp, taken on the wall clock (`CLOCK_REALTIME`),
    /// onto the monotonic clock: `read` minus how long before `wall` (the
    /// wall clock read together with `read`) the datagram arrived. A
    /// timestamp after `wall` or more than [`MAX_SOCKET_WAIT`] before it
    /// means the wall clock was stepped in between; the read time is used
    /// instead.
    pub fn from_kernel(stamp: SystemTime, wall: SystemTime, read: Instant) -> Self {
        match wall.duration_since(stamp) {
            Ok(waited) if waited <= MAX_SOCKET_WAIT => match read.checked_sub(waited) {
                Some(at) => Self {
                    at,
                    source: ReceiveTimeSource::Kernel,
                },
                None => Self {
                    at: read,
                    source: ReceiveTimeSource::Read,
                },
            },
            _ => Self {
                at: read,
                source: ReceiveTimeSource::Read,
            },
        }
    }
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
    /// Encrypt under the base key rather than the session data key. Set for
    /// HELLO/HELLO_ACK so a receiver can bootstrap our epoch (SPEC §3.1/§3.3).
    pub use_base_key: bool,
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
    /// Base key (HELLO/HELLO_ACK). Also the send/recv key until a session is
    /// configured, so a plain `bind` behaves exactly as before.
    base_key: Key,
    /// Our own per-session data key for sending non-HELLO packets (SPEC §3.3).
    /// Defaults to `base_key`; set via [`Self::set_session_key`]. Baseline
    /// suite; superseded per peer by `send_keys` once a cipher is negotiated.
    session_key: Key,
    /// Per-peer send keys: our epoch under the suite negotiated with that peer
    /// (§3.5). Falls back to `session_key` + baseline when absent.
    send_keys: Arc<Mutex<HashMap<SocketAddr, (Key, Suite)>>>,
    /// Per-peer session data keys + negotiated suite (derived from each peer's
    /// HELLO epoch) used to decrypt that peer's data packets. Updated on restart.
    peer_keys: Arc<Mutex<HashMap<SocketAddr, (Key, Suite)>>>,
    /// Last time an authenticated packet arrived from each peer. Drives §4.5
    /// dead-peer detection in the Band.
    last_seen: Arc<Mutex<HashMap<SocketAddr, std::time::Instant>>>,
    band_id: [u8; 16],
    seq: Arc<SequenceCounter>,
    /// Per-channel-type inbound dispatch (control, stream, channel).
    routes: Arc<Mutex<HashMap<ChannelType, mpsc::UnboundedSender<Inbound>>>>,
    /// The socket delivers kernel receive timestamps (see [`ReceiveTime`]).
    kernel_receive_times: bool,
}

impl Transport {
    pub async fn bind(addr: SocketAddr, key: Key, band_id: [u8; 16]) -> io::Result<Self> {
        let socket = UdpSocket::bind(addr).await?;
        let kernel_receive_times = rx_time::enable(&socket);
        debug!(
            kernel_receive_times,
            "telesthete transport bound on {}",
            socket.local_addr()?
        );
        let socket = Arc::new(socket);
        Ok(Self {
            kernel_receive_times,
            socket,
            base_key: key,
            session_key: key, // no session configured yet -> data uses base key
            send_keys: Arc::new(Mutex::new(HashMap::new())),
            peer_keys: Arc::new(Mutex::new(HashMap::new())),
            last_seen: Arc::new(Mutex::new(HashMap::new())),
            band_id,
            seq: Arc::new(SequenceCounter::default()),
            routes: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Configure our own per-session data key (SPEC §3.3), used for all sent
    /// non-HELLO packets. Call once, before the transport is shared/used.
    pub fn set_session_key(&mut self, key: Key) {
        self.session_key = key;
    }

    /// Register (or replace) a peer's per-session data key + negotiated suite,
    /// derived from the epoch in its HELLO. Used to decrypt that peer's data
    /// packets; replacing it on a restart makes the peer's old-session packets
    /// fail authentication.
    pub async fn register_peer_key(&self, addr: SocketAddr, key: Key, suite: Suite) {
        self.peer_keys.lock().await.insert(addr, (key, suite));
    }

    /// Register (or replace) the key we SEND to `addr` with: our epoch under
    /// the suite negotiated with that peer (§3.5). Without an entry, sends use
    /// the baseline `session_key`.
    pub async fn register_send_key(&self, addr: SocketAddr, key: Key, suite: Suite) {
        self.send_keys.lock().await.insert(addr, (key, suite));
    }

    /// When each peer's last authenticated packet arrived (§4.5). Snapshot.
    pub async fn last_seen(&self) -> HashMap<SocketAddr, std::time::Instant> {
        self.last_seen.lock().await.clone()
    }

    /// Forget a peer's liveness + key state (dead-peer eviction, GOODBYE).
    pub async fn forget_peer(&self, addr: SocketAddr) {
        self.last_seen.lock().await.remove(&addr);
        self.peer_keys.lock().await.remove(&addr);
        self.send_keys.lock().await.remove(&addr);
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    pub fn band_id(&self) -> [u8; 16] {
        self.band_id
    }

    /// Whether inbound packets carry the kernel's receive time
    /// ([`ReceiveTimeSource::Kernel`]): Linux and Android, when the kernel
    /// accepted `SO_TIMESTAMPNS`. Otherwise every packet carries its read
    /// time. Even with kernel times, a single packet can fall back to its
    /// read time (see [`ReceiveTimeSource::Read`]).
    pub fn kernel_receive_times(&self) -> bool {
        self.kernel_receive_times
    }

    /// Subscribe to inbound packets for a channel type. Drops any existing
    /// subscription for that type.
    pub async fn route(&self, ty: ChannelType) -> mpsc::UnboundedReceiver<Inbound> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.lock().await.insert(ty, tx);
        rx
    }

    /// Encrypt + send one packet. HELLO/HELLO_ACK (`use_base_key`) go under the
    /// base key so a receiver can bootstrap our epoch; everything else uses our
    /// per-session data key (SPEC §3.1/§3.3).
    pub async fn send(&self, out: Outbound) -> Result<(), TransportError> {
        let seq = self.seq.next();
        let (key, suite) = if out.use_base_key {
            (self.base_key, Suite::ChaCha20Poly1305)
        } else {
            // Per-peer negotiated suite (§3.5) when one is registered, else the
            // baseline session key.
            self.send_keys
                .lock()
                .await
                .get(&out.to)
                .copied()
                .unwrap_or((self.session_key, Suite::ChaCha20Poly1305))
        };
        let pkt = encode_packet_suite(
            suite,
            &key,
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
        let base_key = self.base_key;
        let peer_keys = Arc::clone(&self.peer_keys);
        let last_seen = Arc::clone(&self.last_seen);
        let band_id = self.band_id;
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65_535];
            loop {
                let (n, from, received) = match rx_time::recv_from(&socket, &mut buf).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!("recv_from error: {e}");
                        continue;
                    }
                };
                // Try the peer's session data key first (its data packets), then
                // the base key (its HELLO/HELLO_ACK). An old-session packet after
                // a restart fails both — its epoch's key is gone — so it is
                // dropped (SPEC §3.3).
                let peer_key = peer_keys.lock().await.get(&from).copied();
                let (header, payload) = match peer_key
                    .and_then(|(k, s)| decode_packet_suite(s, &k, &buf[..n]).ok())
                    .or_else(|| {
                        decode_packet_suite(Suite::ChaCha20Poly1305, &base_key, &buf[..n]).ok()
                    }) {
                    Some(v) => v,
                    None => {
                        debug!("decode_packet from {from} failed under all keys");
                        continue;
                    }
                };
                if header.band_id != band_id {
                    debug!("dropping packet from {from}: foreign band_id");
                    continue;
                }
                // Authenticated + our band: refresh liveness (§4.5).
                last_seen
                    .lock()
                    .await
                    .insert(from, std::time::Instant::now());
                let inbound = Inbound {
                    from,
                    header,
                    payload,
                    received,
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

/// Receiving with the kernel's receive timestamp where there is one.
#[cfg(any(target_os = "linux", target_os = "android"))]
mod rx_time {
    use std::io::{self, IoSliceMut};
    use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant, SystemTime};

    use nix::cmsg_space;
    use nix::sys::socket::{
        recvmsg, setsockopt, sockopt, ControlMessageOwned, MsgFlags, SockaddrStorage,
    };
    use nix::sys::time::TimeSpec;
    use tokio::io::Interest;
    use tokio::net::UdpSocket;

    use super::ReceiveTime;

    /// Ask the kernel to timestamp every datagram (`SO_TIMESTAMPNS`).
    pub(super) fn enable(socket: &UdpSocket) -> bool {
        match setsockopt(socket, sockopt::ReceiveTimestampns, &true) {
            Ok(()) => true,
            Err(e) => {
                tracing::debug!("SO_TIMESTAMPNS refused ({e}); receive times are read times");
                false
            }
        }
    }

    /// `recv_from`, plus the datagram's receive time.
    pub(super) async fn recv_from(
        socket: &UdpSocket,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, ReceiveTime)> {
        socket
            .async_io(Interest::READABLE, || recv_once(socket, buf))
            .await
    }

    fn recv_once(
        socket: &UdpSocket,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, ReceiveTime)> {
        let mut cmsg = cmsg_space!(TimeSpec);
        let mut iov = [IoSliceMut::new(buf)];
        let msg = recvmsg::<SockaddrStorage>(
            socket.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg),
            MsgFlags::MSG_DONTWAIT,
        )
        .map_err(io::Error::from)?;
        let read = Instant::now();
        let wall = SystemTime::now();
        let from = msg
            .address
            .as_ref()
            .and_then(to_socket_addr)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "datagram without an IP source")
            })?;
        let stamp = msg.cmsgs().ok().and_then(|mut cmsgs| {
            cmsgs.find_map(|c| match c {
                ControlMessageOwned::ScmTimestampns(ts) => Some(ts),
                _ => None,
            })
        });
        let received = match stamp.and_then(to_system_time) {
            Some(stamp) => ReceiveTime::from_kernel(stamp, wall, read),
            None => ReceiveTime {
                at: read,
                source: super::ReceiveTimeSource::Read,
            },
        };
        Ok((msg.bytes, from, received))
    }

    fn to_socket_addr(addr: &SockaddrStorage) -> Option<SocketAddr> {
        if let Some(v4) = addr.as_sockaddr_in() {
            return Some(SocketAddr::V4(SocketAddrV4::from(*v4)));
        }
        let v6 = addr.as_sockaddr_in6()?;
        Some(SocketAddr::V6(SocketAddrV6::from(*v6)))
    }

    fn to_system_time(ts: TimeSpec) -> Option<SystemTime> {
        let secs = u64::try_from(ts.tv_sec()).ok()?;
        let nanos = u32::try_from(ts.tv_nsec()).ok()?;
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(secs, nanos))
    }
}

/// Elsewhere: the time the receive loop read the datagram.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod rx_time {
    use std::io;
    use std::net::SocketAddr;

    use tokio::net::UdpSocket;

    use super::ReceiveTime;

    pub(super) fn enable(_socket: &UdpSocket) -> bool {
        false
    }

    pub(super) async fn recv_from(
        socket: &UdpSocket,
        buf: &mut [u8],
    ) -> io::Result<(usize, SocketAddr, ReceiveTime)> {
        let (n, from) = socket.recv_from(buf).await?;
        Ok((n, from, ReceiveTime::read_now()))
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
                use_base_key: false,
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

    #[test]
    fn kernel_time_maps_onto_the_monotonic_clock() {
        let read = Instant::now() + Duration::from_secs(60);
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let waited = Duration::from_micros(2_345);
        let t = ReceiveTime::from_kernel(wall - waited, wall, read);
        assert_eq!(t.source, ReceiveTimeSource::Kernel);
        assert_eq!(read - t.at, waited);
        // Same instant as the read.
        let t = ReceiveTime::from_kernel(wall, wall, read);
        assert_eq!((t.at, t.source), (read, ReceiveTimeSource::Kernel));
    }

    #[test]
    fn a_stepped_wall_clock_falls_back_to_the_read_time() {
        let read = Instant::now() + Duration::from_secs(60);
        let wall = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        // Stamped "after" the read: the clock was stepped back in between.
        let t = ReceiveTime::from_kernel(wall + Duration::from_millis(5), wall, read);
        assert_eq!((t.at, t.source), (read, ReceiveTimeSource::Read));
        // Stamped implausibly long before: stepped forward.
        let t = ReceiveTime::from_kernel(wall - Duration::from_secs(3_600), wall, read);
        assert_eq!((t.at, t.source), (read, ReceiveTimeSource::Read));
    }

    /// A datagram that waits in the socket because the receiving runtime is
    /// busy carries the time it arrived, not the time it was read (Linux and
    /// Android); elsewhere it carries its read time, after the wait.
    #[tokio::test(flavor = "current_thread")]
    async fn receive_time_is_the_arrival_not_the_read() {
        let psk = b"rx-time-test";
        let key = derive_key(psk);
        let band_id = derive_band_id(psk);
        let alice = Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
            .await
            .unwrap();
        let bob = Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
            .await
            .unwrap();
        let kernel = cfg!(any(target_os = "linux", target_os = "android"));
        assert_eq!(bob.kernel_receive_times(), kernel);
        let bob_addr = bob.local_addr().unwrap();
        let mut bob_in = bob.route(ChannelType::Stream).await;
        bob.spawn_recv_loop();

        let sent = Instant::now();
        alice
            .send(Outbound {
                to: bob_addr,
                channel_type: ChannelType::Stream,
                channel_id: 7,
                plaintext: b"late read".to_vec(),
                priority: 0,
                use_base_key: false,
            })
            .await
            .unwrap();
        // Block the only runtime thread: bob's receive loop cannot run, so
        // the datagram waits in its socket.
        let wait = Duration::from_millis(80);
        std::thread::sleep(wait);
        let blocked_until = Instant::now();

        let got = tokio::time::timeout(Duration::from_secs(1), bob_in.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(got.payload, b"late read");
        if kernel {
            assert_eq!(got.received.source, ReceiveTimeSource::Kernel);
            assert!(got.received.at >= sent, "{:?}", got.received);
            assert!(
                blocked_until - got.received.at >= wait - Duration::from_millis(5),
                "stamped {:?} after the send, read {:?} after it",
                got.received.at - sent,
                blocked_until - sent
            );
        } else {
            assert_eq!(got.received.source, ReceiveTimeSource::Read);
            assert!(got.received.at >= blocked_until);
        }
    }

    /// Stream messages carry their packet's receive time.
    #[tokio::test(flavor = "current_thread")]
    async fn stream_messages_carry_the_receive_time() {
        let psk = b"rx-time-stream";
        let key = derive_key(psk);
        let band_id = derive_band_id(psk);
        let alice = Arc::new(
            Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
                .await
                .unwrap(),
        );
        let bob = Arc::new(
            Transport::bind("127.0.0.1:0".parse().unwrap(), key, band_id)
                .await
                .unwrap(),
        );
        let (_rebase_tx, rebase_rx) = tokio::sync::broadcast::channel(4);
        let hub = crate::stream::StreamHub::new(Arc::clone(&bob), rebase_rx).await;
        bob.spawn_recv_loop();
        let mut bob_ep = hub.open(alice.local_addr().unwrap(), 9).await;
        let (_alice_rebase_tx, alice_rebase_rx) = tokio::sync::broadcast::channel(4);
        let alice_hub = crate::stream::StreamHub::new(Arc::clone(&alice), alice_rebase_rx).await;
        let alice_ep = alice_hub.open(bob.local_addr().unwrap(), 9).await;

        let before = Instant::now();
        alice_ep.send(3, b"frame").await.unwrap();
        std::thread::sleep(Duration::from_millis(40));
        let msg = tokio::time::timeout(Duration::from_secs(1), bob_ep.recv())
            .await
            .unwrap()
            .unwrap();
        let after = Instant::now();
        assert_eq!((msg.priority, msg.data.as_slice()), (3, &b"frame"[..]));
        assert!(msg.received.at >= before && msg.received.at <= after);
        let expected = if bob.kernel_receive_times() {
            ReceiveTimeSource::Kernel
        } else {
            ReceiveTimeSource::Read
        };
        assert_eq!(msg.received.source, expected);
        if expected == ReceiveTimeSource::Kernel {
            assert!(after - msg.received.at >= Duration::from_millis(35));
        }
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
                use_base_key: false,
            })
            .await
            .unwrap();

        // Should NOT arrive — bob is in a different band.
        let r = tokio::time::timeout(std::time::Duration::from_millis(200), bob_in.recv()).await;
        assert!(r.is_err(), "expected timeout (foreign-band drop)");
    }
}
