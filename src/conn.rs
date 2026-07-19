//! Synchronous, nonblocking TCP+UDP session plumbing shaped to drop into the existing
//! hostd/client loops (which poll rather than await). See spec §4-§6.

use crate::crypto::{self, DatagramKey, RecordKey, Session};
use crate::psk;
use crate::wire::{self, DT_DATA, DT_FRAG, DT_HELLO, DT_PING, DT_PONG};
use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const PING_IDLE: Duration = Duration::from_secs(2);
const DEAD_AFTER: Duration = Duration::from_secs(7);

/// One authenticated session's crypto + sockets + reassembly. Used by both ends after the
/// handshake completes (host via `accept`, client via `connect`).
pub struct Conn {
    tcp: TcpStream,
    udp: UdpSocket,
    tx: RecordKey,
    rx: RecordKey,
    udp_tx: DatagramKey,
    udp_rx: DatagramKey,
    session_id: [u8; 16],
    peer_udp: std::net::SocketAddr,
    peer_bound: bool,
    rx_buf: Vec<u8>,
    seq_tx: u32,
    msg_id_tx: u32,
    reasm: wire::UdpReasm,
    mtu: usize,
    last_rx: Instant,
    last_ping: Instant,
    last_tx: Instant,
    alive: bool,
}

impl Conn {
    fn new(
        tcp: TcpStream,
        udp: UdpSocket,
        sess: Session,
        peer_udp: std::net::SocketAddr,
        peer_bound: bool,
    ) -> io::Result<Self> {
        tcp.set_nodelay(true)?;
        tcp.set_nonblocking(true)?;
        udp.set_nonblocking(true)?;
        let now = Instant::now();
        Ok(Conn {
            tcp,
            udp,
            tx: RecordKey::new(sess.k_tcp_tx),
            rx: RecordKey::new(sess.k_tcp_rx),
            udp_tx: DatagramKey::new(sess.k_udp_tx, true),
            udp_rx: DatagramKey::new(sess.k_udp_rx, false),
            session_id: sess.session_id,
            peer_udp,
            peer_bound,
            rx_buf: Vec::new(),
            seq_tx: 1,
            msg_id_tx: 1,
            reasm: wire::UdpReasm::default(),
            mtu: wire::default_mtu(),
            last_rx: now,
            last_ping: now,
            last_tx: now,
            alive: true,
        })
    }

    pub fn is_alive(&self) -> bool {
        self.alive
    }

    /// Send one proto message on `channel`; routes TCP vs UDP by channel+opcode (spec §2).
    pub fn send(&mut self, channel: u8, msg: &[u8]) -> io::Result<()> {
        let first_op = msg.first().copied().unwrap_or(0);
        if wire::is_udp(channel, first_op) {
            self.send_udp(channel, msg)
        } else {
            self.send_tcp(channel, msg)
        }
    }

    fn send_tcp(&mut self, channel: u8, msg: &[u8]) -> io::Result<()> {
        let frame = wire::frame_tcp(channel, msg);
        let ct = self.tx.seal(&frame);
        let mut rec = Vec::with_capacity(4 + ct.len());
        rec.extend_from_slice(&(ct.len() as u32).to_le_bytes());
        rec.extend_from_slice(&ct);
        self.write_all_tcp(&rec)?;
        self.last_tx = Instant::now();
        Ok(())
    }

    fn send_udp(&mut self, channel: u8, msg: &[u8]) -> io::Result<()> {
        let seq = self.seq_tx;
        self.seq_tx = self.seq_tx.wrapping_add(1);
        let frags = wire::fragment(msg, self.mtu);
        if frags.len() == 1 {
            let body = wire::data_body(msg);
            let ct = self.udp_tx.seal(DT_DATA, channel, seq, &body);
            self.send_datagram(DT_DATA, channel, seq, &ct)?;
        } else {
            let msg_id = self.msg_id_tx;
            self.msg_id_tx = self.msg_id_tx.wrapping_add(1);
            let count = frags.len() as u16;
            for (i, slice) in frags.iter().enumerate() {
                let fseq = self.seq_tx;
                self.seq_tx = self.seq_tx.wrapping_add(1);
                let body = wire::frag_body(msg_id, i as u16, count, slice);
                let ct = self.udp_tx.seal(DT_FRAG, channel, fseq, &body);
                self.send_datagram(DT_FRAG, channel, fseq, &ct)?;
            }
        }
        self.last_tx = Instant::now();
        Ok(())
    }

