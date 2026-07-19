//! LAN peer discovery via UDP broadcast — SPEC §9.2.
//!
//! Announce layout (byte-identical with the Python reference):
//!
//! ```text
//! 0      4 B  magic         0x54454C45 ("TELE")
//! 4      1 B  version       = PROTOCOL_VERSION (§12.4)
//! 5      1 B  hostname_len  uint8
//! 6      var  hostname      UTF-8, hostname_len bytes (no terminator)
//! 6+len  2 B  port          uint16 BE, listening port
//! ```
//!
//! Trailing bytes after the port are ignored (forward-extensible).

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::debug;

use crate::framing::PROTOCOL_VERSION;

pub const MAGIC: &[u8; 4] = b"TELE";
pub const DISCOVERY_PORT: u16 = 9998;
pub const BROADCAST_INTERVAL: Duration = Duration::from_secs(5); // §9.2

/// Pack a §9.2 announce. Hostnames longer than 255 UTF-8 bytes are truncated.
pub fn pack_announce(hostname: &str, port: u16) -> Vec<u8> {
    let hb = hostname.as_bytes();
    let hb = &hb[..hb.len().min(255)];
    let mut out = Vec::with_capacity(8 + hb.len());
    out.extend_from_slice(MAGIC);
    out.push(PROTOCOL_VERSION);
    out.push(hb.len() as u8);
    out.extend_from_slice(hb);
    out.extend_from_slice(&port.to_be_bytes());
    out
}

/// Parse a §9.2 announce -> (hostname, port). `None` for foreign packets or a
/// version we cannot speak; trailing bytes are ignored.
pub fn parse_announce(data: &[u8]) -> Option<(String, u16)> {
    if data.len() < 8 || &data[..4] != MAGIC || data[4] != PROTOCOL_VERSION {
        return None;
    }
    let hostname_len = data[5] as usize;
    let end = 6 + hostname_len;
    if data.len() < end + 2 {
        return None;
    }
    let hostname = std::str::from_utf8(&data[6..end]).ok()?.to_string();
    let port = u16::from_be_bytes([data[end], data[end + 1]]);
    Some((hostname, port))
}

/// A peer seen on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiscoveredPeer {
    pub hostname: String,
    pub ip: IpAddr,
    pub port: u16,
}

/// Broadcast "I exist" every [`BROADCAST_INTERVAL`] and surface newly seen
/// peers. Duplicate detection by (hostname, ip, port) (§9.2); our own
/// hostname is ignored.
pub struct Discovery {
    socket: Arc<UdpSocket>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    found: mpsc::UnboundedReceiver<DiscoveredPeer>,
}

