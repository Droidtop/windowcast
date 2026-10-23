//! Cutting a frame into datagrams and putting it back together.
//!
//! A video frame is far bigger than one datagram, so the sender cuts it into
//! numbered fragments of [`FRAGMENT_SIZE`] bytes (the last may be shorter) and
//! the receiver collects them per frame. Fragments arrive late, out of order,
//! twice or not at all, and a peer can send anything, so the [`Reassembler`]:
//!
//! - holds a bounded number of unfinished frames per stream and a bounded
//!   size per frame, whatever the headers claim;
//! - delivers a frame the moment it is complete (there is no jitter buffer; a
//!   late frame is worth less than a fresh one);
//! - drops a frame older than one it has already delivered, and counts it;
//! - ignores a repeated fragment, and discards a frame whose fragments
//!   disagree about how many there are;
//! - rebuilds lost data fragments from parity ([`crate::parity`]) as soon as a
//!   block has enough shards, so a loss the sender covered costs no resend.

use std::collections::HashMap;

use windowcast_fec::Code;

use crate::header::{Header, FLAG_PARITY};
use crate::parity::{self, ParityPolicy, FRAGMENT_SIZE};

/// The largest frame accepted, far above any encoded frame this transport
/// will carry; it bounds what a peer can make the receiver allocate.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;
/// How many unfinished frames one stream may have at once. A frame that
/// misses this window is lost, and a newer one is worth more anyway.
pub const MAX_IN_FLIGHT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FragmentError {
    #[error("a frame of {0} bytes is more than {MAX_FRAME}")]
    TooLarge(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReassembleError {
    #[error("the frame is larger than {MAX_FRAME} bytes")]
    TooLarge,
    #[error("the fragments of one frame disagree about how many there are")]
    Inconsistent,
    #[error("a data fragment is not the size the sender cuts them to")]
    BadFragment,
}

/// Cuts `data` into the headers and payloads of its data datagrams: as many as
/// [`FRAGMENT_SIZE`] needs, at least one (an empty frame is one empty
/// fragment). `packet` is left 0 for the sealer to fill in.
pub fn fragment(
    stream: u16,
    frame: u32,
    sent_us: u32,
    data: &[u8],
) -> Result<Vec<(Header, &[u8])>, FragmentError> {
    if data.len() > MAX_FRAME {
        return Err(FragmentError::TooLarge(data.len()));
    }
    let count = data.len().div_ceil(FRAGMENT_SIZE).max(1);
    // MAX_FRAME / FRAGMENT_SIZE is about 14,500, well inside a u16.
    let fragments = u16::try_from(count).map_err(|_| FragmentError::TooLarge(data.len()))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let start = index * FRAGMENT_SIZE;
        let end = (start + FRAGMENT_SIZE).min(data.len());
        out.push((
            Header {
                flags: 0,
                stream,
                frame,
                fragment: index as u16,
                fragments,
                packet: 0,
                sent_us,
            },
            &data[start..end],
        ));
    }
    Ok(out)
}

/// A frame as the datagrams to send: its data fragments, then the parity
/// `policy` asks for. Send them in this order; a receiver that gets the data
/// first never needs the rest.
pub fn fragment_with_parity(
    stream: u16,
    frame: u32,
    sent_us: u32,
    data: &[u8],
    policy: &dyn ParityPolicy,
) -> Result<Vec<(Header, Vec<u8>)>, FragmentError> {
    let parts = fragment(stream, frame, sent_us, data)?;
    let parity = parity::encode(&parts, policy);
    let mut out: Vec<(Header, Vec<u8>)> = parts.into_iter().map(|(h, p)| (h, p.to_vec())).collect();
    out.extend(parity);
    Ok(out)
}

/// A frame that is still waiting for data fragments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incomplete {
    pub stream: u16,
    pub frame: u32,
    /// The data fragments that have not arrived and could not be rebuilt.
    pub missing: Vec<u16>,
}

