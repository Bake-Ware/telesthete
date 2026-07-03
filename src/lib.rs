//! Telesthete — the Rook wire transport for the spatial thin client.
//! PSK-authenticated, encrypted hybrid TCP+UDP carrying spatial-proto channels.
//! See spec/TELESTHETE.md. Rust API here; C ABI in `ffi`.

pub mod conn;
pub mod crypto;
mod ffi;
pub mod psk;
pub mod wire;

use std::io;
use std::net::{TcpListener, TcpStream, UdpSocket};

pub use conn::Conn;

/// Host side: listens for one client at a time (matching current hostd). Each accepted
/// connection gets its own UDP socket bound to an ephemeral port.
pub struct TelServer {
    listener: TcpListener,
    psk: [u8; 32],
}

impl TelServer {
    /// Bind the TCP control port. PSK loaded from `psk_path` (None => default config path).
    pub fn bind(addr: &str, psk_path: Option<&str>) -> io::Result<TelServer> {
        let psk = psk::load(psk_path).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let listener = TcpListener::bind(addr)?;
        Ok(TelServer { listener, psk })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    /// Block for one client; run the responder handshake; return a live session.
    pub fn accept(&self) -> io::Result<Conn> {
        let (tcp, _peer) = self.listener.accept()?;
        let bind_ip = self.listener.local_addr()?.ip();
        let udp = UdpSocket::bind((bind_ip, 0))?;
        conn::host_accept(tcp, udp, self.psk)
    }
}

/// Client side: TCP connect + handshake + UDP setup.
pub fn connect(host: &str, tcp_port: u16, psk_path: Option<&str>) -> io::Result<Conn> {
    conn::client_connect(host, tcp_port, psk_path)
}

/// Test helper: run a host responder over an already-accepted TcpStream + UDP socket.
pub fn accept_on(tcp: TcpStream, udp: UdpSocket, psk: [u8; 32]) -> io::Result<Conn> {
    conn::host_accept(tcp, udp, psk)
}
