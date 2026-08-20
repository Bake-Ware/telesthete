//! Hub federation (SPEC §10 extension) — pool the registries of two hubs so a
//! band spans both.
//!
//! Workers connect *out* to their home hub, so a worker on hub A is reachable
//! only *through* A. A federation link is therefore a traffic path, not just a
//! discovery merge: when hub B receives a frame for band X and hub A has peers
//! in X, B relays the frame across the link and A injects it to its local X
//! peers. Discovery-sharing = forwarding.
//!
//! Rules (from the design):
//!   * **One hop.** A frame injected from a link ([`Registry::inject_from_link`])
//!     is delivered to local peers only, never re-sent across another link. The
//!     mesh is a flat pool, never a spanning tree.
//!   * **Sharing hub initiates.** The offering hub dials (`HUB_FED_LINK`); the
//!     consuming hub listens (`HUB_FED_LISTEN`). A shared secret authenticates
//!     the link (`HUB_FED_SECRET`).
//!   * **Default-revoked.** An accepted *inbound* link is inert until enabled by
//!     the operator (`HUB_FED_DEFAULT=active`, the "admin flips it on" step;
//!     `active` is the auto-link world). Outbound links we dialed are active
//!     from our side. A revoked link is authenticated but relays nothing.
//!   * **Scoping is by band.** Only bands both sides actually have are relayed;
//!     band_id keeps unrelated bands from colliding across the pool.
//!
//! Wire protocol (length-prefixed over TCP): `u32 len` + `u8 type` + payload.
//!   HELLO(0x01): secret bytes — sent first by both ends; must match.
//!   BANDS(0x02): u16 count, then count × 16-byte band_id — full snapshot of the
//!                sender's local bands, resent whenever the set changes.
//!   RELAY(0x03): 16-byte band_id, u32 frame_len, frame bytes.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use telesthete::{BandId, BAND_ID_LEN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

use crate::registry::Registry;

const T_HELLO: u8 = 0x01;
const T_BANDS: u8 = 0x02;
const T_RELAY: u8 = 0x03;
const MAX_MSG: u32 = 4 * 1024 * 1024; // sanity bound on a link message
/// How often to check our local band set and advertise it to a linked hub. We
/// only send when the set actually changed, plus a forced keepalive every
/// `BANDS_KEEPALIVE` so a missed update self-heals. A short poll keeps
/// cross-hub discovery responsive (a new band is reachable within a poll or two)
/// while an idle link costs at most one tiny message per keepalive.
const BANDS_POLL: Duration = Duration::from_secs(1);
const BANDS_KEEPALIVE: Duration = Duration::from_secs(15);

/// Resolved federation config (parsed from `HUB_FED_*`).
#[derive(Debug, Clone)]
pub struct FedConfig {
    /// Address to accept inbound hub links on, if any (`HUB_FED_LISTEN`).
    pub listen: Option<String>,
    /// Remote hubs to dial and offer our registry to (`HUB_FED_LINK`, comma-sep).
    pub links: Vec<String>,
    /// Shared secret authenticating every link (`HUB_FED_SECRET`).
    pub secret: String,
    /// Initial state for *inbound* links: `false` = revoked (default), `true` =
    /// active (`HUB_FED_DEFAULT=active`).
    pub inbound_active: bool,
}

impl FedConfig {
    pub fn from_env() -> Option<Self> {
        let listen = std::env::var("HUB_FED_LISTEN").ok().filter(|v| !v.is_empty());
        let links: Vec<String> = std::env::var("HUB_FED_LINK")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        if listen.is_none() && links.is_empty() {
            return None; // federation entirely off — the common case
        }
        let secret = std::env::var("HUB_FED_SECRET").unwrap_or_default();
        let inbound_active = matches!(
            std::env::var("HUB_FED_DEFAULT")
                .map(|v| v.to_ascii_lowercase())
                .as_deref(),
            Ok("active" | "1" | "on" | "yes")
        );
        Some(FedConfig {
            listen,
            links,
            secret,
            inbound_active,
        })
    }
}

