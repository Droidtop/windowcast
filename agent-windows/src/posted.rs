//! Input while this session's input desktop is out of reach (the PC is
//! locked, or UAC's secure desktop is up), when SendInput is refused: each
//! event is posted to the window as the message it would have become
//! (docs/BACKENDS.md, "A locked host"). The pointer goes to the child
//! window under the point, in that window's client coordinates; keys go to
//! the window with the keyboard focus in the target's thread (or the one
//! last clicked), as WM_KEYDOWN/WM_KEYUP with their real scan codes; typed
//! text goes as WM_CHAR.
//!
//! Best effort, with known limits: programs that read raw input,
//! DirectInput or the key state (GetKeyState, GetAsyncKeyState) do not see
//! it (most games among them, and some Chromium and UWP controls), clicks
//! on a window's frame (title bar, borders) do nothing, and Shift does not
//! change the characters a posted key makes (text typed as text does).

use std::sync::Mutex;

use windowcast_protocol::{InputEvent, PointerButton, TouchPhase, WindowId};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VSC_TO_VK_EX};
use windows::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPointEx, GetGUIThreadInfo, GetWindowThreadProcessId, IsChild, IsWindow,
    PostMessageW, CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, GUITHREADINFO, WM_CHAR,
    WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1, XBUTTON2,
};

use crate::input::{bounds, screen_point};
use crate::windows_list::hwnd;

// The key and button state a mouse message carries in its wParam.
const MK_LBUTTON: usize = 0x1;
const MK_RBUTTON: usize = 0x2;
const MK_SHIFT: usize = 0x4;
const MK_CONTROL: usize = 0x8;
const MK_MBUTTON: usize = 0x10;
const MK_XBUTTON1: usize = 0x20;
const MK_XBUTTON2: usize = 0x40;

/// One wheel notch, as Windows counts it.
const WHEEL_DELTA: f32 = 120.0;

#[derive(Default)]
pub struct Posted {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Where the client last put the pointer: a window and a point in its
    /// picture, [0, 1] each way.
    pointer: Option<(WindowId, f32, f32)>,
    /// Mouse buttons held down, as MK_ flags.
    buttons: usize,
    shift: bool,
    control: bool,
    alt: bool,
    /// The touch that drives the pointer while it is down.
    touch: Option<u32>,
    /// The child window last clicked, where keys go when its thread names
    /// no focus.
    clicked: Option<HWND>,
}

// HWNDs are plain handles, usable from any thread.
unsafe impl Send for State {}

/// Where a pointer message goes: the deepest enabled, visible child under
/// the point, and the point on the screen and in that child.
struct Target {
    window: HWND,
    screen: POINT,
    client: POINT,
}

impl Posted {
    /// Keeps the pointer's place while input goes through SendInput too, so
    /// a click right after the PC locks lands where the pointer was.
    pub fn remember_pointer(&self, window: WindowId, x: f32, y: f32) {
        self.state.lock().expect("posted input").pointer = Some((window, x, y));
    }