/// A frame put back together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub stream: u16,
    pub frame: u32,
    /// The send time stamped on the first fragment of it to arrive.
    pub sent_us: u32,
    pub data: Vec<u8>,
}

/// Whether frame number `a` is later than `b`, counting past the wrap of the
/// 32-bit number the way TCP counts sequence numbers.
fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < 1 << 31
}

/// The parity shards received for one block of a frame.
struct BlockParity {
    shards: Vec<Option<Vec<u8>>>,
}

struct Pending {
    frame: u32,
    sent_us: u32,
    slots: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
    parity: HashMap<usize, BlockParity>,
    parity_stored: usize,
    /// The true length of the frame's last data fragment, once a parity
    /// packet has said (padding to a full shard hides it).
    last_len: Option<usize>,
}

impl Pending {
    /// Rebuilds the data fragments of every block that has enough shards.
    fn recover(&mut self) {
        let n = self.slots.len();
        let Some(last_len) = self.last_len else {
            return;
        };
        for (block, range) in parity::blocks(n).into_iter().enumerate() {
            let Some(block_parity) = self.parity.get(&block) else {
                continue;
            };
            if range.clone().all(|i| self.slots[i].is_some()) {
                continue;
            }
            let k = range.len();
            let have = range.clone().filter(|i| self.slots[*i].is_some()).count()
                + block_parity.shards.iter().flatten().count();
            if have < k {
                continue;
            }
            let mut shards: Vec<Option<Vec<u8>>> = range
                .clone()
                .map(|i| {
                    self.slots[i].as_ref().map(|data| {
                        let mut shard = data.clone();
                        shard.resize(FRAGMENT_SIZE, 0);
                        shard
                    })
                })
                .collect();
            shards.extend(block_parity.shards.iter().cloned());
            let Ok(code) = Code::cauchy(k, block_parity.shards.len()) else {
                continue;
            };
            let Some(data) = code.recover(&shards) else {
                continue;
            };
            for (offset, i) in range.enumerate() {
                if self.slots[i].is_none() {
                    let mut shard = data[offset].clone();
                    shard.truncate(if i == n - 1 { last_len } else { FRAGMENT_SIZE });
                    self.bytes += shard.len();
                    self.slots[i] = Some(shard);
                    self.received += 1;
                }
            }
        }
    }
}

#[derive(Default)]
struct StreamState {
    last_delivered: Option<u32>,
    pending: Vec<Pending>,
}

#[derive(Default)]
pub struct Reassembler {
    streams: HashMap<u16, StreamState>,
    dropped: u64,
    recovered: u64,
}

impl Reassembler {
    pub fn new() -> Self {
        Reassembler::default()
    }

    /// Frames given up on: evicted unfinished, or overtaken by a newer frame
    /// that finished first.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Data fragments rebuilt from parity instead of resent.
    pub fn recovered(&self) -> u64 {
        self.recovered
    }

    /// The frames waiting for fragments, and which: what a
    /// [`crate::recovery::LossDetector`] decides about.
    pub fn incomplete(&self) -> Vec<Incomplete> {
        let mut out = Vec::new();
        for (stream, state) in &self.streams {
            for pending in &state.pending {
                out.push(Incomplete {
                    stream: *stream,
                    frame: pending.frame,
                    missing: (0..pending.slots.len())
                        .filter(|i| pending.slots[*i].is_none())
                        .map(|i| i as u16)
                        .collect(),
                });
            }
        }
        out
    }

    /// Gives up on a frame that is waiting, freeing what it holds. A late
    /// fragment of it may start it again; it is dropped like any frame older
    /// than one already delivered. Returns whether there was such a frame.
    pub fn abandon(&mut self, stream: u16, frame: u32) -> bool {
        let Some(state) = self.streams.get_mut(&stream) else {
            return false;
        };
        let Some(index) = state.pending.iter().position(|p| p.frame == frame) else {
            return false;
        };
        state.pending.remove(index);
        self.dropped += 1;
        true
    }

