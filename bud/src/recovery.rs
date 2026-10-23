//! What happens when parity was not enough: defence in depth for loss.
//!
//! Three layers, each cheaper to rely on because the next one is behind it:
//!
//! 1. **Parity** ([`crate::parity`]) recovers a loss with no round trip.
//!    Because the layers below exist, [`crate::AdaptiveParity::layered`] can
//!    cover only the expected loss instead of twice that.
//! 2. **Retransmission.** A frame still missing fragments after a short hold
//!    gets a [`Feedback::Nack`] for them, if a resend can still arrive before
//!    the frame is no longer worth showing. The sender's [`Retransmitter`]
//!    keeps recent frames for this and answers with the fragments again, each
//!    resent a bounded number of times.
//! 3. **Refresh.** A frame that missed its deadline is given up on, and the
//!    receiver sends [`Feedback::Refresh`] so the encoder can send a picture
//!    that does not depend on the lost one (a keyframe, or references that
//!    skip it). It is rate-limited, because a refresh is expensive.
//!
//! Nothing here has a clock or a socket: the caller passes the time in
//! microseconds and carries [`Feedback`] as the payload of ordinary sealed
//! datagrams on [`FEEDBACK_STREAM`]. The receiver's loss figures for
//! [`crate::AdaptiveParity`] come from a [`LossMeter`], also as feedback.

use std::collections::{HashMap, VecDeque};

use crate::fragment::Reassembler;
use crate::header::Header;
use crate::packet::Opener;

/// The stream number feedback travels on; no media stream uses it.
pub const FEEDBACK_STREAM: u16 = u16::MAX;
/// How many fragments one NACK may name, so a peer cannot make the sender
/// resend a whole frame many times over by asking.
pub const MAX_NACK_FRAGMENTS: usize = 4096;
/// How many ranges a NACK is encoded as at most.
const MAX_RANGES: usize = 64;
/// How many times a fragment is resent.
pub const MAX_RESENDS: u8 = 2;
/// How many NACKs one frame gets.
pub const MAX_NACKS: u8 = 2;
/// The most the [`Retransmitter`] holds on to.
pub const MAX_STORED_BYTES: usize = 8 * 1024 * 1024;

/// A message from the receiving side to the sending side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Feedback {
    /// These data fragments of the frame are wanted again.
    Nack {
        stream: u16,
        frame: u32,
        fragments: Vec<u16>,
    },
    /// The receiver gave up on this frame: send a picture that does not need it.
    Refresh { stream: u16, frame: u32 },
    /// Over the last period `sent` packets were numbered and `lost` of them
    /// did not arrive: the figures [`crate::AdaptiveParity::observe`] takes.
    Report { sent: u32, lost: u32 },
}

const NACK: u8 = 1;
const REFRESH: u8 = 2;
const REPORT: u8 = 3;