    pub fn inject(&self, event: &InputEvent, focus: Option<WindowId>) {
        let mut state = self.state.lock().expect("posted input");
        match event {
            InputEvent::PointerMove { window, x, y } => {
                state.pointer = Some((*window, *x, *y));
                if let Some(target) = target(*window, *x, *y) {
                    post(
                        target.window,
                        WM_MOUSEMOVE,
                        state.mouse_keys(),
                        client(&target),
                    );
                }
            }
            InputEvent::PointerButton {
                window,
                button,
                pressed,
            } => {
                // A button press without a position first would land on a
                // guess; it is dropped instead.
                let Some((_, x, y)) = state.pointer.filter(|(w, _, _)| w == window) else {
                    return;
                };
                state.button(*window, x, y, *button, *pressed);
            }
            InputEvent::PointerScroll { window, dx, dy } => {
                let (x, y) = match state.pointer {
                    Some((w, x, y)) if w == *window => (x, y),
                    _ => (0.5, 0.5),
                };
                let Some(target) = target(*window, x, y) else {
                    return;
                };
                for (delta, message) in [(*dy, WM_MOUSEWHEEL), (*dx, WM_MOUSEHWHEEL)] {
                    if delta != 0.0 {
                        let wheel = (delta * WHEEL_DELTA) as i32 as i16 as u16 as usize;
                        let wparam = wheel << 16 | state.mouse_keys();
                        post(target.window, message, wparam, point(target.screen));
                    }
                }
            }
            InputEvent::Key { keycode, pressed } => {
                let Some(focus) = focus else {
                    return;
                };
                let Some((scan, extended)) = windowcast_protocol::keys::scan_from_evdev(*keycode)
                else {
                    return;
                };
                let code = u32::from(scan) | if extended { 0xe000 } else { 0 };
                let vk = generic_key(unsafe { MapVirtualKeyW(code, MAPVK_VSC_TO_VK_EX) });
                match vk {
                    VK_SHIFT => state.shift = *pressed,
                    VK_CONTROL => state.control = *pressed,
                    VK_MENU => state.alt = *pressed,
                    _ => {}
                }
                // Alt (without Ctrl) makes system keys, as Windows does.
                let system = state.alt && !state.control || vk == VK_MENU;
                let message = match (system, *pressed) {
                    (false, true) => WM_KEYDOWN,
                    (false, false) => WM_KEYUP,
                    (true, true) => WM_SYSKEYDOWN,
                    (true, false) => WM_SYSKEYUP,
                };
                let lparam = key_lparam(scan, extended, state.alt, *pressed);
                post(state.keyboard_target(focus), message, vk as usize, lparam);
            }
            InputEvent::Text { text } => {
                let Some(focus) = focus else {
                    return;
                };
                let target = state.keyboard_target(focus);
                for unit in text.encode_utf16() {
                    post(target, WM_CHAR, usize::from(unit), LPARAM(1));
                }
            }
            InputEvent::Touch {
                window,
                id,
                x,
                y,
                phase,
            } => match phase {
                TouchPhase::Start if state.touch.is_none() => {
                    state.touch = Some(*id);
                    state.pointer = Some((*window, *x, *y));
                    state.button(*window, *x, *y, PointerButton::Left, true);
                }
                TouchPhase::Move if state.touch == Some(*id) => {
                    state.pointer = Some((*window, *x, *y));
                    if let Some(target) = target(*window, *x, *y) {
                        post(
                            target.window,
                            WM_MOUSEMOVE,
                            state.mouse_keys(),
                            client(&target),
                        );
                    }
                }
                TouchPhase::End | TouchPhase::Cancel if state.touch == Some(*id) => {
                    state.touch = None;
                    state.pointer = Some((*window, *x, *y));
                    state.button(*window, *x, *y, PointerButton::Left, false);
                }
                _ => {}
            },
            // Delivered to the session's pads by host-core.
            InputEvent::Gamepad { .. } | InputEvent::GamepadGone { .. } => {}
        }
    }
}

const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;

/// The key code a key message carries: Shift, Ctrl and Alt by their
/// generic codes, as Windows sends them, not their left and right ones.
fn generic_key(vk: u32) -> u32 {
    match vk {
        0xa0 | 0xa1 => VK_SHIFT,
        0xa2 | 0xa3 => VK_CONTROL,
        0xa4 | 0xa5 => VK_MENU,
        other => other,
    }
}

/// A key message's lParam: one repeat, the scan code, the extended flag,
/// Alt held, and for a release the previous-state and transition bits.
pub fn key_lparam(scan: u16, extended: bool, alt: bool, pressed: bool) -> LPARAM {
    let mut value = 1u32 | u32::from(scan & 0xff) << 16;
    if extended {
        value |= 1 << 24;
    }
    if alt {
        value |= 1 << 29;
    }
    if !pressed {
        value |= 1 << 30 | 1 << 31;
    }
    LPARAM(value as isize)
}

impl State {
    fn mouse_keys(&self) -> usize {
        let mut keys = self.buttons;
        if self.shift {
            keys |= MK_SHIFT;
        }
        if self.control {
            keys |= MK_CONTROL;
        }
        keys
    }

