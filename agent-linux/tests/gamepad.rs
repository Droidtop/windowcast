//! A pad made through uinput and read back from its evdev node: the
//! buttons, sticks, triggers and d-pad a client sets arrive as xpad would
//! report them, and removing the pad removes the device. Needs write access
//! to /dev/uinput and read access to /dev/input; skipped without them
//! unless WINDOWCAST_TEST_UINPUT is set, which makes it fail instead.
#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use evdev::{AbsoluteAxisCode, Device, KeyCode};
use windowcast_agent_linux::gamepad::{pad_name, LinuxPads};
use windowcast_host::gamepad::GamepadSink;
use windowcast_protocol::{GamepadButtons, GamepadState};

fn find(name: &str) -> Option<Device> {
    evdev::enumerate()
        .map(|(_, device)| device)
        .find(|device| device.name() == Some(name))
}

#[test]
fn a_pad_reads_back_through_evdev() {
    let required = std::env::var_os("WINDOWCAST_TEST_UINPUT").is_some();
    let mut pads = match LinuxPads::open() {
        Ok(pads) => pads,
        Err(e) if !required => {
            println!("skipped: {e}");
            return;
        }
        Err(e) => panic!("{e}"),
    };
    // Pad 4, the last slot.
    let pad = 3;
    let name = pad_name(pad);
    pads.set(
        pad,
        &GamepadState {
            buttons: GamepadButtons::B | GamepadButtons::RIGHT_SHOULDER | GamepadButtons::DPAD_UP,
            left_x: -20000,
            left_y: 20000,
            right_x: 300,
            right_y: -32768,
            left_trigger: 10,
            right_trigger: 255,
        },
    );
    let start = Instant::now();
    let device = loop {
        match find(&name) {
            Some(device) => break device,
            None if start.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(50))
            }
            None if !required => {
                println!("skipped: made {name} but cannot read it from /dev/input");
                return;
            }
            None => panic!("{name} did not appear in /dev/input"),
        }
    };
    assert_eq!(device.input_id().vendor(), 0x045e);
    assert_eq!(device.input_id().product(), 0x028e);
    // The state set before the node was opened is the device's state now.
    let keys = device.get_key_state().unwrap();
    assert!(keys.contains(KeyCode::BTN_EAST));
    assert!(keys.contains(KeyCode::BTN_TR));
    assert!(!keys.contains(KeyCode::BTN_SOUTH));
    let abs = device.get_abs_state().unwrap();
    let at = |axis: AbsoluteAxisCode| abs[usize::from(axis.0)].value;
    assert_eq!(at(AbsoluteAxisCode::ABS_X), -20000);
    assert_eq!(at(AbsoluteAxisCode::ABS_Y), !20000);
    assert_eq!(at(AbsoluteAxisCode::ABS_RX), 300);
    assert_eq!(at(AbsoluteAxisCode::ABS_RY), 32767);
    assert_eq!(at(AbsoluteAxisCode::ABS_Z), 10);
    assert_eq!(at(AbsoluteAxisCode::ABS_RZ), 255);
    assert_eq!(at(AbsoluteAxisCode::ABS_HAT0Y), -1);
    println!("{name} reads back: B, RB, up, sticks and triggers as set");

    // Released.
    pads.set(pad, &GamepadState::default());
    let keys = device.get_key_state().unwrap();
    assert!(keys.iter().next().is_none(), "keys still down: {keys:?}");

    drop(device);
    pads.remove(pad);
    let start = Instant::now();
    while find(&name).is_some() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{name} stayed after removal"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
