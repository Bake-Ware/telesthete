//! Telesthete C ABI — public surface for non-Rust producers.
//!
//! Link the cdylib (`libtelesthete.so` / `.dylib` / `.dll`) and
//! `#include <telesthete.h>`. A typical producer:
//!
//! 1. `tlt_open(NULL)` — bind a producer socket, derive default target.
//! 2. Per frame: build a [`TltPlane`] array describing the dmabuf
//!    layout, call `tlt_send_dmabuf()` with the fd(s).
//! 3. `tlt_close()` on shutdown.
//!
//! Errors are signed return codes; `0` = success, negative = error.
//! Logging goes through `tracing`. Producers that want logs should
//! install a tracing subscriber from Rust before constructing the
//! handle, or wrap the logs in their own facility.
//!
//! See `examples/c-producer/` for a runnable single-file example.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::CStr;
use std::os::fd::{BorrowedFd, RawFd};
use std::path::PathBuf;
use std::sync::Arc;

use telesthete::wire::{DmabufDescriptor, DmabufPlane, StreamFlags, StreamHeader, STREAM_HEADER_LEN};
use telesthete::{
    derive_band_id, derive_key, ChannelType, UnixOutbound, UnixTransport, LOCAL_PSK,
    SOCKET_DIR_ENV, SOCKET_DIR_FALLBACK,
};
use tokio::runtime::Runtime;

/// Stream flags (mirror of [`telesthete::wire::StreamFlags`]).
/// Producers OR these together for the `flags` argument of
/// [`tlt_send_dmabuf`].
pub const TLT_FLAG_INIT: u32 = 0x01;
pub const TLT_FLAG_KEYFRAME: u32 = 0x02;
pub const TLT_FLAG_END_FRAME: u32 = 0x04;
pub const TLT_FLAG_FRAGMENT_CONT: u32 = 0x08;
pub const TLT_FLAG_DMABUF: u32 = 0x10;
pub const TLT_FLAG_WITH_FENCE: u32 = 0x20;
pub const TLT_FLAG_REUSE: u32 = 0x40;

/// Error return codes. `0` = success.
pub const TLT_OK: i32 = 0;
pub const TLT_ERR_NULL_HANDLE: i32 = -1;
pub const TLT_ERR_HEADER_WRITE: i32 = -2;
pub const TLT_ERR_DESCRIPTOR_WRITE: i32 = -3;
pub const TLT_ERR_PLANE_COUNT: i32 = -4;
pub const TLT_ERR_FD_COUNT: i32 = -5;
pub const TLT_ERR_SEND: i32 = -10;

/// Maximum plane count supported by the wire (mirror of
/// [`telesthete::wire::MAX_PLANES`]).
pub const TLT_MAX_PLANES: u8 = 4;
/// Maximum fd count per packet — 4 planes + 1 optional fence.
pub const TLT_MAX_FDS: u8 = 5;

/// Plane descriptor. Matches the on-wire `dmabuf` plane layout in
/// SPEC.md §5.4.
#[repr(C)]
pub struct TltPlane {
    pub offset: u32,
    pub stride: u32,
    /// Index into the `fds` array passed to [`tlt_send_dmabuf`].
    pub fd_index: u8,
}

/// Opaque producer handle. Treat as `void*` from C.
pub struct TltProducer {
    rt: Arc<Runtime>,
    transport: Arc<UnixTransport>,
    target: std::sync::Mutex<PathBuf>,
}

fn default_target_for(psk: &[u8]) -> PathBuf {
    let band = derive_band_id(psk);
    let mut name = String::with_capacity(36);
    for b in band.iter() {
        use std::fmt::Write;
        let _ = write!(name, "{b:02x}");
    }
    name.push_str(".sock");

    let xdg_path = std::env::var(SOCKET_DIR_ENV).ok().map(|dir| {
        let mut p = PathBuf::from(dir);
        p.push("telesthete");
        p.push(&name);
        p
    });
    let mut tmp_path = PathBuf::from(SOCKET_DIR_FALLBACK);
    tmp_path.push("telesthete");
    tmp_path.push(&name);

    if let Some(p) = &xdg_path {
        if p.exists() {
            return p.clone();
        }
    }
    if tmp_path.exists() {
        return tmp_path;
    }
    xdg_path.unwrap_or(tmp_path)
}

