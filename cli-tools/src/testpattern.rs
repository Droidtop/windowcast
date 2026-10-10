//! A window that is a moving test pattern, encoded with OpenH264 (software,
//! BSD-licensed). For trying a client against a host with no capture yet:
//! the pixels are generated, the encoding and everything after it are real.

use std::sync::Arc;
use std::time::{Duration, Instant};

use openh264::encoder::{Encoder, EncoderConfig, FrameRate, IntraFramePeriod};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;
use windowcast_host::{EncodedFrame, FrameSource, Picture, PictureSource, WindowSource};
use windowcast_protocol::{BackendKind, ContentHint, VideoCodec, WindowId, WindowInfo};

pub const WINDOW: WindowId = WindowId(1);
pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 360;
const FPS: u32 = 30;

/// A host's window list with one window: the test pattern.
pub struct TestPatternSource;

impl WindowSource for TestPatternSource {
    fn list_windows(&self) -> Vec<WindowInfo> {
        vec![WindowInfo {
            id: WINDOW,
            title: "windowcast test pattern".into(),
            app_id: "windowcast.testpattern".into(),
            width: WIDTH as u32,
            height: HEIGHT as u32,
            focused: true,
            content: ContentHint::General,
        }]
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        vec![VideoCodec::H264]
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        if window != WINDOW {
            return Err("no such window".into());
        }
        if codec != VideoCodec::H264 {
            return Err("the test pattern only encodes H.264".into());
        }
        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(FPS as f32))
            // Keyframes on request, plus one every two seconds.
            .intra_frame_period(IntraFramePeriod::from_num_frames(FPS * 2));
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .map_err(|e| format!("encoder: {e}"))?;
        Ok(Box::new(TestPattern {
            encoder,
            frame: 0,
            next_at: Instant::now(),
        }))
    }

    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        (window == WINDOW).then(|| {
            Ok(Box::new(TestPictures {
                frame: 0,
                next_at: Instant::now(),
            }) as _)
        })
    }
}

/// The test pattern's pictures, unencoded.
struct TestPictures {
    frame: u64,
    next_at: Instant,
}

impl PictureSource for TestPictures {
    fn next_picture(&mut self) -> Option<Picture> {
        let now = Instant::now();
        if self.next_at > now {
            std::thread::sleep(self.next_at - now);
        }
        self.next_at += FRAME_TIME;
        let picture = draw_bgra(self.frame);
        self.frame += 1;
        Some(picture)
    }
}

/// The pattern as BGRA: a grey ramp, a red bar that sweeps across, and the
/// frame number in binary as a row of white and black blocks along the top.
pub fn draw_bgra(frame: u64) -> Picture {
    let (w, h) = (WIDTH, HEIGHT);
    let mut data = vec![0u8; w * h * 4];
    let bar = (frame as usize * 8) % w;
    for y in 0..h {
        for x in 0..w {
            let grey = (x * 200 / w + 16) as u8;
            let mut pixel = [grey, grey, grey, 255];
            if x >= bar && x < bar + 24 {
                pixel = [40, 40, 220, 255];
            }
            if y < 32 {
                let bit = x / (w / 16);
                let on = (frame >> (15 - bit)) & 1 == 1;
                let v = if on { 235 } else { 16 };
                pixel = [v, v, v, 255];
            }
            data[(y * w + x) * 4..][..4].copy_from_slice(&pixel);
        }
    }
    Picture {
        width: w as u32,
        height: h as u32,
        stride: w * 4,
        data,
    }
}

/// The frame number a [`draw_bgra`] picture (or a copy of it, in any
/// channel order) carries in its top row of blocks.
pub fn frame_number(rgba_or_bgra: &[u8], width: usize) -> u64 {
    (0..16).fold(0, |n, bit| {
        let x = bit * (width / 16) + width / 32;
        let v = rgba_or_bgra[(16 * width + x) * 4 + 1];
        (n << 1) | u64::from(v > 128)
    })
}

/// Another source whose windows all carry `content` as their hint, so the rules pick the
/// backend that hint asks for (Text: RDP) with no capture to classify anything.
pub struct WithContent {
    inner: Arc<dyn WindowSource>,
    content: ContentHint,
}

impl WithContent {
    pub fn new(inner: Arc<dyn WindowSource>, content: ContentHint) -> Self {
        WithContent { inner, content }
    }
}

/// The hint a name stands for (`--content text`), case-insensitively.
pub fn parse_content(name: &str) -> Option<ContentHint> {
    match name.to_ascii_lowercase().as_str() {
        "general" => Some(ContentHint::General),
        "text" => Some(ContentHint::Text),
        "game" => Some(ContentHint::Game),
        "video" => Some(ContentHint::Video),
        _ => None,
    }
}

impl WindowSource for WithContent {
    fn list_windows(&self) -> Vec<WindowInfo> {
        let mut windows = self.inner.list_windows();
        for window in &mut windows {
            window.content = self.content;
        }
        windows
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        self.inner.encoders()
    }

