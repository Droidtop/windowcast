//! GameStream sound: Opus at 48 kHz in RTP packets (payload type 97), from
//! the host's audio port to wherever the client's pings on it come from
//! (moonlight-common-c `AudioStream.c`, `RtpAudioQueue.c`; Sunshine
//! `stream.cpp`, `audioBroadcastThread`).
//!
//! When the client asks for it (`x-nv-general.featureFlags` bit 0x20, or
//! Sunshine's `encryptionEnabled` audio bit) each Opus packet is
//! AES-128-CBC encrypted under the launch's input key, the IV being the
//! input key ID plus the RTP sequence number, big-endian, then zeros.
//!
//! Every four audio packets (sequence numbers from a multiple of four) are
//! followed by two Reed-Solomon parity packets (payload type 127, [`fec`])
//! over their payloads as sent, so a client rebuilds up to two lost packets
//! of the four. The parity needs payloads of one size, so the host encodes
//! at a constant bitrate, as Sunshine does.
//!
//! [`fec`]: crate::fec

use std::collections::HashMap;

use crate::crypto;
use crate::fec::{self, DATA_SHARDS, PARITY_SHARDS, SHARDS};

pub const PAYLOAD_TYPE: u8 = 97;
/// Audio parity packets.
pub const FEC_PAYLOAD_TYPE: u8 = 127;
pub const RTP_HEADER: usize = 12;
/// Shard index, payload type, base sequence number, base timestamp, SSRC.
pub const FEC_HEADER: usize = 12;

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

