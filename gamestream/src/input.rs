//! GameStream input: the NV input packets a client sends inside control
//! messages of type 0x0206 (moonlight-common-c `Input.h`, `InputStream.c`;
//! Sunshine `input.cpp`), read on the host and written by our client.
//!
//! Every packet starts with its length (big-endian, not counting the
//! length field) and a little-endian magic. Byte orders inside differ by
//! packet, as in the reference sources: mouse positions, deltas and scroll
//! amounts are big-endian, key codes, gamepad fields and touch floats
//! little-endian. Keys are Windows virtual-key codes on the wire and
//! windowcast's evdev key codes here; gamepad buttons are XInput's bits on
//! both.

use windowcast_protocol::{GamepadState, PointerButton, TouchPhase};

pub const KEY_DOWN: u32 = 0x03;
pub const KEY_UP: u32 = 0x04;
pub const MOUSE_ABSOLUTE: u32 = 0x05;
pub const MOUSE_RELATIVE: u32 = 0x07;
pub const MOUSE_BUTTON_DOWN: u32 = 0x08;
pub const MOUSE_BUTTON_UP: u32 = 0x09;
pub const SCROLL: u32 = 0x0A;
pub const MULTI_CONTROLLER: u32 = 0x0C;
pub const TEXT: u32 = 0x17;
pub const HORIZONTAL_SCROLL: u32 = 0x5500_0001;
pub const TOUCH: u32 = 0x5500_0002;
pub const PEN: u32 = 0x5500_0003;

/// The DESCRIBE feature flag that lets a client send touch and pen events.
pub const FEATURE_PEN_TOUCH: u32 = 0x01;

/// The control channels a client sends input on.
pub const CHANNEL_KEYBOARD: u8 = 0x02;
pub const CHANNEL_MOUSE: u8 = 0x03;
pub const CHANNEL_TOUCH: u8 = 0x05;
pub const CHANNEL_TEXT: u8 = 0x06;
pub const CHANNEL_GAMEPAD: u8 = 0x10;

/// One input event, with pointer and touch positions normalized to the
/// streamed picture as windowcast's own input is.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// An absolute position, 0.0 to 1.0 on each axis.
    PointerTo {
        x: f32,
        y: f32,
    },
    /// A relative move, in pixels of the client's view of the stream.
    PointerBy {
        dx: i16,
        dy: i16,
    },
    Button {
        button: PointerButton,
        pressed: bool,
    },
    /// In notches; positive `dy` is the wheel away from the user, positive
    /// `dx` to the right.
    Scroll {
        dx: f32,
        dy: f32,
    },
    /// An evdev key code.
    Key {
        keycode: u32,
        pressed: bool,
    },
    Text(String),
    Touch {
        id: u32,
        x: f32,
        y: f32,
        phase: TouchPhase,
    },
    /// The full state of one pad, and which pads the client has.
    Gamepad {
        pad: u8,
        active: u16,
        state: GamepadState,
    },
}

fn packet(magic: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(4 + body.len() as u32).to_be_bytes());
    out.extend_from_slice(&magic.to_le_bytes());
    out.extend_from_slice(body);
    out
}