fn producer_socket_path() -> PathBuf {
    let dir = std::env::var(SOCKET_DIR_ENV).unwrap_or_else(|_| SOCKET_DIR_FALLBACK.to_string());
    let mut p = PathBuf::from(dir);
    p.push("telesthete");
    let _ = std::fs::create_dir_all(&p);
    p.push(format!("tlt-c-{}.sock", std::process::id()));
    p
}

fn open_internal(psk: &[u8], target_override: Option<PathBuf>) -> *mut TltProducer {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!(error = %e, "tlt_open: tokio runtime build failed");
            return std::ptr::null_mut();
        }
    };
    let key = derive_key(psk);
    let band_id = derive_band_id(psk);
    let our_path = producer_socket_path();
    let _ = std::fs::remove_file(&our_path);
    let transport = match rt.block_on(UnixTransport::bind(&our_path, key, band_id)) {
        Ok(t) => Arc::new(t),
        Err(e) => {
            tracing::warn!(error = %e, "tlt_open: UnixTransport::bind failed");
            return std::ptr::null_mut();
        }
    };
    let target = target_override.unwrap_or_else(|| default_target_for(psk));
    let handle = Box::new(TltProducer {
        rt: Arc::new(rt),
        transport,
        target: std::sync::Mutex::new(target),
    });
    Box::into_raw(handle)
}

/// Open a producer using the default local PSK and the default target
/// resolution (XDG_RUNTIME_DIR/telesthete/<band>.sock with /tmp
/// fallback). Returns NULL on failure.
///
/// `target_path` may be NULL to use the default; otherwise a
/// nul-terminated UTF-8 filesystem path overrides the target.
///
/// # Safety
/// `target_path` if non-NULL must point to a valid nul-terminated
/// C string for the duration of this call. The returned pointer must
/// be freed with [`tlt_close`].
#[no_mangle]
pub unsafe extern "C" fn tlt_open(target_path: *const std::os::raw::c_char) -> *mut TltProducer {
    let target_override = if target_path.is_null() {
        None
    } else {
        // SAFETY: caller documented non-NULL is a valid C string.
        let s = unsafe { CStr::from_ptr(target_path) };
        match s.to_str() {
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => return std::ptr::null_mut(),
        }
    };
    open_internal(LOCAL_PSK.as_bytes(), target_override)
}

/// Open with a custom PSK. `psk` may be empty (psk_len = 0) for a
/// fixed local profile; see SPEC.md §3.4. `target_path` follows the
/// same NULL-means-default rule as [`tlt_open`].
///
/// # Safety
/// `psk` must point to `psk_len` valid bytes. `target_path` rules
/// match [`tlt_open`].
#[no_mangle]
pub unsafe extern "C" fn tlt_open_with_psk(
    psk: *const u8,
    psk_len: usize,
    target_path: *const std::os::raw::c_char,
) -> *mut TltProducer {
    let psk_slice: &[u8] = if psk_len == 0 {
        &[]
    } else if psk.is_null() {
        return std::ptr::null_mut();
    } else {
        // SAFETY: caller documented psk has psk_len bytes.
        unsafe { std::slice::from_raw_parts(psk, psk_len) }
    };
    let target_override = if target_path.is_null() {
        None
    } else {
        // SAFETY: caller documented non-NULL is a valid C string.
        let s = unsafe { CStr::from_ptr(target_path) };
        match s.to_str() {
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => return std::ptr::null_mut(),
        }
    };
    open_internal(psk_slice, target_override)
}

