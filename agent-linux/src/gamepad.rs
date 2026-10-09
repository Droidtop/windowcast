//! The client's gamepads on Linux: one uinput device per pad, laid out as
//! the kernel's xpad driver lays out an Xbox 360 pad (same buttons, axes,
//! ranges and USB ids), so SDL, Steam and games map it like the real one.
//! Needs write access to /dev/uinput (root, or a udev rule giving it to a
//! group the host's user is in).

use std::fs::OpenOptions;

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputEvent, InputId, KeyCode,
    UinputAbsSetup,
};
use windowcast_host::gamepad::{GamepadSink, MAX_PADS};
use windowcast_protocol::{GamepadButtons, GamepadState};

pub const UINPUT: &str = "/dev/uinput";

/// Button bits and the keys they press, as xpad reports them.
const BUTTONS: [(u32, KeyCode); 11] = [
    (GamepadButtons::A, KeyCode::BTN_SOUTH),
    (GamepadButtons::B, KeyCode::BTN_EAST),
    (GamepadButtons::X, KeyCode::BTN_NORTH),
    (GamepadButtons::Y, KeyCode::BTN_WEST),
    (GamepadButtons::LEFT_SHOULDER, KeyCode::BTN_TL),
    (GamepadButtons::RIGHT_SHOULDER, KeyCode::BTN_TR),
    (GamepadButtons::BACK, KeyCode::BTN_SELECT),
    (GamepadButtons::START, KeyCode::BTN_START),
    (GamepadButtons::GUIDE, KeyCode::BTN_MODE),
    (GamepadButtons::LEFT_THUMB, KeyCode::BTN_THUMBL),
    (GamepadButtons::RIGHT_THUMB, KeyCode::BTN_THUMBR),
];

/// The name pad `pad` (0 to 3) gets.
pub fn pad_name(pad: u8) -> String {
    format!("windowcast gamepad {}", pad + 1)
}

/// One session's pads.
pub struct LinuxPads {
    pads: [Option<VirtualDevice>; MAX_PADS as usize],
}

impl LinuxPads {
    /// Checks that pads can be made at all, so a host without access says
    /// why once instead of failing at every event.
    pub fn open() -> Result<Self, String> {
        OpenOptions::new().write(true).open(UINPUT).map_err(|e| {
            format!(
                "gamepads need write access to {UINPUT} ({e}); a udev rule can give it to the host's user"
            )
        })?;
        Ok(LinuxPads {
            pads: Default::default(),
        })
    }
}

fn make(pad: u8) -> std::io::Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for (_, key) in BUTTONS {
        keys.insert(key);
    }
    let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
    let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
    let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
    let name = pad_name(pad);
    let mut builder = VirtualDevice::builder()?
        .name(&name)
        // The wired Xbox 360 pad's ids, which game controller databases know.
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
        .with_keys(&keys)?;
    for (axis, info) in [
        (AbsoluteAxisCode::ABS_X, stick),
        (AbsoluteAxisCode::ABS_Y, stick),
        (AbsoluteAxisCode::ABS_RX, stick),
        (AbsoluteAxisCode::ABS_RY, stick),
        (AbsoluteAxisCode::ABS_Z, trigger),
        (AbsoluteAxisCode::ABS_RZ, trigger),
        (AbsoluteAxisCode::ABS_HAT0X, hat),
        (AbsoluteAxisCode::ABS_HAT0Y, hat),
    ] {
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
    }
    builder.build()
}

