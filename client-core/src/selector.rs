//! The automatic carrier selector (docs/BACKENDS.md, "When a carrier
//! switches, and why it does not flap"; Droidtop/tracker#457 step 3).
//!
//! For each window this client shows, it scores the carriers it could use
//! from what it sees: how much of the time the window changes (pictures or
//! frames arriving per second against the frame rate a moving window
//! sends), what the host says the window shows (text or not), and whether
//! RDP can reach the host at all. It switches only when another carrier
//! has scored better than the current one by a margin for a while, never
//! more often than a minimum interval, and it backs off from a carrier
//! whose switch failed. A window the user pinned (a rule of theirs for its
//! app, or a switch by hand) is left alone.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windowcast_protocol::{BackendKind, ContentHint};

/// How much better another carrier must score.
pub const MARGIN: f32 = 1.25;
/// How long it must stay better.
pub const DWELL: Duration = Duration::from_secs(3);
/// The least time between two switches of one window.
pub const MIN_INTERVAL: Duration = Duration::from_secs(15);
/// The first wait after a failed switch to a carrier; doubled each time.
pub const BACKOFF: Duration = Duration::from_secs(30);
pub const MAX_BACKOFF: Duration = Duration::from_secs(300);
/// Pictures or frames per second a window that moves all the time sends.
pub const MOVING_FPS: f32 = 24.0;
/// How quickly the motion estimate follows (seconds of history it weighs).
const MOTION_SECONDS: f32 = 2.0;

/// What the selector knows of one window.
#[derive(Debug)]
pub struct Window {
    /// The share of time it changes, 0 to 1, smoothed.
    pub motion: f32,
    count: Option<(u64, Instant)>,
    /// The carrier that has been scoring better, and since when.
    better: Option<(BackendKind, Instant)>,
    last_switch: Instant,
    /// Carriers not to try again before the instant, with the next wait.
    backoff: HashMap<BackendKind, (Instant, Duration)>,
    /// The user chose: no automatic switch.
    pub pinned: bool,
    /// An automatic switch under way: its generation and carrier.
    pub pending: Option<(u32, BackendKind)>,
}

impl Window {
    pub fn new(now: Instant) -> Self {
        Window {
            motion: 0.0,
            count: None,
            better: None,
            last_switch: now,
            backoff: HashMap::new(),
            pinned: false,
            pending: None,
        }
    }

    /// Takes the window's running count of pictures or frames received.
    pub fn arrived(&mut self, count: u64, now: Instant) {
        if let Some((then, at)) = self.count {
            let seconds = now.duration_since(at).as_secs_f32();
            if seconds > 0.0 && count >= then {
                let rate = (count - then) as f32 / seconds;
                let sample = (rate / MOVING_FPS).clamp(0.0, 1.0);
                let weight = (seconds / MOTION_SECONDS).clamp(0.0, 1.0);
                self.motion += (sample - self.motion) * weight;
            }
        }
        self.count = Some((count, now));
    }

    /// A switch to `to` happened: the interval starts, and the count
    /// starts over on the new carrier.
    pub fn switched(&mut self, to: BackendKind, now: Instant) {
        self.last_switch = now;
        self.better = None;
        self.count = None;
        self.backoff.remove(&to);
    }

    /// A switch to `to` failed: wait before trying it again.
    pub fn failed(&mut self, to: BackendKind, now: Instant) {
        let wait = self
            .backoff
            .get(&to)
            .map_or(BACKOFF, |(_, wait)| (*wait * 2).min(MAX_BACKOFF));
        self.backoff.insert(to, (now + wait, wait));
        self.better = None;
        self.last_switch = now;
    }

    /// The carrier to switch to now, if any.
    pub fn choose(
        &mut self,
        current: BackendKind,
        content: ContentHint,
        rdp_possible: bool,
        now: Instant,
    ) -> Option<BackendKind> {
        if self.pinned {
            return None;
        }
        let candidates = [BackendKind::Native, BackendKind::Rdp];
        let score = |kind: BackendKind| score(kind, content, self.motion, rdp_possible);
        let mine = score(current);
        let best = candidates
            .into_iter()
            .filter(|k| *k != current)
            .filter(|k| self.backoff.get(k).is_none_or(|(until, _)| now >= *until))
            .map(|k| (k, score(k)))
            .filter(|(_, s)| *s > mine * MARGIN && *s > 0.0)
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(k, _)| k);
        match (best, self.better) {
            (None, _) => {
                self.better = None;
                None
            }
            (Some(kind), Some((was, since))) if was == kind => {
                if now.duration_since(since) >= DWELL
                    && now.duration_since(self.last_switch) >= MIN_INTERVAL
                {
                    Some(kind)
                } else {
                    None
                }
            }
            (Some(kind), _) => {
                self.better = Some((kind, now));
                None
            }
        }
    }
}