    /// Datagram on wire: [type][channel][seq u32 le][ciphertext]. type/channel/seq are the
    /// nonce inputs, so they're authenticated as AAD (type,channel) + nonce (seq).
    fn send_datagram(&self, dtype: u8, channel: u8, seq: u32, ct: &[u8]) -> io::Result<()> {
        let mut dg = Vec::with_capacity(6 + ct.len());
        dg.push(dtype);
        dg.push(channel);
        dg.extend_from_slice(&seq.to_le_bytes());
        dg.extend_from_slice(ct);
        match self.udp.send_to(&dg, self.peer_udp) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(()), // drop, lossy channel
            Err(e) => Err(e),
        }
    }

    fn write_all_tcp(&mut self, mut data: &[u8]) -> io::Result<()> {
        while !data.is_empty() {
            match self.tcp.write(data) {
                Ok(0) => {
                    self.alive = false;
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "peer closed"));
                }
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::yield_now(); // records are small; brief spin is fine
                }
                Err(e) => {
                    self.alive = false;
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Drain everything currently readable on both sockets; return (channel, proto message).
    /// Also services keepalive; sets `alive=false` on dead peer.
    pub fn poll(&mut self) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        self.poll_tcp(&mut out);
        self.poll_udp(&mut out);
        self.service_keepalive();
        out
    }

    fn poll_tcp(&mut self, out: &mut Vec<(u8, Vec<u8>)>) {
        let mut tmp = [0u8; 64 * 1024];
        loop {
            match self.tcp.read(&mut tmp) {
                Ok(0) => {
                    self.alive = false;
                    break;
                }
                Ok(n) => {
                    self.rx_buf.extend_from_slice(&tmp[..n]);
                    self.last_rx = Instant::now();
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.alive = false;
                    break;
                }
            }
        }
        // records: [u32 ct_len][ct]
        loop {
            if self.rx_buf.len() < 4 {
                break;
            }
            let ct_len =
                u32::from_le_bytes([self.rx_buf[0], self.rx_buf[1], self.rx_buf[2], self.rx_buf[3]])
                    as usize;
            if self.rx_buf.len() < 4 + ct_len {
                break;
            }
            let ct: Vec<u8> = self.rx_buf[4..4 + ct_len].to_vec();
            self.rx_buf.drain(0..4 + ct_len);
            match self.rx.open(&ct) {
                Some(frame) => {
                    if let Some((ch, msg, _)) = wire::parse_tcp(&frame) {
                        if ch == 0xFE {
                            self.on_ctrl(&msg); // PING/PONG ride a reserved channel
                        } else {
                            out.push((ch, msg));
                        }
                    }
                }
                None => {
                    self.alive = false; // auth failure => tampered/desynced stream
                    break;
                }
            }
        }
    }

    fn poll_udp(&mut self, out: &mut Vec<(u8, Vec<u8>)>) {
        let mut buf = [0u8; 64 * 1024];
        loop {
            let (n, from) = match self.udp.recv_from(&mut buf) {
                Ok(v) => v,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => break,
            };
            if n < 6 {
                continue;
            }
            let dtype = buf[0];
            let channel = buf[1];
            let seq = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
            let ct = &buf[6..n];
            let pt = match self.udp_rx.open(dtype, channel, seq, ct) {
                Some(p) => p,
                None => continue, // forged/corrupt datagram
            };
            // Bind peer UDP address on the first authenticated datagram (spec §3).
            if !self.peer_bound {
                self.peer_udp = from;
                self.peer_bound = true;
            }
            self.last_rx = Instant::now();
            match dtype {
                DT_HELLO => { /* session_id echo; binding already done above */ }
                DT_DATA => {
                    if let Some(msg) = self.reasm.on_data(channel, seq, pt) {
                        out.push((channel, msg));
                    }
                }
                DT_FRAG => {
                    if let Some((mid, idx, count, slice)) = wire::parse_frag(&pt) {
                        if let Some(msg) = self.reasm.on_frag(channel, mid, idx, count, slice) {
                            out.push((channel, msg));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn on_ctrl(&mut self, msg: &[u8]) {
        match msg.first().copied() {
            Some(DT_PING) => {
                let _ = self.send_tcp(0xFE, &[DT_PONG]);
            }
            Some(DT_PONG) => {}
            _ => {}
        }
    }

    fn service_keepalive(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_rx) > DEAD_AFTER {
            self.alive = false;
            return;
        }
        if now.duration_since(self.last_tx) > PING_IDLE && now.duration_since(self.last_ping) > PING_IDLE {
            let _ = self.send_tcp(0xFE, &[DT_PING]);
            self.last_ping = now;
        }
    }

    /// Send the first UDP hello so the peer can bind our source address (spec §3).
    fn send_udp_hello(&mut self) -> io::Result<()> {
        let seq = self.seq_tx;
        self.seq_tx = self.seq_tx.wrapping_add(1);
        let ct = self.udp_tx.seal(DT_HELLO, 0, seq, &self.session_id);
        self.send_datagram(DT_HELLO, 0, seq, &ct)
    }
}

// ---- handshake drivers (blocking, used once at connect/accept) ----

fn read_exact_blocking(tcp: &mut TcpStream, n: usize) -> io::Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    tcp.read_exact(&mut v)?;
    Ok(v)
}

fn read_framed_blocking(tcp: &mut TcpStream) -> io::Result<Vec<u8>> {
    let hdr = read_exact_blocking(tcp, 4)?;
    let len = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as usize;
    read_exact_blocking(tcp, len)
}

fn write_framed_blocking(tcp: &mut TcpStream, body: &[u8]) -> io::Result<()> {
    tcp.write_all(&(body.len() as u32).to_le_bytes())?;
    tcp.write_all(body)
}

/// Client: TCP connect + handshake + UDP setup. `psk_path` None => default config path.
pub fn client_connect(host: &str, tcp_port: u16, psk_path: Option<&str>) -> io::Result<Conn> {
    let psk = psk::load(psk_path).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut tcp = TcpStream::connect((host, tcp_port))?;
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    let udp_port = udp.local_addr()?.port();

    let hs = crypto::ClientHandshake::new(psk);
    write_framed_blocking(&mut tcp, &hs.hello1(udp_port))?;
    let hello2 = read_framed_blocking(&mut tcp)?;
    let (sess, host_udp, confirm3) = hs
        .recv_hello2(&hello2)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("handshake: {e:?}")))?;
    write_framed_blocking(&mut tcp, &confirm3)?;

    let peer_udp = format!("{host}:{host_udp}").parse().map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "bad host udp addr")
    })?;
    let mut conn = Conn::new(tcp, udp, sess, peer_udp, true)?;
    conn.send_udp_hello()?;
    Ok(conn)
}

/// Host: accept an already-accepted TcpStream + a bound UDP socket; run the responder side.
pub fn host_accept(mut tcp: TcpStream, udp: UdpSocket, psk: [u8; 32]) -> io::Result<Conn> {
    let host_udp_port = udp.local_addr()?.port();
    let hello1 = read_framed_blocking(&mut tcp)?;
    let (hello2, sess, pending, client_udp) = crypto::host_respond(&hello1, psk, host_udp_port)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("handshake: {e:?}")))?;
    write_framed_blocking(&mut tcp, &hello2)?;
    let confirm3 = read_framed_blocking(&mut tcp)?;
    pending
        .verify(&confirm3)
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, format!("auth: {e:?}")))?;

    // Peer UDP addr from client's TCP source ip + advertised client udp port; but we bind
    // definitively on the first authenticated datagram, so start unbound.
    let peer_ip = tcp.peer_addr()?.ip();
    let peer_udp = std::net::SocketAddr::new(peer_ip, client_udp);
    Conn::new(tcp, udp, sess, peer_udp, false)
}
