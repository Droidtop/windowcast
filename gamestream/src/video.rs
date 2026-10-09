//! GameStream's video packets, as Sunshine sends them (`stream.cpp`,
//! `videoBroadcastThread`) and moonlight-common-c takes them apart
//! (`RtpVideoQueue.c`, `VideoDepacketizer.c`):
//!
//! - Each frame is an 8-byte short frame header (type 0x01, host latency,
//!   frame type 1 P or 2 IDR, the last packet's payload length) then the
//!   Annex B access unit, cut into packets of the client's `packetSize`.
//! - Each packet: a 12-byte RTP header (0x90: version 2 with the extension
//!   bit, sequence number, 90 kHz timestamp), 4 reserved bytes, the 16-byte
//!   `NV_VIDEO_PACKET` (stream packet index << 8, frame index, flags SOF /
//!   EOF / picture data, multi-FEC fields, `fecInfo` = shard index << 12 |
//!   data shards << 22 | FEC percent << 4), then the payload, every packet
//!   padded to the same size.
//! - A frame of up to 255 packets is one FEC block; larger ones are split
//!   into up to four blocks.
//!
//! windowcast sends no parity shards (FEC 0%, which Moonlight takes: it then
//! needs every data packet) and, receiving, uses only the data shards: a
//! lost packet loses the frame, and the client asks for a keyframe.
//!
//! When the client asks for encrypted video (Sunshine's `SS_ENC_VIDEO`),
//! every whole packet is sealed with AES-128-GCM under the launch's input
//! key ([`VideoCipher`]): a 32-byte prefix of the 12-byte IV (a 64-bit
//! counter, little-endian, then zeros and `V`), the frame index and the
//! tag, then the ciphertext.

use std::collections::BTreeMap;

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce, Tag};

pub const FLAG_CONTAINS_PIC_DATA: u8 = 0x1;
pub const FLAG_EOF: u8 = 0x2;
pub const FLAG_SOF: u8 = 0x4;
/// RTP version 2 with the header extension bit.
const RTP_HEADER: u8 = 0x90;
/// RTP header, 4 reserved bytes.
pub const RTP_SIZE: usize = 16;
pub const NV_SIZE: usize = 16;
const FRAME_HEADER: usize = 8;
const MAX_DATA_SHARDS: usize = 255;
const MAX_FEC_BLOCKS: usize = 4;

/// Host side: frames to packets.
pub struct Packetizer {
    /// The client's `packetSize`: the NV header and payload of a packet.
    packet_size: usize,
    sequence: u32,
    frame_index: u32,
}

impl Packetizer {
    pub fn new(packet_size: usize) -> Self {
        Packetizer {
            packet_size: packet_size.max(NV_SIZE + 64),
            sequence: 0,
            frame_index: 0,
        }
    }

    /// The packets of one encoded frame (Annex B), to send in order.
    pub fn packets(&mut self, frame: &[u8], idr: bool, timestamp: u32) -> Vec<Vec<u8>> {
        self.frame_index = self.frame_index.wrapping_add(1);
        let chunk = self.packet_size - NV_SIZE;
        let block_size = self.packet_size + RTP_SIZE;
        let mut payload = Vec::with_capacity(FRAME_HEADER + frame.len());
        let mut last = ((frame.len() + FRAME_HEADER) % chunk) as u16;
        if last == 0 {
            last = chunk as u16;
        }
        payload.push(0x01);
        payload.extend_from_slice(&0u16.to_le_bytes());
        payload.push(if idr { 2 } else { 1 });
        payload.extend_from_slice(&last.to_le_bytes());
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(frame);

        let chunks: Vec<&[u8]> = payload.chunks(chunk).collect();
        let blocks = chunks
            .len()
            .div_ceil(MAX_DATA_SHARDS)
            .clamp(1, MAX_FEC_BLOCKS);
        let per_block = chunks.len().div_ceil(blocks);
        let mut out = Vec::with_capacity(chunks.len());
        for (block, shards) in chunks.chunks(per_block).enumerate() {
            let low = self.sequence;
            for (x, data) in shards.iter().enumerate() {
                let mut packet = vec![0u8; block_size];
                packet[0] = RTP_HEADER;
                packet[2..4].copy_from_slice(&((low + x as u32) as u16).to_be_bytes());
                packet[4..8].copy_from_slice(&timestamp.to_be_bytes());
                let nv = &mut packet[RTP_SIZE..RTP_SIZE + NV_SIZE];
                nv[0..4].copy_from_slice(&((low + x as u32) << 8).to_le_bytes());
                nv[4..8].copy_from_slice(&self.frame_index.to_le_bytes());
                let mut flags = FLAG_CONTAINS_PIC_DATA;
                if x == 0 {
                    flags |= FLAG_SOF;
                }
                if x == shards.len() - 1 {
                    flags |= FLAG_EOF;
                }
                nv[8] = flags;
                nv[10] = 0x10;
                nv[11] = ((block << 4) | ((blocks - 1) << 6)) as u8;
                let fec_info = ((x as u32) << 12) | ((shards.len() as u32) << 22);
                nv[12..16].copy_from_slice(&fec_info.to_le_bytes());
                packet[RTP_SIZE + NV_SIZE..RTP_SIZE + NV_SIZE + data.len()].copy_from_slice(data);
                out.push(packet);
            }
            self.sequence = low.wrapping_add(shards.len() as u32);
        }
        out
    }
}