fn be16(b: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le16(b: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn float(b: &[u8], at: usize) -> Option<f32> {
    Some(f32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn button_number(button: PointerButton) -> u8 {
    match button {
        PointerButton::Left => 1,
        PointerButton::Middle => 2,
        PointerButton::Right => 3,
        PointerButton::Back => 4,
        PointerButton::Forward => 5,
    }
}

fn button_of(number: u8) -> Option<PointerButton> {
    Some(match number {
        1 => PointerButton::Left,
        2 => PointerButton::Middle,
        3 => PointerButton::Right,
        4 => PointerButton::Back,
        5 => PointerButton::Forward,
        _ => return None,
    })
}

/// Reads one input packet. Packets windowcast has no use for (haptics,
/// controller arrival, motion and battery reports) and malformed ones give
/// nothing.
pub fn parse(data: &[u8]) -> Vec<Input> {
    parse_one(data).unwrap_or_default()
}

fn parse_one(data: &[u8]) -> Option<Vec<Input>> {
    let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
    let data = data.get(..4 + size)?;
    let magic = le32(data, 4)?;
    let body = &data[8..];
    let one = |input| Some(vec![input]);
    match magic {
        KEY_DOWN | KEY_UP => {
            let vk = le16(body, 1)? as u16 & 0xff;
            one(Input::Key {
                keycode: evdev_of_vk(vk as u8)?,
                pressed: magic == KEY_DOWN,
            })
        }
        TEXT => one(Input::Text(String::from_utf8(body.to_vec()).ok()?)),
        MOUSE_RELATIVE => one(Input::PointerBy {
            dx: be16(body, 0)?,
            dy: be16(body, 2)?,
        }),
        MOUSE_ABSOLUTE => {
            let (x, y) = (be16(body, 0)?, be16(body, 2)?);
            // The reference size is sent less one.
            let (w, h) = (be16(body, 6)? as f32 + 1.0, be16(body, 8)? as f32 + 1.0);
            if w <= 1.0 || h <= 1.0 {
                return None;
            }
            one(Input::PointerTo {
                x: (x as f32 / (w - 1.0)).clamp(0.0, 1.0),
                y: (y as f32 / (h - 1.0)).clamp(0.0, 1.0),
            })
        }
        MOUSE_BUTTON_DOWN | MOUSE_BUTTON_UP => one(Input::Button {
            button: button_of(*body.first()?)?,
            pressed: magic == MOUSE_BUTTON_DOWN,
        }),
        SCROLL => one(Input::Scroll {
            dx: 0.0,
            dy: be16(body, 0)? as f32 / 120.0,
        }),
        HORIZONTAL_SCROLL => one(Input::Scroll {
            dx: be16(body, 0)? as f32 / 120.0,
            dy: 0.0,
        }),
        MULTI_CONTROLLER => {
            let pad = le16(body, 2)?;
            let active = le16(body, 4)? as u16;
            let low = le16(body, 8)? as u16 as u32;
            let high = le16(body, 22)? as u16 as u32;
            one(Input::Gamepad {
                pad: u8::try_from(pad).ok()?,
                active,
                state: GamepadState {
                    // The extra Sunshine buttons (paddles, touchpad, misc)
                    // have no XInput bit.
                    buttons: (low | (high << 16)) & 0xffff,
                    left_trigger: *body.get(10)?,
                    right_trigger: *body.get(11)?,
                    left_x: le16(body, 12)?,
                    left_y: le16(body, 14)?,
                    right_x: le16(body, 16)?,
                    right_y: le16(body, 18)?,
                },
            })
        }
        TOUCH | PEN => {
            // Touch: event, reserved, rotation, pointer id, x, y. Pen: event,
            // tool, buttons, reserved, x, y; a pen is one more finger.
            let event = *body.first()?;
            let (id, at) = if magic == TOUCH {
                (le32(body, 4)?, 8)
            } else {
                (u32::MAX, 4)
            };
            let phase = match event {
                0x01 => TouchPhase::Start,
                0x02 => TouchPhase::End,
                0x03 => TouchPhase::Move,
                0x04 | 0x07 => TouchPhase::Cancel,
                _ => return None,
            };
            one(Input::Touch {
                id,
                x: float(body, at)?.clamp(0.0, 1.0),
                y: float(body, at + 4)?.clamp(0.0, 1.0),
                phase,
            })
        }
        _ => None,
    }
}

/// Writes one input packet and the channel it goes on, as Moonlight sends
/// it to a Sunshine host.
pub fn encode(input: &Input) -> Option<(Vec<u8>, u8)> {
    Some(match input {
        Input::Key { keycode, pressed } => {
            let vk = vk_of_evdev(*keycode)?;
            let mut body = vec![0u8];
            body.extend_from_slice(&(0x8000u16 | u16::from(vk)).to_le_bytes());
            body.extend_from_slice(&[0, 0, 0]);
            (
                packet(if *pressed { KEY_DOWN } else { KEY_UP }, &body),
                CHANNEL_KEYBOARD,
            )
        }
        Input::Text(text) => (packet(TEXT, text.as_bytes()), CHANNEL_TEXT),
        Input::PointerBy { dx, dy } => {
            let mut body = dx.to_be_bytes().to_vec();
            body.extend_from_slice(&dy.to_be_bytes());
            (packet(MOUSE_RELATIVE, &body), CHANNEL_MOUSE)
        }
        Input::PointerTo { x, y } => {
            // A reference size of 4096 keeps the fraction's precision.
            const SIZE: f32 = 4096.0;
            let mut body = Vec::with_capacity(10);
            body.extend_from_slice(&((x.clamp(0.0, 1.0) * (SIZE - 1.0)) as i16).to_be_bytes());
            body.extend_from_slice(&((y.clamp(0.0, 1.0) * (SIZE - 1.0)) as i16).to_be_bytes());
            body.extend_from_slice(&[0, 0]);
            body.extend_from_slice(&((SIZE - 1.0) as i16).to_be_bytes());
            body.extend_from_slice(&((SIZE - 1.0) as i16).to_be_bytes());
            (packet(MOUSE_ABSOLUTE, &body), CHANNEL_MOUSE)
        }
        Input::Button { button, pressed } => (
            packet(
                if *pressed {
                    MOUSE_BUTTON_DOWN
                } else {
                    MOUSE_BUTTON_UP
                },
                &[button_number(*button)],
            ),
            CHANNEL_MOUSE,
        ),
        Input::Scroll { dx, dy } => {
            if *dx != 0.0 {
                let amount = (dx * 120.0) as i16;
                (
                    packet(HORIZONTAL_SCROLL, &amount.to_be_bytes()),
                    CHANNEL_MOUSE,
                )
            } else {
                let amount = (dy * 120.0) as i16;
                let mut body = amount.to_be_bytes().to_vec();
                body.extend_from_slice(&amount.to_be_bytes());
                body.extend_from_slice(&[0, 0]);
                (packet(SCROLL, &body), CHANNEL_MOUSE)
            }
        }
        Input::Touch { id, x, y, phase } => {
            let event = match phase {
                TouchPhase::Start => 0x01,
                TouchPhase::End => 0x02,
                TouchPhase::Move => 0x03,
                TouchPhase::Cancel => 0x04,
            };
            let mut body = vec![event, 0, 0, 0];
            body.extend_from_slice(&id.to_le_bytes());
            for value in [*x, *y, 1.0, 0.0, 0.0] {
                body.extend_from_slice(&value.to_le_bytes());
            }
            (packet(TOUCH, &body), CHANNEL_TOUCH)
        }
        Input::Gamepad { pad, active, state } => {
            let mut body = Vec::with_capacity(26);
            for field in [0x001A, i16::from(*pad), *active as i16, 0x0014] {
                body.extend_from_slice(&field.to_le_bytes());
            }
            body.extend_from_slice(&(state.buttons as u16).to_le_bytes());
            body.extend_from_slice(&[state.left_trigger, state.right_trigger]);
            for field in [
                state.left_x,
                state.left_y,
                state.right_x,
                state.right_y,
                0x009C,
                (state.buttons >> 16) as i16,
                0x0055,
            ] {
                body.extend_from_slice(&field.to_le_bytes());
            }
            (
                packet(MULTI_CONTROLLER, &body),
                CHANNEL_GAMEPAD + (pad & 0x0f),
            )
        }
    })
}

/// Windows virtual-key codes and the evdev key codes windowcast uses, for
/// the keys a GameStream client sends.
const KEYS: &[(u8, u32)] = &[
    (0x08, 14),  // Backspace
    (0x09, 15),  // Tab
    (0x0D, 28),  // Enter
    (0x10, 42),  // Shift
    (0x11, 29),  // Control
    (0x12, 56),  // Alt
    (0x13, 119), // Pause
    (0x14, 58),  // Caps Lock
    (0x1B, 1),   // Escape
    (0x20, 57),  // Space
    (0x21, 104), // Page Up
    (0x22, 109), // Page Down
    (0x23, 107), // End
    (0x24, 102), // Home
    (0x25, 105), // Left
    (0x26, 103), // Up
    (0x27, 106), // Right
    (0x28, 108), // Down
    (0x2C, 99),  // Print Screen
    (0x2D, 110), // Insert
    (0x2E, 111), // Delete
    (0x30, 11),  // 0
    (0x31, 2),
    (0x32, 3),
    (0x33, 4),
    (0x34, 5),
    (0x35, 6),
    (0x36, 7),
    (0x37, 8),
    (0x38, 9),
    (0x39, 10),
    (0x41, 30), // A
    (0x42, 48),
    (0x43, 46),
    (0x44, 32),
    (0x45, 18),
    (0x46, 33),
    (0x47, 34),
    (0x48, 35),
    (0x49, 23),
    (0x4A, 36),
    (0x4B, 37),
    (0x4C, 38),
    (0x4D, 50),
    (0x4E, 49),
    (0x4F, 24),
    (0x50, 25),
    (0x51, 16),
    (0x52, 19),
    (0x53, 31),
    (0x54, 20),
    (0x55, 22),
    (0x56, 47),
    (0x57, 17),
    (0x58, 45),
    (0x59, 21),
    (0x5A, 44),  // Z
    (0x5B, 125), // Left Windows
    (0x5C, 126), // Right Windows
    (0x5D, 127), // Menu
    (0x60, 82),  // Keypad 0
    (0x61, 79),
    (0x62, 80),
    (0x63, 81),
    (0x64, 75),
    (0x65, 76),
    (0x66, 77),
    (0x67, 71),
    (0x68, 72),
    (0x69, 73), // Keypad 9
    (0x6A, 55), // Keypad *
    (0x6B, 78), // Keypad +
    (0x6D, 74), // Keypad -
    (0x6E, 83), // Keypad .
    (0x6F, 98), // Keypad /
    (0x70, 59), // F1
    (0x71, 60),
    (0x72, 61),
    (0x73, 62),
    (0x74, 63),
    (0x75, 64),
    (0x76, 65),
    (0x77, 66),
    (0x78, 67),
    (0x79, 68),  // F10
    (0x7A, 87),  // F11
    (0x7B, 88),  // F12
    (0x7C, 183), // F13
    (0x7D, 184),
    (0x7E, 185),
    (0x7F, 186),
    (0x80, 187),
    (0x81, 188),
    (0x82, 189),
    (0x83, 190),
    (0x84, 191),
    (0x85, 192),
    (0x86, 193),
    (0x87, 194), // F24
    (0x90, 69),  // Num Lock
    (0x91, 70),  // Scroll Lock
    (0xA0, 42),  // Left Shift
    (0xA1, 54),  // Right Shift
    (0xA2, 29),  // Left Control
    (0xA3, 97),  // Right Control
    (0xA4, 56),  // Left Alt
    (0xA5, 100), // Right Alt
    (0xAD, 113), // Mute
    (0xAE, 114), // Volume Down
    (0xAF, 115), // Volume Up
    (0xB0, 163), // Next Track
    (0xB1, 165), // Previous Track
    (0xB2, 166), // Stop
    (0xB3, 164), // Play/Pause
    (0xBA, 39),  // ;
    (0xBB, 13),  // =
    (0xBC, 51),  // ,
    (0xBD, 12),  // -
    (0xBE, 52),  // .
    (0xBF, 53),  // /
    (0xC0, 41),  // `
    (0xDB, 26),  // [
    (0xDC, 43),  // \
    (0xDD, 27),  // ]
    (0xDE, 40),  // '
    (0xE2, 86),  // the key left of Z on ISO keyboards
];

pub fn evdev_of_vk(vk: u8) -> Option<u32> {
    KEYS.iter().find(|(v, _)| *v == vk).map(|(_, e)| *e)
}

/// The sided virtual key for an evdev code (Shift, Control and Alt as the
/// left ones, as Moonlight sends them).
pub fn vk_of_evdev(keycode: u32) -> Option<u8> {
    KEYS.iter()
        .filter(|(v, _)| !(0x10..=0x12).contains(v))
        .find(|(_, e)| *e == keycode)
        .map(|(v, _)| *v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(input: Input) {
        let (packet, _) = encode(&input).unwrap();
        assert_eq!(parse(&packet), vec![input]);
    }

    #[test]
    fn packets_round_trip() {
        round_trip(Input::Key {
            keycode: 30,
            pressed: true,
        });
        round_trip(Input::Key {
            keycode: 97,
            pressed: false,
        });
        round_trip(Input::Text("héllo".into()));
        round_trip(Input::PointerBy { dx: -5, dy: 300 });
        round_trip(Input::PointerTo { x: 0.0, y: 1.0 });
        round_trip(Input::Button {
            button: PointerButton::Right,
            pressed: true,
        });
        round_trip(Input::Scroll { dx: 0.0, dy: -1.0 });
        round_trip(Input::Scroll { dx: 2.0, dy: 0.0 });
        round_trip(Input::Touch {
            id: 7,
            x: 0.25,
            y: 0.75,
            phase: TouchPhase::Move,
        });
        round_trip(Input::Gamepad {
            pad: 1,
            active: 0b11,
            state: GamepadState {
                buttons: 0x9201,
                left_x: -32768,
                left_y: 32767,
                right_x: 12,
                right_y: -12,
                left_trigger: 255,
                right_trigger: 3,
            },
        });
    }

    /// The bytes moonlight-common-c writes for a key press of A with the
    /// Sunshine "normalized" flag clear: length 10, magic 3, flags 0, the
    /// key code 0x8041 little-endian, modifiers and two zero bytes.
    #[test]
    fn a_key_press_is_moonlights_bytes() {
        let (packet, channel) = encode(&Input::Key {
            keycode: 30,
            pressed: true,
        })
        .unwrap();
        assert_eq!(packet, [0, 0, 0, 10, 3, 0, 0, 0, 0, 0x41, 0x80, 0, 0, 0]);
        assert_eq!(channel, CHANNEL_KEYBOARD);
    }

    #[test]
    fn the_absolute_mouse_scales_by_the_reference_size() {
        // x 959 of a 1920-wide reference (sent as 1919).
        let mut body = 959i16.to_be_bytes().to_vec();
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&[0, 0]);
        body.extend_from_slice(&1919i16.to_be_bytes());
        body.extend_from_slice(&1079i16.to_be_bytes());
        match parse(&packet(MOUSE_ABSOLUTE, &body)).as_slice() {
            [Input::PointerTo { x, y }] => {
                assert!((x - 0.4997).abs() < 0.001, "{x}");
                assert_eq!(*y, 0.0);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn every_key_maps_back() {
        for (vk, keycode) in KEYS {
            let back = vk_of_evdev(*keycode).unwrap();
            assert_eq!(evdev_of_vk(back), Some(*keycode), "vk {vk:#x}");
        }
    }

    #[test]
    fn short_and_unknown_packets_give_nothing() {
        assert!(parse(&[0, 0, 0, 9, 3, 0, 0, 0]).is_empty());
        assert!(parse(&packet(0x0D, &[1, 0])).is_empty());
        assert!(parse(&[]).is_empty());
    }
}