    fn button(&mut self, window: WindowId, x: f32, y: f32, button: PointerButton, pressed: bool) {
        let Some(target) = target(window, x, y) else {
            return;
        };
        let (down, up, flag, extra) = match button {
            PointerButton::Left => (WM_LBUTTONDOWN, WM_LBUTTONUP, MK_LBUTTON, 0),
            PointerButton::Right => (WM_RBUTTONDOWN, WM_RBUTTONUP, MK_RBUTTON, 0),
            PointerButton::Middle => (WM_MBUTTONDOWN, WM_MBUTTONUP, MK_MBUTTON, 0),
            PointerButton::Back => (WM_XBUTTONDOWN, WM_XBUTTONUP, MK_XBUTTON1, XBUTTON1),
            PointerButton::Forward => (WM_XBUTTONDOWN, WM_XBUTTONUP, MK_XBUTTON2, XBUTTON2),
        };
        if pressed {
            self.buttons |= flag;
            self.clicked = Some(target.window);
        } else {
            self.buttons &= !flag;
        }
        let wparam = usize::from(extra) << 16 | self.mouse_keys();
        post(
            target.window,
            if pressed { down } else { up },
            wparam,
            client(&target),
        );
    }

    /// Where keys for `focus` go: the window with the keyboard focus in its
    /// thread when that is inside it, else the child last clicked inside
    /// it, else the window itself.
    fn keyboard_target(&self, focus: WindowId) -> HWND {
        let top = hwnd(focus);
        let inside =
            |w: HWND| !w.is_invalid() && (w == top || unsafe { IsChild(top, w) }.as_bool());
        let thread = unsafe { GetWindowThreadProcessId(top, None) };
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetGUIThreadInfo(thread, &mut info) }.is_ok() && inside(info.hwndFocus) {
            return info.hwndFocus;
        }
        match self.clicked {
            Some(clicked) if inside(clicked) && unsafe { IsWindow(Some(clicked)) }.as_bool() => {
                clicked
            }
            _ => top,
        }
    }
}

fn target(window: WindowId, x: f32, y: f32) -> Option<Target> {
    let top = hwnd(window);
    let (sx, sy) = screen_point(&bounds(top)?, x, y);
    let screen = POINT { x: sx, y: sy };
    let mut current = top;
    // Down through the children under the point, each asked in its
    // parent's client coordinates.
    for _ in 0..32 {
        let mut local = screen;
        let _ = unsafe { ScreenToClient(current, &mut local) };
        let child = unsafe {
            ChildWindowFromPointEx(
                current,
                local,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        if child.is_invalid() || child == current {
            break;
        }
        current = child;
    }
    let mut client = screen;
    let _ = unsafe { ScreenToClient(current, &mut client) };
    Some(Target {
        window: current,
        screen,
        client,
    })
}

/// A point as mouse messages carry it: x in the low word, y in the high.
fn point(p: POINT) -> LPARAM {
    let x = p.x as i16 as u16 as u32;
    let y = p.y as i16 as u16 as u32;
    LPARAM((y << 16 | x) as isize)
}

fn client(target: &Target) -> LPARAM {
    point(target.client)
}

fn post(window: HWND, message: u32, wparam: usize, lparam: LPARAM) {
    let _ = unsafe { PostMessageW(Some(window), message, WPARAM(wparam), lparam) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_messages_carry_the_scan_code_and_release_bits() {
        // 'A' pressed: one repeat, scan 0x1e.
        assert_eq!(key_lparam(0x1e, false, false, true).0, 0x001e_0001);
        // Up arrow released: extended, previous state and transition set.
        assert_eq!(key_lparam(0x48, true, false, false).0 as u32, 0xc148_0001);
        // F4 with Alt held.
        assert_eq!(key_lparam(0x3e, false, true, true).0, 0x203e_0001);
    }

    #[test]
    fn points_pack_low_word_x_high_word_y() {
        assert_eq!(point(POINT { x: 160, y: 120 }).0, 120 << 16 | 160);
        // Negative coordinates (a monitor left of the main one) keep their sign per word.
        assert_eq!(point(POINT { x: -2, y: 3 }).0 as u32, 3 << 16 | 0xfffe);
    }

    #[test]
    fn left_and_right_modifiers_become_generic() {
        assert_eq!(generic_key(0xa1), VK_SHIFT);
        assert_eq!(generic_key(0xa2), VK_CONTROL);
        assert_eq!(generic_key(0xa5), VK_MENU);
        assert_eq!(generic_key(0x41), 0x41);
    }
}