impl Feedback {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Feedback::Nack {
                stream,
                frame,
                fragments,
            } => {
                let mut sorted = fragments.clone();
                sorted.sort_unstable();
                sorted.dedup();
                // Runs of consecutive fragments as (start, length).
                let mut ranges: Vec<(u16, u16)> = Vec::new();
                for f in sorted {
                    match ranges.last_mut() {
                        Some((start, len))
                            if u32::from(*start) + u32::from(*len) == u32::from(f) =>
                        {
                            *len += 1
                        }
                        _ => ranges.push((f, 1)),
                    }
                }
                ranges.truncate(MAX_RANGES);
                let mut out = vec![NACK];
                out.extend_from_slice(&stream.to_be_bytes());
                out.extend_from_slice(&frame.to_be_bytes());
                out.extend_from_slice(&(ranges.len() as u16).to_be_bytes());
                for (start, len) in ranges {
                    out.extend_from_slice(&start.to_be_bytes());
                    out.extend_from_slice(&len.to_be_bytes());
                }
                out
            }
            Feedback::Refresh { stream, frame } => {
                let mut out = vec![REFRESH];
                out.extend_from_slice(&stream.to_be_bytes());
                out.extend_from_slice(&frame.to_be_bytes());
                out
            }
            Feedback::Report { sent, lost } => {
                let mut out = vec![REPORT];
                out.extend_from_slice(&sent.to_be_bytes());
                out.extend_from_slice(&lost.to_be_bytes());
                out
            }
        }
    }

    /// Reads feedback from a datagram's payload. `None` for anything that is
    /// not well formed, including a NACK that asks for more than
    /// [`MAX_NACK_FRAGMENTS`].
    pub fn decode(bytes: &[u8]) -> Option<Feedback> {
        let be16 = |at: usize| Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]));
        let be32 = |at: usize| {
            Some(u32::from_be_bytes([
                *bytes.get(at)?,
                *bytes.get(at + 1)?,
                *bytes.get(at + 2)?,
                *bytes.get(at + 3)?,
            ]))
        };
        match *bytes.first()? {
            NACK => {
                let stream = be16(1)?;
                let frame = be32(3)?;
                let count = usize::from(be16(7)?);
                if count > MAX_RANGES || bytes.len() != 9 + count * 4 {
                    return None;
                }
                let mut fragments = Vec::new();
                for i in 0..count {
                    let start = be16(9 + i * 4)?;
                    let len = be16(11 + i * 4)?;
                    if len == 0 || u32::from(start) + u32::from(len) > u32::from(u16::MAX) + 1 {
                        return None;
                    }
                    if fragments.len() + usize::from(len) > MAX_NACK_FRAGMENTS {
                        return None;
                    }
                    // Counted in 32 bits: the last range may end at 65536.
                    fragments.extend(
                        (u32::from(start)..u32::from(start) + u32::from(len)).map(|f| f as u16),
                    );
                }
                Some(Feedback::Nack {
                    stream,
                    frame,
                    fragments,
                })
            }
            REFRESH if bytes.len() == 7 => Some(Feedback::Refresh {
                stream: be16(1)?,
                frame: be32(3)?,
            }),
            REPORT if bytes.len() == 9 => Some(Feedback::Report {
                sent: be32(1)?,
                lost: be32(5)?,
            }),
            _ => None,
        }
    }
}

/// The sending side's memory of recent frames, so a NACK can be answered.
#[derive(Default)]
pub struct Retransmitter {
    frames: VecDeque<Stored>,
    bytes: usize,
}

struct Stored {
    stream: u16,
    frame: u32,
    parts: Vec<(Header, Vec<u8>)>,
    resends: Vec<u8>,
}

impl Retransmitter {
    pub fn new() -> Self {
        Retransmitter::default()
    }

    /// Remembers a frame's data fragments (the parity is not kept: a resend
    /// is of what was lost). The oldest frames go first when the memory is
    /// full.
    pub fn remember(&mut self, stream: u16, frame: u32, packets: &[(Header, Vec<u8>)]) {
        let parts: Vec<(Header, Vec<u8>)> = packets
            .iter()
            .filter(|(h, _)| h.flags & crate::header::FLAG_PARITY == 0)
            .cloned()
            .collect();
        let size: usize = parts.iter().map(|(_, p)| p.len()).sum();
        if size > MAX_STORED_BYTES {
            return;
        }
        self.bytes += size;
        self.frames.push_back(Stored {
            stream,
            frame,
            resends: vec![0; parts.len()],
            parts,
        });
        while self.bytes > MAX_STORED_BYTES {
            if let Some(old) = self.frames.pop_front() {
                self.bytes -= old.parts.iter().map(|(_, p)| p.len()).sum::<usize>();
            } else {
                break;
            }
        }
    }

    /// The fragments to send again for a NACK: those still remembered and not
    /// yet resent [`MAX_RESENDS`] times. The caller seals them afresh, so each
    /// goes out under a new packet number.
    pub fn respond(
        &mut self,
        stream: u16,
        frame: u32,
        fragments: &[u16],
    ) -> Vec<(Header, Vec<u8>)> {
        let Some(stored) = self
            .frames
            .iter_mut()
            .find(|s| s.stream == stream && s.frame == frame)
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &index in fragments {
            let index = usize::from(index);
            if let (Some(part), Some(count)) =
                (stored.parts.get(index), stored.resends.get_mut(index))
            {
                if *count < MAX_RESENDS {
                    *count += 1;
                    out.push(part.clone());
                }
            }
        }
        out
    }

    pub fn stored_bytes(&self) -> usize {
        self.bytes
    }
}