fn rtp(payload_type: u8, sequence: u16, timestamp: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(RTP_HEADER + FEC_HEADER + 256);
    out.extend_from_slice(&[0x80, payload_type]);
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

/// Numbers, encrypts and protects a stream's Opus packets.
pub struct AudioPacketizer {
    sequence: u16,
    duration_ms: u32,
    key: Option<AudioKey>,
    /// The payloads of the current block of four, as sent.
    block: Vec<Vec<u8>>,
}

impl AudioPacketizer {
    pub fn new(duration_ms: u32, key: Option<AudioKey>) -> Self {
        AudioPacketizer {
            sequence: 0,
            duration_ms,
            key,
            block: Vec::with_capacity(DATA_SHARDS),
        }
    }

    /// The packets to send for `opus`: its RTP packet, and after every
    /// fourth the block's two parity packets. The timestamp counts
    /// milliseconds of sound (sequence times duration), as Moonlight's queue
    /// expects.
    pub fn packets(&mut self, opus: &[u8]) -> Vec<Vec<u8>> {
        let sequence = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let timestamp = u32::from(sequence).wrapping_mul(self.duration_ms);
        let payload = match self.key {
            Some(key) => crypto::cbc_encrypt(&key.key, &iv(key.key_id, sequence), opus),
            None => opus.to_vec(),
        };
        let mut packet = rtp(PAYLOAD_TYPE, sequence, timestamp);
        packet.extend_from_slice(&payload);
        let mut out = vec![packet];

        if usize::from(sequence).is_multiple_of(DATA_SHARDS) {
            self.block.clear();
        }
        self.block.push(payload);
        let complete = self.block.len() == DATA_SHARDS
            && self.block.iter().all(|p| p.len() == self.block[0].len());
        if usize::from(sequence) % DATA_SHARDS == DATA_SHARDS - 1 && complete {
            let base = sequence.wrapping_sub(DATA_SHARDS as u16 - 1);
            let base_timestamp = u32::from(base).wrapping_mul(self.duration_ms);
            let parity = fec::parity(std::array::from_fn(|i| self.block[i].as_slice()));
            for (index, shard) in parity.iter().enumerate() {
                // Sunshine numbers parity packets after the block's last.
                let mut packet = rtp(FEC_PAYLOAD_TYPE, sequence.wrapping_add(index as u16 + 1), 0);
                packet.extend_from_slice(&[index as u8, PAYLOAD_TYPE]);
                packet.extend_from_slice(&base.to_be_bytes());
                packet.extend_from_slice(&base_timestamp.to_be_bytes());
                packet.extend_from_slice(&0u32.to_be_bytes());
                packet.extend_from_slice(shard);
                out.push(packet);
            }
        }
        out
    }
}

#[derive(Default)]
struct Block {
    shards: [Option<Vec<u8>>; SHARDS],
}

/// A client's side: audio and parity packets in, Opus packets out in
/// sequence order, lost ones rebuilt from parity where a block allows.
pub struct AudioDepacketizer {
    key: Option<AudioKey>,
    next: Option<u16>,
    blocks: HashMap<u16, Block>,
    /// Packets rebuilt from parity so far.
    pub recovered: u64,
    /// Packets given up on.
    pub lost: u64,
}

fn block_of(sequence: u16) -> u16 {
    sequence - sequence % DATA_SHARDS as u16
}

/// How far `a` is after `b`, sequence numbers wrapping.
fn after(a: u16, b: u16) -> i16 {
    a.wrapping_sub(b) as i16
}

impl AudioDepacketizer {
    pub fn new(key: Option<AudioKey>) -> Self {
        AudioDepacketizer {
            key,
            next: None,
            blocks: HashMap::new(),
            recovered: 0,
            lost: 0,
        }
    }

    /// Takes one packet; returns the Opus packets it lets out, in order.
    pub fn add(&mut self, data: &[u8]) -> Vec<Vec<u8>> {
        if data.len() <= RTP_HEADER {
            return Vec::new();
        }
        let sequence = u16::from_be_bytes([data[2], data[3]]);
        let (base, index, payload) = match data[1] & 0x7f {
            PAYLOAD_TYPE => (
                block_of(sequence),
                usize::from(sequence) % DATA_SHARDS,
                &data[RTP_HEADER..],
            ),
            FEC_PAYLOAD_TYPE if data.len() > RTP_HEADER + FEC_HEADER => {
                let header = &data[RTP_HEADER..RTP_HEADER + FEC_HEADER];
                let shard = usize::from(header[0]);
                let base = u16::from_be_bytes([header[2], header[3]]);
                if shard >= PARITY_SHARDS || !base.is_multiple_of(DATA_SHARDS as u16) {
                    return Vec::new();
                }
                (base, DATA_SHARDS + shard, &data[RTP_HEADER + FEC_HEADER..])
            }
            _ => return Vec::new(),
        };
        let next = *self
            .next
            .get_or_insert(if index < DATA_SHARDS { sequence } else { base });
        if after(base, block_of(next)) < 0 {
            return Vec::new(); // a block already played
        }
        self.blocks.entry(base).or_default().shards[index] = Some(payload.to_vec());
        self.drain()
    }

    fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(next) = self.next {
            let base = block_of(next);
            let index = usize::from(next - base);
            let block = self.blocks.entry(base).or_default();
            if block.shards[index].is_none() {
                if let Some(data) = fec::recover(&block.shards) {
                    for (i, shard) in data.into_iter().enumerate() {
                        if block.shards[i].is_none() {
                            block.shards[i] = Some(shard);
                            self.recovered += 1;
                        }
                    }
                }
            }
            match block.shards[index].clone() {
                Some(payload) => {
                    let opus = match &self.key {
                        Some(key) => crypto::cbc_decrypt(&key.key, &iv(key.key_id, next), &payload),
                        None => Some(payload),
                    };
                    out.extend(opus);
                }
                None => {
                    // Wait while this block can still complete: give up once
                    // a later block has enough to play.
                    let ahead = self.blocks.iter().any(|(b, block)| {
                        after(*b, base) > 0
                            && block.shards.iter().filter(|s| s.is_some()).count() >= DATA_SHARDS
                    });
                    if !ahead {
                        break;
                    }
                    self.lost += 1;
                }
            }
            let next = next.wrapping_add(1);
            self.next = Some(next);
            if next.is_multiple_of(DATA_SHARDS as u16) {
                self.blocks.remove(&base);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opus(n: u16) -> Vec<u8> {
        // Constant size, as the host's constant-rate encoder makes them.
        (0..40).map(|b| (b as u16 * 3 + n) as u8).collect()
    }

    #[test]
    fn packets_come_out_in_order_with_and_without_the_key() {
        let key = AudioKey {
            key: [7; 16],
            key_id: 0xffff_fffe,
        };
        for key in [None, Some(key)] {
            let mut packetizer = AudioPacketizer::new(5, key);
            let mut depacketizer = AudioDepacketizer::new(key);
            let mut got = Vec::new();
            for n in 0..12u16 {
                let packets = packetizer.packets(&opus(n));
                assert_eq!(packets.len(), if n % 4 == 3 { 3 } else { 1 });
                assert_eq!(
                    u32::from_be_bytes(packets[0][4..8].try_into().unwrap()),
                    u32::from(n) * 5
                );
                for packet in packets {
                    got.extend(depacketizer.add(&packet));
                }
            }
            assert_eq!(got, (0..12).map(opus).collect::<Vec<_>>());
            assert_eq!(depacketizer.recovered, 0);
        }
    }

    #[test]
    fn two_lost_packets_of_a_block_come_back_from_parity() {
        let key = Some(AudioKey {
            key: [3; 16],
            key_id: 99,
        });
        let mut packetizer = AudioPacketizer::new(5, key);
        let mut depacketizer = AudioDepacketizer::new(key);
        let mut got = Vec::new();
        for n in 0..8u16 {
            for (i, packet) in packetizer.packets(&opus(n)).into_iter().enumerate() {
                // Lose audio packets 1 and 2 (the first block) and 6.
                if i == 0 && matches!(n, 1 | 2 | 6) {
                    continue;
                }
                got.extend(depacketizer.add(&packet));
            }
        }
        assert_eq!(got, (0..8).map(opus).collect::<Vec<_>>());
        assert_eq!(depacketizer.recovered, 3);
    }

    #[test]
    fn a_block_lost_beyond_repair_is_skipped() {
        let mut packetizer = AudioPacketizer::new(5, None);
        let mut depacketizer = AudioDepacketizer::new(None);
        let mut got = Vec::new();
        for n in 0..12u16 {
            for (i, packet) in packetizer.packets(&opus(n)).into_iter().enumerate() {
                // Three of the second block's four, and its parity.
                if (4..7).contains(&n) && i == 0 || n == 7 && i > 0 {
                    continue;
                }
                got.extend(depacketizer.add(&packet));
            }
        }
        let expected: Vec<Vec<u8>> = [0, 1, 2, 3, 7, 8, 9, 10, 11]
            .into_iter()
            .map(opus)
            .collect();
        assert_eq!(got, expected);
        assert_eq!(depacketizer.lost, 3);
    }

    #[test]
    fn the_wrong_key_gives_nothing() {
        let key = AudioKey {
            key: [1; 16],
            key_id: 5,
        };
        let packets = AudioPacketizer::new(5, Some(key)).packets(&[1, 2, 3]);
        let mut depacketizer = AudioDepacketizer::new(Some(AudioKey {
            key: [2; 16],
            key_id: 5,
        }));
        assert!(depacketizer.add(&packets[0]).is_empty());
    }

    /// The IV is the key ID plus the sequence number, big-endian, as
    /// moonlight-common-c builds it (`BE32(avRiKeyId + rtp->sequenceNumber)`).
    #[test]
    fn the_iv_is_the_key_id_plus_the_sequence() {
        assert_eq!(iv(0x0102_0304, 1)[..4], [1, 2, 3, 5]);
        assert_eq!(iv(0x0102_0304, 1)[4..], [0; 12]);
    }
}
