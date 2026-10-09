//! windowcast's Windows client end: each streamed window is decoded on the
//! GPU and shown in a native window of its own, with nothing in between.
//!
//! - `decoder`: Media Foundation's decoder for the stream's codec (H.264,
//!   H.265 or AV1), given a Direct3D 11 device so it decodes in hardware
//!   (DXVA) into GPU textures.
//! - `present`: the decoded NV12 texture goes through the GPU's video
//!   processor (colour conversion and scaling) straight into a flip-model
//!   swap chain's back buffer.
//! - `window`: the stream window itself, on a thread of its own: it pulls
//!   frames from `client-core`, decodes and presents them, and turns the
//!   user's pointer and keys into windowcast input when that is switched on.
//! - `audio`: the window's sound, Opus decoded and played through WASAPI
//!   on a thread beside it.

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod decoder;
#[cfg(windows)]
mod keys;
#[cfg(windows)]
mod microphone;
#[cfg(windows)]
mod present;
#[cfg(windows)]
mod window;

#[cfg(windows)]
pub use microphone::Microphone;
#[cfg(windows)]
pub use window::{open, StreamWindow};

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// How a stream window is placed.
#[derive(Debug, Clone, Default)]
pub struct Placement {
    /// Borderless, covering one display.
    pub fullscreen: bool,
    /// The display by Windows' number ([`Display::number`]) for a
    /// fullscreen window, or the one a normal window opens on. `None`, or a
    /// number no display has: the primary display.
    pub display: Option<u32>,
}

/// One display, in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Display {
    /// Windows' number for it: 2 is `DISPLAY2`.
    pub number: u32,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

/// What a stream window reports, for a control window to show.
#[derive(Debug, Clone, Default)]
pub struct StreamStats {
    /// The decoder in use, e.g. "Microsoft H264 Video Decoder MFT (DXVA)".
    pub decoder: String,
    pub size: Option<(u32, u32)>,
    pub frames_received: u64,
    pub frames_shown: u64,
    pub keyframes: u64,
    pub bytes: u64,
    /// Shown frames per second and megabits per second over the last second.
    pub fps: f64,
    pub mbps: f64,
    /// From a frame's arrival in the client to its present, averaged.
    pub latency_ms: f64,
    /// Decoder errors that made the window ask for a keyframe.
    pub resets: u64,
    /// The window has sound, and its Opus packets played so far.
    pub audio: bool,
    pub audio_packets: u64,
    /// Why its sound stopped, if it did.
    pub audio_error: Option<String>,
    /// Set when the window closed (by the user, the stream ending, or an
    /// error, in `error`).
    pub closed: bool,
    pub error: Option<String>,
}

/// Shared between a stream window and whoever opened it.
#[derive(Default)]
pub struct Shared {
    pub stats: Mutex<StreamStats>,
    /// Pointer and keys go to the host while this is set.
    pub send_input: AtomicBool,
    /// The window's sound is silenced while this is set.
    pub muted: AtomicBool,
}

pub type SharedStats = Arc<Shared>;

/// The displays, by Windows' number.
#[cfg(windows)]
pub fn displays() -> Vec<Display> {
    window::displays()
}

#[cfg(not(windows))]
pub fn displays() -> Vec<Display> {
    Vec::new()
}

/// The codecs this PC has a decoder for, most preferred first (H.265,
/// H.264, AV1).
#[cfg(windows)]
pub fn decodable() -> Vec<windowcast_protocol::VideoCodec> {
    decoder::decodable()
}

#[cfg(not(windows))]
pub fn decodable() -> Vec<windowcast_protocol::VideoCodec> {
    Vec::new()
}