/// Send a dmabuf-backed frame.
///
/// `flags` is the bitwise OR of `TLT_FLAG_*` constants. The `DMABUF`
/// flag is forced on by this function — callers should not include
/// it themselves but it is not an error to.
///
/// `planes` describes the on-wire plane table; `fds` is the array
/// the kernel duplicates via SCM_RIGHTS. With `TLT_FLAG_WITH_FENCE`,
/// the last fd in `fds` is the sync_file release fence.
///
/// # Safety
/// `handle` must be a valid pointer from [`tlt_open`] /
/// [`tlt_open_with_psk`]. `planes` must point to `plane_count` valid
/// `TltPlane` structs. `fds` must point to `fd_count` valid file
/// descriptors that remain live for the call's duration; the kernel
/// duplicates them during sendmsg, the caller retains ownership.
#[no_mangle]
pub unsafe extern "C" fn tlt_send_dmabuf(
    handle: *mut TltProducer,
    channel_id: u16,
    frame_id: u32,
    flags: u32,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    planes: *const TltPlane,
    plane_count: u8,
    fds: *const RawFd,
    fd_count: u8,
) -> i32 {
    if handle.is_null() {
        return TLT_ERR_NULL_HANDLE;
    }
    if plane_count == 0 || plane_count > TLT_MAX_PLANES {
        return TLT_ERR_PLANE_COUNT;
    }
    if fd_count > TLT_MAX_FDS {
        return TLT_ERR_FD_COUNT;
    }
    // SAFETY: caller-supplied non-null pointer from tlt_open.
    let h = unsafe { &*handle };

    // SAFETY: caller documented plane_count valid TltPlane structs.
    let plane_slice = unsafe { std::slice::from_raw_parts(planes, plane_count as usize) };
    let descriptor_planes: Vec<DmabufPlane> = plane_slice
        .iter()
        .map(|p| DmabufPlane {
            offset: p.offset,
            stride: p.stride,
            fd_index: p.fd_index,
        })
        .collect();
    let descriptor = DmabufDescriptor {
        width,
        height,
        fourcc,
        modifier,
        planes: descriptor_planes,
        fd_count,
    };

    let stream_flags = StreamFlags::from_bits(((flags & 0xFF) as u8) | StreamFlags::DMABUF.bits())
        .unwrap_or(StreamFlags::DMABUF);

    let need = STREAM_HEADER_LEN + DmabufDescriptor::encoded_len(plane_count as usize);
    let mut payload = vec![0u8; need];
    let hdr = StreamHeader {
        flags: stream_flags,
        frame_id,
    };
    if hdr.write(&mut payload).is_err() {
        return TLT_ERR_HEADER_WRITE;
    }
    if descriptor
        .write(&mut payload[STREAM_HEADER_LEN..])
        .is_err()
    {
        return TLT_ERR_DESCRIPTOR_WRITE;
    }

    // SAFETY: caller documented fd_count valid fds live for the call.
    let fd_slice: &[RawFd] = if fd_count == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(fds, fd_count as usize) }
    };
    let borrowed: Vec<BorrowedFd<'_>> = fd_slice
        .iter()
        .map(|&fd| {
            // SAFETY: caller documented each fd is live.
            unsafe { BorrowedFd::borrow_raw(fd) }
        })
        .collect();

    let target = h.target.lock().unwrap().clone();
    let transport = Arc::clone(&h.transport);

    h.rt.block_on(async move {
        match transport
            .send(UnixOutbound {
                to: target,
                channel_type: ChannelType::Stream,
                channel_id,
                plaintext: payload,
                priority: 0,
                fds: &borrowed,
            })
            .await
        {
            Ok(()) => TLT_OK,
            Err(e) => {
                tracing::warn!(channel_id, error = %e, "tlt_send_dmabuf failed");
                TLT_ERR_SEND
            }
        }
    })
}

/// Tear down a producer handle. NULL is a no-op.
///
/// # Safety
/// `handle` must be a pointer from [`tlt_open`] / [`tlt_open_with_psk`]
/// and not previously freed.
#[no_mangle]
pub unsafe extern "C" fn tlt_close(handle: *mut TltProducer) {
    if handle.is_null() {
        return;
    }
    // SAFETY: caller documented non-null and not previously freed.
    let _ = unsafe { Box::from_raw(handle) };
}
