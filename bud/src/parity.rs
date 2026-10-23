//! Parity for loss recovery without a resend.
//!
//! A frame's data fragments are cut into blocks of at most [`MAX_BLOCK_DATA`]
//! (Reed-Solomon over GF(2^8) allows 256 shards in all, and a big frame has
//! thousands of fragments), and each block gets its own parity fragments from
//! `windowcast-fec`'s Cauchy code: any `k` of a block's `k + m` shards give the
//! block back. How many parity fragments a block gets is a [`ParityPolicy`]'s
//! decision, so it can change without touching the packet code:
//!
//! - [`NoParity`] sends none.
//! - [`AdaptiveParity`] follows the loss the receiver reports.
//!
//! GameStream audio does not use this: it keeps its fixed 4+2 code with
//! Sunshine's matrix (`windowcast-gamestream`), which stock Moonlight reads.
//!
//! # On the wire
//!
//! A parity packet has [`FLAG_PARITY`], the frame's `stream` and `frame`,
//! `fragments` = the frame's data fragment count, and `fragment` = its number
//! among the frame's parity packets. Its payload is a 6-byte prefix, then one
//! parity shard of exactly [`FRAGMENT_SIZE`] bytes:
//!
//! ```text
//! block (u16) | index (u8) | parity count of the block (u8) | last fragment length (u16)
//! ```
//!
//! The receiver works out each block's data fragments from the data fragment
//! count alone ([`blocks`]), so the prefix only has to say which block, which
//! parity shard of it, how many there are, and how long the frame's last
//! (short) data fragment really was, which padding to a full shard hides.

use std::ops::Range;

use windowcast_fec::Code;

use crate::header::{Header, FLAG_PARITY, MAX_PAYLOAD};

/// The bytes at the front of a parity packet's payload.
pub const PREFIX_LEN: usize = 6;
/// The size of every data fragment but a frame's last, and of every parity
/// shard: what is left of a datagram once a parity packet's prefix is in.
pub const FRAGMENT_SIZE: usize = MAX_PAYLOAD - PREFIX_LEN;
/// The most data fragments in one block, so a block with up to as many parity
/// fragments still fits the 256 shards the code allows.
pub const MAX_BLOCK_DATA: usize = 128;

/// How the data fragments of a frame are grouped into blocks: as few blocks
/// as [`MAX_BLOCK_DATA`] allows, as even as possible (the first ones get the
/// extra fragment). Sender and receiver both compute this from the data
/// fragment count, so it never travels.
pub fn blocks(data_fragments: usize) -> Vec<Range<usize>> {
    if data_fragments == 0 {
        return Vec::new();
    }
    let count = data_fragments.div_ceil(MAX_BLOCK_DATA);
    let base = data_fragments / count;
    let extra = data_fragments % count;
    let mut out = Vec::with_capacity(count);
    let mut start = 0;
    for block in 0..count {
        let len = base + usize::from(block < extra);
        out.push(start..start + len);
        start += len;
    }
    out
}

/// Decides how many parity fragments a block of `data` fragments gets.
pub trait ParityPolicy {
    fn parity_for(&self, data: usize) -> usize;
}

/// No parity: a lost fragment is lost.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoParity;

impl ParityPolicy for NoParity {
    fn parity_for(&self, _data: usize) -> usize {
        0
    }
}

/// Parity that follows the loss the receiver reports: the more packets go
/// missing, the more of each block is spent on recovering them.
///
/// The loss rate is smoothed (an exponential average) so one bad moment does
/// not whipsaw the overhead, and the parity count is the expected number of
/// lost fragments in a block times a safety factor, rounded up, so a typical
/// burst still falls inside it. It is capped, because past a point more parity
/// costs more than the loss it covers.
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveParity {
    loss: f32,
    safety: f32,
    max_overhead: f32,
}

impl AdaptiveParity {
    /// Below this smoothed loss, no parity is sent at all.
    pub const FLOOR: f32 = 0.002;
    /// Parity is never more than this share of a block's data (the default).
    pub const MAX_OVERHEAD: f32 = 0.5;
    /// How much more than the expected loss to cover (the default).
    pub const SAFETY: f32 = 2.0;
    /// How fast the estimate follows a new report (the weight of the newest).
    pub const SMOOTHING: f32 = 0.25;