    /// Adds one authenticated packet, data or parity. Returns the frame if
    /// this packet completed it.
    pub fn push(
        &mut self,
        header: &Header,
        payload: &[u8],
    ) -> Result<Option<Frame>, ReassembleError> {
        let is_parity = header.flags & FLAG_PARITY != 0;
        let fragments = usize::from(header.fragments);
        let parsed = if is_parity {
            // A parity packet that is not shaped like one is noise to skip,
            // not a reason to give up the frame.
            let Some(parsed) = parity::parse(payload) else {
                return Ok(None);
            };
            if parsed.0.block >= parity::blocks(fragments).len() {
                return Ok(None);
            }
            Some(parsed)
        } else {
            let last = usize::from(header.fragment) + 1 == fragments;
            if payload.len() > FRAGMENT_SIZE || (!last && payload.len() != FRAGMENT_SIZE) {
                return Err(ReassembleError::BadFragment);
            }
            None
        };

        let state = self.streams.entry(header.stream).or_default();
        if let Some(last) = state.last_delivered {
            // The frame already delivered, or an older one: too late.
            if !newer(header.frame, last) {
                return Ok(None);
            }
        }
        let index = match state.pending.iter().position(|p| p.frame == header.frame) {
            Some(index) => {
                if state.pending[index].slots.len() != fragments {
                    state.pending.remove(index);
                    self.dropped += 1;
                    return Err(ReassembleError::Inconsistent);
                }
                index
            }
            None => {
                if fragments * FRAGMENT_SIZE > MAX_FRAME + FRAGMENT_SIZE {
                    return Err(ReassembleError::TooLarge);
                }
                if state.pending.len() >= MAX_IN_FLIGHT {
                    // Make room by giving up on the oldest unfinished frame.
                    let oldest = (0..state.pending.len())
                        .reduce(|a, b| {
                            if newer(state.pending[a].frame, state.pending[b].frame) {
                                b
                            } else {
                                a
                            }
                        })
                        .expect("pending is not empty");
                    state.pending.remove(oldest);
                    self.dropped += 1;
                }
                state.pending.push(Pending {
                    frame: header.frame,
                    sent_us: header.sent_us,
                    slots: vec![None; fragments],
                    received: 0,
                    bytes: 0,
                    parity: HashMap::new(),
                    parity_stored: 0,
                    last_len: None,
                });
                state.pending.len() - 1
            }
        };
        let pending = &mut state.pending[index];

        let before = pending.received;
        match parsed {
            None => {
                let slot = &mut pending.slots[usize::from(header.fragment)];
                if slot.is_some() {
                    return Ok(None);
                }
                pending.bytes += payload.len();
                if pending.bytes > MAX_FRAME {
                    state.pending.remove(index);
                    self.dropped += 1;
                    return Err(ReassembleError::TooLarge);
                }
                *slot = Some(payload.to_vec());
                pending.received += 1;
                // Parity may already be here, waiting for exactly this piece.
                if !pending.parity.is_empty() {
                    let arrived = pending.received;
                    pending.recover();
                    self.recovered += (pending.received - arrived) as u64;
                }
            }
            Some((prefix, shard)) => {
                // A frame never needs more parity than data to be covered;
                // anything past that is a peer filling memory.
                if pending.parity_stored >= fragments.max(8) {
                    return Ok(None);
                }
                let entry = pending
                    .parity
                    .entry(prefix.block)
                    .or_insert_with(|| BlockParity {
                        shards: vec![None; prefix.count],
                    });
                if entry.shards.len() != prefix.count {
                    state.pending.remove(index);
                    self.dropped += 1;
                    return Err(ReassembleError::Inconsistent);
                }
                if entry.shards[prefix.index].is_some() {
                    return Ok(None);
                }
                entry.shards[prefix.index] = Some(shard.to_vec());
                pending.parity_stored += 1;
                pending.last_len = Some(prefix.last_len);
                pending.recover();
                self.recovered += (pending.received - before) as u64;
            }
        }
        if pending.received < pending.slots.len() {
            return Ok(None);
        }

        let done = state.pending.remove(index);
        let mut data = Vec::with_capacity(done.bytes);
        for part in done.slots.into_iter().flatten() {
            data.extend_from_slice(&part);
        }
        state.last_delivered = Some(done.frame);
        // Anything unfinished and older than what was just delivered is stale.
        let before = state.pending.len();
        state.pending.retain(|p| newer(p.frame, done.frame));
        self.dropped += (before - state.pending.len()) as u64;
        Ok(Some(Frame {
            stream: header.stream,
            frame: done.frame,
            sent_us: done.sent_us,
            data,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parity::{AdaptiveParity, NoParity, MAX_BLOCK_DATA};

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn feed(r: &mut Reassembler, parts: &[(Header, &[u8])]) -> Vec<Frame> {
        parts
            .iter()
            .filter_map(|(h, p)| r.push(h, p).unwrap())
            .collect()
    }

    /// Always the same number of parity fragments per block.
    struct Fixed(usize);

    impl ParityPolicy for Fixed {
        fn parity_for(&self, _data: usize) -> usize {
            self.0
        }
    }

    fn feed_owned(r: &mut Reassembler, parts: &[&(Header, Vec<u8>)]) -> Vec<Frame> {
        parts
            .iter()
            .filter_map(|(h, p)| r.push(h, p).unwrap())
            .collect()
    }

    #[test]
    fn a_frame_is_cut_to_the_fragment_size_and_comes_back_whole() {
        let data = bytes(FRAGMENT_SIZE * 3 + 17);
        let parts = fragment(4, 10, 99, &data).unwrap();
        assert_eq!(parts.len(), 4);
        assert!(parts
            .iter()
            .all(|(h, p)| h.fragments == 4 && p.len() <= FRAGMENT_SIZE));
        let mut r = Reassembler::new();
        let frames = feed(&mut r, &parts);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, data);
        assert_eq!(
            (frames[0].stream, frames[0].frame, frames[0].sent_us),
            (4, 10, 99)
        );
    }

    #[test]
    fn small_and_empty_frames_are_one_fragment() {
        for len in [0, 1, FRAGMENT_SIZE] {
            let data = bytes(len);
            let parts = fragment(0, 0, 0, &data).unwrap();
            assert_eq!(parts.len(), 1, "len {len}");
            let mut r = Reassembler::new();
            assert_eq!(feed(&mut r, &parts)[0].data, data);
        }
        assert_eq!(
            fragment(0, 0, 0, &bytes(FRAGMENT_SIZE + 1)).unwrap().len(),
            2
        );
    }

    #[test]
    fn fragments_in_any_order_and_repeated_still_give_one_frame() {
        let data = bytes(FRAGMENT_SIZE * 5 + 3);
        let parts = fragment(1, 1, 0, &data).unwrap();
        let order = [3usize, 0, 5, 3, 1, 4, 0, 2, 5];
        let shuffled: Vec<_> = order.iter().map(|&i| parts[i]).collect();
        let mut r = Reassembler::new();
        let frames = feed(&mut r, &shuffled);
        assert_eq!(frames.len(), 1, "a repeat must not deliver twice");
        assert_eq!(frames[0].data, data);
    }

    #[test]
    fn a_frame_with_a_missing_fragment_is_not_delivered() {
        let data = bytes(FRAGMENT_SIZE * 3);
        let parts = fragment(1, 1, 0, &data).unwrap();
        let mut r = Reassembler::new();
        assert!(feed(&mut r, &parts[..2]).is_empty());
    }

    #[test]
    fn a_late_older_frame_is_dropped_once_a_newer_one_was_delivered() {
        let old_data = bytes(FRAGMENT_SIZE * 2);
        let old = fragment(1, 5, 0, &old_data).unwrap();
        let new_data = bytes(100);
        let new = fragment(1, 6, 0, &new_data).unwrap();
        let mut r = Reassembler::new();
        assert!(feed(&mut r, &old[..1]).is_empty());
        assert_eq!(feed(&mut r, &new).len(), 1);
        // The rest of frame 5 arrives too late, and its half-built state
        // was already thrown away when frame 6 finished.
        assert!(feed(&mut r, &old[1..]).is_empty());
        assert_eq!(r.dropped(), 1);
        // Frame 6 again is a duplicate, not a new frame.
        assert!(feed(&mut r, &new).is_empty());
    }

    #[test]
    fn frame_numbers_count_past_the_wrap() {
        let mut r = Reassembler::new();
        for frame in [u32::MAX - 1, u32::MAX, 0, 1] {
            let data = bytes(10);
            let parts = fragment(1, frame, 0, &data).unwrap();
            assert_eq!(feed(&mut r, &parts).len(), 1, "frame {frame}");
        }
        // And an old number is old, not "far ahead".
        let stale_data = bytes(10);
        let stale = fragment(1, u32::MAX, 0, &stale_data).unwrap();
        assert!(feed(&mut r, &stale).is_empty());
    }

    #[test]
    fn unfinished_frames_are_bounded_and_the_oldest_goes_first() {
        let mut r = Reassembler::new();
        // Frames 0..=5, each with its first of two fragments only.
        for frame in 0..=(MAX_IN_FLIGHT as u32 + 1) {
            let data = bytes(FRAGMENT_SIZE + 1);
            let parts = fragment(1, frame, 0, &data).unwrap();
            assert!(feed(&mut r, &parts[..1]).is_empty());
        }
        assert_eq!(r.dropped(), 2);
        // Frame 0 was evicted, so its second half cannot finish it; frame 5 can.
        let data = bytes(FRAGMENT_SIZE + 1);
        let zero = fragment(1, 0, 0, &data).unwrap();
        assert!(feed(&mut r, &zero[1..]).is_empty());
        let five = fragment(1, 5, 0, &data).unwrap();
        assert_eq!(feed(&mut r, &five[1..]).len(), 1);
    }

    #[test]
    fn fragments_that_disagree_discard_the_frame() {
        let data = bytes(FRAGMENT_SIZE * 2);
        let parts = fragment(1, 1, 0, &data).unwrap();
        let mut liar = parts[1].0;
        liar.fragments = 3;
        let mut r = Reassembler::new();
        assert!(r.push(&parts[0].0, parts[0].1).unwrap().is_none());
        assert_eq!(
            r.push(&liar, parts[1].1),
            Err(ReassembleError::Inconsistent)
        );
        assert_eq!(r.dropped(), 1);
    }

    #[test]
    fn a_frame_that_claims_to_be_huge_is_refused_before_anything_is_allocated() {
        let mut r = Reassembler::new();
        let header = Header {
            flags: 0,
            stream: 1,
            frame: 1,
            fragment: 0,
            fragments: u16::MAX,
            packet: 0,
            sent_us: 0,
        };
        assert_eq!(
            r.push(&header, &vec![0u8; FRAGMENT_SIZE]),
            Err(ReassembleError::TooLarge)
        );
    }

    #[test]
    fn a_data_fragment_of_the_wrong_size_is_refused() {
        let data = bytes(FRAGMENT_SIZE * 2);
        let parts = fragment(1, 1, 0, &data).unwrap();
        let mut r = Reassembler::new();
        // A first fragment that is not full, or any fragment past full size.
        assert_eq!(
            r.push(&parts[0].0, &parts[0].1[..FRAGMENT_SIZE - 1]),
            Err(ReassembleError::BadFragment)
        );
        assert_eq!(
            r.push(&parts[1].0, &vec![0u8; FRAGMENT_SIZE + 1]),
            Err(ReassembleError::BadFragment)
        );
    }

    #[test]
    fn streams_do_not_disturb_each_other() {
        let mut r = Reassembler::new();
        let (a_data, b_data) = (bytes(50), bytes(60));
        let a = fragment(1, 7, 0, &a_data).unwrap();
        let b = fragment(2, 7, 0, &b_data).unwrap();
        assert_eq!(feed(&mut r, &a)[0].data.len(), 50);
        assert_eq!(feed(&mut r, &b)[0].data.len(), 60);
    }

    #[test]
    fn oversized_frames_are_refused_by_the_sender() {
        let data = vec![0u8; MAX_FRAME + 1];
        assert_eq!(
            fragment(0, 0, 0, &data).map(|v| v.len()),
            Err(FragmentError::TooLarge(MAX_FRAME + 1))
        );
    }

    // --- parity ---

    #[test]
    fn no_policy_no_parity_packets() {
        let data = bytes(FRAGMENT_SIZE * 4);
        let all = fragment_with_parity(1, 1, 0, &data, &NoParity).unwrap();
        assert_eq!(all.len(), 4);
        assert!(all.iter().all(|(h, _)| h.flags & FLAG_PARITY == 0));
    }

    #[test]
    fn lost_data_fragments_come_back_from_parity_up_to_the_parity_count() {
        let data = bytes(FRAGMENT_SIZE * 9 + 123);
        let all = fragment_with_parity(1, 1, 0, &data, &Fixed(3)).unwrap();
        assert_eq!(all.len(), 10 + 3);
        // Lose any three data fragments (including the short last one).
        for lost in [[0usize, 1, 2], [3, 5, 9], [7, 8, 9], [0, 4, 9]] {
            let mut r = Reassembler::new();
            let kept: Vec<_> = all
                .iter()
                .enumerate()
                .filter(|(i, _)| !lost.contains(i))
                .map(|(_, p)| p)
                .collect();
            let frames = feed_owned(&mut r, &kept);
            assert_eq!(frames.len(), 1, "lost {lost:?}");
            assert_eq!(frames[0].data, data, "lost {lost:?}");
            assert_eq!(r.recovered(), 3);
        }
    }

    #[test]
    fn one_loss_too_many_is_not_recovered() {
        let data = bytes(FRAGMENT_SIZE * 9 + 123);
        let all = fragment_with_parity(1, 1, 0, &data, &Fixed(3)).unwrap();
        let mut r = Reassembler::new();
        let kept: Vec<_> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| ![0usize, 1, 2, 3].contains(i))
            .map(|(_, p)| p)
            .collect();
        assert!(feed_owned(&mut r, &kept).is_empty());
    }

    #[test]
    fn the_short_last_fragment_comes_back_at_its_true_length() {
        let data = bytes(FRAGMENT_SIZE * 2 + 7);
        let all = fragment_with_parity(1, 1, 0, &data, &Fixed(1)).unwrap();
        let mut r = Reassembler::new();
        // Lose only the last data fragment (index 2, 7 bytes long).
        let kept: Vec<_> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != 2)
            .map(|(_, p)| p)
            .collect();
        let frames = feed_owned(&mut r, &kept);
        assert_eq!(frames[0].data.len(), data.len());
        assert_eq!(frames[0].data, data);
    }

    #[test]
    fn parity_that_arrives_first_still_works() {
        let data = bytes(FRAGMENT_SIZE * 4);
        let all = fragment_with_parity(1, 1, 0, &data, &Fixed(2)).unwrap();
        let mut r = Reassembler::new();
        // Both parity packets, then only two of the four data fragments.
        let order = [&all[4], &all[5], &all[1], &all[3]];
        let frames = feed_owned(&mut r, &order);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, data);
    }