/// How long the receiver is willing to wait, in microseconds.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The round trip to the sender.
    pub rtt_us: u64,
    /// How long a frame may be incomplete before it counts as having a loss
    /// at all: at least the time the sender spreads a frame's packets over
    /// (pacing) and some jitter, or a frame still arriving is NACKed.
    pub hold_us: u64,
    /// How old an incomplete frame may get before it is given up on.
    pub deadline_us: u64,
    /// The least time between two refresh requests for one stream.
    pub refresh_gap_us: u64,
}

struct Seen {
    first_us: u64,
    nacks: u8,
    last_nack_us: u64,
}

/// The receiving side's decisions about frames that are still incomplete.
#[derive(Default)]
pub struct LossDetector {
    seen: HashMap<(u16, u32), Seen>,
    last_refresh: HashMap<u16, u64>,
}

impl LossDetector {
    pub fn new() -> Self {
        LossDetector::default()
    }

    /// Looks at the reassembler's waiting frames at time `now_us`, asks for
    /// what is worth asking for, gives up on what is not (freeing it in the
    /// reassembler), and returns the feedback to send.
    pub fn poll(
        &mut self,
        now_us: u64,
        timing: Timing,
        reassembler: &mut Reassembler,
    ) -> Vec<Feedback> {
        let waiting = reassembler.incomplete();
        // Forget frames that completed or were given up on.
        self.seen
            .retain(|key, _| waiting.iter().any(|w| (w.stream, w.frame) == *key));
        let mut out = Vec::new();
        for w in waiting {
            let seen = self.seen.entry((w.stream, w.frame)).or_insert(Seen {
                first_us: now_us,
                nacks: 0,
                last_nack_us: 0,
            });
            let age = now_us.saturating_sub(seen.first_us);
            if age >= timing.deadline_us {
                reassembler.abandon(w.stream, w.frame);
                self.seen.remove(&(w.stream, w.frame));
                let due = self
                    .last_refresh
                    .get(&w.stream)
                    .is_none_or(|last| now_us.saturating_sub(*last) >= timing.refresh_gap_us);
                if due {
                    self.last_refresh.insert(w.stream, now_us);
                    out.push(Feedback::Refresh {
                        stream: w.stream,
                        frame: w.frame,
                    });
                }
                continue;
            }
            let spaced = seen.nacks == 0
                || now_us.saturating_sub(seen.last_nack_us) >= timing.rtt_us * 3 / 2;
            // A resend takes about a round trip: worth asking only if it can
            // still arrive in time.
            let in_time = age + timing.rtt_us <= timing.deadline_us;
            if age >= timing.hold_us && seen.nacks < MAX_NACKS && spaced && in_time {
                seen.nacks += 1;
                seen.last_nack_us = now_us;
                out.push(Feedback::Nack {
                    stream: w.stream,
                    frame: w.frame,
                    fragments: w.missing,
                });
            }
        }
        out
    }
}

/// Turns an [`Opener`]'s counts into loss reports: packets numbered since the
/// last sample, and how many of them have not arrived.
#[derive(Default)]
pub struct LossMeter {
    last_highest: Option<u64>,
    last_accepted: u64,
}

impl LossMeter {
    pub fn new() -> Self {
        LossMeter::default()
    }

