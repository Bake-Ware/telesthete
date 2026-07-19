//! Cleartext header inspection for routing (SPEC §1).
//!
//! The hub reads ONLY the fields §1 marks cleartext: `band_id` (routing) and
//! `channel_type` / `channel_id` (WebTransport carrier selection, §9.6).
//! Everything from offset 27 on is opaque ciphertext the hub never touches —
//! it holds no PSK and cannot decrypt (§10).
//!
//! Offsets, the [`ChannelType`] enum, and [`MIN_PACKET_LEN`] come from the
//! reference library (`telesthete::framing`) so the hub and the library agree
//! byte-for-byte (conformance A5).

use telesthete::{BandId, ChannelType, BAND_ID_LEN, MIN_PACKET_LEN};

/// Everything the hub needs to route one frame, pulled from the cleartext
/// header. `Copy` so callers can drop the borrow on the packet buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteInfo {
    pub band_id: BandId,
    /// `None` when byte 16 is not a known channel type (0x05+). Routing still
    /// works — `band_id` is valid — but WebTransport carrier selection (§9.6)
    /// has no hint and falls back to a reliable carrier.
    pub channel_type: Option<ChannelType>,
    pub channel_id: u16,
}

/// Inspect a packet's cleartext header.
///
/// Returns `None` if the packet is shorter than a legal Telesthete frame
/// (`MIN_PACKET_LEN` = 43 = 27-byte header + 16-byte tag, SPEC §1/§12.4). Such
/// a packet is malformed and MUST NOT be routed (conformance A3).
pub fn route_info(pkt: &[u8]) -> Option<RouteInfo> {
    if pkt.len() < MIN_PACKET_LEN {
        return None;
    }
    let mut band_id: BandId = [0u8; BAND_ID_LEN];
    band_id.copy_from_slice(&pkt[..BAND_ID_LEN]);
    Some(RouteInfo {
        band_id,
        channel_type: ChannelType::from_u8(pkt[16]),
        channel_id: u16::from_be_bytes([pkt[17], pkt[18]]),
    })
}

/// Lowercase hex of a `band_id`, for logs and the AF_UNIX / WebTransport paths
/// (`$XDG_RUNTIME_DIR/telesthete/<band_id_hex>.sock`, `?band=<hex>`).
pub fn band_hex(b: &BandId) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(BAND_ID_LEN * 2);
    for byte in b {
        let _ = write!(s, "{byte:02x}");
    }
    s
}

/// Parse a `band_id` from a lowercase/uppercase hex string (the WebTransport
/// `?band=` query, §9.6). Returns `None` on wrong length or non-hex.
pub fn band_from_hex(s: &str) -> Option<BandId> {
    if s.len() != BAND_ID_LEN * 2 {
        return None;
    }
    let mut out: BandId = [0u8; BAND_ID_LEN];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use telesthete::{Header, HEADER_LEN};

    /// Build a minimal well-formed frame (43 bytes) with a chosen header.
    fn make_frame(band: BandId, ct: u8, cid: u16) -> Vec<u8> {
        let mut v = vec![0u8; MIN_PACKET_LEN];
        v[..BAND_ID_LEN].copy_from_slice(&band);
        v[16] = ct;
        v[17..19].copy_from_slice(&cid.to_be_bytes());
        // bytes 19..27 sequence = 0; 27..43 opaque "tag" = 0
        v
    }

    #[test]
    fn band_id_is_first_16() {
        // A1
        let band = [0xABu8; BAND_ID_LEN];
        let f = make_frame(band, 0x01, 0);
        assert_eq!(route_info(&f).unwrap().band_id, band);
    }

    #[test]
    fn reads_channel_type_and_id_be() {
        // A2
        let f = make_frame([0u8; BAND_ID_LEN], 0x02, 0xBEEF);
        let info = route_info(&f).unwrap();
        assert_eq!(info.channel_type, Some(ChannelType::Channel));
        assert_eq!(info.channel_id, 0xBEEF);
    }

    #[test]
    fn rejects_below_min_packet() {
        // A3 — 42 bytes is one below the spec minimum.
        assert!(route_info(&[0u8; MIN_PACKET_LEN - 1]).is_none());
        assert!(route_info(&[]).is_none());
    }

    #[test]
    fn accepts_exactly_min() {
        // A3 — exactly 43 bytes is the smallest legal frame.
        assert!(route_info(&[0u8; MIN_PACKET_LEN]).is_some());
    }

    #[test]
    fn unknown_channel_type_still_routable() {
        // A4 — byte 16 = 0x05 is not a defined channel type, but band_id and
        // channel_id remain usable for routing.
        let band = [9u8; BAND_ID_LEN];
        let f = make_frame(band, 0x05, 7);
        let info = route_info(&f).unwrap();
        assert_eq!(info.channel_type, None);
        assert_eq!(info.band_id, band);
        assert_eq!(info.channel_id, 7);
    }

    #[test]
    fn matches_library_header() {
        // A5 — for a well-formed known-type frame, our routing view agrees with
        // the library's authoritative Header::parse on every cleartext field.
        let band = [3u8; BAND_ID_LEN];
        let f = make_frame(band, 0x00, 0x1234);
        let hdr = Header::parse(&f).unwrap();
        let info = route_info(&f).unwrap();
        assert_eq!(info.band_id, hdr.band_id);
        assert_eq!(info.channel_id, hdr.channel_id);
        assert_eq!(info.channel_type, Some(hdr.channel_type));
        assert_eq!(HEADER_LEN, 27);
    }

    #[test]
    fn band_hex_round_trips() {
        let band = [0x0f, 0xa0, 0x00, 0xff, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let hex = band_hex(&band);
        assert_eq!(hex.len(), 32);
        assert_eq!(band_from_hex(&hex), Some(band));
        assert_eq!(band_from_hex("nothex"), None);
        assert_eq!(band_from_hex(&hex[..30]), None); // wrong length
    }
}