    #[test]
    fn a_big_frame_is_cut_into_blocks_each_with_its_own_parity() {
        let n = MAX_BLOCK_DATA * 2 + 44; // three blocks
        let data = bytes(FRAGMENT_SIZE * n - 9);
        let all = fragment_with_parity(1, 1, 0, &data, &Fixed(4)).unwrap();
        assert_eq!(all.len(), n + 3 * 4);
        // Lose four data fragments in every block.
        let ranges = parity::blocks(n);
        let lost: Vec<usize> = ranges.iter().flat_map(|r| r.start..r.start + 4).collect();
        let kept: Vec<_> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| !lost.contains(i))
            .map(|(_, p)| p)
            .collect();
        let mut r = Reassembler::new();
        let frames = feed_owned(&mut r, &kept);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, data);
        assert_eq!(r.recovered(), 12);
    }

    #[test]
    fn random_losses_within_the_parity_always_recover() {
        let data = bytes(FRAGMENT_SIZE * 20 + 500);
        let all = fragment_with_parity(1, 1, 0, &data, &Fixed(5)).unwrap();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..200 {
            // Choose up to five distinct packets of the 26 to lose.
            let mut lost = Vec::new();
            while lost.len() < (next() % 6) as usize {
                let i = (next() % all.len() as u64) as usize;
                if !lost.contains(&i) {
                    lost.push(i);
                }
            }
            let mut kept: Vec<_> = all
                .iter()
                .enumerate()
                .filter(|(i, _)| !lost.contains(i))
                .map(|(_, p)| p)
                .collect();
            // And deliver them in a scrambled order.
            for i in (1..kept.len()).rev() {
                let j = (next() % (i as u64 + 1)) as usize;
                kept.swap(i, j);
            }
            let mut r = Reassembler::new();
            let frames = feed_owned(&mut r, &kept);
            assert_eq!(frames.len(), 1, "lost {lost:?}");
            assert_eq!(frames[0].data, data, "lost {lost:?}");
        }
    }

    #[test]
    fn the_adaptive_policy_covers_the_loss_it_was_told_about() {
        let mut policy = AdaptiveParity::new();
        for _ in 0..40 {
            policy.observe(1000, 100); // 10 % loss
        }
        let data = bytes(FRAGMENT_SIZE * 30);
        let all = fragment_with_parity(1, 1, 0, &data, &policy).unwrap();
        let parity_count = all.len() - 30;
        assert!(parity_count >= 6, "got {parity_count}");
        // Lose a tenth of everything sent.
        let lost: Vec<usize> = (0..all.len()).filter(|i| i % 10 == 3).collect();
        assert!(lost.len() <= parity_count);
        let kept: Vec<_> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| !lost.contains(i))
            .map(|(_, p)| p)
            .collect();
        let mut r = Reassembler::new();
        assert_eq!(feed_owned(&mut r, &kept)[0].data, data);
    }

    #[test]
    fn malformed_and_contradictory_parity_does_no_harm() {
        let data = bytes(FRAGMENT_SIZE * 4);
        let all = fragment_with_parity(1, 1, 0, &data, &Fixed(2)).unwrap();
        let mut r = Reassembler::new();
        // A parity packet of the wrong size is skipped.
        let mut short = all[4].clone();
        short.1.pop();
        assert!(r.push(&short.0, &short.1).unwrap().is_none());
        // One for a block the frame does not have is skipped.
        let mut far = all[4].clone();
        far.1[1] = 9;
        assert!(r.push(&far.0, &far.1).unwrap().is_none());
        // A second parity count for the same block discards the frame.
        assert!(r.push(&all[4].0, &all[4].1).unwrap().is_none());
        let mut liar = all[5].clone();
        liar.1[3] = 3;
        assert_eq!(r.push(&liar.0, &liar.1), Err(ReassembleError::Inconsistent));
    }
}
