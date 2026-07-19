//! dmabuf descriptor for Telesthete v1.1 §5.4.
//!
//! Pure data: pack/unpack of the on-wire descriptor that rides inside a
//! `Stream` payload when [`StreamFlags::DMABUF`] is set. The actual file
//! descriptors travel out-of-band via `SCM_RIGHTS`; this module never
//! sees them.
//!
//! ```text
//! Offset  Size  Field
//! 0       4 B   width        uint32 BE
//! 4       4 B   height       uint32 BE
//! 8       4 B   fourcc       uint32 LE   (DRM convention)
//! 12      8 B   modifier     uint64 BE   (DRM modifier; LINEAR=0, INVALID=0x00FFFFFFFFFFFFFF)
//! 20      1 B   plane_count  uint8       (1..=4)
//! 21      1 B   fd_count     uint8       (1..=5; up to 4 planes + optional fence)
//! 22      var   plane[i]:    9 B each — offset(4 BE) + stride(4 BE) + fd_index(1)
//! ```

use thiserror::Error;

use crate::wire::stream::StreamFlags;

pub const MAX_PLANES: usize = 4;
pub const MAX_FDS_PER_PACKET: usize = 5; // 4 planes + 1 sync_file fence

pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
pub const DRM_FORMAT_MOD_INVALID: u64 = 0x00FF_FFFF_FFFF_FFFF;

/// Descriptor for a single plane of a dmabuf-backed image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufPlane {
    pub offset: u32,
    pub stride: u32,
    /// Index into the SCM_RIGHTS fd array for the fd backing this plane.
    pub fd_index: u8,
}

/// Full dmabuf descriptor body (after the 8-byte StreamHeader).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmabufDescriptor {
    pub width: u32,
    pub height: u32,
    /// FourCC code, DRM convention. Stored little-endian on the wire so
    /// the four ASCII bytes match `fourcc_to_str` round-trips.
    pub fourcc: u32,
    pub modifier: u64,
    pub planes: Vec<DmabufPlane>,
    /// Number of fds in the accompanying SCM_RIGHTS message. Equals
    /// `planes.len()` (or higher if `WITH_FENCE` adds a sync_file at the
    /// end, or zero if `REUSE` is set).
    pub fd_count: u8,
}

#[derive(Debug, Error)]
pub enum DmabufError {
    #[error("dmabuf payload too short: {got} bytes, need at least {need}")]
    TooShort { got: usize, need: usize },
    #[error("plane_count {0} out of range (1..={MAX_PLANES})")]
    PlaneCountOutOfRange(u8),
    #[error("fd_count {0} exceeds MAX_FDS_PER_PACKET ({MAX_FDS_PER_PACKET})")]
    FdCountOutOfRange(u8),
    #[error("fd_count {fd} cannot serve plane fd_index {idx}")]
    FdIndexOutOfRange { fd: u8, idx: u8 },
    #[error("REUSE set but fd_count={0} (expected 0 or 1 with WITH_FENCE)")]
    ReuseFdMismatch(u8),
}

pub const HEADER_BYTES: usize = 22;
pub const PLANE_BYTES: usize = 9;

impl DmabufDescriptor {
    /// Serialized size for `n` planes.
    pub const fn encoded_len(plane_count: usize) -> usize {
        HEADER_BYTES + PLANE_BYTES * plane_count
    }

    /// Pack into `out`. Returns the number of bytes written. `out` must be
    /// at least [`Self::encoded_len`] long.
    pub fn write(&self, out: &mut [u8]) -> Result<usize, DmabufError> {
        let need = Self::encoded_len(self.planes.len());
        if out.len() < need {
            return Err(DmabufError::TooShort {
                got: out.len(),
                need,
            });
        }
        // Validate the count as usize BEFORE the u8 cast — `260 as u8 == 4`
        // would otherwise pass the range check and write a header claiming 4
        // planes while the loop emits all 260.
        if !(1..=MAX_PLANES).contains(&self.planes.len()) {
            return Err(DmabufError::PlaneCountOutOfRange(self.planes.len().min(255) as u8));
        }
        if self.fd_count as usize > MAX_FDS_PER_PACKET {
            return Err(DmabufError::FdCountOutOfRange(self.fd_count));
        }
        let plane_count = self.planes.len() as u8; // in 1..=MAX_PLANES; cast is exact

        out[0..4].copy_from_slice(&self.width.to_be_bytes());
        out[4..8].copy_from_slice(&self.height.to_be_bytes());
        out[8..12].copy_from_slice(&self.fourcc.to_le_bytes());
        out[12..20].copy_from_slice(&self.modifier.to_be_bytes());
        out[20] = plane_count;
        out[21] = self.fd_count;

        let mut off = HEADER_BYTES;
        for p in &self.planes {
            out[off..off + 4].copy_from_slice(&p.offset.to_be_bytes());
            out[off + 4..off + 8].copy_from_slice(&p.stride.to_be_bytes());
            out[off + 8] = p.fd_index;
            off += PLANE_BYTES;
        }
        Ok(off)
    }

