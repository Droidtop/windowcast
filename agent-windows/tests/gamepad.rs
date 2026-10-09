//! A pad plugged into ViGEmBus and read back through XInput, then
//! unplugged. Plugging a pad in is a real device arrival: Windows plays its
//! hardware sounds and running games see a pad for a moment, so this runs
//! only when WINDOWCAST_TEST_VIGEM is set.
#![cfg(windows)]

use std::time::{Duration, Instant};

use windowcast_agent_windows::gamepad::WindowsPads;
use windowcast_host::gamepad::GamepadSink;
use windowcast_protocol::{GamepadButtons, GamepadState};
use windows::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};

fn read(slot: u32) -> Option<XINPUT_STATE> {
    let mut state = XINPUT_STATE::default();
    (unsafe { XInputGetState(slot, &mut state) } == 0).then_some(state)
}

#[test]
fn a_pad_reads_back_through_xinput() {
    if std::env::var_os("WINDOWCAST_TEST_VIGEM").is_none() {
        println!("skipped: plugs in a pad (device sounds); set WINDOWCAST_TEST_VIGEM to run");
        return;
    }
    let mut pads = WindowsPads::open().unwrap();
    let state = GamepadState {
        buttons: GamepadButtons::B | GamepadButtons::RIGHT_SHOULDER | GamepadButtons::DPAD_UP,
        left_x: -20000,
        left_y: 20000,
        right_x: 300,
        right_y: -32768,
        left_trigger: 10,
        right_trigger: 255,
    };
    pads.set(0, &state);
    let slot = pads.user_index(0).expect("the pad has no XInput slot");
    let start = Instant::now();
    let got = loop {
        if let Some(got) = read(slot).filter(|s| s.Gamepad.wButtons.0 != 0) {
            break got;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "XInput slot {slot} never showed the pad"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let pad = got.Gamepad;
    println!(
        "XInput slot {slot}: buttons {:#06x}, sticks ({}, {}) ({}, {}), triggers {} {}",
        pad.wButtons.0,
        pad.sThumbLX,
        pad.sThumbLY,
        pad.sThumbRX,
        pad.sThumbRY,
        pad.bLeftTrigger,
        pad.bRightTrigger
    );
    assert_eq!(u32::from(pad.wButtons.0), state.buttons);
    assert_eq!(
        (pad.sThumbLX, pad.sThumbLY, pad.sThumbRX, pad.sThumbRY),
        (-20000, 20000, 300, -32768)
    );
    assert_eq!((pad.bLeftTrigger, pad.bRightTrigger), (10, 255));

    pads.remove(0);
    let start = Instant::now();
    while read(slot).is_some() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "slot {slot} still connected"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