/// One received packet's headers.
#[derive(Debug, Clone, Copy)]
struct Header {
    frame_index: u32,
    block: u8,
    last_block: u8,
    shard: u16,
    data_shards: u16,
}

fn header(packet: &[u8]) -> Option<Header> {
    if packet.len() < RTP_SIZE + NV_SIZE || packet[0] & 0x10 == 0 {
        return None;
    }
    let nv = &packet[RTP_SIZE..RTP_SIZE + NV_SIZE];
    let fec_info = u32::from_le_bytes(nv[12..16].try_into().ok()?);
    Some(Header {
        frame_index: u32::from_le_bytes(nv[4..8].try_into().ok()?),
        block: (nv[11] >> 4) & 0x3,
        last_block: (nv[11] >> 6) & 0x3,
        shard: ((fec_info >> 12) & 0x3ff) as u16,
        data_shards: (fec_info >> 22) as u16,
    })
}

/// A whole received frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub index: u32,
    /// Annex B (H.264/H.265) or OBUs (AV1).
    pub data: Vec<u8>,
    pub idr: bool,
    pub rtp_timestamp: u32,
}

/// What a received packet brought.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    /// Nothing yet (internal).
    Pending,
    Frame(Frame),
    /// A frame was lost (a packet never came): ask for a keyframe.
    Lost(u32),
}

/// Client side: packets to frames, from the data shards alone.
#[derive(Default)]
pub struct Depacketizer {
    current: Option<u32>,
    /// The current frame's data shards by block, then shard index.
    blocks: BTreeMap<u8, BTreeMap<u16, Vec<u8>>>,
    shards_per_block: BTreeMap<u8, u16>,
    last_block: u8,
    timestamp: u32,
    /// Frames before this are over.
    next: u32,
}

impl Depacketizer {
    /// Takes one packet; returns what it brought: a frame lost before it
    /// (when it starts a new one), and the frame it completes.
    pub fn add(&mut self, packet: &[u8]) -> Vec<Received> {
        let mut out = Vec::new();
        let Some(h) = header(packet) else {
            return out;
        };
        if h.frame_index.wrapping_sub(self.next) as i32 >= 0 && self.current != Some(h.frame_index)
        {
            // A new frame: whatever was pending is lost.
            if let Some(lost) = self.current.filter(|_| !self.blocks.is_empty()) {
                out.push(Received::Lost(lost));
            }
            self.current = Some(h.frame_index);
            self.blocks.clear();
            self.shards_per_block.clear();
            self.last_block = h.last_block;
            self.timestamp = u32::from_be_bytes(packet[4..8].try_into().expect("4 bytes"));
        } else if self.current != Some(h.frame_index) {
            // An old frame's straggler.
            return out;
        }
        self.add_shard(&h, packet);
        match self.complete() {
            Received::Pending => {}
            done => out.push(done),
        }
        out
    }

    fn add_shard(&mut self, h: &Header, packet: &[u8]) {
        if h.shard >= h.data_shards {
            // A parity shard: not used.
            return;
        }
        self.shards_per_block.insert(h.block, h.data_shards);
        self.blocks
            .entry(h.block)
            .or_default()
            .insert(h.shard, packet[RTP_SIZE + NV_SIZE..].to_vec());
    }

    fn complete(&mut self) -> Received {
        let whole = (0..=self.last_block).all(|block| {
            let need = self
                .shards_per_block
                .get(&block)
                .copied()
                .unwrap_or(u16::MAX);
            self.blocks
                .get(&block)
                .is_some_and(|b| b.len() == usize::from(need))
        });
        if !whole {
            return Received::Pending;
        }
        let Some(index) = self.current else {
            return Received::Pending;
        };
        let chunks: Vec<Vec<u8>> = std::mem::take(&mut self.blocks)
            .into_values()
            .flat_map(BTreeMap::into_values)
            .collect();
        self.shards_per_block.clear();
        self.next = index.wrapping_add(1);
        self.current = None;
        let Some(first) = chunks.first() else {
            return Received::Pending;
        };
        if first.len() < FRAME_HEADER {
            return Received::Lost(index);
        }
        let idr = first[3] == 2;
        let last_len = usize::from(u16::from_le_bytes([first[4], first[5]]));
        let mut data = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            let piece = if i == chunks.len() - 1 {
                &chunk[..last_len.min(chunk.len())]
            } else {
                &chunk[..]
            };
            data.extend_from_slice(piece);
        }
        data.drain(..FRAME_HEADER.min(data.len()));
        Received::Frame(Frame {
            index,
            data,
            idr,
            rtp_timestamp: self.timestamp,
        })
    }
}

