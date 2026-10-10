//! windowcast's Windows host agent: the Windows end of the windowcast
//! library. `windowcast-host` does the serving (pairing, sessions,
//! streams); this crate supplies the desktop's windows, captures one with
//! Windows.Graphics.Capture and encodes it (encoder.rs: Media Foundation
//! hardware or software, or OpenH264).

pub mod convert;

#[cfg(windows)]
pub mod audio;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod clipboard;
#[cfg(windows)]
pub mod cursor;
#[cfg(windows)]
pub mod encoder;
#[cfg(windows)]
pub mod gamepad;
#[cfg(windows)]
mod gpu_convert;
#[cfg(windows)]
pub mod input;
#[cfg(windows)]
pub mod microphone;
#[cfg(windows)]
mod source;
#[cfg(windows)]
pub mod windows_list;

#[cfg(windows)]
pub use source::{Options, WindowsSource};
