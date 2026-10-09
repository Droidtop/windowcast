//! GameStream sound: Opus at 48 kHz in RTP packets (payload type 97), from
//! the host's audio port to wherever the client's pings on it come from
//! (moonlight-common-c `AudioStream.c`, `RtpAudioQueue.c`).
//!
//! When the client asks for it (`x-nv-general.featureFlags` bit 0x20, or
//! Sunshine's `encryptionEnabled` audio bit) each Opus packet is
//! AES-128-CBC encrypted under the launch's input key, the IV being the
//! input key ID plus the RTP sequence number, big-endian, then zeros.
//!
//! Sunshine also sends Reed-Solomon parity packets (type 127) for each four
//! audio packets; Moonlight plays in-order packets as they come and only
//! needs parity to rebuild lost ones, so the host here sends none and the
//! client ignores any it gets.

use crate::crypto;

pub const PAYLOAD_TYPE: u8 = 97;
/// Sunshine's audio parity packets.
pub const FEC_PAYLOAD_TYPE: u8 = 127;
pub const RTP_HEADER: usize = 12;

/// The DESCRIBE feature bit for audio encryption in `x-nv-general.featureFlags`.
pub const NV_AUDIO_ENCRYPTION: u32 = 0x20;
/// Sunshine's audio bit in `x-ss-general.encryption*`.
pub const SS_AUDIO_ENCRYPTION: u32 = 0x04;

/// The launch's input key and key ID, for encrypted audio.
#[derive(Clone, Copy)]
pub struct AudioKey {
    pub key: [u8; 16],
    pub key_id: u32,
}

fn iv(key_id: u32, sequence: u16) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[..4].copy_from_slice(&key_id.wrapping_add(u32::from(sequence)).to_be_bytes());
    iv
}

/// Numbers and wraps a stream's Opus packets.
pub struct AudioPacketizer {
    sequence: u16,
    duration_ms: u32,
    key: Option<AudioKey>,
}

impl AudioPacketizer {
    pub fn new(duration_ms: u32, key: Option<AudioKey>) -> Self {
        AudioPacketizer {
            sequence: 0,
            duration_ms,
            key,
        }
    }

    /// One RTP packet carrying `opus`. The timestamp counts milliseconds of
    /// sound, as Moonlight's queue expects (sequence times duration).
    pub fn packet(&mut self, opus: &[u8]) -> Vec<u8> {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let timestamp = u32::from(sequence).wrapping_mul(self.duration_ms);
        let mut out = Vec::with_capacity(RTP_HEADER + opus.len() + 16);
        out.extend_from_slice(&[0x80, PAYLOAD_TYPE]);
        out.extend_from_slice(&sequence.to_be_bytes());
        out.extend_from_slice(&timestamp.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        match self.key {
            Some(key) => out.extend_from_slice(&crypto::cbc_encrypt(
                &key.key,
                &iv(key.key_id, sequence),
                opus,
            )),
            None => out.extend_from_slice(opus),
        }
        out
    }
}

/// The sequence number and Opus packet in an audio RTP packet; `None` for
/// parity packets and anything that is not audio or does not decrypt.
pub fn open_packet(data: &[u8], key: Option<&AudioKey>) -> Option<(u16, Vec<u8>)> {
    if data.len() <= RTP_HEADER || data[1] & 0x7f != PAYLOAD_TYPE {
        return None;
    }
    let sequence = u16::from_be_bytes([data[2], data[3]]);
    let payload = &data[RTP_HEADER..];
    let opus = match key {
        Some(key) => crypto::cbc_decrypt(&key.key, &iv(key.key_id, sequence), payload)?,
        None => payload.to_vec(),
    };
    Some((sequence, opus))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip_with_and_without_the_key() {
        let key = AudioKey {
            key: [7; 16],
            key_id: 0xffff_fffe,
        };
        for key in [None, Some(key)] {
            let mut packetizer = AudioPacketizer::new(5, key);
            for n in 0..3u16 {
                let opus = vec![0xfc; 16 + usize::from(n)];
                let packet = packetizer.packet(&opus);
                assert_eq!(packet[1], PAYLOAD_TYPE);
                assert_eq!(
                    u32::from_be_bytes(packet[4..8].try_into().unwrap()),
                    u32::from(n) * 5
                );
                assert_eq!(open_packet(&packet, key.as_ref()), Some((n, opus)));
            }
        }
    }

    #[test]
    fn the_wrong_key_and_parity_packets_give_nothing() {
        let key = AudioKey {
            key: [1; 16],
            key_id: 5,
        };
        let packet = AudioPacketizer::new(5, Some(key)).packet(&[1, 2, 3]);
        let wrong = AudioKey {
            key: [2; 16],
            key_id: 5,
        };
        assert_eq!(open_packet(&packet, Some(&wrong)), None);
        let mut parity = packet.clone();
        parity[1] = FEC_PAYLOAD_TYPE;
        assert_eq!(open_packet(&parity, Some(&key)), None);
    }

    /// The IV is the key ID plus the sequence number, big-endian, as
    /// moonlight-common-c builds it (`BE32(avRiKeyId + rtp->sequenceNumber)`).
    #[test]
    fn the_iv_is_the_key_id_plus_the_sequence() {
        assert_eq!(iv(0x0102_0304, 1)[..4], [1, 2, 3, 5]);
        assert_eq!(iv(0x0102_0304, 1)[4..], [0; 12]);
    }
}
