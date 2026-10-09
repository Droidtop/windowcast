//! The agent's `WindowSource`: the compositor's windows, per streamed
//! window a capture (`capture.rs`) plus OpenH264 on the stream's own
//! thread, and input through virtual devices (`input.rs`).

use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use windowcast_host::quality::Quality;
use windowcast_host::video::{self, OpenH264};
use windowcast_host::{EncodedFrame, FrameSource, WindowSource};
use windowcast_protocol::{BackendKind, InputEvent, VideoCodec, WindowId, WindowInfo};

use crate::capture::Capture;
use crate::input::Injector;
use crate::toplevels;

/// Agent options, from the command line.
#[derive(Debug, Clone)]
pub struct Options {
    pub fps: u32,
    /// Bits per second at 1920x1080; scaled by area for other sizes.
    pub bitrate_1080p: u32,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            fps: 30,
            bitrate_1080p: 8_000_000,
        }
    }
}

pub struct LinuxSource {
    /// Taken by each stream when it starts.
    options: RwLock<Options>,
    /// Made on first use, on the session's input thread.
    injector: Mutex<Option<Result<Injector, String>>>,
}

impl LinuxSource {
    pub fn new(options: Options) -> Self {
        LinuxSource {
            options: RwLock::new(options),
            injector: Mutex::new(None),
        }
    }

    /// Changes the frame rate and bitrate of streams started from now on.
    pub fn set_options(&self, options: Options) {
        *self.options.write().expect("options") = options;
    }

    fn options(&self) -> Options {
        self.options.read().expect("options").clone()
    }
}

impl WindowSource for LinuxSource {
    fn list_windows(&self) -> Vec<WindowInfo> {
        toplevels::list_windows().unwrap_or_else(|e| {
            eprintln!("failed to list windows: {e}");
            Vec::new()
        })
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        vec![VideoCodec::H264]
    }

    fn backends(&self) -> Vec<BackendKind> {
        vec![BackendKind::Native, BackendKind::Desktop]
    }

    fn open(&self, window: WindowId, _codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        let capture = Capture::window(window)?;
        Ok(Box::new(WindowStream::new(capture, &self.options())))
    }

    fn open_desktop(
        &self,
        window: WindowId,
        _codec: VideoCodec,
    ) -> Result<Box<dyn FrameSource>, String> {
        let capture = Capture::desktop(window)?;
        Ok(Box::new(WindowStream::new(capture, &self.options())))
    }

    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        Some(crate::audio::WindowAudio::open(window).map(|audio| Box::new(audio) as _))
    }

    fn microphone(
        &self,
    ) -> Option<Result<Box<dyn windowcast_host::audio::MicrophoneSink>, String>> {
        Some(crate::microphone::LinuxMicrophone::open().map(|mic| Box::new(mic) as _))
    }

    fn gamepads(&self) -> Option<Result<Box<dyn windowcast_host::gamepad::GamepadSink>, String>> {
        Some(crate::gamepad::LinuxPads::open().map(|pads| Box::new(pads) as _))
    }

    fn input(&self, event: &InputEvent, focus: Option<WindowId>) {
        let mut injector = self.injector.lock().expect("injector");
        let injector = injector.get_or_insert_with(|| {
            Injector::new().inspect_err(|e| eprintln!("no input on this host: {e}"))
        });
        if let Ok(injector) = injector {
            injector.inject(event, focus);
        }
    }
}

/// A static window still gets a frame this often, so a client that just
/// joined or lost a packet is never left without a picture.
const REFRESH: Duration = Duration::from_secs(1);

struct WindowStream {
    capture: Capture,
    options: Options,
    encoder: Option<OpenH264>,
    size: (usize, usize),
    next_at: Instant,
    last_sent: Instant,
    /// What adaptive quality holds the stream to.
    quality: Quality,
    /// The bitrate and frame rate the encoder runs at now.
    rate: (u32, u32),
    /// The picture scaled down, when the quality scales it.
    scaled: Vec<u8>,
}

impl WindowStream {
    fn new(capture: Capture, options: &Options) -> Self {
        WindowStream {
            capture,
            options: options.clone(),
            encoder: None,
            size: (0, 0),
            next_at: Instant::now(),
            last_sent: Instant::now(),
            quality: Quality::default(),
            rate: (0, 0),
            scaled: Vec::new(),
        }
    }
}

/// The frame rate in force: the setting, or less under adaptive quality.
fn fps(options: &Options, quality: &Quality) -> u32 {
    quality
        .fps
        .map_or(options.fps, |fps| fps.min(options.fps))
        .max(1)
}

/// The bitrate for a picture of `size`: the setting scaled by area, or less
/// under adaptive quality.
fn bitrate(options: &Options, quality: &Quality, size: (usize, usize)) -> u32 {
    let area = (size.0 * size.1) as u64;
    let nominal =
        (u64::from(options.bitrate_1080p) * area / (1920 * 1080)).clamp(500_000, 50_000_000) as u32;
    quality.bitrate.map_or(nominal, |held| held.min(nominal))
}

impl FrameSource for WindowStream {
    fn describe(&self) -> String {
        "OpenH264 (software)".into()
    }

    fn frame_rate(&self) -> u32 {
        self.options.fps
    }

    fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
    }

    fn next_frame(&mut self, mut keyframe: bool) -> Option<EncodedFrame> {
        loop {
            let frame_time =
                Duration::from_nanos(1_000_000_000 / u64::from(fps(&self.options, &self.quality)));
            // Never faster than the frame rate.
            std::thread::sleep(self.next_at.saturating_duration_since(Instant::now()));
            let changed = match self.capture.poll(frame_time) {
                Ok(changed) => changed,
                Err(e) => {
                    eprintln!("capture stopped: {e}");
                    return None;
                }
            };
            if !(changed || keyframe || self.last_sent.elapsed() >= REFRESH) {
                continue;
            }
            let Some(picture) = self.capture.picture() else {
                continue;
            };
            let size = video::scaled(picture.width, picture.height, self.quality.scale);
            let rate = (
                bitrate(&self.options, &self.quality, size),
                fps(&self.options, &self.quality),
            );
            let mut announce = None;
            if size != self.size || self.encoder.is_none() {
                match OpenH264::new(rate.0, rate.1) {
                    Ok(encoder) => self.encoder = Some(encoder),
                    Err(e) => {
                        eprintln!("encoder: {e}");
                        return None;
                    }
                }
                println!("streaming {}x{} with OpenH264", size.0, size.1);
                self.size = size;
                self.rate = rate;
                announce = Some((size.0 as u32, size.1 as u32));
                keyframe = true;
            }
            let encoder = self.encoder.as_mut().expect("encoder");
            if rate != self.rate {
                if let Err(e) = encoder.set_rate(rate.0, rate.1) {
                    eprintln!("{e}");
                }
                self.rate = rate;
            }
            self.next_at = Instant::now() + frame_time;
            let picture = if size == video::even(picture.width, picture.height) {
                picture
            } else {
                video::scale_bgra(&picture, size, &mut self.scaled)
            };
            let encoded = encoder.encode(size.0, size.1, keyframe, |i420| {
                video::to_i420(&picture, i420);
                Ok(())
            });
            match encoded {
                Ok(Some(data)) => {
                    let duration = self.last_sent.elapsed();
                    self.last_sent = Instant::now();
                    return Some(EncodedFrame {
                        data,
                        duration,
                        size: announce,
                    });
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!("encoding failed: {e}");
                    return None;
                }
            }
        }
    }
}