    pub fn parse(input: &[u8]) -> Result<Self, DmabufError> {
        if input.len() < HEADER_BYTES {
            return Err(DmabufError::TooShort {
                got: input.len(),
                need: HEADER_BYTES,
            });
        }
        let width = u32::from_be_bytes(input[0..4].try_into().unwrap());
        let height = u32::from_be_bytes(input[4..8].try_into().unwrap());
        let fourcc = u32::from_le_bytes(input[8..12].try_into().unwrap());
        let modifier = u64::from_be_bytes(input[12..20].try_into().unwrap());
        let plane_count = input[20];
        let fd_count = input[21];

        if !(1..=MAX_PLANES as u8).contains(&plane_count) {
            return Err(DmabufError::PlaneCountOutOfRange(plane_count));
        }
        // Bound fd_count so a descriptor can't claim more fds than a packet may
        // carry (§9.4 MAX_FDS_PER_PACKET); check_flags only bounds the planes.
        if fd_count as usize > MAX_FDS_PER_PACKET {
            return Err(DmabufError::FdCountOutOfRange(fd_count));
        }
        let need = Self::encoded_len(plane_count as usize);
        if input.len() < need {
            return Err(DmabufError::TooShort {
                got: input.len(),
                need,
            });
        }

        let mut planes = Vec::with_capacity(plane_count as usize);
        let mut off = HEADER_BYTES;
        for _ in 0..plane_count {
            let offset = u32::from_be_bytes(input[off..off + 4].try_into().unwrap());
            let stride = u32::from_be_bytes(input[off + 4..off + 8].try_into().unwrap());
            let fd_index = input[off + 8];
            if fd_count > 0 && fd_index >= fd_count {
                return Err(DmabufError::FdIndexOutOfRange {
                    fd: fd_count,
                    idx: fd_index,
                });
            }
            planes.push(DmabufPlane {
                offset,
                stride,
                fd_index,
            });
            off += PLANE_BYTES;
        }
        Ok(Self {
            width,
            height,
            fourcc,
            modifier,
            planes,
            fd_count,
        })
    }

    /// Validate consistency between this descriptor and the StreamFlags
    /// of the carrying packet. Call after `parse`.
    pub fn check_flags(&self, flags: StreamFlags) -> Result<(), DmabufError> {
        let expected_fence_fds = if flags.contains(StreamFlags::WITH_FENCE) {
            1
        } else {
            0
        };

        if flags.contains(StreamFlags::REUSE) {
            // REUSE means the consumer already imported the buffer; only
            // a fresh fence fd may accompany the packet.
            if self.fd_count != expected_fence_fds {
                return Err(DmabufError::ReuseFdMismatch(self.fd_count));
            }
        } else {
            // Normal case: planes carry fds. Highest fd_index referenced
            // by a plane must fit within `fd_count - expected_fence_fds`.
            let plane_fds_needed = self
                .planes
                .iter()
                .map(|p| p.fd_index as u16 + 1)
                .max()
                .unwrap_or(0);
            let plane_fds_available = self.fd_count.saturating_sub(expected_fence_fds);
            if plane_fds_needed > plane_fds_available as u16 {
                return Err(DmabufError::FdIndexOutOfRange {
                    fd: self.fd_count,
                    idx: (plane_fds_needed - 1) as u8,
                });
            }
        }
        Ok(())
    }
}

