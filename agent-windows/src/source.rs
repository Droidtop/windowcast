//! The agent's `WindowSource`: the desktop's windows, and per streamed
//! window a capture plus encoder on the stream's own thread. The options
//! can change while streams run: each stream reopens its encoder with the
//! new ones at its next picture.

use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use windowcast_host::{EncodedFrame, FrameSource, WindowSource};
use windowcast_protocol::{BackendKind, InputEvent, VideoCodec, WindowId, WindowInfo};
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

use crate::capture::{self, Capture};
use crate::clipboard;
use crate::convert;
use crate::encoder::{self, Encoder, EncoderChoice, Settings};
use crate::input::Injector;
use crate::windows_list;

/// Agent options, from the command line or a host application.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub encoder: EncoderChoice,
    /// Offer only this codec; `None` offers every codec the encoder has.
    pub codec: Option<VideoCodec>,
    pub fps: u32,
    /// Bits per second at 1920x1080; scaled by area for other sizes.
    pub bitrate_1080p: u32,
    /// The output device a client's microphone plays into (part of its
    /// name); `None` picks a known virtual audio cable.
    pub microphone: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            encoder: EncoderChoice::Auto,
            codec: None,
            fps: 30,
            bitrate_1080p: 8_000_000,
            microphone: None,
        }
    }
}

/// The options in force, and a counter that changes with them.
struct Current {
    options: Options,
    codecs: Vec<VideoCodec>,
    generation: u64,
}

pub struct WindowsSource {
    current: Arc<RwLock<Current>>,
    injector: Injector,
}

impl WindowsSource {
    /// Probes the encoders once; fails when this Windows cannot capture
    /// windows at all (Windows.Graphics.Capture needs Windows 10 1903).
    pub fn new(options: Options) -> Result<Self, String> {
        if !capture::supported() {
            return Err("Windows.Graphics.Capture is not supported on this system".into());
        }
        let codecs = codecs_for(&options)?;
        Ok(WindowsSource {
            current: Arc::new(RwLock::new(Current {
                options,
                codecs,
                generation: 0,
            })),
            injector: Injector::default(),
        })
    }

    pub fn options(&self) -> Options {
        self.current.read().expect("options").options.clone()
    }

    /// Changes the options. Running streams take the new encoder, frame
    /// rate and bitrate at their next picture; a codec change applies to
    /// streams started afterwards (a track keeps its codec).
    pub fn set_options(&self, options: Options) -> Result<(), String> {
        let codecs = codecs_for(&options)?;
        let mut current = self.current.write().expect("options");
        if current.options != options {
            current.options = options;
            current.codecs = codecs;
            current.generation += 1;
        }
        Ok(())
    }

    fn open_stream(&self, window: WindowId, codec: VideoCodec, desktop: bool) -> WindowStream {
        WindowStream {
            window,
            codec,
            desktop,
            current: Arc::clone(&self.current),
            generation: u64::MAX,
            state: None,
            next_at: Instant::now(),
            last_sent: Instant::now(),
            frames: 0,
            pending: Default::default(),
            announce: None,
            encoder_name: String::new(),
            timings: std::env::var_os("WINDOWCAST_TIMINGS").map(|_| Timings::new()),
        }
    }
}

fn codecs_for(options: &Options) -> Result<Vec<VideoCodec>, String> {
    let codecs: Vec<VideoCodec> = encoder::available_codecs(options.encoder)
        .into_iter()
        .filter(|codec| options.codec.is_none_or(|only| only == *codec))
        .collect();
    if codecs.is_empty() {
        return Err(match options.codec {
            Some(codec) => format!("{} cannot encode {codec:?}", options.encoder.name()),
            None => format!("no encoder available for {}", options.encoder.name()),
        });
    }
    Ok(codecs)
}

impl WindowSource for WindowsSource {
    fn list_windows(&self) -> Vec<WindowInfo> {
        windows_list::list()
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        self.current.read().expect("options").codecs.clone()
    }

    fn backends(&self) -> Vec<BackendKind> {
        vec![BackendKind::Native, BackendKind::Desktop]
    }

    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        self.injector.inject(event, focus);
    }

    fn clipboard(&self) -> Option<(u64, String)> {
        Some((clipboard::sequence(), clipboard::text().unwrap_or_default()))
    }

    fn set_clipboard(&self, text: &str) {
        clipboard::set_text(text);
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        check_window(window)?;
        Ok(Box::new(self.open_stream(window, codec, false)))
    }

    fn open_desktop(
        &self,
        window: WindowId,
        codec: VideoCodec,
    ) -> Result<Box<dyn FrameSource>, String> {
        check_window(window)?;
        Ok(Box::new(self.open_stream(window, codec, true)))
    }

    fn microphone(
        &self,
    ) -> Option<Result<Box<dyn windowcast_host::audio::MicrophoneSink>, String>> {
        let named = self.options().microphone;
        Some(
            crate::microphone::WindowsMicrophone::open(named.as_deref())
                .map(|mic| Box::new(mic) as _),
        )
    }

    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        Some(crate::audio::ProcessAudio::open(window).map(|audio| Box::new(audio) as _))
    }
}