    /// Parity as the only defence: it covers twice the expected loss and may
    /// take up to half a block, so a burst usually still falls inside it.
    pub fn new() -> Self {
        AdaptiveParity {
            loss: 0.0,
            safety: Self::SAFETY,
            max_overhead: Self::MAX_OVERHEAD,
        }
    }

    /// Parity as the first of several defences, for when retransmission and a
    /// refresh request ([`crate::recovery`]) stand behind it: it covers the
    /// expected loss and no more, and never takes more than a quarter of a
    /// block. A burst that gets past it is handled by the layers behind, so
    /// the steady cost on the wire is lower.
    pub fn layered() -> Self {
        AdaptiveParity {
            loss: 0.0,
            safety: 1.0,
            max_overhead: 0.25,
        }
    }

    /// The smoothed loss rate, 0 to 1.
    pub fn loss(&self) -> f32 {
        self.loss
    }

    /// One report from the receiver: `sent` packets in a period, of which
    /// `lost` did not arrive.
    pub fn observe(&mut self, sent: u32, lost: u32) {
        if sent == 0 {
            return;
        }
        let rate = (lost.min(sent) as f32) / sent as f32;
        self.loss += Self::SMOOTHING * (rate - self.loss);
    }
}

impl Default for AdaptiveParity {
    fn default() -> Self {
        AdaptiveParity::new()
    }
}

impl ParityPolicy for AdaptiveParity {
    fn parity_for(&self, data: usize) -> usize {
        if data == 0 || self.loss < Self::FLOOR {
            return 0;
        }
        let wanted = (data as f32 * self.loss * self.safety).ceil() as usize;
        let cap = ((data as f32 * self.max_overhead).ceil() as usize).max(1);
        wanted.max(1).min(cap)
    }
}

/// The parity packets for a frame's data fragments (as `fragment` made them:
/// every payload [`FRAGMENT_SIZE`] long but the last), as headers and
/// payloads for the sealer.
pub fn encode(data: &[(Header, &[u8])], policy: &dyn ParityPolicy) -> Vec<(Header, Vec<u8>)> {
    let Some((first, _)) = data.first() else {
        return Vec::new();
    };
    let last_len = data.last().map_or(0, |(_, p)| p.len());
    let mut out = Vec::new();
    for (block, range) in blocks(data.len()).into_iter().enumerate() {
        let k = range.len();
        let m = policy.parity_for(k).min(256 - k);
        if m == 0 {
            continue;
        }
        let shards: Vec<Vec<u8>> = data[range]
            .iter()
            .map(|(_, payload)| {
                let mut shard = payload.to_vec();
                shard.resize(FRAGMENT_SIZE, 0);
                shard
            })
            .collect();
        let refs: Vec<&[u8]> = shards.iter().map(Vec::as_slice).collect();
        let code = Code::cauchy(k, m).expect("k is 1..=128 and k + m is at most 256");
        let parity = code.encode(&refs).expect("equal-sized shards, k of them");
        for (index, shard) in parity.into_iter().enumerate() {
            let mut payload = Vec::with_capacity(PREFIX_LEN + FRAGMENT_SIZE);
            payload.extend_from_slice(&(block as u16).to_be_bytes());
            payload.push(index as u8);
            payload.push(m as u8);
            payload.extend_from_slice(&(last_len as u16).to_be_bytes());
            payload.extend_from_slice(&shard);
            out.push((
                Header {
                    flags: FLAG_PARITY,
                    stream: first.stream,
                    frame: first.frame,
                    fragment: out.len() as u16,
                    fragments: first.fragments,
                    packet: 0,
                    sent_us: first.sent_us,
                },
                payload,
            ));
        }
    }
    out
}

/// What a parity packet's prefix says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Prefix {
    pub block: usize,
    pub index: usize,
    pub count: usize,
    pub last_len: usize,
}