/// Convert a fourcc to its 4-byte ASCII representation. Useful for
/// logging.
pub fn fourcc_to_str(fourcc: u32) -> [u8; 4] {
    fourcc.to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_le_bytes(*s)
    }

    #[test]
    fn xr24_single_plane_round_trip() {
        let desc = DmabufDescriptor {
            width: 1920,
            height: 1080,
            fourcc: fourcc(b"XR24"),
            modifier: DRM_FORMAT_MOD_LINEAR,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: 1920 * 4,
                fd_index: 0,
            }],
            fd_count: 1,
        };
        let mut buf = vec![0u8; DmabufDescriptor::encoded_len(1)];
        let n = desc.write(&mut buf).unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(n, 31);
        let parsed = DmabufDescriptor::parse(&buf).unwrap();
        assert_eq!(parsed, desc);
    }

    #[test]
    fn nv12_two_plane_round_trip() {
        let desc = DmabufDescriptor {
            width: 3840,
            height: 2160,
            fourcc: fourcc(b"NV12"),
            modifier: 0xFFFF_0001_0203_0405,
            planes: vec![
                DmabufPlane {
                    offset: 0,
                    stride: 3840,
                    fd_index: 0,
                },
                DmabufPlane {
                    offset: 3840 * 2160,
                    stride: 3840,
                    fd_index: 0,
                },
            ],
            fd_count: 1,
        };
        let mut buf = vec![0u8; DmabufDescriptor::encoded_len(2)];
        desc.write(&mut buf).unwrap();
        let parsed = DmabufDescriptor::parse(&buf).unwrap();
        assert_eq!(parsed, desc);
        assert_eq!(buf.len(), 40);
    }

    #[test]
    fn fourcc_endianness_matches_drm_ascii() {
        // 'XR24' on the wire must be 0x58 0x52 0x32 0x34 in that byte
        // order, because tools dump it as the ASCII string "XR24".
        let desc = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: 4,
                fd_index: 0,
            }],
            fd_count: 1,
        };
        let mut buf = vec![0u8; DmabufDescriptor::encoded_len(1)];
        desc.write(&mut buf).unwrap();
        assert_eq!(&buf[8..12], b"XR24");
        assert_eq!(fourcc_to_str(desc.fourcc), *b"XR24");
    }

    #[test]
    fn check_flags_with_fence_offsets_plane_fd_budget() {
        let desc = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: 4,
                fd_index: 0,
            }],
            fd_count: 2, // plane fd + fence fd
        };
        desc.check_flags(StreamFlags::DMABUF | StreamFlags::WITH_FENCE)
            .unwrap();
    }

    #[test]
    fn check_flags_reuse_requires_zero_fds() {
        let desc = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: 4,
                fd_index: 0,
            }],
            fd_count: 0,
        };
        desc.check_flags(StreamFlags::DMABUF | StreamFlags::REUSE)
            .unwrap();

        // REUSE + fd_count=1 without WITH_FENCE is malformed.
        let mut bad = desc.clone();
        bad.fd_count = 1;
        assert!(matches!(
            bad.check_flags(StreamFlags::DMABUF | StreamFlags::REUSE),
            Err(DmabufError::ReuseFdMismatch(1))
        ));

        // REUSE + WITH_FENCE allows exactly one fd (the fence).
        let mut with_fence = desc.clone();
        with_fence.fd_count = 1;
        with_fence
            .check_flags(StreamFlags::DMABUF | StreamFlags::REUSE | StreamFlags::WITH_FENCE)
            .unwrap();
    }

    #[test]
    fn rejects_zero_planes() {
        let mut buf = vec![0u8; HEADER_BYTES];
        // width/height/fourcc/modifier all zero, plane_count=0, fd_count=0.
        assert!(matches!(
            DmabufDescriptor::parse(&buf),
            Err(DmabufError::PlaneCountOutOfRange(0))
        ));
        // Also reject too-many.
        buf[20] = (MAX_PLANES + 1) as u8;
        assert!(matches!(
            DmabufDescriptor::parse(&buf),
            Err(DmabufError::PlaneCountOutOfRange(_))
        ));
    }

    #[test]
    fn write_rejects_260_planes_instead_of_truncating() {
        // 260 as u8 == 4: without the usize-first check, the header would
        // claim 4 planes while the body carries 260.
        let plane = DmabufPlane {
            offset: 0,
            stride: 4,
            fd_index: 0,
        };
        let desc = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![plane; 260],
            fd_count: 1,
        };
        let mut buf = vec![0u8; DmabufDescriptor::encoded_len(260)];
        assert!(matches!(
            desc.write(&mut buf),
            Err(DmabufError::PlaneCountOutOfRange(_))
        ));
    }

    #[test]
    fn write_rejects_fd_count_above_packet_max() {
        let desc = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: 4,
                fd_index: 0,
            }],
            fd_count: 200,
        };
        let mut buf = vec![0u8; DmabufDescriptor::encoded_len(1)];
        assert!(matches!(
            desc.write(&mut buf),
            Err(DmabufError::FdCountOutOfRange(200))
        ));
    }

    #[test]
    fn parse_rejects_fd_count_above_packet_max() {
        let good = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: 4,
                fd_index: 0,
            }],
            fd_count: 1,
        };
        let mut buf = vec![0u8; DmabufDescriptor::encoded_len(1)];
        good.write(&mut buf).unwrap();
        buf[21] = 200; // fd_count way past MAX_FDS_PER_PACKET
        assert!(matches!(
            DmabufDescriptor::parse(&buf),
            Err(DmabufError::FdCountOutOfRange(200))
        ));
    }

    /// Byte-identical with the Python reference: tests/vectors.json `dmabuf`
    /// and `stream_header`.
    #[test]
    fn conformance_vectors_match_python() {
        use crate::wire::stream::{StreamFlags, StreamHeader, STREAM_HEADER_LEN};
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/vectors.json");
        let raw = std::fs::read_to_string(path).expect("read tests/vectors.json");
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();

        fn to_hex(b: &[u8]) -> String {
            b.iter().map(|x| format!("{x:02x}")).collect()
        }

        if let Some(cases) = v.get("stream_header").and_then(|c| c.as_array()) {
            assert!(!cases.is_empty());
            for case in cases {
                let h = StreamHeader {
                    flags: StreamFlags::from_bits(case["flags"].as_u64().unwrap() as u8)
                        .unwrap(),
                    frame_id: case["frame_id"].as_u64().unwrap() as u32,
                };
                let mut buf = [0u8; STREAM_HEADER_LEN];
                h.write(&mut buf).unwrap();
                assert_eq!(to_hex(&buf), case["packed_hex"].as_str().unwrap());
            }
        }

        if let Some(cases) = v.get("dmabuf").and_then(|c| c.as_array()) {
            assert!(!cases.is_empty());
            for case in cases {
                let planes: Vec<DmabufPlane> = case["planes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| DmabufPlane {
                        offset: p["offset"].as_u64().unwrap() as u32,
                        stride: p["stride"].as_u64().unwrap() as u32,
                        fd_index: p["fd_index"].as_u64().unwrap() as u8,
                    })
                    .collect();
                let desc = DmabufDescriptor {
                    width: case["width"].as_u64().unwrap() as u32,
                    height: case["height"].as_u64().unwrap() as u32,
                    fourcc: u32::from_le_bytes(
                        case["fourcc"].as_str().unwrap().as_bytes().try_into().unwrap(),
                    ),
                    modifier: u64::from_str_radix(case["modifier"].as_str().unwrap(), 16)
                        .unwrap(),
                    fd_count: case["fd_count"].as_u64().unwrap() as u8,
                    planes,
                };
                let mut buf = vec![0u8; DmabufDescriptor::encoded_len(desc.planes.len())];
                desc.write(&mut buf).unwrap();
                assert_eq!(to_hex(&buf), case["packed_hex"].as_str().unwrap(), "{case}");
                assert_eq!(DmabufDescriptor::parse(&buf).unwrap(), desc);
            }
        }
    }

    #[test]
    fn rejects_truncated_plane_table() {
        let desc = DmabufDescriptor {
            width: 1,
            height: 1,
            fourcc: fourcc(b"XR24"),
            modifier: 0,
            planes: vec![DmabufPlane {
                offset: 0,
                stride: 4,
                fd_index: 0,
            }],
            fd_count: 1,
        };
        let mut buf = vec![0u8; DmabufDescriptor::encoded_len(1)];
        desc.write(&mut buf).unwrap();
        // Drop the last byte of the plane table.
        let truncated = &buf[..buf.len() - 1];
        assert!(matches!(
            DmabufDescriptor::parse(truncated),
            Err(DmabufError::TooShort { .. })
        ));
    }
}
