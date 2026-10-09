//! The agent's `WindowSource`: the desktop's windows, and per streamed
//! window a capture plus encoder on the stream's own thread.

use std::time::{Duration, Instant};

use windowcast_host::{EncodedFrame, FrameSource, WindowSource};
use windowcast_protocol::{VideoCodec, WindowId, WindowInfo};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::capture::{self, Capture};
use crate::convert;
use crate::encoder::{self, Encoder, EncoderChoice, Settings};
use crate::windows_list;

/// Agent options, from the command line.
#[derive(Debug, Clone)]
pub struct Options {
    pub encoder: EncoderChoice,
    pub fps: u32,
    /// Bits per second at 1920x1080; scaled by area for other sizes.
    pub bitrate_1080p: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            encoder: EncoderChoice::Auto,
            fps: 30,
            bitrate_1080p: 8_000_000,
        }
    }
}

pub struct WindowsSource {
    options: Options,
    codecs: Vec<VideoCodec>,
}

impl WindowsSource {
    /// Probes the encoders once; fails when this Windows cannot capture
    /// windows at all (Windows.Graphics.Capture needs Windows 10 1903).
    pub fn new(options: Options) -> Result<Self, String> {
        if !capture::supported() {
            return Err("Windows.Graphics.Capture is not supported on this system".into());
        }
        let codecs = encoder::available_codecs(options.encoder);
        if codecs.is_empty() {
            return Err(format!("no encoder available for {:?}", options.encoder));
        }
        Ok(WindowsSource { options, codecs })
    }
}

impl WindowSource for WindowsSource {
    fn list_windows(&self) -> Vec<WindowInfo> {
        windows_list::list()
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        self.codecs.clone()
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        let hwnd = windows_list::hwnd(window);
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            return Err("no such window".into());
        }
        Ok(Box::new(WindowStream {
            window,
            codec,
            options: self.options.clone(),
            state: None,
            next_at: Instant::now(),
            last_sent: Instant::now(),
            frames: 0,
            pending: Default::default(),
        }))
    }
}

struct State {
    capture: Capture,
    encoder: Box<dyn Encoder>,
    size: (usize, usize),
}

/// One streamed window. Its capture and encoder are created on, and only
/// used from, the stream's thread (see host-core's `feed`).
struct WindowStream {
    window: WindowId,
    codec: VideoCodec,
    options: Options,
    state: Option<State>,
    next_at: Instant,
    last_sent: Instant,
    frames: u64,
    pending: std::collections::VecDeque<Vec<u8>>,
}

impl WindowStream {
    fn take_pending(&mut self) -> Option<EncodedFrame> {
        let data = self.pending.pop_front()?;
        let duration = self.last_sent.elapsed();
        self.last_sent = Instant::now();
        Some(EncodedFrame { data, duration })
    }
}

// Every Windows object in `state` is created on the stream's own thread
// after the move, in the multithreaded apartment.
unsafe impl Send for WindowStream {}

/// A static window still gets a frame this often, so a client that just
/// joined or lost a packet is never left without a picture.
const REFRESH: Duration = Duration::from_secs(1);

fn frame_time(options: &Options) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(options.fps))
}

/// Encoder settings for a picture size; the bitrate scales with area.
fn settings(options: &Options, codec: VideoCodec, width: usize, height: usize) -> Settings {
    let area = (width * height) as u64;
    let bitrate =
        (u64::from(options.bitrate_1080p) * area / (1920 * 1080)).clamp(500_000, 50_000_000) as u32;
    Settings {
        codec,
        width,
        height,
        fps: options.fps,
        bitrate,
    }
}

impl FrameSource for WindowStream {
    fn next_frame(&mut self, mut keyframe: bool) -> Option<EncodedFrame> {
        // An encoder that caught up gave more than one frame last time.
        if !keyframe {
            if let Some(frame) = self.take_pending() {
                return Some(frame);
            }
        }
        capture::init_thread();
        let hwnd = windows_list::hwnd(self.window);
        if self.state.is_none() {
            let capture = match Capture::new(hwnd) {
                Ok(capture) => capture,
                Err(e) => {
                    eprintln!("capture of window {} failed: {e}", self.window.0);
                    return None;
                }
            };
            // The encoder is opened at the first picture's size.
            self.state = Some(State {
                capture,
                encoder: Box::new(NoEncoder),
                size: (0, 0),
            });
        }
        loop {
            if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                return None;
            }
            let frame_time = frame_time(&self.options);
            let wait = self.next_at.saturating_duration_since(Instant::now());
            let state = self.state.as_mut().expect("state");
            let changed = match state.capture.poll(wait) {
                Ok(changed) => changed,
                Err(e) => {
                    eprintln!("capture of window {} stopped: {e}", self.window.0);
                    return None;
                }
            };
            self.next_at = Instant::now().max(self.next_at) + frame_time;

            let Some(picture) = state.capture.picture() else {
                continue;
            };
            if !(changed || keyframe || self.last_sent.elapsed() >= REFRESH) {
                continue;
            }
            let size = convert::even(picture.width, picture.height);
            if size != state.size {
                let settings = settings(&self.options, self.codec, size.0, size.1);
                state.encoder = match encoder::open(self.options.encoder, &settings) {
                    Ok(encoder) => {
                        println!(
                            "window {}: {}x{} {:?} with {}",
                            self.window.0,
                            size.0,
                            size.1,
                            self.codec,
                            encoder.describe()
                        );
                        encoder
                    }
                    Err(e) => {
                        eprintln!("window {}: {e}", self.window.0);
                        return None;
                    }
                };
                state.size = size;
                keyframe = true;
            }
            let time = frame_time * self.frames as u32;
            match state.encoder.encode(&picture, keyframe, time) {
                Ok(units) => {
                    self.frames += 1;
                    self.pending.extend(units);
                    if let Some(frame) = self.take_pending() {
                        return Some(frame);
                    }
                }
                Err(e) => {
                    eprintln!("window {}: encoding failed: {e}", self.window.0);
                    return None;
                }
            }
        }
    }
}

/// Stands in until the first picture gives the encoder its size.
struct NoEncoder;

impl Encoder for NoEncoder {
    fn encode(
        &mut self,
        _: &convert::Bgra<'_>,
        _: bool,
        _: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        Ok(Vec::new())
    }

    fn describe(&self) -> String {
        "none yet".into()
    }
}
