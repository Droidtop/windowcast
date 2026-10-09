//! Adaptive quality: each stream's bitrate, frame rate and picture scale
//! follow what the network carries, from the client's receiver reports
//! (packet loss) and the session's round trip, never above the host's own
//! settings or the client's limits for that stream.
//!
//! The rate is held as a target the encoder is kept under. Loss above
//! [`LOSS_HIGH`], or a round trip well above the best seen lately (a queue
//! filling somewhere), cuts the target to a fraction of what actually went
//! out; a clean network lets it grow a little each report, but only while
//! the encoder uses a fair share of what it is given (encoders undershoot,
//! OpenH264 by half on busy pictures), so a quiet window does not run the
//! target up to numbers never tested. Once the target passes what the
//! stream sent before anything held it, it is let go: the settings rule
//! again. When the rate left is thin for the
//! pixels being sent, the frame rate comes down to 30, then the picture to
//! three quarters and a half, then the frame rate to 20 and 15; with room
//! again they go back up in reverse order.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use windowcast_protocol::StreamLimits;

/// Above this fraction of packets lost the target is cut.
pub const LOSS_HIGH: f32 = 0.10;
/// Below this the network counts as clean.
pub const LOSS_LOW: f32 = 0.02;
/// The target is cut to this share of what went out.
const CUT: f64 = 0.7;
/// A round trip this much above the best of the last [`RTT_WINDOW`] cuts
/// by [`CUT_DELAY`] instead.
const RTT_SLACK: Duration = Duration::from_millis(80);
const CUT_DELAY: f64 = 0.85;
const RTT_WINDOW: Duration = Duration::from_secs(30);
/// Growth per clean report.
const GROW: f64 = 1.08;
/// Growth needs at least this share of the target sent.
const FILLED: f64 = 0.4;
/// The target is let go once it is this far above the unheld rate.
const RELEASE: f64 = 1.25;
/// The lowest target: a picture still moves at this.
pub const FLOOR_BPS: u32 = 150_000;
/// Bits per pixel per frame below which the picture is too thin and the
/// frame rate or size steps down, and above which (at the next step up)
/// it steps back up.
const THIN: f64 = 0.04;
const ROOMY: f64 = 0.09;
/// How long a condition must hold before a step down or up.
const STEP_DOWN_AFTER: Duration = Duration::from_secs(3);
const STEP_UP_AFTER: Duration = Duration::from_secs(10);

/// Frame rate and scale, from the best down.
const STEPS: [(u32, f32); 6] = [
    (u32::MAX, 1.0),
    (30, 1.0),
    (30, 0.75),
    (30, 0.5),
    (20, 0.5),
    (15, 0.5),
];

/// What a stream's encoder is held to. `None` and 1.0 leave the agent's
/// own settings in force.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quality {
    /// Bits a second.
    pub bitrate: Option<u32>,
    pub fps: Option<u32>,
    /// Of the window's own size, 0 to 1.
    pub scale: f32,
}

impl Default for Quality {
    fn default() -> Self {
        Quality {
            bitrate: None,
            fps: None,
            scale: 1.0,
        }
    }
}

/// What the controller decides from.
#[derive(Debug, Clone, Copy)]
pub struct Observation {
    /// Bits a second actually sent over the last second or so.
    pub sent_bps: u32,
    /// The window's own size (before any scaling).
    pub window: (u32, u32),
    /// The agent's frame rate setting.
    pub host_fps: u32,
    /// The client's latest loss report, if one came since the last
    /// observation.
    pub loss: Option<f32>,
    pub rtt: Option<Duration>,
}

pub struct Controller {
    limits: StreamLimits,
    target: Option<u32>,
    step: usize,
    rtts: VecDeque<(Instant, Duration)>,
    thin_since: Option<Instant>,
    roomy_since: Option<Instant>,
    last_cut: Option<Instant>,
    /// What the stream sends while nothing holds it, smoothed.
    unheld_bps: Option<f64>,
}

impl Controller {
    pub fn new(limits: StreamLimits) -> Self {
        Controller {
            limits,
            target: None,
            step: 0,
            rtts: VecDeque::new(),
            thin_since: None,
            roomy_since: None,
            last_cut: None,
            unheld_bps: None,
        }
    }

    pub fn set_limits(&mut self, limits: StreamLimits) {
        self.limits = limits;
    }