/// How well `kind` suits a window. Video (Native) wins when the window
/// moves; RDP pictures win for still text, sharp and cheap, and only for
/// text unless the window is almost still. Carriers that cannot run score
/// nothing.
pub fn score(kind: BackendKind, content: ContentHint, motion: f32, rdp_possible: bool) -> f32 {
    match kind {
        BackendKind::Native => 0.2 + motion,
        BackendKind::Rdp if !rdp_possible => 0.0,
        BackendKind::Rdp => match content {
            ContentHint::Text => 1.2 - motion,
            ContentHint::General => 0.3 - motion,
            ContentHint::Game | ContentHint::Video => 0.0,
        },
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `rate` arrivals a second for `seconds`, choosing each second.
    fn run(
        w: &mut Window,
        start: Instant,
        (from, to): (u64, u64),
        rate: u64,
        current: BackendKind,
        content: ContentHint,
    ) -> Vec<(u64, BackendKind)> {
        let mut out = Vec::new();
        let mut count = 0;
        for second in from..to {
            let now = start + Duration::from_secs(second);
            count += rate;
            w.arrived(count, now);
            if let Some(kind) = w.choose(current, content, true, now) {
                out.push((second, kind));
            }
        }
        out
    }

    #[test]
    fn still_text_goes_to_rdp_after_the_dwell_and_the_interval() {
        let start = Instant::now();
        let mut w = Window::new(start);
        let chosen = run(
            &mut w,
            start,
            (0, 30),
            1,
            BackendKind::Native,
            ContentHint::Text,
        );
        // Never before the minimum interval since the stream started.
        let first = chosen.first().expect("a switch");
        assert_eq!(first.1, BackendKind::Rdp);
        assert!(first.0 >= MIN_INTERVAL.as_secs(), "{chosen:?}");
    }

    #[test]
    fn moving_text_stays_video_and_a_still_general_window_stays_too() {
        let start = Instant::now();
        let mut w = Window::new(start);
        assert!(run(
            &mut w,
            start,
            (0, 60),
            30,
            BackendKind::Native,
            ContentHint::Text
        )
        .is_empty());
        let mut w = Window::new(start);
        assert!(run(
            &mut w,
            start,
            (0, 60),
            1,
            BackendKind::Native,
            ContentHint::General
        )
        .is_empty());
    }

    #[test]
    fn rdp_text_that_starts_playing_video_goes_back_to_video() {
        let start = Instant::now();
        let mut w = Window::new(start);
        assert!(run(
            &mut w,
            start,
            (0, 20),
            1,
            BackendKind::Rdp,
            ContentHint::Text
        )
        .is_empty());
        let chosen = run(
            &mut w,
            start,
            (20, 40),
            30,
            BackendKind::Rdp,
            ContentHint::Text,
        );
        assert_eq!(
            chosen.first().map(|c| c.1),
            Some(BackendKind::Native),
            "{chosen:?}"
        );
    }

    #[test]
    fn a_flickering_window_does_not_flap() {
        // Alternating still and moving every two seconds: below the dwell,
        // so nothing switches.
        let start = Instant::now();
        let mut w = Window::new(start);
        let mut count = 0;
        let mut switches = 0;
        for second in 0..120u64 {
            let now = start + Duration::from_secs(second);
            count += if (second / 2) % 2 == 0 { 30 } else { 1 };
            w.arrived(count, now);
            if w.choose(BackendKind::Native, ContentHint::Text, true, now)
                .is_some()
            {
                switches += 1;
            }
        }
        assert_eq!(switches, 0);
    }

    #[test]
    fn a_failed_carrier_waits_longer_each_time_and_a_pin_stops_everything() {
        let start = Instant::now();
        let mut w = Window::new(start);
        w.failed(BackendKind::Rdp, start);
        assert_eq!(w.backoff[&BackendKind::Rdp].1, BACKOFF);
        w.failed(BackendKind::Rdp, start);
        assert_eq!(w.backoff[&BackendKind::Rdp].1, BACKOFF * 2);
        for _ in 0..10 {
            w.failed(BackendKind::Rdp, start);
        }
        assert_eq!(w.backoff[&BackendKind::Rdp].1, MAX_BACKOFF);
        let mut w = Window::new(start);
        w.pinned = true;
        assert!(run(
            &mut w,
            start,
            (0, 60),
            1,
            BackendKind::Native,
            ContentHint::Text
        )
        .is_empty());
    }

    #[test]
    fn rdp_out_of_reach_is_never_chosen() {
        let start = Instant::now();
        let mut w = Window::new(start);
        let mut count = 0;
        for second in 0..60u64 {
            let now = start + Duration::from_secs(second);
            count += 1;
            w.arrived(count, now);
            assert_eq!(
                w.choose(BackendKind::Native, ContentHint::Text, false, now),
                None
            );
        }
    }
}
