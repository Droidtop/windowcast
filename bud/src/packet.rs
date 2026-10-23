//! Sealing and opening datagrams: ChaCha20-Poly1305 with the packet number as
//! the nonce and the header as associated data.
//!
//! Each direction has its own key (the handshake derives two), so a packet
//! one side sent can never be accepted as one it received, and the 64-bit
//! packet number is the whole nonce: a sender numbers its packets 0, 1, 2 and
//! refuses to send at all rather than reuse a number. The nonce is laid out as
//! Noise lays out its own: four zero bytes, then the number little-endian.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

use crate::header::{Header, HeaderError, HEADER_LEN, MAX_PAYLOAD, TAG_LEN};
use crate::replay::ReplayWindow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SealError {
    #[error("the payload is {0} bytes; a datagram carries at most {MAX_PAYLOAD}")]
    TooLarge(usize),
    #[error("the packet numbers of this key are used up; a new handshake is needed")]
    Exhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OpenError {
    #[error("bad header: {0}")]
    Header(#[from] HeaderError),
    #[error("the datagram is too short to hold an authentication tag")]
    Short,
    #[error("a packet with this number was already received, or is too old")]
    Replay,
    #[error("the datagram did not authenticate")]
    Authentication,
}

fn nonce(packet: u64) -> Nonce {
    let mut bytes = [0u8; 12];
    bytes[4..].copy_from_slice(&packet.to_le_bytes());
    Nonce::from(bytes)
}

/// The sending half: numbers and encrypts packets.
pub struct Sealer {
    key: ChaCha20Poly1305,
    next: u64,
}

impl Sealer {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        Sealer {
            key: ChaCha20Poly1305::new(Key::from_slice(key)),
            next: 0,
        }
    }

    /// The packet number the next datagram gets.
    pub fn next_packet(&self) -> u64 {
        self.next
    }

    /// Encrypts `payload` under `header` (whose `packet` is replaced with the
    /// next number) and returns the datagram: the header, then the ciphertext
    /// and its tag.
    pub fn seal(&mut self, mut header: Header, payload: &[u8]) -> Result<Vec<u8>, SealError> {
        if payload.len() > MAX_PAYLOAD {
            return Err(SealError::TooLarge(payload.len()));
        }
        // u64::MAX is never used: the counter must not wrap onto a number
        // that was already sent.
        if self.next == u64::MAX {
            return Err(SealError::Exhausted);
        }
        header.packet = self.next;
        let aad = header.encode();
        let sealed = self
            .key
            .encrypt(
                &nonce(header.packet),
                Payload {
                    msg: payload,
                    aad: &aad,
                },
            )
            // Encryption only fails on a message too long for the cipher,
            // which MAX_PAYLOAD rules out.
            .map_err(|_| SealError::TooLarge(payload.len()))?;
        self.next += 1;
        let mut datagram = Vec::with_capacity(HEADER_LEN + sealed.len());
        datagram.extend_from_slice(&aad);
        datagram.extend_from_slice(&sealed);
        Ok(datagram)
    }
}

/// The receiving half: checks, decrypts and de-duplicates packets.
pub struct Opener {
    key: ChaCha20Poly1305,
    replay: ReplayWindow,
}

impl Opener {
    pub(crate) fn new(key: &[u8; 32]) -> Self {
        Opener {
            key: ChaCha20Poly1305::new(Key::from_slice(key)),
            replay: ReplayWindow::new(),
        }
    }

    /// The highest packet number received and how many packets have been:
    /// the difference is what the network lost (so far), which is what a
    /// [`crate::recovery::LossMeter`] reports back to the sender.
    pub fn stats(&self) -> (Option<u64>, u64) {
        (self.replay.highest(), self.replay.accepted())
    }

    /// Opens one datagram. The replay window only moves for a datagram that
    /// authenticated, so garbage and forgeries cannot disturb it.
    pub fn open(&mut self, datagram: &[u8]) -> Result<(Header, Vec<u8>), OpenError> {
        let header = Header::decode(datagram)?;
        if datagram.len() < HEADER_LEN + TAG_LEN {
            return Err(OpenError::Short);
        }
        if !self.replay.check(header.packet) {
            return Err(OpenError::Replay);
        }
        let plain = self
            .key
            .decrypt(
                &nonce(header.packet),
                Payload {
                    msg: &datagram[HEADER_LEN..],
                    aad: &datagram[..HEADER_LEN],
                },
            )
            .map_err(|_| OpenError::Authentication)?;
        self.replay.accept(header.packet);
        Ok((header, plain))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (Sealer, Opener) {
        let key = [7u8; 32];
        (Sealer::new(&key), Opener::new(&key))
    }

    fn header() -> Header {
        Header {
            flags: 0,
            stream: 3,
            frame: 9,
            fragment: 0,
            fragments: 1,
            packet: 0,
            sent_us: 1234,
        }
    }

    #[test]
    fn a_sealed_packet_opens_with_its_header_and_payload() {
        let (mut s, mut o) = pair();
        let datagram = s.seal(header(), b"a frame").unwrap();
        assert_eq!(datagram.len(), HEADER_LEN + 7 + TAG_LEN);
        let (h, plain) = o.open(&datagram).unwrap();
        assert_eq!(plain, b"a frame");
        assert_eq!((h.stream, h.frame, h.packet), (3, 9, 0));
    }

    #[test]
    fn the_sealer_numbers_the_packets_itself() {
        let (mut s, mut o) = pair();
        let mut h = header();
        h.packet = 99;
        for expected in 0..3u64 {
            let d = s.seal(h, b"x").unwrap();
            assert_eq!(o.open(&d).unwrap().0.packet, expected);
        }
    }

    #[test]
    fn a_replayed_packet_is_refused_and_a_reordered_one_is_not() {
        let (mut s, mut o) = pair();
        let first = s.seal(header(), b"1").unwrap();
        let second = s.seal(header(), b"2").unwrap();
        assert!(o.open(&second).is_ok());
        assert!(o.open(&first).is_ok());
        assert_eq!(o.open(&first), Err(OpenError::Replay));
        assert_eq!(o.open(&second), Err(OpenError::Replay));
    }

    #[test]
    fn any_changed_byte_fails_to_authenticate_and_does_not_burn_the_number() {
        let (mut s, mut o) = pair();
        let good = s.seal(header(), b"payload").unwrap();
        for at in 0..good.len() {
            let mut bad = good.clone();
            bad[at] ^= 0x01;
            // A flipped header byte may be refused as a bad header or as a
            // failed authentication, but never opened.
            assert!(o.open(&bad).is_err(), "byte {at} was accepted");
        }
        // The real one still opens: the failures did not mark its number seen.
        assert!(o.open(&good).is_ok());
    }

    #[test]
    fn a_packet_from_another_key_does_not_open() {
        let (mut s, _) = pair();
        let mut other = Opener::new(&[8u8; 32]);
        let d = s.seal(header(), b"x").unwrap();
        assert_eq!(other.open(&d), Err(OpenError::Authentication));
    }

    #[test]
    fn an_oversized_payload_and_an_exhausted_counter_are_refused() {
        let (mut s, _) = pair();
        assert_eq!(
            s.seal(header(), &vec![0u8; MAX_PAYLOAD + 1]),
            Err(SealError::TooLarge(MAX_PAYLOAD + 1))
        );
        assert!(s.seal(header(), &vec![0u8; MAX_PAYLOAD]).is_ok());
        s.next = u64::MAX;
        assert_eq!(s.seal(header(), b"x"), Err(SealError::Exhausted));
    }

    #[test]
    fn a_datagram_too_short_for_a_tag_is_refused() {
        let (_, mut o) = pair();
        let d = header().encode();
        assert_eq!(o.open(&d), Err(OpenError::Short));
    }
}