/// Start federation: the egress fan-out task (registry → links), inbound
/// listener, and outbound dialers. Registers the egress channel on the registry.
pub fn spawn(
    cfg: FedConfig,
    registry: Arc<Registry>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();

    // Egress hub: locally-sourced frames arrive here from Registry::forward and
    // are broadcast to every connected link's writer (each writer filters to the
    // bands its remote peer advertised).
    let (egress_tx, egress_rx) = mpsc::channel::<(BandId, Arc<[u8]>)>(4096);
    registry.set_federation(egress_tx);
    let links_reg = Arc::new(LinkSet::new());

    {
        let links_reg = links_reg.clone();
        tasks.push(tokio::spawn(async move {
            fan_egress(egress_rx, links_reg).await;
        }));
    }

    // Inbound listener (consuming hub).
    if let Some(addr) = cfg.listen.clone() {
        let (registry, cfg2, links_reg) = (registry.clone(), cfg.clone(), links_reg.clone());
        let mut sd = shutdown.clone();
        tasks.push(tokio::spawn(async move {
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => {
                    tracing::info!(addr = %addr, active = cfg2.inbound_active,
                        "federation: listening for hub links");
                    l
                }
                Err(e) => {
                    tracing::error!(addr = %addr, error = %e, "federation: listen failed");
                    return;
                }
            };
            loop {
                tokio::select! {
                    _ = sd.wait_for(|v| *v) => break,
                    accept = listener.accept() => match accept {
                        Ok((stream, peer)) => {
                            let (registry, cfg2, links_reg) =
                                (registry.clone(), cfg2.clone(), links_reg.clone());
                            tokio::spawn(async move {
                                if let Err(e) = handle_link(
                                    stream, registry, cfg2.secret.clone(),
                                    cfg2.inbound_active, links_reg, format!("in:{peer}"),
                                ).await {
                                    tracing::info!(peer = %peer, error = %e,
                                        "federation: inbound link closed");
                                }
                            });
                        }
                        Err(e) => tracing::warn!(error = %e, "federation: accept error"),
                    }
                }
            }
        }));
    }

    // Outbound dialers (sharing hub) — one supervised task per configured link.
    for remote in cfg.links.clone() {
        let (registry, secret, links_reg) =
            (registry.clone(), cfg.secret.clone(), links_reg.clone());
        let mut sd = shutdown.clone();
        tasks.push(tokio::spawn(async move {
            loop {
                if *sd.borrow() {
                    break;
                }
                match TcpStream::connect(&remote).await {
                    Ok(stream) => {
                        tracing::info!(remote = %remote, "federation: dialed hub link");
                        // A link we dialed is active from our side (we offered).
                        if let Err(e) = handle_link(
                            stream, registry.clone(), secret.clone(), true,
                            links_reg.clone(), format!("out:{remote}"),
                        ).await {
                            tracing::info!(remote = %remote, error = %e,
                                "federation: outbound link closed; will redial");
                        }
                    }
                    Err(e) => tracing::debug!(remote = %remote, error = %e,
                        "federation: dial failed; retrying"),
                }
                // Backoff before redial, but wake immediately on shutdown.
                tokio::select! {
                    _ = sd.wait_for(|v| *v) => break,
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                }
            }
        }));
    }

    tasks
}

/// One writer handle per live link: an mpsc sender its task drains, plus the
/// set of bands the remote advertised (so egress only sends frames it wants).
struct LinkHandle {
    id: u64,
    tx: mpsc::Sender<(BandId, Arc<[u8]>)>,
    bands: std::sync::Mutex<HashSet<BandId>>,
    active: std::sync::atomic::AtomicBool,
}

struct LinkSet {
    links: std::sync::Mutex<Vec<Arc<LinkHandle>>>,
    next: std::sync::atomic::AtomicU64,
}

