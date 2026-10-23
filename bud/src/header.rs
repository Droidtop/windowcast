//! The packet header: 24 bytes in front of every datagram, sent in the clear
//! and authenticated as the AEAD's associated data, so a changed header makes
//! the packet fail to open instead of being misread.
//!
//! ```text
//!  0        1        2..4      4..8     8..10     10..12     12..20    20..24
//! +--------+--------+---------+--------+---------+----------+---------+---------+
//! |version | flags  | stream  | frame  |fragment |fragments | packet  | sent_us |
//! +--------+--------+---------+--------+---------+----------+---------+---------+
//! ```
//!
//! All numbers are big-endian. `frame` counts a stream's frames, `fragment`
//! and `fragments` say which piece of the frame this is and how many there
//! are (a frame that fits one datagram is 0 of 1), `packet` is the sender's
//! packet number (it is also the AEAD nonce), and `sent_us` is the low 32 bits
//! of the sender's microsecond clock, for the receiver's delay measurements.

/// The wire version this crate speaks.
pub const VERSION: u8 = 1;
/// The header's length.
pub const HEADER_LEN: usize = 24;
/// The AEAD tag appended to every payload.
pub const TAG_LEN: usize = 16;
/// The largest datagram sent, so it fits a path with a 1280-byte minimum MTU
/// (IPv6's) after the IP and UDP headers and some tunnelling overhead.
pub const MAX_DATAGRAM: usize = 1200;
/// The most payload one datagram carries.
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM - HEADER_LEN - TAG_LEN;

/// `flags` bit: this packet is parity, not frame data. For a parity packet
/// `fragment` numbers the frame's parity packets and `fragments` is the
/// frame's data fragment count (see [`crate::parity`]).
pub const FLAG_PARITY: u8 = 0x01;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    /// Which stream: a window's video, audio, control or input. What the
    /// numbers mean is up to the layer above.
    pub stream: u16,
    pub frame: u32,
    pub fragment: u16,
    pub fragments: u16,
    /// Set by the sealer; whatever a caller puts here is replaced.
    pub packet: u64,
    pub sent_us: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HeaderError {
    #[error("the datagram is shorter than a header")]
    Short,
    #[error("unknown wire version {0}")]
    Version(u8),
    #[error("fragment {fragment} of {fragments} is not a valid position")]
    Fragment { fragment: u16, fragments: u16 },
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = VERSION;
        out[1] = self.flags;
        out[2..4].copy_from_slice(&self.stream.to_be_bytes());
        out[4..8].copy_from_slice(&self.frame.to_be_bytes());
        out[8..10].copy_from_slice(&self.fragment.to_be_bytes());
        out[10..12].copy_from_slice(&self.fragments.to_be_bytes());
        out[12..20].copy_from_slice(&self.packet.to_be_bytes());
        out[20..24].copy_from_slice(&self.sent_us.to_be_bytes());
        out
    }

    /// Reads the header at the front of `datagram`. Anything the sender could
    /// have got wrong is an error here, so nothing past this point has to
    /// trust a field.
    pub fn decode(datagram: &[u8]) -> Result<Header, HeaderError> {
        if datagram.len() < HEADER_LEN {
            return Err(HeaderError::Short);
        }
        if datagram[0] != VERSION {
            return Err(HeaderError::Version(datagram[0]));
        }
        let be16 = |at: usize| u16::from_be_bytes([datagram[at], datagram[at + 1]]);
        let be32 = |at: usize| {
            u32::from_be_bytes([
                datagram[at],
                datagram[at + 1],
                datagram[at + 2],
                datagram[at + 3],
            ])
        };
        let mut packet = [0u8; 8];
        packet.copy_from_slice(&datagram[12..20]);
        let header = Header {
            flags: datagram[1],
            stream: be16(2),
            frame: be32(4),
            fragment: be16(8),
            fragments: be16(10),
            packet: u64::from_be_bytes(packet),
            sent_us: be32(20),
        };
        // A parity packet's `fragment` counts parity packets, which a small
        // frame can have more of than it has data fragments; its `fragments`
        // is the frame's data fragment count and must still be at least 1.
        let parity = header.flags & FLAG_PARITY != 0;
        if header.fragments == 0 || (!parity && header.fragment >= header.fragments) {
            return Err(HeaderError::Fragment {
                fragment: header.fragment,
                fragments: header.fragments,
            });
        }
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            flags: FLAG_PARITY,
            stream: 0x0102,
            frame: 0x0a0b_0c0d,
            fragment: 2,
            fragments: 5,
            packet: 0x1122_3344_5566_7788,
            sent_us: 0xdead_beef,
        }
    }

    #[test]
    fn a_header_round_trips_and_has_the_documented_layout() {
        let bytes = sample().encode();
        assert_eq!(bytes[0], VERSION);
        assert_eq!(bytes[1], FLAG_PARITY);
        assert_eq!(&bytes[2..4], &[0x01, 0x02]);
        assert_eq!(
            &bytes[12..20],
            &[0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88]
        );
        assert_eq!(Header::decode(&bytes), Ok(sample()));
    }

    #[test]
    fn a_short_or_foreign_datagram_is_refused() {
        let bytes = sample().encode();
        assert_eq!(
            Header::decode(&bytes[..HEADER_LEN - 1]),
            Err(HeaderError::Short)
        );
        let mut other = bytes;
        other[0] = VERSION + 1;
        assert_eq!(
            Header::decode(&other),
            Err(HeaderError::Version(VERSION + 1))
        );
    }

    #[test]
    fn a_fragment_outside_its_frame_is_refused() {
        let mut h = sample();
        h.flags = 0;
        h.fragment = 5;
        assert!(matches!(
            Header::decode(&h.encode()),
            Err(HeaderError::Fragment { .. })
        ));
        h.fragments = 0;
        h.fragment = 0;
        assert!(matches!(
            Header::decode(&h.encode()),
            Err(HeaderError::Fragment { .. })
        ));
    }

    #[test]
    fn a_parity_packet_may_number_past_the_data_fragments() {
        let mut h = sample();
        h.flags = FLAG_PARITY;
        h.fragments = 1;
        h.fragment = 7;
        assert_eq!(Header::decode(&h.encode()), Ok(h));
        h.fragments = 0;
        assert!(matches!(
            Header::decode(&h.encode()),
            Err(HeaderError::Fragment { .. })
        ));
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_decoder() {
        // A cheap deterministic sweep; the decoder only ever indexes after a
        // length check.
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for len in 0..64 {
            let mut buf = vec![0u8; len];
            for byte in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *byte = x as u8;
            }
            let _ = Header::decode(&buf);
        }
    }
}