/// The prefix of an encrypted video packet: IV, frame index, tag.
pub const ENC_HEADER: usize = 32;

/// Seals and opens encrypted video packets (Sunshine `stream.cpp`
/// `video_packet_enc_prefix_t`; moonlight-common-c `VideoStream.c`).
pub struct VideoCipher {
    cipher: Aes128Gcm,
    counter: u64,
}

impl VideoCipher {
    pub fn new(key: &[u8; 16]) -> Self {
        VideoCipher {
            cipher: Aes128Gcm::new(key.into()),
            counter: 0,
        }
    }

    /// One packet from [`Packetizer::packets`], sealed.
    pub fn seal(&mut self, packet: &[u8]) -> Vec<u8> {
        let mut iv = [0u8; 12];
        iv[..8].copy_from_slice(&self.counter.to_le_bytes());
        iv[11] = b'V';
        self.counter += 1;
        let frame = packet
            .get(RTP_SIZE + 4..RTP_SIZE + 8)
            .map_or([0; 4], |f| f.try_into().expect("four bytes"));
        let mut body = packet.to_vec();
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&iv), &[], &mut body)
            .expect("a video packet fits AES-GCM");
        let mut out = Vec::with_capacity(ENC_HEADER + body.len());
        out.extend_from_slice(&iv);
        out.extend_from_slice(&frame);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&body);
        out
    }

    /// A sealed packet's plain packet; `None` if it was not sealed with
    /// this key or was changed.
    pub fn open(&self, data: &[u8]) -> Option<Vec<u8>> {
        if data.len() <= ENC_HEADER {
            return None;
        }
        let mut body = data[ENC_HEADER..].to_vec();
        self.cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(&data[..12]),
                &[],
                &mut body,
                Tag::from_slice(&data[16..32]),
            )
            .ok()?;
        Some(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len: usize, seed: u8) -> Vec<u8> {
        let mut data = vec![0, 0, 0, 1, 0x65];
        data.extend((0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)));
        data
    }

    #[test]
    fn frames_survive_packets_of_every_size() {
        let mut packetizer = Packetizer::new(1024);
        let mut depacketizer = Depacketizer::default();
        for (n, len) in [10usize, 999, 1000, 1001, 5000, 300_000]
            .into_iter()
            .enumerate()
        {
            let data = frame(len, n as u8);
            let packets = packetizer.packets(&data, n == 0, 90 * n as u32);
            assert!(packets.iter().all(|p| p.len() == 1024 + RTP_SIZE));
            let mut got = None;
            for packet in &packets {
                for received in depacketizer.add(packet) {
                    if let Received::Frame(f) = received {
                        got = Some(f);
                    }
                }
            }
            let got = got.unwrap_or_else(|| panic!("no frame for {len} bytes"));
            assert_eq!(got.data, data, "{len} bytes");
            assert_eq!(got.idr, n == 0);
        }
    }

    #[test]
    fn a_lost_packet_loses_the_frame_and_the_next_one_still_arrives() {
        let mut packetizer = Packetizer::new(1024);
        let mut depacketizer = Depacketizer::default();
        let first = packetizer.packets(&frame(5000, 1), true, 0);
        for packet in first.iter().skip(1) {
            assert!(depacketizer.add(packet).is_empty());
        }
        // A one-packet frame both reports the loss and arrives whole.
        let second = packetizer.packets(&frame(100, 2), false, 3000);
        assert_eq!(second.len(), 1);
        let got = depacketizer.add(&second[0]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], Received::Lost(1));
        assert!(matches!(&got[1], Received::Frame(f) if f.data == frame(100, 2)));
    }

    #[test]
    fn sealed_packets_open_only_with_the_key() {
        let mut packetizer = Packetizer::new(256);
        let packets = packetizer.packets(&[0, 0, 0, 1, 0x65, 1, 2, 3], true, 90);
        let mut cipher = VideoCipher::new(&[9; 16]);
        let sealed = cipher.seal(&packets[0]);
        assert_eq!(sealed.len(), packets[0].len() + ENC_HEADER);
        assert_eq!(sealed[11], b'V');
        assert_eq!(sealed[12..16], 1u32.to_le_bytes(), "the frame index");
        assert_eq!(cipher.open(&sealed), Some(packets[0].clone()));
        assert_eq!(VideoCipher::new(&[8; 16]).open(&sealed), None);
        // Each packet gets the next IV.
        assert_eq!(cipher.seal(&packets[0])[..8], 1u64.to_le_bytes());
    }
}
