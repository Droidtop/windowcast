//! Cutting a frame into datagrams and putting it back together.
//!
//! A video frame is far bigger than one datagram ([`MAX_PAYLOAD`] bytes), so
//! the sender cuts it into numbered fragments and the receiver collects them
//! per frame. Fragments arrive late, out of order, twice or not at all, and a
//! peer can send anything, so the [`Reassembler`]:
//!
//! - holds a bounded number of unfinished frames per stream and a bounded
//!   size per frame, whatever the headers claim;
//! - delivers a frame the moment its last fragment arrives (there is no jitter
//!   buffer; a late frame is worth less than a fresh one);
//! - drops a frame older than one it has already delivered, and counts it;
//! - ignores a repeated fragment, and discards a frame whose fragments
//!   disagree about how many there are.
//!
//! Parity fragments (loss recovery without a resend) are not handled yet: a
//! packet with [`FLAG_PARITY`] is ignored here.

use std::collections::HashMap;

use crate::header::{Header, FLAG_PARITY, MAX_PAYLOAD};

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
}

/// Cuts `data` into the headers and payloads of its datagrams: as many as
/// [`MAX_PAYLOAD`] needs, at least one (an empty frame is one empty fragment).
/// `packet` is left 0 for the sealer to fill in.
pub fn fragment(
    stream: u16,
    frame: u32,
    sent_us: u32,
    data: &[u8],
) -> Result<Vec<(Header, &[u8])>, FragmentError> {
    if data.len() > MAX_FRAME {
        return Err(FragmentError::TooLarge(data.len()));
    }
    let count = data.len().div_ceil(MAX_PAYLOAD).max(1);
    // MAX_FRAME / MAX_PAYLOAD is about 14,500, well inside a u16.
    let fragments = u16::try_from(count).map_err(|_| FragmentError::TooLarge(data.len()))?;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let start = index * MAX_PAYLOAD;
        let end = (start + MAX_PAYLOAD).min(data.len());
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

struct Pending {
    frame: u32,
    sent_us: u32,
    slots: Vec<Option<Vec<u8>>>,
    received: usize,
    bytes: usize,
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

    /// Adds one authenticated fragment. Returns the frame if this fragment
    /// completed it.
    pub fn push(
        &mut self,
        header: &Header,
        payload: &[u8],
    ) -> Result<Option<Frame>, ReassembleError> {
        if header.flags & FLAG_PARITY != 0 {
            return Ok(None);
        }
        let state = self.streams.entry(header.stream).or_default();
        if let Some(last) = state.last_delivered {
            // The frame already delivered, or an older one: too late.
            if !newer(header.frame, last) {
                return Ok(None);
            }
        }
        let fragments = usize::from(header.fragments);
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
                if fragments * MAX_PAYLOAD > MAX_FRAME + MAX_PAYLOAD {
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
                });
                state.pending.len() - 1
            }
        };
        let pending = &mut state.pending[index];
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

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn feed(r: &mut Reassembler, parts: &[(Header, &[u8])]) -> Vec<Frame> {
        parts
            .iter()
            .filter_map(|(h, p)| r.push(h, p).unwrap())
            .collect()
    }

    #[test]
    fn a_frame_is_cut_to_the_datagram_size_and_comes_back_whole() {
        let data = bytes(MAX_PAYLOAD * 3 + 17);
        let parts = fragment(4, 10, 99, &data).unwrap();
        assert_eq!(parts.len(), 4);
        assert!(parts
            .iter()
            .all(|(h, p)| h.fragments == 4 && p.len() <= MAX_PAYLOAD));
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
        for len in [0, 1, MAX_PAYLOAD] {
            let data = bytes(len);
            let parts = fragment(0, 0, 0, &data).unwrap();
            assert_eq!(parts.len(), 1, "len {len}");
            let mut r = Reassembler::new();
            assert_eq!(feed(&mut r, &parts)[0].data, data);
        }
        assert_eq!(fragment(0, 0, 0, &bytes(MAX_PAYLOAD + 1)).unwrap().len(), 2);
    }

    #[test]
    fn fragments_in_any_order_and_repeated_still_give_one_frame() {
        let data = bytes(MAX_PAYLOAD * 5 + 3);
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
        let parts_data = bytes(MAX_PAYLOAD * 3);
        let parts = fragment(1, 1, 0, &parts_data).unwrap();
        let mut r = Reassembler::new();
        assert!(feed(&mut r, &parts[..2]).is_empty());
    }

    #[test]
    fn a_late_older_frame_is_dropped_once_a_newer_one_was_delivered() {
        let old_data = bytes(MAX_PAYLOAD * 2);
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
            let parts_data = bytes(10);
            let parts = fragment(1, frame, 0, &parts_data).unwrap();
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
            let parts_data = bytes(MAX_PAYLOAD + 1);
            let parts = fragment(1, frame, 0, &parts_data).unwrap();
            assert!(feed(&mut r, &parts[..1]).is_empty());
        }
        assert_eq!(r.dropped(), 2);
        // Frame 0 was evicted, so its second half cannot finish it; frame 5 can.
        let zero_data = bytes(MAX_PAYLOAD + 1);
        let zero = fragment(1, 0, 0, &zero_data).unwrap();
        assert!(feed(&mut r, &zero[1..]).is_empty());
        let five_data = bytes(MAX_PAYLOAD + 1);
        let five = fragment(1, 5, 0, &five_data).unwrap();
        assert_eq!(feed(&mut r, &five[1..]).len(), 1);
    }

    #[test]
    fn fragments_that_disagree_discard_the_frame() {
        let parts_data = bytes(MAX_PAYLOAD * 2);
        let parts = fragment(1, 1, 0, &parts_data).unwrap();
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
        assert_eq!(r.push(&header, b"x"), Err(ReassembleError::TooLarge));
    }

    #[test]
    fn streams_do_not_disturb_each_other_and_parity_is_ignored() {
        let mut r = Reassembler::new();
        let a_data = bytes(50);
        let a = fragment(1, 7, 0, &a_data).unwrap();
        let b_data = bytes(60);
        let b = fragment(2, 7, 0, &b_data).unwrap();
        assert_eq!(feed(&mut r, &a)[0].data.len(), 50);
        assert_eq!(feed(&mut r, &b)[0].data.len(), 60);
        let mut parity = a[0].0;
        parity.flags = FLAG_PARITY;
        parity.frame = 8;
        assert!(r.push(&parity, b"parity").unwrap().is_none());
    }

    #[test]
    fn oversized_frames_are_refused_by_the_sender() {
        let data = vec![0u8; MAX_FRAME + 1];
        assert_eq!(
            fragment(0, 0, 0, &data).map(|v| v.len()),
            Err(FragmentError::TooLarge(MAX_FRAME + 1))
        );
    }
}