    fn cap_bps(&self) -> Option<u32> {
        self.limits.max_bitrate_kbps.map(|k| k.saturating_mul(1000))
    }

    /// The best round trip of the last half minute, with `rtt` added.
    fn baseline(&mut self, now: Instant, rtt: Duration) -> Duration {
        self.rtts.push_back((now, rtt));
        while self
            .rtts
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) > RTT_WINDOW)
        {
            self.rtts.pop_front();
        }
        self.rtts.iter().map(|(_, r)| *r).min().unwrap_or(rtt)
    }

    /// One step of the loop, about once a second.
    pub fn observe(&mut self, now: Instant, seen: Observation) -> Quality {
        let delayed = seen
            .rtt
            .is_some_and(|rtt| rtt > self.baseline(now, rtt) + RTT_SLACK);
        // At most one cut per round of reports, so one bad second is not
        // counted twice.
        let may_cut = self
            .last_cut
            .is_none_or(|at| now.duration_since(at) >= Duration::from_millis(900));
        let sent = seen.sent_bps.max(FLOOR_BPS);
        if self.target.is_none() {
            let now_bps = f64::from(seen.sent_bps);
            self.unheld_bps = Some(self.unheld_bps.map_or(now_bps, |b| 0.7 * b + 0.3 * now_bps));
        }
        let lossy = seen.loss.is_some_and(|loss| loss > LOSS_HIGH);
        if (lossy || delayed) && may_cut {
            let share = if lossy { CUT } else { CUT_DELAY };
            let from = self.target.map_or(sent, |t| t.min(sent));
            self.target = Some(((f64::from(from) * share).round() as u32).max(FLOOR_BPS));
            self.last_cut = Some(now);
        } else if let (Some(target), Some(loss)) = (self.target, seen.loss) {
            // Grow only while the encoder fills a fair share of what it
            // is given; let go past the unheld rate.
            if loss < LOSS_LOW && !delayed && f64::from(seen.sent_bps) > FILLED * f64::from(target)
            {
                let grown = f64::from(target) * GROW;
                self.target = match self.unheld_bps {
                    Some(unheld) if grown >= unheld * RELEASE => None,
                    _ => Some(grown.round() as u32),
                };
            }
        }
        if let (Some(target), Some(cap)) = (self.target, self.cap_bps()) {
            self.target = Some(target.min(cap));
        }

        // Frame rate and size, by how many bits each pixel gets.
        let budget = self.target.or(self.cap_bps());
        if let Some(budget) = budget {
            let bpp = |step: usize| {
                let (fps, scale) = self.step_at(step, seen.host_fps);
                let pixels = f64::from(seen.window.0)
                    * f64::from(seen.window.1)
                    * f64::from(scale)
                    * f64::from(scale);
                f64::from(budget) / (pixels * f64::from(fps)).max(1.0)
            };
            if bpp(self.step) < THIN && self.step + 1 < STEPS.len() {
                self.roomy_since = None;
                let since = *self.thin_since.get_or_insert(now);
                if now.duration_since(since) >= STEP_DOWN_AFTER {
                    self.step += 1;
                    self.thin_since = None;
                }
            } else if self.step > 0 && bpp(self.step - 1) > ROOMY && !lossy && !delayed {
                self.thin_since = None;
                let since = *self.roomy_since.get_or_insert(now);
                if now.duration_since(since) >= STEP_UP_AFTER {
                    self.step -= 1;
                    self.roomy_since = None;
                }
            } else {
                self.thin_since = None;
                self.roomy_since = None;
            }
        } else {
            self.step = 0;
        }
        self.quality(seen.host_fps, seen.window)
    }

    /// The frame rate and scale of `step`, within the limits.
    fn step_at(&self, step: usize, host_fps: u32) -> (u32, f32) {
        let (fps, scale) = STEPS[step];
        let fps = fps
            .min(host_fps)
            .min(self.limits.max_fps.unwrap_or(u32::MAX))
            .max(1);
        (fps, scale)
    }

    /// The quality now, with the client's height limit applied.
    pub fn quality(&self, host_fps: u32, window: (u32, u32)) -> Quality {
        let (fps, mut scale) = self.step_at(self.step, host_fps);
        if let Some(max_height) = self.limits.max_height {
            if window.1 > 0 {
                scale = scale.min(max_height as f32 / window.1 as f32);
            }
        }
        Quality {
            bitrate: self.target.or(self.cap_bps()),
            fps: (fps < host_fps).then_some(fps),
            scale: scale.clamp(0.1, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: (u32, u32) = (1920, 1080);

    fn seen(sent_bps: u32, loss: f32, rtt_ms: u64) -> Observation {
        Observation {
            sent_bps,
            window: WINDOW,
            host_fps: 60,
            loss: Some(loss),
            rtt: Some(Duration::from_millis(rtt_ms)),
        }
    }

    #[test]
    fn a_clean_network_leaves_the_settings_alone() {
        let mut c = Controller::new(StreamLimits::default());
        let start = Instant::now();
        for i in 0..30 {
            let q = c.observe(start + Duration::from_secs(i), seen(8_000_000, 0.0, 20));
            assert_eq!(q, Quality::default());
        }
    }

    #[test]
    fn loss_cuts_then_a_clean_network_grows_back() {
        let mut c = Controller::new(StreamLimits::default());
        let t = Instant::now();
        let q = c.observe(t, seen(8_000_000, 0.2, 20));
        assert_eq!(q.bitrate, Some(5_600_000));
        // Loss again: cut from what went out.
        let q = c.observe(t + Duration::from_secs(1), seen(5_000_000, 0.2, 20));
        assert_eq!(q.bitrate, Some(3_500_000));
        // Clean and the encoder fills it: grows 8% a report.
        let q = c.observe(t + Duration::from_secs(2), seen(3_400_000, 0.0, 20));
        assert_eq!(q.bitrate, Some(3_780_000));
        // Clean but a quiet window: holds.
        let q = c.observe(t + Duration::from_secs(3), seen(500_000, 0.0, 20));
        assert_eq!(q.bitrate, Some(3_780_000));
        // Clean and full for long enough: let go past the 8 Mbit/s the
        // stream sent before anything held it.
        let mut now = t + Duration::from_secs(3);
        let mut q = q;
        for _ in 0..30 {
            now += Duration::from_secs(1);
            q = c.observe(now, seen(q.bitrate.unwrap_or(8_000_000), 0.0, 20));
        }
        assert_eq!(q.bitrate, None);
    }

    #[test]
    fn a_growing_round_trip_cuts_gently() {
        let mut c = Controller::new(StreamLimits::default());
        let t = Instant::now();
        c.observe(t, seen(4_000_000, 0.0, 20));
        let q = c.observe(t + Duration::from_secs(1), seen(4_000_000, 0.0, 250));
        assert_eq!(q.bitrate, Some(3_400_000));
    }

    #[test]
    fn the_client_limits_are_ceilings() {
        let mut c = Controller::new(StreamLimits {
            max_bitrate_kbps: Some(2000),
            max_fps: Some(30),
            max_height: Some(720),
        });
        let q = c.observe(Instant::now(), seen(1_900_000, 0.0, 20));
        assert_eq!(q.bitrate, Some(2_000_000));
        assert_eq!(q.fps, Some(30));
        assert!((q.scale - 720.0 / 1080.0).abs() < 1e-6);
    }

    #[test]
    fn a_thin_rate_steps_frame_rate_then_size_down_and_back_up() {
        let mut c = Controller::new(StreamLimits::default());
        let t = Instant::now();
        // 1 Mbit/s for 1080p60 is far too thin.
        let mut q = c.observe(t, seen(1_400_000, 0.3, 20));
        assert_eq!(q.bitrate, Some(980_000));
        let mut now = t;
        for _ in 0..20 {
            now += Duration::from_secs(1);
            q = c.observe(now, seen(980_000, 0.05, 20));
        }
        // 30 fps at half size gives each pixel enough again; it stops there.
        assert_eq!(q.fps, Some(30), "{q:?}");
        assert_eq!(q.scale, 0.5);
        // Lots of room again (a raised limit stands in for a network
        // that grew): back to the top step, one step at a time.
        c.set_limits(StreamLimits::default());
        c.target = Some(100_000_000);
        for _ in 0..80 {
            now += Duration::from_secs(1);
            q = c.observe(now, seen(90_000_000, 0.0, 20));
        }
        assert_eq!(q.fps, None);
        assert_eq!(q.scale, 1.0);
    }
}