impl Discovery {
    /// Bind the discovery socket (broadcast-enabled). `discovery_port` is
    /// normally [`DISCOVERY_PORT`]; tests pass 0 for an ephemeral port.
    pub async fn bind(
        hostname: String,
        listen_port: u16,
        discovery_port: u16,
    ) -> std::io::Result<Self> {
        let std_sock = std::net::UdpSocket::bind(("0.0.0.0", discovery_port))?;
        std_sock.set_broadcast(true)?;
        std_sock.set_nonblocking(true)?;
        let socket = Arc::new(UdpSocket::from_std(std_sock)?);
        let (tx, rx) = mpsc::unbounded_channel();

        let bcast_sock = Arc::clone(&socket);
        let announce = pack_announce(&hostname, listen_port);
        let target: SocketAddr = ("255.255.255.255".parse::<IpAddr>().unwrap(), DISCOVERY_PORT).into();
        let bcast = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(BROADCAST_INTERVAL);
            loop {
                ticker.tick().await;
                if let Err(e) = bcast_sock.send_to(&announce, target).await {
                    debug!("discovery broadcast failed: {e}");
                }
            }
        });

        let recv_sock = Arc::clone(&socket);
        let own_hostname = hostname;
        let recv = tokio::spawn(async move {
            // Bounded dedup set: unauthenticated LAN beacons can spoof unlimited
            // (hostname, ip, port) triples, so cap the set and evict the oldest
            // (insertion-ordered) past the cap rather than growing forever.
            const MAX_SEEN: usize = 4096;
            let mut seen: HashSet<DiscoveredPeer> = HashSet::new();
            let mut order: std::collections::VecDeque<DiscoveredPeer> =
                std::collections::VecDeque::new();
            let mut buf = [0u8; 1024];
            loop {
                let Ok((n, from)) = recv_sock.recv_from(&mut buf).await else {
                    continue;
                };
                let Some((peer_hostname, port)) = parse_announce(&buf[..n]) else {
                    continue;
                };
                if peer_hostname == own_hostname {
                    continue; // our own broadcast
                }
                let peer = DiscoveredPeer {
                    hostname: peer_hostname,
                    ip: from.ip(),
                    port,
                };
                if seen.insert(peer.clone()) {
                    order.push_back(peer.clone());
                    while order.len() > MAX_SEEN {
                        if let Some(old) = order.pop_front() {
                            seen.remove(&old);
                        }
                    }
                    if tx.send(peer).is_err() {
                        return;
                    }
                }
            }
        });

        Ok(Self {
            socket: Arc::clone(&socket),
            tasks: vec![bcast, recv],
            found: rx,
        })
    }

    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Next newly discovered peer (each (hostname, ip, port) fires once).
    pub async fn recv(&mut self) -> Option<DiscoveredPeer> {
        self.found.recv().await
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_layout_matches_spec() {
        let pkt = pack_announce("alice", 20001);
        assert_eq!(&pkt[..4], b"TELE");
        assert_eq!(pkt[4], PROTOCOL_VERSION);
        assert_eq!(pkt[5], 5);
        assert_eq!(&pkt[6..11], b"alice");
        assert_eq!(&pkt[11..13], &20001u16.to_be_bytes());
        assert_eq!(pkt.len(), 13);
    }

    #[test]
    fn announce_round_trip_and_bounds() {
        assert_eq!(
            parse_announce(&pack_announce("héllo-host", 65535)),
            Some(("héllo-host".into(), 65535))
        );
        assert_eq!(parse_announce(&pack_announce("", 1)), Some((String::new(), 1)));
        // Truncated to 255 hostname bytes.
        let long = "a".repeat(300);
        let (h, p) = parse_announce(&pack_announce(&long, 9)).unwrap();
        assert_eq!(h, "a".repeat(255));
        assert_eq!(p, 9);
    }

    #[test]
    fn parse_rejects_foreign_stale_truncated() {
        assert!(parse_announce(b"NOPE\x03\x00\x00\x00").is_none());
        let mut stale = pack_announce("x", 1);
        stale[4] = PROTOCOL_VERSION + 1;
        assert!(parse_announce(&stale).is_none());
        let pkt = pack_announce("x", 1);
        assert!(parse_announce(&pkt[..pkt.len() - 1]).is_none());
        assert!(parse_announce(b"").is_none());
        // Trailing bytes ignored (forward-extensible).
        let mut extended = pack_announce("h", 7);
        extended.extend_from_slice(b"future");
        assert_eq!(parse_announce(&extended), Some(("h".into(), 7)));
    }

    #[tokio::test]
    async fn discovery_surfaces_directly_delivered_announce() {
        // No real broadcast (CI-hostile); inject an announce straight into the
        // discovery socket and watch it surface exactly once.
        let mut d = Discovery::bind("me".into(), 4242, 0).await.unwrap();
        let daddr = d.local_addr().unwrap();
        let tx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let announce = pack_announce("other-host", 5555);
        tx.send_to(&announce, ("127.0.0.1", daddr.port())).await.unwrap();
        tx.send_to(&announce, ("127.0.0.1", daddr.port())).await.unwrap(); // dup

        let peer = tokio::time::timeout(Duration::from_secs(2), d.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.hostname, "other-host");
        assert_eq!(peer.port, 5555);
        // The duplicate must NOT produce a second event.
        let dup = tokio::time::timeout(Duration::from_millis(200), d.recv()).await;
        assert!(dup.is_err(), "duplicate announce must be deduplicated");
    }

    #[tokio::test]
    async fn own_hostname_is_ignored() {
        let mut d = Discovery::bind("self-host".into(), 1, 0).await.unwrap();
        let daddr = d.local_addr().unwrap();
        let tx = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tx.send_to(&pack_announce("self-host", 1), ("127.0.0.1", daddr.port()))
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_millis(300), d.recv()).await;
        assert!(got.is_err(), "own announce must be ignored");
    }
}
