//! Telesthete v1.2 — encrypted P2P transport for window/desktop sharing.
//!
//! Wire spec: `SPEC.md` at the repo root. The crate exposes the band/channel/
//! stream session layer plus on-the-wire types under [`mod@wire`].
//!
//! Public API mirrors the original Python reference (`Band`, `Stream`,
//! `Channel`, `Control`) with idiomatic Rust async on top of `tokio`.

// AF_UNIX transport requires a small unsafe block (recvmsg with SCM_RIGHTS
// uses raw fd ops). Scoped to `unix_transport.rs`; everything else still
// rejects unsafe.
#![deny(unsafe_op_in_unsafe_fn)]

pub mod band;
pub mod channel;
pub mod control;
pub mod crypto;
pub mod framing;
pub mod stream;
pub mod transport;
pub mod unix_transport;
pub mod wire;

pub use band::{Band, BandError};
pub use channel::{ChannelEndpoint, ChannelError, ChannelHub, ChannelMessage, ChannelSender};
pub use control::{capability, ControlChannel, ControlError, ControlEvent, LOCAL_PSK};
pub use crypto::{build_aad, derive_band_id, derive_key, BandId, CryptoError, Key, BAND_ID_LEN};
pub use framing::{
    decode_packet, encode_packet, ChannelType, FramingError, Header, HEADER_LEN, MIN_PACKET_LEN,
};
pub use stream::{StreamEndpoint, StreamError, StreamHub, StreamMessage};
pub use transport::{Inbound, Outbound, Transport, TransportError};
pub use unix_transport::{
    UnixInbound, UnixOutbound, UnixTransport, UnixTransportError, MAX_FDS_PER_PACKET,
    SOCKET_DIR_ENV, SOCKET_DIR_FALLBACK,
};
pub use wire::{
    DmabufDescriptor, DmabufError, DmabufPlane, StreamFlags, StreamHeader, StreamHeaderError,
    DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR, MAX_PLANES, STREAM_HEADER_LEN,
};
