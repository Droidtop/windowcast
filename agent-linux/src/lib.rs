//! windowcast's Linux/Wayland host agent: the Linux end of the windowcast
//! library. `windowcast-host` does the serving (pairing, sessions,
//! streams); this crate supplies the compositor's windows
//! (ext-foreign-toplevel-list), captures one with ext-image-copy-capture
//! (or, for the desktop backend, its output with the window cut out),
//! encodes it with OpenH264, and delivers input through a virtual pointer
//! and keyboard. Window positions and focus come from sway's IPC, so input
//! and the desktop backend need sway; listing and capture work on any
//! compositor with the ext protocols (wlroots 0.19 and later, sway 1.11 and
//! later).

pub mod capture;
pub mod input;
mod source;
pub mod sway;
pub mod toplevels;

pub use source::{LinuxSource, Options};
