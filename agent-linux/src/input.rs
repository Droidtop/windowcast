//! Input on Wayland: a virtual pointer (`zwlr_virtual_pointer_v1`) moved to
//! absolute positions on the output layout, and a virtual keyboard
//! (`zwp_virtual_keyboard_v1`) with a US keymap compiled by xkbcommon,
//! whose modifier state it tracks and reports. Where a window is, and
//! focusing it, come from sway's IPC: a pointer position is mapped from the
//! streamed picture onto the window's rectangle, and keys and text go to
//! the session's focus window after it is focused. Gamepads do not come
//! here: they are uinput pads (`crate::gamepad`).

use std::collections::HashMap;
use std::io::Write;
use std::os::fd::AsFd;
use std::time::Instant;

use rustix::fs::{memfd_create, MemfdFlags};
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use windowcast_protocol::{InputEvent, PointerButton, TouchPhase, WindowId};
use xkbcommon::xkb;

use crate::sway::{self, Sway};
use crate::toplevels;

struct State;

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for State {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as wayland_client::Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore_events!(
    wl_seat::WlSeat,
    ZwlrVirtualPointerManagerV1,
    ZwlrVirtualPointerV1,
    ZwpVirtualKeyboardManagerV1,
    ZwpVirtualKeyboardV1
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// evdev codes of the pointer buttons (BTN_LEFT and on); back and forward
/// are the side buttons browsers use for them.
fn button_code(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => 0x110,
        PointerButton::Right => 0x111,
        PointerButton::Middle => 0x112,
        PointerButton::Back => 0x113,
        PointerButton::Forward => 0x114,
    }
}

/// evdev code of the left Shift key.
const KEY_LEFTSHIFT: u32 = 42;

pub struct Injector {
    conn: Connection,
    queue: EventQueue<State>,
    pointer: ZwlrVirtualPointerV1,
    keyboard: ZwpVirtualKeyboardV1,
    xkb: xkb::State,
    /// Characters the keymap types, as (evdev code, needs Shift).
    typeable: HashMap<char, (u32, bool)>,
    sway: Sway,
    start: Instant,
    /// The window keys last went to, so it is focused only on a change.
    focused: Option<WindowId>,
}

// Used only from the session's input thread after creation.
unsafe impl Send for Injector {}

impl Injector {
    pub fn new() -> Result<Self, String> {
        let sway = Sway::connect().ok_or("input needs sway (no $SWAYSOCK)")?;
        let conn = Connection::connect_to_env().map_err(|e| format!("wayland: {e}"))?;
        let (globals, mut queue) =
            registry_queue_init::<State>(&conn).map_err(|e| format!("wayland: {e}"))?;
        let qh = queue.handle();
        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "no wl_seat".to_owned())?;
        let pointers: ZwlrVirtualPointerManagerV1 = globals
            .bind(&qh, 1..=2, ())
            .map_err(|_| "the compositor has no zwlr_virtual_pointer_manager_v1".to_owned())?;
        let keyboards: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| "the compositor has no zwp_virtual_keyboard_manager_v1".to_owned())?;
        let pointer = pointers.create_virtual_pointer(Some(&seat), &qh, ());
        let keyboard = keyboards.create_virtual_keyboard(&seat, &qh, ());

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or("xkbcommon could not compile a US keymap")?;
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let fd = memfd_create("windowcast-keymap", MemfdFlags::CLOEXEC)
            .map_err(|e| format!("memfd: {e}"))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(text.as_bytes())
            .and_then(|_| file.write_all(&[0]))
            .map_err(|e| format!("keymap: {e}"))?;
        keyboard.keymap(
            1, // WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1
            file.as_fd(),
            text.len() as u32 + 1,
        );

        let mut typeable = HashMap::new();
        let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
        for code in min..=max {
            for (level, shift) in [(0, false), (1, true)] {
                for sym in keymap.key_get_syms_by_level(xkb::Keycode::new(code), 0, level) {
                    if let Some(c) = char::from_u32(xkb::keysym_to_utf32(*sym)) {
                        if c != '\0' && code >= 8 {
                            typeable.entry(c).or_insert((code - 8, shift));
                        }
                    }
                }
            }
        }
        queue
            .roundtrip(&mut State)
            .map_err(|e| format!("wayland: {e}"))?;
        Ok(Injector {
            conn,
            queue,
            pointer,
            keyboard,
            xkb: xkb::State::new(&keymap),
            typeable,
            sway,
            start: Instant::now(),
            focused: None,
        })
    }

    fn time(&self) -> u32 {
        self.start.elapsed().as_millis() as u32
    }

    fn window(&mut self, window: WindowId) -> Option<sway::Window> {
        self.sway
            .windows()
            .ok()?
            .into_iter()
            .find(|w| toplevels::window_id(&w.identifier) == window)
    }

    fn focus(&mut self, window: WindowId) {
        if self.focused == Some(window) {
            return;
        }
        if let Some(placed) = self.window(window) {
            let _ = self.sway.focus(placed.con_id);
            self.focused = Some(window);
        }
    }

    /// Moves the pointer to (x, y), 0 to 1 across the window's picture.
    fn move_to(&mut self, window: WindowId, x: f32, y: f32) {
        let Some(placed) = self.window(window) else {
            return;
        };
        let Ok(layout) = self.sway.layout() else {
            return;
        };
        let r = placed.rect;
        let lx = f64::from(r.x) + f64::from(x.clamp(0.0, 1.0)) * f64::from(r.width - 1);
        let ly = f64::from(r.y) + f64::from(y.clamp(0.0, 1.0)) * f64::from(r.height - 1);
        let time = self.time();
        self.pointer.motion_absolute(
            time,
            lx.max(0.0) as u32,
            ly.max(0.0) as u32,
            layout.width.max(1) as u32,
            layout.height.max(1) as u32,
        );
        self.pointer.frame();
    }

    fn button(&mut self, code: u32, pressed: bool) {
        let time = self.time();
        self.pointer.button(
            time,
            code,
            if pressed {
                wl_pointer::ButtonState::Pressed
            } else {
                wl_pointer::ButtonState::Released
            },
        );
        self.pointer.frame();
    }

    fn key(&mut self, code: u32, pressed: bool) {
        let time = self.time();
        self.keyboard.key(time, code, u32::from(pressed));
        let direction = if pressed {
            xkb::KeyDirection::Down
        } else {
            xkb::KeyDirection::Up
        };
        let changed = self.xkb.update_key(xkb::Keycode::new(code + 8), direction);
        if changed != 0 {
            self.keyboard.modifiers(
                self.xkb.serialize_mods(xkb::STATE_MODS_DEPRESSED),
                self.xkb.serialize_mods(xkb::STATE_MODS_LATCHED),
                self.xkb.serialize_mods(xkb::STATE_MODS_LOCKED),
                self.xkb.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
            );
        }
    }

    pub fn inject(&mut self, event: &InputEvent, focus: Option<WindowId>) {
        match event {
            InputEvent::PointerMove { window, x, y } => self.move_to(*window, *x, *y),
            InputEvent::PointerButton {
                window,
                button,
                pressed,
            } => {
                if *pressed {
                    self.focus(*window);
                }
                self.button(button_code(*button), *pressed);
            }
            InputEvent::PointerScroll { dx, dy, .. } => {
                let time = self.time();
                // Wheel notches as 15 units each, scrolling content up
                // for a positive dy (wheel away from the user).
                if *dy != 0.0 {
                    self.pointer.axis(
                        time,
                        wl_pointer::Axis::VerticalScroll,
                        -f64::from(*dy) * 15.0,
                    );
                }
                if *dx != 0.0 {
                    self.pointer.axis(
                        time,
                        wl_pointer::Axis::HorizontalScroll,
                        f64::from(*dx) * 15.0,
                    );
                }
                self.pointer.frame();
            }
            InputEvent::Key { keycode, pressed } => {
                if let Some(window) = focus {
                    self.focus(window);
                }
                self.key(*keycode, *pressed);
            }
            InputEvent::Text { text } => {
                if let Some(window) = focus {
                    self.focus(window);
                }
                for c in text.chars() {
                    let Some(&(code, shift)) = self.typeable.get(&c) else {
                        continue;
                    };
                    if shift {
                        self.key(KEY_LEFTSHIFT, true);
                    }
                    self.key(code, true);
                    self.key(code, false);
                    if shift {
                        self.key(KEY_LEFTSHIFT, false);
                    }
                }
            }
            InputEvent::Touch {
                window,
                x,
                y,
                phase,
                ..
            } => {
                self.move_to(*window, *x, *y);
                match phase {
                    TouchPhase::Start => {
                        self.focus(*window);
                        self.button(button_code(PointerButton::Left), true);
                    }
                    TouchPhase::End | TouchPhase::Cancel => {
                        self.button(button_code(PointerButton::Left), false)
                    }
                    TouchPhase::Move => {}
                }
            }
            // Delivered to the session's pads by host-core.
            InputEvent::Gamepad { .. } | InputEvent::GamepadGone { .. } => {}
        }
        let _ = self.conn.flush();
        let _ = self.queue.dispatch_pending(&mut State);
    }
}
