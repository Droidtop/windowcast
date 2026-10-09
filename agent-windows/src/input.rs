//! Input from the client, injected with SendInput: the pointer in screen
//! coordinates mapped from the window's captured picture, keys as scan
//! codes (the evdev keycodes windowcast sends are the XT set-1 codes for
//! the main block), typed text as Unicode characters. A touch drives the
//! pointer (the first finger only). Gamepads need a virtual gamepad driver
//! Windows does not have built in, so they are not delivered yet.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use windowcast_protocol::{InputEvent, PointerButton, TouchPhase, WindowId};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, SetForegroundWindow, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};

use crate::windows_list::hwnd;

/// One wheel notch, as Windows counts it.
const WHEEL_DELTA: f32 = 120.0;

#[derive(Default)]
pub struct Injector {
    /// The touch that drives the pointer while it is down.
    primary_touch: Mutex<Option<u32>>,
    gamepad_noted: AtomicBool,
}

impl Injector {
    pub fn inject(&self, event: &InputEvent, focus: Option<WindowId>) {
        match event {
            InputEvent::PointerMove { window, x, y } => move_to(*window, *x, *y),
            InputEvent::PointerButton {
                window,
                button,
                pressed,
            } => {
                if *pressed {
                    bring_forward(*window);
                }
                send(&[mouse(
                    0,
                    0,
                    button_data(*button),
                    button_flags(*button, *pressed),
                )]);
            }
            InputEvent::PointerScroll { dy, dx, .. } => {
                let mut inputs = Vec::new();
                if *dy != 0.0 {
                    inputs.push(mouse(
                        0,
                        0,
                        (*dy * WHEEL_DELTA) as i32 as u32,
                        MOUSEEVENTF_WHEEL,
                    ));
                }
                if *dx != 0.0 {
                    inputs.push(mouse(
                        0,
                        0,
                        (*dx * WHEEL_DELTA) as i32 as u32,
                        MOUSEEVENTF_HWHEEL,
                    ));
                }
                send(&inputs);
            }
            InputEvent::Key { keycode, pressed } => {
                if let Some(focus) = focus {
                    bring_forward(focus);
                }
                if let Some((scan, extended)) = scan_code(*keycode) {
                    let mut flags = KEYEVENTF_SCANCODE;
                    if extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    if !pressed {
                        flags |= KEYEVENTF_KEYUP;
                    }
                    send(&[key(scan, flags)]);
                }
            }
            InputEvent::Text { text } => {
                if let Some(focus) = focus {
                    bring_forward(focus);
                }
                let inputs: Vec<INPUT> = text
                    .encode_utf16()
                    .flat_map(|unit| {
                        [
                            key(unit, KEYEVENTF_UNICODE),
                            key(unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                        ]
                    })
                    .collect();
                send(&inputs);
            }
            InputEvent::Touch {
                window,
                id,
                x,
                y,
                phase,
            } => self.touch(*window, *id, *x, *y, *phase),
            InputEvent::Gamepad { .. } | InputEvent::GamepadGone { .. } => {
                if !self.gamepad_noted.swap(true, Ordering::SeqCst) {
                    eprintln!("gamepad input is not delivered on Windows yet (needs a virtual gamepad driver)");
                }
            }
        }
    }

    fn touch(&self, window: WindowId, id: u32, x: f32, y: f32, phase: TouchPhase) {
        let mut primary = self.primary_touch.lock().expect("touch state");
        match phase {
            TouchPhase::Start if primary.is_none() => {
                *primary = Some(id);
                bring_forward(window);
                move_to(window, x, y);
                send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTDOWN)]);
            }
            TouchPhase::Move if *primary == Some(id) => move_to(window, x, y),
            TouchPhase::End | TouchPhase::Cancel if *primary == Some(id) => {
                *primary = None;
                move_to(window, x, y);
                send(&[mouse(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
            }
            _ => {}
        }
    }
}

fn send(inputs: &[INPUT]) {
    if !inputs.is_empty() {
        unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    }
}

fn mouse(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn button_flags(button: PointerButton, pressed: bool) -> MOUSE_EVENT_FLAGS {
    match (button, pressed) {
        (PointerButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (PointerButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (PointerButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (PointerButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (PointerButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (PointerButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        (_, true) => MOUSEEVENTF_XDOWN,
        (_, false) => MOUSEEVENTF_XUP,
    }
}

fn button_data(button: PointerButton) -> u32 {
    match button {
        PointerButton::Back => u32::from(XBUTTON1),
        PointerButton::Forward => u32::from(XBUTTON2),
        _ => 0,
    }
}

/// The window's captured area on screen: its visible bounds, which is what
/// Windows.Graphics.Capture records.
fn bounds(window: HWND) -> Option<RECT> {
    let mut rect = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    Some(rect)
}

/// Screen position of a point given in the window's picture, [0, 1] each way.
pub fn screen_point(rect: &RECT, x: f32, y: f32) -> (i32, i32) {
    let x = x.clamp(0.0, 1.0);
    let y = y.clamp(0.0, 1.0);
    (
        rect.left + ((rect.right - rect.left - 1) as f32 * x).round() as i32,
        rect.top + ((rect.bottom - rect.top - 1) as f32 * y).round() as i32,
    )
}

fn move_to(window: WindowId, x: f32, y: f32) {
    let Some(rect) = bounds(hwnd(window)) else {
        return;
    };
    let (sx, sy) = screen_point(&rect, x, y);
    // Absolute coordinates span the whole virtual desktop, 0 to 65535.
    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
            GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
        )
    };
    let ax = ((sx - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
    let ay = ((sy - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
    send(&[mouse(
        ax,
        ay,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )]);
}

fn bring_forward(window: WindowId) {
    let target = hwnd(window);
    unsafe {
        if GetForegroundWindow() != target {
            let _ = SetForegroundWindow(target);
        }
    }
}

/// evdev keycode to set-1 scan code and whether it is an extended (E0) key.
/// The main block (1 to 88) is the same number in both.
pub fn scan_code(keycode: u32) -> Option<(u16, bool)> {
    let extended = match keycode {
        96 => 0x1c,  // keypad Enter
        97 => 0x1d,  // right Ctrl
        98 => 0x35,  // keypad /
        99 => 0x37,  // Print Screen
        100 => 0x38, // right Alt
        102 => 0x47, // Home
        103 => 0x48, // Up
        104 => 0x49, // Page Up
        105 => 0x4b, // Left
        106 => 0x4d, // Right
        107 => 0x4f, // End
        108 => 0x50, // Down
        109 => 0x51, // Page Down
        110 => 0x52, // Insert
        111 => 0x53, // Delete
        125 => 0x5b, // left Windows
        126 => 0x5c, // right Windows
        127 => 0x5d, // Menu
        1..=88 => return Some((keycode as u16, false)),
        _ => return None,
    };
    Some((extended, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_block_keys_keep_their_number_and_arrows_are_extended() {
        assert_eq!(scan_code(30), Some((0x1e, false))); // A
        assert_eq!(scan_code(28), Some((0x1c, false))); // Enter
        assert_eq!(scan_code(103), Some((0x48, true))); // Up
        assert_eq!(scan_code(111), Some((0x53, true))); // Delete
        assert_eq!(scan_code(500), None);
    }

    #[test]
    fn picture_corners_map_to_the_window_corners() {
        let rect = RECT {
            left: 100,
            top: 50,
            right: 420,
            bottom: 290,
        };
        assert_eq!(screen_point(&rect, 0.0, 0.0), (100, 50));
        assert_eq!(screen_point(&rect, 1.0, 1.0), (419, 289));
        assert_eq!(screen_point(&rect, 2.0, -1.0), (419, 50));
    }
}
