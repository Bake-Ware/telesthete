//! Handshake + record encryption. All primitives are from audited crates; nothing here
//! implements a cipher or KDF by hand. See spec/TELESTHETE.md §3.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey};

pub const VERSION: u8 = 1;
pub const NONCE16: usize = 16;
pub const KEY: usize = 32;
pub const TAG: usize = 16;
pub const CONFIRM: usize = 32;

#[derive(Debug, PartialEq)]
pub enum HsError {
    BadVersion,
    ShortMessage,
    BadConfirm, // wrong PSK, tampered transcript, or replay
}

/// The four directional record keys + a UDP key pair + session id, derived from the
/// X25519 shared secret mixed with the PSK. See spec §3.
#[derive(Clone, Debug)]
pub struct Session {
    pub k_tcp_tx: [u8; KEY],
    pub k_tcp_rx: [u8; KEY],
    pub k_udp_tx: [u8; KEY],
    pub k_udp_rx: [u8; KEY],
    pub session_id: [u8; 16],
}

fn derive(dh: &[u8], psk: &[u8; 32], transcript: &[u8]) -> ([u8; KEY], [u8; KEY], [u8; KEY], [u8; KEY], [u8; 16]) {
    // ikm = dh ‖ psk, salt = transcript, info = "telesthete-v1"
    let mut ikm = Vec::with_capacity(dh.len() + 32);
    ikm.extend_from_slice(dh);
    ikm.extend_from_slice(psk);
    let hk = Hkdf::<Sha256>::new(Some(transcript), &ikm);
    let mut okm = [0u8; KEY * 4 + 16];
    hk.expand(b"telesthete-v1", &mut okm).expect("hkdf len ok");
    let mut c2h = [0u8; KEY];
    let mut h2c = [0u8; KEY];
    let mut uc2h = [0u8; KEY];
    let mut uh2c = [0u8; KEY];
    let mut sid = [0u8; 16];
    c2h.copy_from_slice(&okm[0..32]);
    h2c.copy_from_slice(&okm[32..64]);
    uc2h.copy_from_slice(&okm[64..96]);
    uh2c.copy_from_slice(&okm[96..128]);
    sid.copy_from_slice(&okm[128..144]);
    (c2h, h2c, uc2h, uh2c, sid)
}

fn confirm(key: &[u8; KEY], label: &[u8], transcript: &[u8]) -> [u8; CONFIRM] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("hmac key");
    mac.update(label);
    mac.update(transcript);
    let out = mac.finalize().into_bytes();
    let mut c = [0u8; CONFIRM];
    c.copy_from_slice(&out);
    c
}

fn transcript(ec: &[u8; 32], eh: &[u8; 32], nc: &[u8; NONCE16], nh: &[u8; NONCE16]) -> Vec<u8> {
    let mut t = Vec::with_capacity(1 + 32 + 32 + 16 + 16);
    t.push(VERSION);
    t.extend_from_slice(ec);
    t.extend_from_slice(eh);
    t.extend_from_slice(nc);
    t.extend_from_slice(nh);
    t
}

// ---- client side ----

pub struct ClientHandshake {
    secret: Option<EphemeralSecret>,
    ec_pub: [u8; 32],
    nc: [u8; NONCE16],
    psk: [u8; 32],
}

impl ClientHandshake {
    pub fn new(psk: [u8; 32]) -> Self {
        let secret = EphemeralSecret::random_from_rng(OsRng);
        let ec_pub = PublicKey::from(&secret).to_bytes();
        let mut nc = [0u8; NONCE16];
        OsRng.fill_bytes(&mut nc);
        ClientHandshake { secret: Some(secret), ec_pub, nc, psk }
    }

    /// Hello1 body: version ‖ eC.pub(32) ‖ nC(16) ‖ udp_port(2 le)
    pub fn hello1(&self, udp_port: u16) -> Vec<u8> {
        let mut m = Vec::with_capacity(1 + 32 + 16 + 2);
        m.push(VERSION);
        m.extend_from_slice(&self.ec_pub);
        m.extend_from_slice(&self.nc);
        m.extend_from_slice(&udp_port.to_le_bytes());
        m
    }

    /// Consume Hello2 { eH.pub(32), nH(16), confirmH(32), udp_port(2) }; verify host's PSK
    /// proof. Returns (Session, host_udp_port, Confirm3 bytes) on success.
    pub fn recv_hello2(mut self, hello2: &[u8]) -> Result<(Session, u16, Vec<u8>), HsError> {
        if hello2.len() < 32 + NONCE16 + CONFIRM + 2 {
            return Err(HsError::ShortMessage);
        }
        let mut eh = [0u8; 32];
        eh.copy_from_slice(&hello2[0..32]);
        let mut nh = [0u8; NONCE16];
        nh.copy_from_slice(&hello2[32..48]);
        let confirm_h = &hello2[48..48 + CONFIRM];
        let host_udp = u16::from_le_bytes([hello2[80], hello2[81]]);

        let dh = self.secret.take().unwrap().diffie_hellman(&PublicKey::from(eh));
        let t = transcript(&self.ec_pub, &eh, &self.nc, &nh);
        let (k_c2h, k_h2c, k_uc2h, k_uh2c, sid) = derive(dh.as_bytes(), &self.psk, &t);

        let expect_h = confirm(&k_h2c, b"host-confirm", &t);
        if !ct_eq(&expect_h, confirm_h) {
            return Err(HsError::BadConfirm);
        }
        let confirm_c = confirm(&k_c2h, b"client-confirm", &t).to_vec();
        let sess = Session {
            k_tcp_tx: k_c2h,
            k_tcp_rx: k_h2c,
            k_udp_tx: k_uc2h,
            k_udp_rx: k_uh2c,
            session_id: sid,
        };
        Ok((sess, host_udp, confirm_c))
    }
}