/// Reads a parity packet's payload into its prefix and shard, or `None` if it
/// is not shaped like one.
pub(crate) fn parse(payload: &[u8]) -> Option<(Prefix, &[u8])> {
    if payload.len() != PREFIX_LEN + FRAGMENT_SIZE {
        return None;
    }
    let prefix = Prefix {
        block: usize::from(u16::from_be_bytes([payload[0], payload[1]])),
        index: usize::from(payload[2]),
        count: usize::from(payload[3]),
        last_len: usize::from(u16::from_be_bytes([payload[4], payload[5]])),
    };
    if prefix.count == 0 || prefix.index >= prefix.count || prefix.last_len > FRAGMENT_SIZE {
        return None;
    }
    Some((prefix, &payload[PREFIX_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_cover_every_fragment_once_and_stay_even() {
        for n in [
            0usize, 1, 2, 127, 128, 129, 255, 256, 257, 300, 1000, 14_000,
        ] {
            let b = blocks(n);
            let total: usize = b.iter().map(Range::len).sum();
            assert_eq!(total, n, "n = {n}");
            let mut next = 0;
            for r in &b {
                assert_eq!(r.start, next);
                assert!(r.len() <= MAX_BLOCK_DATA);
                next = r.end;
            }
            if let (Some(min), Some(max)) = (
                b.iter().map(Range::len).min(),
                b.iter().map(Range::len).max(),
            ) {
                assert!(max - min <= 1, "n = {n}: uneven blocks");
            }
        }
        assert_eq!(blocks(129).len(), 2);
        assert_eq!(blocks(128).len(), 1);
    }

    #[test]
    fn no_loss_means_no_parity_and_loss_means_some_but_bounded() {
        let mut p = AdaptiveParity::new();
        assert_eq!(p.parity_for(100), 0);
        p.observe(1000, 0);
        assert_eq!(p.parity_for(100), 0);
        // A few percent of loss, steadily, settles on a few parity fragments.
        for _ in 0..60 {
            p.observe(1000, 30);
        }
        assert!((p.loss() - 0.03).abs() < 0.005);
        let m = p.parity_for(100);
        assert!((5..=8).contains(&m), "got {m}");
        // Total loss is capped at half the block.
        for _ in 0..60 {
            p.observe(1000, 1000);
        }
        assert_eq!(p.parity_for(100), 50);
        assert_eq!(p.parity_for(1), 1);
    }

    #[test]
    fn the_estimate_follows_loss_up_and_back_down_without_jumping() {
        let mut p = AdaptiveParity::new();
        p.observe(100, 20);
        let after_one = p.loss();
        assert!(after_one > 0.0 && after_one < 0.2, "{after_one}");
        for _ in 0..80 {
            p.observe(100, 0);
        }
        assert_eq!(
            p.parity_for(100),
            0,
            "recovers to none once the loss is gone"
        );
        // Reports with nothing sent are ignored.
        let before = p.loss();
        p.observe(0, 0);
        assert_eq!(p.loss(), before);
    }

    #[test]
    fn a_parity_payload_round_trips_through_parse() {
        let mut payload = vec![0, 3, 1, 4, 0x01, 0x02];
        payload.extend(std::iter::repeat_n(9u8, FRAGMENT_SIZE));
        let (prefix, shard) = parse(&payload).unwrap();
        assert_eq!(
            prefix,
            Prefix {
                block: 3,
                index: 1,
                count: 4,
                last_len: 0x0102
            }
        );
        assert_eq!(shard.len(), FRAGMENT_SIZE);
    }

    #[test]
    fn a_parity_payload_with_a_bad_shape_is_refused() {
        let good = || {
            let mut p = vec![0, 0, 0, 2, 0, 5];
            p.extend(std::iter::repeat_n(1u8, FRAGMENT_SIZE));
            p
        };
        assert!(parse(&good()).is_some());
        let mut short = good();
        short.pop();
        assert!(parse(&short).is_none());
        let mut zero_count = good();
        zero_count[3] = 0;
        assert!(parse(&zero_count).is_none());
        let mut index_past = good();
        index_past[2] = 2;
        assert!(parse(&index_past).is_none());
        let mut long_last = good();
        long_last[4] = 0xff;
        long_last[5] = 0xff;
        assert!(parse(&long_last).is_none());
    }
}
