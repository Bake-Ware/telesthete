//! Telesthete wire types — on-the-wire formats that producers and consumers
//! must agree on byte-for-byte. See `SPEC.md` (v1.1).
//!
//! - [`stream`] — Stream packet payload header + StreamFlags (§4 + v1.1 §1.2)
//! - [`dmabuf`] — dmabuf descriptor body for `StreamFlags::DMABUF` packets (v1.1 §5.4)
//! - [`fragment`] — Channel message fragmentation envelope (v1.2 §6.6)

pub mod dmabuf;
pub mod fragment;
pub mod stream;

pub use dmabuf::{
    DmabufDescriptor, DmabufError, DmabufPlane, DRM_FORMAT_MOD_INVALID, DRM_FORMAT_MOD_LINEAR,
    MAX_FDS_PER_PACKET, MAX_PLANES,
};
pub use fragment::{
    fragment, pack_chunk, parse_chunk, Reassembler, Chunk, FRAG_HEADER_LEN, MAX_CHUNK_PAYLOAD,
};
pub use stream::{StreamFlags, StreamHeader, StreamHeaderError, STREAM_HEADER_LEN};