fn check_window(window: WindowId) -> Result<(), String> {
    if unsafe { IsWindow(Some(windows_list::hwnd(window))) }.as_bool() {
        Ok(())
    } else {
        Err("no such window".into())
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
    /// Cut out of a whole-screen capture (the desktop backend).
    desktop: bool,
    current: Arc<RwLock<Current>>,
    /// The options generation the encoder was opened with.
    generation: u64,
    state: Option<State>,
    next_at: Instant,
    last_sent: Instant,
    frames: u64,
    pending: std::collections::VecDeque<Vec<u8>>,
    /// The picture size to announce with the next frame sent.
    announce: Option<(u32, u32)>,
    encoder_name: String,
    /// Per-stage timings, printed every few seconds when the environment
    /// sets WINDOWCAST_TIMINGS (for measuring the capture pipeline).
    timings: Option<Timings>,
}

struct Timings {
    since: Instant,
    frames: u32,
    readback: Duration,
    encode: Duration,
    /// Keyframes asked of the encoder (first frame, size changes and the
    /// client's requests); the rest it makes on its own interval.
    forced: u32,
}

impl Timings {
    fn add(&mut self, window: WindowId, readback: Duration, encode: Duration, forced: bool) {
        self.frames += 1;
        self.forced += u32::from(forced);
        self.readback += readback;
        self.encode += encode;
        let elapsed = self.since.elapsed();
        if elapsed >= Duration::from_secs(5) {
            let per = |total: Duration| total.as_secs_f64() * 1000.0 / f64::from(self.frames);
            println!(
                "window {}: {:.1} fps; capture and read back {:.2} ms, convert and encode {:.2} ms a frame; {} keyframes asked for",
                window.0,
                f64::from(self.frames) / elapsed.as_secs_f64(),
                per(self.readback),
                per(self.encode),
                self.forced
            );
            *self = Timings::new();
        }
    }

    fn new() -> Self {
        Timings {
            since: Instant::now(),
            frames: 0,
            readback: Duration::ZERO,
            encode: Duration::ZERO,
            forced: 0,
        }
    }
}

impl WindowStream {
    fn take_pending(&mut self) -> Option<EncodedFrame> {
        let data = self.pending.pop_front()?;
        let duration = self.last_sent.elapsed();
        self.last_sent = Instant::now();
        Some(EncodedFrame {
            data,
            duration,
            size: self.announce.take(),
        })
    }

    fn options(&self) -> (Options, u64) {
        let current = self.current.read().expect("options");
        (current.options.clone(), current.generation)
    }
}

// Every Windows object in `state` is created on the stream's own thread
// after the move, in the multithreaded apartment.
unsafe impl Send for WindowStream {}

/// A static window still gets a frame this often, so a client that just
/// joined or lost a packet is never left without a picture.
const REFRESH: Duration = Duration::from_secs(1);

fn frame_time(options: &Options) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(options.fps.max(1)))
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
    fn describe(&self) -> String {
        self.encoder_name.clone()
    }

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
            let capture = if self.desktop {
                Capture::desktop(hwnd)
            } else {
                Capture::new(hwnd)
            };
            let capture = match capture {
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
            let (options, generation) = self.options();
            let frame_time = frame_time(&options);
            // Never faster than the frame rate: wait out the frame time, then
            // up to one more frame time for a new picture.
            std::thread::sleep(self.next_at.saturating_duration_since(Instant::now()));
            let state = self.state.as_mut().expect("state");
            let changed = match state.capture.poll(frame_time) {
                Ok(changed) => changed,
                Err(e) => {
                    eprintln!("capture of window {} stopped: {e}", self.window.0);
                    return None;
                }
            };

            let Some(picture) = state.capture.picture() else {
                continue;
            };
            if !(changed || keyframe || self.last_sent.elapsed() >= REFRESH) {
                continue;
            }
            let size = convert::even(picture.width(), picture.height());
            let mut texture_output = None;
            if size != state.size || generation != self.generation {
                let settings = settings(&options, self.codec, size.0, size.1);
                // A live change to an encoder that cannot make this
                // stream's codec keeps the stream going on the best one
                // that can.
                let device = state.capture.device();
                let opened = encoder::open_on(options.encoder, &settings, device.as_ref())
                    .or_else(|_| encoder::open_on(EncoderChoice::Auto, &settings, device.as_ref()));
                state.encoder = match opened {
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
                self.encoder_name = state.encoder.describe();
                // From the next picture on (this one is already read).
                texture_output = Some(state.encoder.takes_textures());
                self.generation = generation;
                state.size = size;
                self.announce = Some((size.0 as u32, size.1 as u32));
                self.pending.clear();
                keyframe = true;
            }
            self.next_at = Instant::now() + frame_time;
            let time = frame_time * self.frames as u32;
            let started = Instant::now();
            let encoded = state.encoder.encode(&picture, keyframe, time);
            if let Some(on) = texture_output {
                state.capture.set_texture_output(on);
            }
            if let Some(timings) = &mut self.timings {
                timings.add(
                    self.window,
                    state.capture.readback,
                    started.elapsed(),
                    keyframe,
                );
            }
            match encoded {
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
        _: &convert::Picture<'_>,
        _: bool,
        _: Duration,
    ) -> Result<Vec<Vec<u8>>, String> {
        Ok(Vec::new())
    }

    fn describe(&self) -> String {
        "none yet".into()
    }
}
