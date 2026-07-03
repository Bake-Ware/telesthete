//! C ABI for the C++ client. Mirrors spatial-proto conventions: opaque handle, borrowed
//! buffers, explicit error codes, no allocation crossing the boundary except the handle.

use crate::conn::Conn;
use std::ffi::c_char;

pub const TEL_OK: i32 = 0;
pub const TEL_ERR_NULL: i32 = -1;
pub const TEL_ERR_CONNECT: i32 = -2;
pub const TEL_ERR_DEAD: i32 = -3;

pub struct TelClient {
    conn: Conn,
    // Drain buffer reused across polls; each entry owned until the next poll.
    drained: Vec<(u8, Vec<u8>)>,
}

/// Connect + handshake. Returns an opaque handle or NULL. `psk_path` may be NULL (default).
/// # Safety: `host`/`psk_path` are NUL-terminated C strings (psk_path nullable).
#[no_mangle]
pub unsafe extern "C" fn tel_client_connect(
    host: *const c_char,
    tcp_port: u16,
    psk_path: *const c_char,
) -> *mut TelClient {
    if host.is_null() {
        return std::ptr::null_mut();
    }
    let host = match cstr(host) {
        Some(s) => s,
        None => return std::ptr::null_mut(),
    };
    let psk = if psk_path.is_null() { None } else { cstr(psk_path) };
    match crate::connect(&host, tcp_port, psk.as_deref()) {
        Ok(conn) => Box::into_raw(Box::new(TelClient { conn, drained: Vec::new() })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Send one proto message on `channel`. Routes TCP/UDP internally. Returns TEL_OK / error.
/// # Safety: `c` is a live handle; `msg` points to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn tel_client_send(
    c: *mut TelClient,
    channel: u8,
    msg: *const u8,
    len: usize,
) -> i32 {
    let Some(c) = c.as_mut() else { return TEL_ERR_NULL };
    if msg.is_null() && len != 0 {
        return TEL_ERR_NULL;
    }
    let slice = if len == 0 { &[][..] } else { std::slice::from_raw_parts(msg, len) };
    match c.conn.send(channel, slice) {
        Ok(()) => TEL_OK,
        Err(_) => TEL_ERR_DEAD,
    }
}

pub type TelRecvCb = extern "C" fn(user: *mut core::ffi::c_void, channel: u8, msg: *const u8, len: usize);

/// Drain all ready messages, invoking `cb` per message. Returns count, or negative error.
/// Buffers are valid only for the duration of the callback.
/// # Safety: `c` live; `cb` valid.
#[no_mangle]
pub unsafe extern "C" fn tel_client_poll(
    c: *mut TelClient,
    cb: TelRecvCb,
    user: *mut core::ffi::c_void,
) -> i32 {
    let Some(c) = c.as_mut() else { return TEL_ERR_NULL };
    c.drained.clear();
    c.drained = c.conn.poll();
    for (ch, msg) in &c.drained {
        cb(user, *ch, msg.as_ptr(), msg.len());
    }
    let n = c.drained.len() as i32;
    if !c.conn.is_alive() {
        return TEL_ERR_DEAD;
    }
    n
}

/// 1 if the session is alive, 0 otherwise.
/// # Safety: `c` live or NULL.
#[no_mangle]
pub unsafe extern "C" fn tel_client_connected(c: *const TelClient) -> i32 {
    match c.as_ref() {
        Some(c) => c.conn.is_alive() as i32,
        None => 0,
    }
}

/// Free the handle.
/// # Safety: `c` came from tel_client_connect and is not used afterward.
#[no_mangle]
pub unsafe extern "C" fn tel_client_free(c: *mut TelClient) {
    if !c.is_null() {
        drop(Box::from_raw(c));
    }
}

unsafe fn cstr(p: *const c_char) -> Option<String> {
    std::ffi::CStr::from_ptr(p).to_str().ok().map(|s| s.to_string())
}