    fn backends(&self) -> Vec<BackendKind> {
        self.inner.backends()
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        self.inner.open(window, codec)
    }

    fn open_pictures(&self, window: WindowId) -> Option<Result<Box<dyn PictureSource>, String>> {
        self.inner.open_pictures(window)
    }

    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        self.inner.open_audio(window)
    }

    fn microphone(
        &self,
    ) -> Option<Result<Box<dyn windowcast_host::audio::MicrophoneSink>, String>> {
        self.inner.microphone()
    }
}

/// The test pattern with a sound: a 440 Hz tone, generated (no capture),
/// for trying a client's audio playback.
pub struct TestPatternWithTone;

impl WindowSource for TestPatternWithTone {
    fn list_windows(&self) -> Vec<WindowInfo> {
        TestPatternSource.list_windows()
    }

    fn encoders(&self) -> Vec<VideoCodec> {
        TestPatternSource.encoders()
    }

    fn open(&self, window: WindowId, codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
        TestPatternSource.open(window, codec)
    }

    fn open_audio(
        &self,
        window: WindowId,
    ) -> Option<Result<Box<dyn windowcast_host::audio::AudioSource>, String>> {
        (window == WINDOW).then(|| {
            Ok(Box::new(Tone {
                phase: 0.0,
                next_at: Instant::now(),
            }) as _)
        })
    }
}

/// A 440 Hz sine at a quarter of full scale, in real time.
struct Tone {
    phase: f32,
    next_at: Instant,
}

impl windowcast_host::audio::AudioSource for Tone {
    fn next_samples(&mut self) -> Option<Vec<i16>> {
        const CHUNK: Duration = Duration::from_millis(20);
        let now = Instant::now();
        if self.next_at > now {
            std::thread::sleep(self.next_at - now);
        }
        self.next_at += CHUNK;
        let mut samples = Vec::with_capacity(960 * 2);
        for _ in 0..960 {
            let value = (self.phase.sin() * 8192.0) as i16;
            samples.extend([value, value]);
            self.phase += 440.0 * std::f32::consts::TAU / 48_000.0;
            if self.phase > std::f32::consts::TAU {
                self.phase -= std::f32::consts::TAU;
            }
        }
        Some(samples)
    }
}

struct TestPattern {
    encoder: Encoder,
    frame: u64,
    next_at: Instant,
}

const FRAME_TIME: Duration = Duration::from_nanos(1_000_000_000 / FPS as u64);

impl FrameSource for TestPattern {
    fn next_frame(&mut self, keyframe: bool) -> Option<EncodedFrame> {
        let now = Instant::now();
        if self.next_at > now {
            std::thread::sleep(self.next_at - now);
        }
        self.next_at += FRAME_TIME;

        let picture = YUVBuffer::from_vec(draw(self.frame), WIDTH, HEIGHT);
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let data = self.encoder.encode(&picture).ok()?.to_vec();
        let first = self.frame == 0;
        self.frame += 1;
        Some(EncodedFrame {
            data,
            duration: FRAME_TIME,
            size: first.then_some((WIDTH as u32, HEIGHT as u32)),
        })
    }
}

/// One I420 picture: a luma ramp, a bar that sweeps across, and the frame
/// number in binary as a row of blocks along the top.
fn draw(frame: u64) -> Vec<u8> {
    let (w, h) = (WIDTH, HEIGHT);
    let mut yuv = vec![0u8; w * h * 3 / 2];
    let (luma, chroma) = yuv.split_at_mut(w * h);
    let bar = (frame as usize * 8) % w;
    for y in 0..h {
        for x in 0..w {
            let mut value = (x * 200 / w + 16) as u8;
            if x >= bar && x < bar + 24 {
                value = 235;
            }
            if y < 32 {
                let bit = x / (w / 16);
                value = if (frame >> (15 - bit)) & 1 == 1 {
                    235
                } else {
                    16
                };
            }
            luma[y * w + x] = value;
        }
    }
    let (u, v) = chroma.split_at_mut(w * h / 4);
    for cy in 0..h / 2 {
        for cx in 0..w / 2 {
            let x = cx * 2;
            let in_bar = x >= bar && x < bar + 24 && cy * 2 >= 32;
            u[cy * (w / 2) + cx] = if in_bar { 90 } else { 128 };
            v[cy * (w / 2) + cx] = if in_bar { 240 } else { 128 };
        }
    }
    yuv
}

#[cfg(test)]
mod content_tests {
    use super::*;

    #[test]
    fn with_content_overrides_every_windows_hint() {
        let source = WithContent::new(Arc::new(TestPatternSource), ContentHint::Text);
        let windows = source.list_windows();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].content, ContentHint::Text);
        assert!(
            source.open_pictures(WINDOW).is_some(),
            "pictures still come from the pattern"
        );
    }

    #[test]
    fn content_names_parse() {
        assert_eq!(parse_content("text"), Some(ContentHint::Text));
        assert_eq!(parse_content("Game"), Some(ContentHint::Game));
        assert_eq!(parse_content("nope"), None);
    }
}
