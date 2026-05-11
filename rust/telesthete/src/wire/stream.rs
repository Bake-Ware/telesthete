//! Stream payload header — see `SPEC.md` §5.4.
//!
//! Each Telesthete Stream packet carries one H.264 Annex-B NAL unit (or
//! SPS+PPS init blob). The header is 8 bytes:
//!
//! ```text
//! Offset  Size  Field
//! 0       1 B   flags        bitfield (StreamFlags)
//! 1       3 B   reserved     must be 0
//! 4       4 B   frame_id     uint32 BE, monotonic per window
//! 8       var   nal_data     raw NAL bytes (no 0x00000001 prefix)
//! ```

use thiserror::Error;

pub const STREAM_HEADER_LEN: usize = 8;

bitflags::bitflags! {
    /// Bit 0 INIT — payload is SPS+PPS concatenated (Annex-B framed).
    /// Bit 1 KEYFRAME — NAL is IDR slice.
    /// Bit 2 END_FRAME — last NAL of this `frame_id`.
    /// Bit 3 FRAGMENT_CONT — this packet is a continuation of the
    /// previous packet's NAL (same `frame_id`). Decoder appends the
    /// payload bytes to the in-flight NAL without prepending a fresh
    /// Annex-B start code. Used when an encoded NAL is too large to
    /// fit one Telesthete UDP packet (e.g. high-resolution IDR).
    ///
    /// Bits 4–7 are reserved by Telesthete v1.1 (see `SPEC.md`
    /// §5.4.1). Senders MUST NOT set them without prior capability
    /// negotiation; receivers parse them even when the capability is
    /// absent so an offending packet can be dropped explicitly rather
    /// than silently misread as a v1.0 NAL.
    ///
    /// Bit 4 DMABUF — payload is a dmabuf descriptor (§5.4), not a
    /// NAL. Implies the packet is one full frame (no FRAGMENT_CONT).
    /// Bit 5 WITH_FENCE — ancillary fd list ends with a sync_file
    /// release fence; consumer must wait it before sampling.
    /// Bit 6 REUSE — producer hint that the dmabuf is the same one
    /// as the previous frame; only valid alongside DMABUF.
    /// Bit 7 EXTENDED — reserved for future header growth; parsers
    /// that don't understand the extension MUST drop the packet.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct StreamFlags: u8 {
        const INIT = 0x01;
        const KEYFRAME = 0x02;
        const END_FRAME = 0x04;
        const FRAGMENT_CONT = 0x08;
        const DMABUF = 0x10;
        const WITH_FENCE = 0x20;
        const REUSE = 0x40;
        const EXTENDED = 0x80;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamHeader {
    pub flags: StreamFlags,
    pub frame_id: u32,
}

#[derive(Debug, Error)]
pub enum StreamHeaderError {
    #[error("stream payload too short: {0} bytes (need at least {STREAM_HEADER_LEN})")]
    TooShort(usize),
    #[error("reserved bytes nonzero")]
    ReservedNonzero,
    #[error("unknown flag bits set: 0x{0:02x}")]
    UnknownFlags(u8),
}

impl StreamHeader {
    /// Pack header into the first 8 bytes of `out`. Returns the offset where
    /// NAL bytes should be appended (always 8). Caller is responsible for
    /// providing a buffer of at least `STREAM_HEADER_LEN + nal.len()`.
    pub fn write(&self, out: &mut [u8]) -> Result<usize, StreamHeaderError> {
        if out.len() < STREAM_HEADER_LEN {
            return Err(StreamHeaderError::TooShort(out.len()));
        }
        out[0] = self.flags.bits();
        out[1] = 0;
        out[2] = 0;
        out[3] = 0;
        out[4..8].copy_from_slice(&self.frame_id.to_be_bytes());
        Ok(STREAM_HEADER_LEN)
    }

    /// Parse a Stream packet payload. Returns the header and the NAL slice.
    pub fn parse(input: &[u8]) -> Result<(Self, &[u8]), StreamHeaderError> {
        if input.len() < STREAM_HEADER_LEN {
            return Err(StreamHeaderError::TooShort(input.len()));
        }
        let raw_flags = input[0];
        let flags =
            StreamFlags::from_bits(raw_flags).ok_or(StreamHeaderError::UnknownFlags(raw_flags))?;
        if input[1] != 0 || input[2] != 0 || input[3] != 0 {
            return Err(StreamHeaderError::ReservedNonzero);
        }
        let frame_id = u32::from_be_bytes([input[4], input[5], input[6], input[7]]);
        Ok((Self { flags, frame_id }, &input[STREAM_HEADER_LEN..]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let h = StreamHeader {
            flags: StreamFlags::KEYFRAME | StreamFlags::END_FRAME,
            frame_id: 0xDEADBEEF,
        };
        let mut buf = vec![0u8; STREAM_HEADER_LEN + 3];
        let off = h.write(&mut buf).unwrap();
        assert_eq!(off, STREAM_HEADER_LEN);
        buf[off..].copy_from_slice(b"abc");

        let (parsed, rest) = StreamHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert_eq!(rest, b"abc");
    }

    #[test]
    fn parses_v1_1_flag_bits() {
        // v1.1 reserved DMABUF (0x10), WITH_FENCE (0x20), REUSE (0x40),
        // EXTENDED (0x80). All eight bits are now defined; the parser
        // accepts every combination and lets higher layers decide
        // whether they are willing to honour them.
        let h = StreamHeader {
            flags: StreamFlags::DMABUF | StreamFlags::WITH_FENCE | StreamFlags::END_FRAME,
            frame_id: 7,
        };
        let mut buf = vec![0u8; STREAM_HEADER_LEN];
        h.write(&mut buf).unwrap();
        let (parsed, _) = StreamHeader::parse(&buf).unwrap();
        assert_eq!(parsed, h);
        assert!(parsed.flags.contains(StreamFlags::DMABUF));
        assert!(parsed.flags.contains(StreamFlags::WITH_FENCE));
        assert!(!parsed.flags.contains(StreamFlags::REUSE));
    }

    #[test]
    fn rejects_reserved_nonzero() {
        let mut buf = vec![0u8; STREAM_HEADER_LEN];
        buf[2] = 1;
        assert!(matches!(
            StreamHeader::parse(&buf),
            Err(StreamHeaderError::ReservedNonzero)
        ));
    }
}
