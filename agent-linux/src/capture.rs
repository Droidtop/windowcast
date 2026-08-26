//! Per-window pixel capture and encode — NOT implemented yet.
//!
//! Real state of the world: `vendor/wlroots` (as consumed by droidtop's
//! own `host-bridge` module) only ships `wlr-screencopy-unstable-v1.xml`
//! (whole-*output* capture) and `wlr-foreign-toplevel-management-
//! unstable-v1.xml` (listing/control, not capture). True per-toplevel
//! capture needs `ext-image-copy-capture-v1` (or a compositor-specific
//! equivalent), which isn't vendored anywhere in this project yet and
//! doesn't have mature Rust bindings in `wayland-protocols-wlr` at the
//! time this was written.
//!
//! This is flagged here deliberately rather than papered over with a
//! whole-output capture masquerading as "the window" — see the project
//! plan's honest-fallback note. The real next slice of work is: vendor
//! `ext-image-copy-capture-v1`, generate bindings via `wayland-scanner`,
//! and implement `capture_window` for real; until then, calling this is a
//! documented, deliberate `todo!()`, not a silent no-op.

// Not wired into main.rs yet (see module docs above) — allowed dead code
// rather than a warning that could be mistaken for an oversight.
#![allow(dead_code)]

use windowcast_protocol::WindowId;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("per-window capture is not implemented yet (needs ext-image-copy-capture-v1)")]
    NotImplemented,
}

pub struct CapturedFrame {
    pub width: u32,
    pub height: u32,
    /// Encoded H.264 bytes, once an encoder is wired in.
    pub data: Vec<u8>,
}

pub fn capture_window(_window: WindowId) -> Result<CapturedFrame, CaptureError> {
    Err(CaptureError::NotImplemented)
}