impl LinkSet {
    fn new() -> Self {
        Self {
            links: std::sync::Mutex::new(Vec::new()),
            next: std::sync::atomic::AtomicU64::new(1),
        }
    }
    fn add(&self, h: Arc<LinkHandle>) {
        self.links.lock().unwrap().push(h);
    }
    fn remove(&self, id: u64) {
        self.links.lock().unwrap().retain(|h| h.id != id);
    }
    fn snapshot(&self) -> Vec<Arc<LinkHandle>> {
        self.links.lock().unwrap().clone()
    }
}

/// Fan a locally-sourced frame out to every active link whose remote advertised
/// the band. The per-link channel applies backpressure-by-drop.
async fn fan_egress(mut rx: mpsc::Receiver<(BandId, Arc<[u8]>)>, links: Arc<LinkSet>) {
    while let Some((band, frame)) = rx.recv().await {
        for h in links.snapshot() {
            if !h.active.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            let wants = h.bands.lock().unwrap().contains(&band);
            if wants {
                let _ = h.tx.try_send((band, frame.clone()));
            }
        }
    }
}

/// Drive one authenticated link: HELLO exchange, then concurrently pump BANDS/
/// RELAY reads (remote → our registry) and the egress writer (our frames →
/// remote). `active` gates whether frames actually flow (default-revoked).
async fn handle_link(
    mut stream: TcpStream,
    registry: Arc<Registry>,
    secret: String,
    active: bool,
    links: Arc<LinkSet>,
    label: String,
) -> std::io::Result<()> {
    // HELLO both ways.
    write_msg(&mut stream, T_HELLO, secret.as_bytes()).await?;
    let (t, payload) = read_msg(&mut stream).await?;
    if t != T_HELLO || payload != secret.as_bytes() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "federation: link secret mismatch",
        ));
    }

    let id = links.next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (wtx, mut wrx) = mpsc::channel::<(BandId, Arc<[u8]>)>(4096);
    let handle = Arc::new(LinkHandle {
        id,
        tx: wtx,
        bands: std::sync::Mutex::new(HashSet::new()),
        active: std::sync::atomic::AtomicBool::new(active),
    });
    links.add(handle.clone());
    tracing::info!(link = %label, active, "federation: link up");

    // Split the stream: reader half handles inbound, writer half drains egress.
    let (mut rd, mut wr) = stream.into_split();

    // Advertise our local bands + keep them fresh.
    {
        let registry = registry.clone();
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(BANDS_POLL);
            let mut last: Option<HashSet<BandId>> = None;
            let mut since_send = Duration::ZERO;
            loop {
                ticker.tick().await;
                if handle.tx.is_closed() {
                    break;
                }
                let bands = registry.local_bands();
                let set: HashSet<BandId> = bands.iter().copied().collect();
                since_send += BANDS_POLL;
                let changed = last.as_ref() != Some(&set);
                // Send on change, or as a periodic keepalive.
                if changed || since_send >= BANDS_KEEPALIVE {
                    let _ = handle
                        .tx
                        .try_send((BANDS_MARKER, encode_bands(&bands).into()));
                    last = Some(set);
                    since_send = Duration::ZERO;
                }
            }
        });
    }

    // Writer task: pull (band, frame) from the channel; BANDS_MARKER frames are
    // pre-encoded BANDS messages, everything else is a RELAY.
    let writer = tokio::spawn(async move {
        while let Some((band, frame)) = wrx.recv().await {
            let res = if band == BANDS_MARKER {
                write_msg(&mut wr, T_BANDS, &frame).await
            } else {
                let mut buf = Vec::with_capacity(BAND_ID_LEN + 4 + frame.len());
                buf.extend_from_slice(&band);
                buf.extend_from_slice(&(frame.len() as u32).to_be_bytes());
                buf.extend_from_slice(&frame);
                write_msg(&mut wr, T_RELAY, &buf).await
            };
            if res.is_err() {
                break;
            }
        }
    });

    // Reader loop: remote → us.
    let read_result = async {
        loop {
            let (t, payload) = read_msg_split(&mut rd).await?;
            match t {
                T_BANDS => {
                    let set = decode_bands(&payload);
                    *handle.bands.lock().unwrap() = set;
                }
                T_RELAY => {
                    if payload.len() < BAND_ID_LEN + 4 {
                        continue;
                    }
                    // Only inject if this link is active (default-revoked gate).
                    if !handle.active.load(std::sync::atomic::Ordering::Relaxed) {
                        continue;
                    }
                    let mut band: BandId = [0u8; BAND_ID_LEN];
                    band.copy_from_slice(&payload[..BAND_ID_LEN]);
                    let flen = u32::from_be_bytes(
                        payload[BAND_ID_LEN..BAND_ID_LEN + 4].try_into().unwrap(),
                    ) as usize;
                    let start = BAND_ID_LEN + 4;
                    if payload.len() < start + flen {
                        continue;
                    }
                    let frame: Arc<[u8]> = payload[start..start + flen].into();
                    // One hop: inject to local peers only, never re-federate.
                    registry.inject_from_link(&band, frame);
                }
                _ => {}
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    }
    .await;

    links.remove(id);
    writer.abort();
    tracing::info!(link = %label, "federation: link down");
    read_result
}

/// A reserved band_id sentinel used internally to route a pre-encoded BANDS
/// message through the writer channel. All-0xFF is not a real derived band_id in
/// practice; even a collision would just mean one stray control frame.
const BANDS_MARKER: BandId = [0xFFu8; BAND_ID_LEN];

fn encode_bands(bands: &[BandId]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + bands.len() * BAND_ID_LEN);
    out.extend_from_slice(&(bands.len().min(u16::MAX as usize) as u16).to_be_bytes());
    for b in bands.iter().take(u16::MAX as usize) {
        out.extend_from_slice(b);
    }
    out
}

