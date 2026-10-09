//! Virtual gamepads: the client's controllers, up to four, appear on the
//! host as Xbox-layout pads that games read like real ones. A session gets
//! its pads when its first gamepad event arrives and loses them when it
//! ends.

use windowcast_protocol::GamepadState;

/// The most pads one session drives.
pub const MAX_PADS: u8 = 4;

/// One session's virtual pads. Lives on the session's input thread;
/// dropping it unplugs every pad it made.
pub trait GamepadSink {
    /// Sets pad `pad`'s whole state, plugging the pad in first if it is
    /// new. `pad` is below [`MAX_PADS`].
    fn set(&mut self, pad: u8, state: &GamepadState);
    /// Unplugs pad `pad`, if it is plugged in.
    fn remove(&mut self, pad: u8);
}
