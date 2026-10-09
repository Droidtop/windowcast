//! Per-window pixel capture and encode — NOT implemented yet.
//!
//! Real state of the world: `vendor/wlroots` (as consumed by droidtop's
//! own `host-bridge` module) only ships `wlr-screencopy-unstable-v1.xml`
//! (whole-*output* capture) and `wlr-foreign-toplevel-management-
//! unstable-v1.xml` (listing/control, not capture). True per-toplevel
//! capture needs `ext-image-copy-capture-v1` with its toplevel capture
//! source, which is the next slice of work here (#105).
//!
//! This is flagged deliberately rather than papered over with a
//! whole-output capture masquerading as "the window": cutting a window out
//! of a whole-output capture is the `Desktop` backend's job
//! (docs/BACKENDS.md), not this one's.

use windowcast_host::FrameSource;
use windowcast_protocol::{VideoCodec, WindowId};

pub const NOT_IMPLEMENTED: &str =
    "per-window capture is not implemented yet on Linux (needs ext-image-copy-capture-v1)";

pub fn open(_window: WindowId, _codec: VideoCodec) -> Result<Box<dyn FrameSource>, String> {
    Err(NOT_IMPLEMENTED.to_owned())
}