fn decode_bands(payload: &[u8]) -> HashSet<BandId> {
    let mut set = HashSet::new();
    if payload.len() < 2 {
        return set;
    }
    let count = u16::from_be_bytes([payload[0], payload[1]]) as usize;
    let mut off = 2;
    for _ in 0..count {
        if off + BAND_ID_LEN > payload.len() {
            break;
        }
        let mut b: BandId = [0u8; BAND_ID_LEN];
        b.copy_from_slice(&payload[off..off + BAND_ID_LEN]);
        set.insert(b);
        off += BAND_ID_LEN;
    }
    set
}

async fn write_msg<W: AsyncWriteExt + Unpin>(w: &mut W, t: u8, payload: &[u8]) -> std::io::Result<()> {
    let len = 1 + payload.len();
    if len as u32 > MAX_MSG {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "msg too large"));
    }
    w.write_all(&(len as u32).to_be_bytes()).await?;
    w.write_all(&[t]).await?;
    w.write_all(payload).await?;
    w.flush().await
}

async fn read_msg(stream: &mut TcpStream) -> std::io::Result<(u8, Vec<u8>)> {
    let mut lenb = [0u8; 4];
    stream.read_exact(&mut lenb).await?;
    let len = u32::from_be_bytes(lenb);
    if len == 0 || len > MAX_MSG {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad msg len"));
    }
    let mut buf = vec![0u8; len as usize];
    stream.read_exact(&mut buf).await?;
    Ok((buf[0], buf[1..].to_vec()))
}

async fn read_msg_split<R: AsyncReadExt + Unpin>(rd: &mut R) -> std::io::Result<(u8, Vec<u8>)> {
    let mut lenb = [0u8; 4];
    rd.read_exact(&mut lenb).await?;
    let len = u32::from_be_bytes(lenb);
    if len == 0 || len > MAX_MSG {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad msg len"));
    }
    let mut buf = vec![0u8; len as usize];
    rd.read_exact(&mut buf).await?;
    Ok((buf[0], buf[1..].to_vec()))
}
