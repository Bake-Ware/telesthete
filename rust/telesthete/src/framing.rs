//! 27-byte header pack/unpack per SPEC §1.
//!
//! ```text
//!  0      16 B   band_id    (cleartext)
//!  16      1 B   channel_type (uint8)
//!  17      2 B   channel_id   (uint16 BE)
//!  19      8 B   sequence     (uint64 BE)
//!  27    var     ciphertext   (XChaCha20-Poly1305 output)
//! ```

use thiserror::Error;

use crate::crypto::{build_aad, decrypt, encrypt, BandId, CryptoError, Key, BAND_ID_LEN, TAG_LEN};

pub const HEADER_LEN: usize = 27;
pub const MIN_PACKET_LEN: usize = HEADER_LEN + TAG_LEN; // 43

/// Multiplexing selector. SPEC §2.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChannelType {
    Control = 0x00,
    Stream = 0x01,
    Channel = 0x02,
    Board = 0x03,
    Drop = 0x04,
}

impl ChannelType {
    pub fn from_u8(x: u8) -> Option<Self> {
        match x {
            0x00 => Some(Self::Control),
            0x01 => Some(Self::Stream),
            0x02 => Some(Self::Channel),
            0x03 => Some(Self::Board),
            0x04 => Some(Self::Drop),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub band_id: BandId,
    pub channel_type: ChannelType,
    pub channel_id: u16,
    pub sequence: u64,
}

#[derive(Debug, Error)]
pub enum FramingError {
    #[error("packet shorter than minimum {MIN_PACKET_LEN} bytes ({0})")]
    TooShort(usize),
    #[error("unknown channel_type byte: 0x{0:02x}")]
    UnknownChannelType(u8),
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
}

impl Header {
    /// Serialize the 27-byte cleartext header into the start of `out`.
    /// Returns the offset where ciphertext should begin (always
    /// [`HEADER_LEN`]).
    pub fn write(&self, out: &mut [u8]) -> usize {
        debug_assert!(out.len() >= HEADER_LEN, "buffer too small");
        out[..BAND_ID_LEN].copy_from_slice(&self.band_id);
        out[16] = self.channel_type as u8;
        out[17..19].copy_from_slice(&self.channel_id.to_be_bytes());
        out[19..27].copy_from_slice(&self.sequence.to_be_bytes());
        HEADER_LEN
    }

    /// Parse the 27-byte header from the start of `input`.
    pub fn parse(input: &[u8]) -> Result<Self, FramingError> {
        if input.len() < HEADER_LEN {
            return Err(FramingError::TooShort(input.len()));
        }
        let mut band_id = [0u8; BAND_ID_LEN];
        band_id.copy_from_slice(&input[..BAND_ID_LEN]);
        let channel_type =
            ChannelType::from_u8(input[16]).ok_or(FramingError::UnknownChannelType(input[16]))?;
        let channel_id = u16::from_be_bytes([input[17], input[18]]);
        let sequence = u64::from_be_bytes([
            input[19], input[20], input[21], input[22], input[23], input[24], input[25], input[26],
        ]);
        Ok(Self {
            band_id,
            channel_type,
            channel_id,
            sequence,
        })
    }
}

/// Encode plaintext payload into a full Telesthete packet (header || ciphertext).
pub fn encode_packet(
    key: &Key,
    band_id: &BandId,
    channel_type: ChannelType,
    channel_id: u16,
    sequence: u64,
    plaintext: &[u8],
) -> Result<Vec<u8>, FramingError> {
    let aad = build_aad(channel_type as u8, channel_id);
    let ct = encrypt(key, sequence, &aad, plaintext)?;
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.resize(HEADER_LEN, 0);
    let header = Header {
        band_id: *band_id,
        channel_type,
        channel_id,
        sequence,
    };
    header.write(&mut out);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decode + decrypt a Telesthete packet. Returns the parsed header and the
/// recovered plaintext payload.
pub fn decode_packet(key: &Key, packet: &[u8]) -> Result<(Header, Vec<u8>), FramingError> {
    if packet.len() < MIN_PACKET_LEN {
        return Err(FramingError::TooShort(packet.len()));
    }
    let header = Header::parse(packet)?;
    let aad = build_aad(header.channel_type as u8, header.channel_id);
    let pt = decrypt(key, header.sequence, &aad, &packet[HEADER_LEN..])?;
    Ok((header, pt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{derive_band_id, derive_key};

    const PSK: &[u8] = b"telesthete-test-psk";

    #[test]
    fn header_round_trip() {
        let h = Header {
            band_id: [7u8; BAND_ID_LEN],
            channel_type: ChannelType::Stream,
            channel_id: 0xBEEF,
            sequence: 0x0123_4567_89AB_CDEF,
        };
        let mut buf = vec![0u8; HEADER_LEN];
        let off = h.write(&mut buf);
        assert_eq!(off, HEADER_LEN);
        let parsed = Header::parse(&buf).unwrap();
        assert_eq!(parsed, h);
    }

    #[test]
    fn header_unknown_channel_type() {
        let mut buf = vec![0u8; HEADER_LEN];
        buf[16] = 0xFF;
        assert!(matches!(
            Header::parse(&buf),
            Err(FramingError::UnknownChannelType(0xFF))
        ));
    }

    #[test]
    fn packet_round_trip() {
        let key = derive_key(PSK);
        let band_id = derive_band_id(PSK);
        let payload = b"hello, telesthete";
        let pkt = encode_packet(&key, &band_id, ChannelType::Stream, 1, 42, payload).unwrap();
        assert_eq!(pkt.len(), HEADER_LEN + payload.len() + TAG_LEN);

        let (header, recovered) = decode_packet(&key, &pkt).unwrap();
        assert_eq!(header.band_id, band_id);
        assert_eq!(header.channel_type, ChannelType::Stream);
        assert_eq!(header.channel_id, 1);
        assert_eq!(header.sequence, 42);
        assert_eq!(recovered, payload);
    }

    #[test]
    fn tampered_header_rejected() {
        // Modifying channel_id flips the AAD; decrypt should fail.
        let key = derive_key(PSK);
        let band_id = derive_band_id(PSK);
        let mut pkt = encode_packet(&key, &band_id, ChannelType::Stream, 1, 1, b"x").unwrap();
        pkt[18] ^= 0xFF; // corrupt channel_id low byte
        assert!(decode_packet(&key, &pkt).is_err());
    }

    #[test]
    fn tampered_ciphertext_rejected() {
        let key = derive_key(PSK);
        let band_id = derive_band_id(PSK);
        let mut pkt = encode_packet(&key, &band_id, ChannelType::Stream, 1, 1, b"x").unwrap();
        let last = pkt.len() - 1;
        pkt[last] ^= 0x01;
        assert!(decode_packet(&key, &pkt).is_err());
    }
}