/// The events that report `state`. Sticks are up positive in windowcast
/// and down positive in evdev; xpad flips them with a bitwise not, which
/// maps the whole range onto itself.
pub fn events(state: &GamepadState) -> Vec<InputEvent> {
    let key = |code: KeyCode, on: bool| InputEvent::new(EventType::KEY.0, code.0, i32::from(on));
    let abs =
        |axis: AbsoluteAxisCode, value: i32| InputEvent::new(EventType::ABSOLUTE.0, axis.0, value);
    let pressed = |bit: u32| state.buttons & bit != 0;
    let hat = |minus: u32, plus: u32| i32::from(pressed(plus)) - i32::from(pressed(minus));
    let mut events: Vec<InputEvent> = BUTTONS
        .iter()
        .map(|(bit, code)| key(*code, pressed(*bit)))
        .collect();
    events.extend([
        abs(AbsoluteAxisCode::ABS_X, i32::from(state.left_x)),
        abs(AbsoluteAxisCode::ABS_Y, i32::from(!state.left_y)),
        abs(AbsoluteAxisCode::ABS_RX, i32::from(state.right_x)),
        abs(AbsoluteAxisCode::ABS_RY, i32::from(!state.right_y)),
        abs(AbsoluteAxisCode::ABS_Z, i32::from(state.left_trigger)),
        abs(AbsoluteAxisCode::ABS_RZ, i32::from(state.right_trigger)),
        abs(
            AbsoluteAxisCode::ABS_HAT0X,
            hat(GamepadButtons::DPAD_LEFT, GamepadButtons::DPAD_RIGHT),
        ),
        abs(
            AbsoluteAxisCode::ABS_HAT0Y,
            hat(GamepadButtons::DPAD_UP, GamepadButtons::DPAD_DOWN),
        ),
    ]);
    events
}

impl GamepadSink for LinuxPads {
    fn set(&mut self, pad: u8, state: &GamepadState) {
        let Some(slot) = self.pads.get_mut(usize::from(pad)) else {
            return;
        };
        if slot.is_none() {
            match make(pad) {
                Ok(device) => *slot = Some(device),
                Err(e) => {
                    eprintln!("gamepad {}: {e}", pad + 1);
                    return;
                }
            }
        }
        if let Some(device) = slot.as_mut() {
            if let Err(e) = device.emit(&events(state)) {
                eprintln!("gamepad {}: {e}", pad + 1);
            }
        }
    }

    fn remove(&mut self, pad: u8) {
        if let Some(slot) = self.pads.get_mut(usize::from(pad)) {
            // Dropping the device destroys it.
            slot.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(events: &[InputEvent], kind: EventType, code: u16) -> i32 {
        events
            .iter()
            .find(|e| e.event_type() == kind && e.code() == code)
            .map(|e| e.value())
            .unwrap()
    }

    #[test]
    fn the_state_maps_as_xpad_reports_it() {
        let events = events(&GamepadState {
            buttons: GamepadButtons::A
                | GamepadButtons::Y
                | GamepadButtons::DPAD_LEFT
                | GamepadButtons::DPAD_DOWN,
            left_x: 12000,
            left_y: 32767,
            right_x: -32768,
            right_y: -32768,
            left_trigger: 200,
            right_trigger: 0,
        });
        let abs = |axis: AbsoluteAxisCode| value(&events, EventType::ABSOLUTE, axis.0);
        assert_eq!(value(&events, EventType::KEY, KeyCode::BTN_SOUTH.0), 1);
        assert_eq!(value(&events, EventType::KEY, KeyCode::BTN_WEST.0), 1);
        assert_eq!(value(&events, EventType::KEY, KeyCode::BTN_EAST.0), 0);
        assert_eq!(abs(AbsoluteAxisCode::ABS_X), 12000);
        // Full up is the bottom of evdev's range, full down the top.
        assert_eq!(abs(AbsoluteAxisCode::ABS_Y), -32768);
        assert_eq!(abs(AbsoluteAxisCode::ABS_RY), 32767);
        assert_eq!(abs(AbsoluteAxisCode::ABS_RX), -32768);
        assert_eq!(abs(AbsoluteAxisCode::ABS_Z), 200);
        assert_eq!(abs(AbsoluteAxisCode::ABS_HAT0X), -1);
        assert_eq!(abs(AbsoluteAxisCode::ABS_HAT0Y), 1);
    }
}