// ---- host side ----

/// Given the client's Hello1, produce Hello2 + the Session; the returned `pending` verifies
/// the client's Confirm3.
pub fn host_respond(
    hello1: &[u8],
    psk: [u8; 32],
    host_udp_port: u16,
) -> Result<(Vec<u8>, Session, PendingConfirm, u16), HsError> {
    if hello1.is_empty() || hello1[0] != VERSION {
        return Err(HsError::BadVersion);
    }
    if hello1.len() < 1 + 32 + NONCE16 + 2 {
        return Err(HsError::ShortMessage);
    }
    let mut ec = [0u8; 32];
    ec.copy_from_slice(&hello1[1..33]);
    let mut nc = [0u8; NONCE16];
    nc.copy_from_slice(&hello1[33..49]);
    let client_udp = u16::from_le_bytes([hello1[49], hello1[50]]);

    let secret = EphemeralSecret::random_from_rng(OsRng);
    let eh_pub = PublicKey::from(&secret).to_bytes();
    let mut nh = [0u8; NONCE16];
    OsRng.fill_bytes(&mut nh);

    let dh = secret.diffie_hellman(&PublicKey::from(ec));
    let t = transcript(&ec, &eh_pub, &nc, &nh);
    let (k_c2h, k_h2c, k_uc2h, k_uh2c, sid) = derive(dh.as_bytes(), &psk, &t);

    let confirm_h = confirm(&k_h2c, b"host-confirm", &t);
    let mut hello2 = Vec::with_capacity(32 + NONCE16 + CONFIRM + 2);
    hello2.extend_from_slice(&eh_pub);
    hello2.extend_from_slice(&nh);
    hello2.extend_from_slice(&confirm_h);
    hello2.extend_from_slice(&host_udp_port.to_le_bytes());

    let sess = Session {
        k_tcp_tx: k_h2c, // host transmits with h2c
        k_tcp_rx: k_c2h,
        k_udp_tx: k_uh2c,
        k_udp_rx: k_uc2h,
        session_id: sid,
    };
    let pending = PendingConfirm { k_c2h, transcript: t };
    Ok((hello2, sess, pending, client_udp))
}

pub struct PendingConfirm {
    k_c2h: [u8; KEY],
    transcript: Vec<u8>,
}

impl PendingConfirm {
    /// Verify Confirm3 { confirmC(32) }. Wrong PSK / replay → BadConfirm.
    pub fn verify(&self, confirm3: &[u8]) -> Result<(), HsError> {
        if confirm3.len() < CONFIRM {
            return Err(HsError::ShortMessage);
        }
        let expect = confirm(&self.k_c2h, b"client-confirm", &self.transcript);
        if ct_eq(&expect, &confirm3[..CONFIRM]) {
            Ok(())
        } else {
            Err(HsError::BadConfirm)
        }
    }
}

// ---- record encryption ----

/// A per-direction ChaCha20-Poly1305 record channel with a monotonic 96-bit counter nonce.
pub struct RecordKey {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl RecordKey {
    pub fn new(key: [u8; KEY]) -> Self {
        RecordKey { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)), counter: 0 }
    }

    fn nonce(counter: u64) -> Nonce {
        let mut n = [0u8; 12];
        n[4..12].copy_from_slice(&counter.to_le_bytes());
        *Nonce::from_slice(&n)
    }

    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let n = Self::nonce(self.counter);
        self.counter += 1;
        self.cipher.encrypt(&n, Payload { msg: plaintext, aad: b"" }).expect("seal")
    }

    pub fn open(&mut self, ct: &[u8]) -> Option<Vec<u8>> {
        let n = Self::nonce(self.counter);
        let pt = self.cipher.decrypt(&n, Payload { msg: ct, aad: b"" }).ok()?;
        self.counter += 1;
        Some(pt)
    }
}

/// A UDP key with explicit per-datagram nonces (out-of-order safe). Nonce is derived from
/// the datagram's (type, channel, seq). The two directions use *different keys* (c2h vs
/// h2c) so cross-direction confusion is already impossible without a direction bit; within
/// a direction, (type,channel,seq) is unique per datagram. `_dir_tx` is kept in the ctor
/// signature for call-site clarity but does not enter the nonce (both ends must match).
pub struct DatagramKey {
    cipher: ChaCha20Poly1305,
}

impl DatagramKey {
    pub fn new(key: [u8; KEY], _dir_tx: bool) -> Self {
        DatagramKey { cipher: ChaCha20Poly1305::new(Key::from_slice(&key)) }
    }

    fn nonce(&self, dtype: u8, channel: u8, seq: u32) -> Nonce {
        let mut n = [0u8; 12];
        n[0] = dtype;
        n[1] = channel;
        n[3..7].copy_from_slice(&seq.to_le_bytes());
        *Nonce::from_slice(&n)
    }

    pub fn seal(&self, dtype: u8, channel: u8, seq: u32, plaintext: &[u8]) -> Vec<u8> {
        let n = self.nonce(dtype, channel, seq);
        self.cipher.encrypt(&n, Payload { msg: plaintext, aad: &[dtype, channel] }).expect("seal")
    }

    pub fn open(&self, dtype: u8, channel: u8, seq: u32, ct: &[u8]) -> Option<Vec<u8>> {
        let n = self.nonce(dtype, channel, seq);
        self.cipher.decrypt(&n, Payload { msg: ct, aad: &[dtype, channel] }).ok()
    }
}

/// Constant-time compare (avoid a timing oracle on the confirm tag).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    d == 0
}