    /// `Some(Feedback::Report)` for the period since the last sample, or
    /// `None` if nothing new was numbered. A packet that is only late counts as
    /// lost until it arrives, so the figure runs a little high on a network
    /// that reorders.
    pub fn sample(&mut self, opener: &Opener) -> Option<Feedback> {
        let (highest, accepted) = opener.stats();
        let highest = highest?;
        let sent = match self.last_highest {
            Some(last) => highest.saturating_sub(last),
            None => highest + 1,
        };
        let received = accepted.saturating_sub(self.last_accepted);
        self.last_highest = Some(highest);
        self.last_accepted = accepted;
        if sent == 0 {
            return None;
        }
        let sent = sent.min(u64::from(u32::MAX)) as u32;
        let lost = (sent as u64).saturating_sub(received) as u32;
        Some(Feedback::Report { sent, lost })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::{fragment, fragment_with_parity, Reassembler};
    use crate::packet::Sealer;
    use crate::parity::{AdaptiveParity, NoParity, ParityPolicy, FRAGMENT_SIZE};

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 31 % 251) as u8).collect()
    }

    // --- feedback on the wire ---

    #[test]
    fn feedback_round_trips() {
        let cases = [
            Feedback::Nack {
                stream: 3,
                frame: 0xdead_beef,
                fragments: vec![0, 1, 2, 7, 9, 10, 65535],
            },
            Feedback::Refresh {
                stream: 1,
                frame: 9,
            },
            Feedback::Report {
                sent: 1000,
                lost: 12,
            },
        ];
        for original in cases {
            assert_eq!(
                Feedback::decode(&original.encode()),
                Some(original.clone()),
                "{original:?}"
            );
        }
    }

    #[test]
    fn a_nack_sends_runs_of_fragments_as_ranges() {
        let nack = Feedback::Nack {
            stream: 1,
            frame: 1,
            fragments: vec![5, 3, 4, 4, 5, 20],
        };
        let bytes = nack.encode();
        // tag + stream + frame + count, then two (start, length) ranges.
        assert_eq!(bytes.len(), 9 + 2 * 4);
        assert_eq!(
            Feedback::decode(&bytes),
            Some(Feedback::Nack {
                stream: 1,
                frame: 1,
                fragments: vec![3, 4, 5, 20]
            })
        );
    }

    #[test]
    fn malformed_feedback_is_refused_and_never_panics() {
        assert_eq!(Feedback::decode(&[]), None);
        assert_eq!(Feedback::decode(&[9, 0, 0]), None);
        assert_eq!(Feedback::decode(&[REFRESH, 0, 1]), None);
        assert_eq!(Feedback::decode(&[REPORT, 0, 0, 0, 1]), None);
        // A NACK whose count does not match its length, with a zero-length
        // range, running off the end of the numbers, or asking for too much.
        let nack = |ranges: &[(u16, u16)]| {
            let mut v = vec![NACK, 0, 1, 0, 0, 0, 1];
            v.extend_from_slice(&(ranges.len() as u16).to_be_bytes());
            for (s, l) in ranges {
                v.extend_from_slice(&s.to_be_bytes());
                v.extend_from_slice(&l.to_be_bytes());
            }
            v
        };
        assert!(Feedback::decode(&nack(&[(0, 1)])).is_some());
        assert_eq!(Feedback::decode(&nack(&[(0, 0)])), None);
        assert_eq!(Feedback::decode(&nack(&[(65535, 2)])), None);
        assert_eq!(Feedback::decode(&nack(&[(0, 5000)])), None);
        let mut short = nack(&[(0, 1), (4, 1)]);
        short.pop();
        assert_eq!(Feedback::decode(&short), None);
        let too_many: Vec<(u16, u16)> = (0..65).map(|i| (i * 2, 1)).collect();
        assert_eq!(Feedback::decode(&nack(&too_many)), None);
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        for len in 0..80 {
            let mut buf = vec![0u8; len];
            for byte in buf.iter_mut() {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *byte = x as u8;
            }
            let _ = Feedback::decode(&buf);
            if len > 0 {
                buf[0] = NACK;
                let _ = Feedback::decode(&buf);
            }
        }
    }

    // --- the sender's memory ---

    fn owned(data: &[u8]) -> Vec<(Header, Vec<u8>)> {
        fragment(1, 1, 0, data)
            .unwrap()
            .into_iter()
            .map(|(h, p)| (h, p.to_vec()))
            .collect()
    }

    #[test]
    fn a_nack_is_answered_with_the_fragments_asked_for() {
        let data = bytes(FRAGMENT_SIZE * 5);
        let packets = owned(&data);
        let mut r = Retransmitter::new();
        r.remember(1, 1, &packets);
        let again = r.respond(1, 1, &[1, 3]);
        assert_eq!(again.len(), 2);
        assert_eq!(again[0], packets[1]);
        assert_eq!(again[1], packets[3]);
        // A fragment that does not exist, and a frame that is not remembered.
        assert!(r.respond(1, 1, &[9]).is_empty());
        assert!(r.respond(1, 2, &[0]).is_empty());
        assert!(r.respond(2, 1, &[0]).is_empty());
    }

    #[test]
    fn a_fragment_is_resent_only_so_many_times() {
        let packets = owned(&bytes(FRAGMENT_SIZE * 2));
        let mut r = Retransmitter::new();
        r.remember(1, 1, &packets);
        for _ in 0..MAX_RESENDS {
            assert_eq!(r.respond(1, 1, &[0]).len(), 1);
        }
        assert!(r.respond(1, 1, &[0]).is_empty());
        // The other fragment still has its resends.
        assert_eq!(r.respond(1, 1, &[1]).len(), 1);
    }

    #[test]
    fn parity_is_not_kept_and_old_frames_make_room() {
        struct Two;
        impl ParityPolicy for Two {
            fn parity_for(&self, _data: usize) -> usize {
                2
            }
        }
        let with_parity = fragment_with_parity(1, 1, 0, &bytes(FRAGMENT_SIZE * 3), &Two).unwrap();
        assert_eq!(with_parity.len(), 5);
        let mut r = Retransmitter::new();
        r.remember(1, 1, &with_parity);
        assert_eq!(r.stored_bytes(), FRAGMENT_SIZE * 3);
        assert!(
            r.respond(1, 1, &[3]).is_empty(),
            "fragment 3 would be parity"
        );

        // Filling the memory pushes the oldest frame out first.
        let big = bytes(MAX_STORED_BYTES / 2 + 1);
        r.remember(1, 2, &owned_frame(2, &big));
        r.remember(1, 3, &owned_frame(3, &big));
        assert!(r.stored_bytes() <= MAX_STORED_BYTES);
        assert!(r.respond(1, 1, &[0]).is_empty(), "frame 1 was evicted");
        assert!(r.respond(1, 2, &[0]).is_empty(), "frame 2 was evicted");
        assert_eq!(r.respond(1, 3, &[0]).len(), 1);
    }

    fn owned_frame(frame: u32, data: &[u8]) -> Vec<(Header, Vec<u8>)> {
        fragment(1, frame, 0, data)
            .unwrap()
            .into_iter()
            .map(|(h, p)| (h, p.to_vec()))
            .collect()
    }

    // --- the receiver's decisions ---

    const TIMING: Timing = Timing {
        rtt_us: 4_000,
        hold_us: 3_000,
        deadline_us: 30_000,
        refresh_gap_us: 250_000,
    };

    /// A reassembler holding frame 1 with fragment 2 of 4 missing.
    fn one_missing() -> Reassembler {
        let data = bytes(FRAGMENT_SIZE * 4);
        let parts = fragment(1, 1, 0, &data).unwrap();
        let mut r = Reassembler::new();
        for (i, (h, p)) in parts.iter().enumerate() {
            if i != 2 {
                r.push(h, p).unwrap();
            }
        }
        r
    }

    #[test]
    fn a_nack_waits_for_the_hold_then_repeats_no_faster_than_the_round_trip() {
        let mut r = one_missing();
        let mut d = LossDetector::new();
        assert!(d.poll(0, TIMING, &mut r).is_empty(), "first seen");
        assert!(d.poll(2_999, TIMING, &mut r).is_empty(), "still holding");
        let nack = d.poll(3_000, TIMING, &mut r);
        assert_eq!(
            nack,
            vec![Feedback::Nack {
                stream: 1,
                frame: 1,
                fragments: vec![2]
            }]
        );
        assert!(
            d.poll(5_000, TIMING, &mut r).is_empty(),
            "less than 1.5 round trips"
        );
        assert_eq!(d.poll(9_000, TIMING, &mut r).len(), 1, "second NACK");
        assert!(d.poll(20_000, TIMING, &mut r).is_empty(), "no third NACK");
    }

    #[test]
    fn a_frame_that_misses_its_deadline_is_given_up_with_one_refresh() {
        let mut r = one_missing();
        let mut d = LossDetector::new();
        d.poll(0, TIMING, &mut r);
        let out = d.poll(30_000, TIMING, &mut r);
        assert_eq!(
            out,
            vec![Feedback::Refresh {
                stream: 1,
                frame: 1
            }]
        );
        assert!(r.incomplete().is_empty(), "the frame was abandoned");
        assert_eq!(r.dropped(), 1);
        assert!(d.poll(40_000, TIMING, &mut r).is_empty());
    }

    #[test]
    fn no_nack_when_a_resend_could_not_arrive_in_time() {
        let mut r = one_missing();
        let mut d = LossDetector::new();
        let tight = Timing {
            deadline_us: 6_000,
            ..TIMING
        };
        d.poll(0, tight, &mut r);
        // At 3 ms the hold is over, but 3 ms + a 4 ms round trip is past 6 ms.
        assert!(d.poll(3_000, tight, &mut r).is_empty());
        assert_eq!(
            d.poll(6_000, tight, &mut r),
            vec![Feedback::Refresh {
                stream: 1,
                frame: 1
            }]
        );
    }

    #[test]
    fn refreshes_are_rate_limited_per_stream() {
        let mut r = Reassembler::new();
        let mut d = LossDetector::new();
        for frame in 1..=3u32 {
            let data = bytes(FRAGMENT_SIZE * 2);
            let parts = fragment(1, frame, 0, &data).unwrap();
            r.push(&parts[0].0, parts[0].1).unwrap();
        }
        d.poll(0, TIMING, &mut r);
        let out = d.poll(30_000, TIMING, &mut r);
        let refreshes = out
            .iter()
            .filter(|f| matches!(f, Feedback::Refresh { .. }))
            .count();
        assert_eq!(refreshes, 1, "three frames given up, one refresh asked for");
        assert!(r.incomplete().is_empty());
    }

    #[test]
    fn a_frame_that_completes_in_time_is_forgotten_without_feedback() {
        let data = bytes(FRAGMENT_SIZE * 4);
        let parts = fragment(1, 1, 0, &data).unwrap();
        let mut r = Reassembler::new();
        let mut d = LossDetector::new();
        for (i, (h, p)) in parts.iter().enumerate() {
            if i != 2 {
                r.push(h, p).unwrap();
            }
        }
        d.poll(0, TIMING, &mut r);
        r.push(&parts[2].0, parts[2].1).unwrap();
        assert!(d.poll(3_000, TIMING, &mut r).is_empty());
        assert!(d.poll(100_000, TIMING, &mut r).is_empty());
    }

    // --- the loss meter ---

    #[test]
    fn the_meter_reports_what_the_network_lost_since_the_last_sample() {
        let key = [5u8; 32];
        let mut sealer = Sealer::new(&key);
        let mut opener = Opener::new(&key);
        let mut meter = LossMeter::new();
        assert_eq!(meter.sample(&opener), None, "nothing received yet");
        let header = Header {
            flags: 0,
            stream: 1,
            frame: 0,
            fragment: 0,
            fragments: 1,
            packet: 0,
            sent_us: 0,
        };
        for i in 0..100 {
            let d = sealer.seal(header, b"x").unwrap();
            // Packets 5, 15, ... 95 are lost.
            if i % 10 != 5 {
                opener.open(&d).unwrap();
            }
        }
        assert_eq!(
            meter.sample(&opener),
            Some(Feedback::Report {
                sent: 100,
                lost: 10
            })
        );
        assert_eq!(meter.sample(&opener), None, "nothing new since");
        for _ in 0..50 {
            let d = sealer.seal(header, b"x").unwrap();
            opener.open(&d).unwrap();
        }
        assert_eq!(
            meter.sample(&opener),
            Some(Feedback::Report { sent: 50, lost: 0 })
        );
    }

    // --- all of it, on a lossy link ---

    struct Outcome {
        delivered: u32,
        refreshes: u32,
        packets: u32,
        frames: u32,
    }

    /// 60 frames per second of 20 fragments each over a link that loses
    /// `loss_percent` of packets at random and takes 2 ms each way. Feedback
    /// is not lost, which flatters the result a little.
    fn simulate(loss_percent: u64, layered: bool, recovery: bool, frames: u32) -> Outcome {
        let key = [9u8; 32];
        let mut sealer = Sealer::new(&key);
        let mut opener = Opener::new(&key);
        let mut reassembler = Reassembler::new();
        let mut detector = LossDetector::new();
        let mut meter = LossMeter::new();
        let mut retransmitter = Retransmitter::new();
        let mut adaptive = AdaptiveParity::layered();
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut roll = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % 100
        };
        let mut to_receiver: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut to_sender: Vec<(u64, Feedback)> = Vec::new();
        let mut out = Outcome {
            delivered: 0,
            refreshes: 0,
            packets: 0,
            frames,
        };
        let expected = bytes(FRAGMENT_SIZE * 20 - 100);
        let mut next_frame = 0u32;
        let end = u64::from(frames) * 16_667 + 200_000;
        let mut t = 0u64;
        while t < end {
            if next_frame < frames && t >= u64::from(next_frame) * 16_667 {
                let packets = if layered {
                    fragment_with_parity(1, next_frame, t as u32, &expected, &adaptive)
                } else {
                    fragment_with_parity(1, next_frame, t as u32, &expected, &NoParity)
                }
                .unwrap();
                retransmitter.remember(1, next_frame, &packets);
                for (h, p) in packets {
                    let d = sealer.seal(h, &p).unwrap();
                    out.packets += 1;
                    if roll() >= loss_percent {
                        to_receiver.push((t + 2_000, d));
                    }
                }
                next_frame += 1;
            }
            // The sender reads feedback.
            let (ready, later): (Vec<_>, Vec<_>) =
                to_sender.drain(..).partition(|(at, _)| *at <= t);
            to_sender = later;
            for (_, f) in ready {
                match f {
                    Feedback::Nack {
                        stream,
                        frame,
                        fragments,
                    } => {
                        for (h, p) in retransmitter.respond(stream, frame, &fragments) {
                            let d = sealer.seal(h, &p).unwrap();
                            out.packets += 1;
                            if roll() >= loss_percent {
                                to_receiver.push((t + 2_000, d));
                            }
                        }
                    }
                    Feedback::Refresh { .. } => out.refreshes += 1,
                    Feedback::Report { sent, lost } => adaptive.observe(sent, lost),
                }
            }
            // The receiver reads packets.
            let (ready, later): (Vec<_>, Vec<_>) =
                to_receiver.drain(..).partition(|(at, _)| *at <= t);
            to_receiver = later;
            for (_, d) in ready {
                let (h, p) = opener.open(&d).unwrap();
                if let Some(frame) = reassembler.push(&h, &p).unwrap() {
                    assert_eq!(frame.data, expected);
                    out.delivered += 1;
                }
            }
            if recovery {
                for f in detector.poll(t, TIMING, &mut reassembler) {
                    to_sender.push((t + 2_000, f));
                }
            }
            if t.is_multiple_of(100_000) {
                if let Some(report) = meter.sample(&opener) {
                    to_sender.push((t + 2_000, report));
                }
            }
            t += 1_000;
        }
        out
    }

    #[test]
    fn the_layers_together_beat_no_defence_on_a_lossy_link() {
        let none = simulate(8, false, false, 300);
        let layered = simulate(8, true, true, 300);
        let overhead = 100.0 * (f64::from(layered.packets) / f64::from(none.packets) - 1.0);
        println!(
            "8% loss, 300 frames of 20 fragments: no defence delivered {} ({} packets); \
             parity + NACK + refresh delivered {} ({} packets, {overhead:.0}% more), {} refreshes",
            none.delivered, none.packets, layered.delivered, layered.packets, layered.refreshes
        );
        assert!(
            none.delivered < none.frames / 2,
            "the baseline should suffer: {}",
            none.delivered
        );
        assert!(
            layered.delivered * 100 >= layered.frames * 95,
            "layered delivered only {} of {}",
            layered.delivered,
            layered.frames
        );
        assert!(layered.delivered > none.delivered);
    }

    #[test]
    fn each_layer_earns_its_place() {
        for loss in [3u64, 8, 15, 25] {
            let none = simulate(loss, false, false, 100);
            let parity = simulate(loss, true, false, 100);
            let nack = simulate(loss, false, true, 100);
            let all = simulate(loss, true, true, 100);
            println!(
                "{loss:>2}% loss, of {} frames delivered: none {:>3}, parity only {:>3}, \
                 NACK + refresh only {:>3}, all three {:>3} ({} refreshes, {} packets vs {})",
                none.frames,
                none.delivered,
                parity.delivered,
                nack.delivered,
                all.delivered,
                all.refreshes,
                all.packets,
                none.packets
            );
            assert!(
                all.delivered >= parity.delivered,
                "{loss}%: NACKs must not hurt"
            );
            assert!(
                all.delivered >= nack.delivered,
                "{loss}%: parity must not hurt"
            );
            assert!(all.delivered > none.delivered);
        }
    }

    #[test]
    fn with_no_loss_the_layers_cost_nothing_and_ask_for_nothing() {
        let r = simulate(0, true, true, 120);
        assert_eq!(r.delivered, 120);
        assert_eq!(r.refreshes, 0);
        assert_eq!(r.packets, 120 * 20, "no parity, no resends");
    }
}
