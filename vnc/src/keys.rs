//! VNC keys (X keysyms, as RFB sends them) as windowcast input. Printable
//! characters are typed as text, so the viewer's keyboard layout and Shift
//! decide what arrives; with Ctrl, Alt or the logo key held, letters and
//! digits go as keys (evdev codes, the protocol's), so shortcuts work; the
//! keys that are not characters (Enter, arrows, F1 ...) always go as keys.

use windowcast_protocol::InputEvent;

/// Which modifiers the viewer holds.
#[derive(Debug, Default)]
pub struct Modifiers {
    control: bool,
    alt: bool,
    logo: bool,
}

/// Keysyms that are keys, and their evdev codes.
const KEYS: &[(u32, u32)] = &[
    (0xff08, 14),  // BackSpace
    (0xff09, 15),  // Tab
    (0xff0d, 28),  // Return
    (0xff1b, 1),   // Escape
    (0xffff, 111), // Delete
    (0xff50, 102), // Home
    (0xff51, 105), // Left
    (0xff52, 103), // Up
    (0xff53, 106), // Right
    (0xff54, 108), // Down
    (0xff55, 104), // Page Up
    (0xff56, 109), // Page Down
    (0xff57, 107), // End
    (0xff63, 110), // Insert
    (0xff67, 127), // Menu
    (0xff8d, 96),  // keypad Enter
    (0xffbe, 59),  // F1
    (0xffbf, 60),
    (0xffc0, 61),
    (0xffc1, 62),
    (0xffc2, 63),
    (0xffc3, 64),
    (0xffc4, 65),
    (0xffc5, 66),
    (0xffc6, 67),
    (0xffc7, 68),  // F10
    (0xffc8, 87),  // F11
    (0xffc9, 88),  // F12
    (0xffe1, 42),  // Shift (left)
    (0xffe2, 54),  // Shift (right)
    (0xffe3, 29),  // Control (left)
    (0xffe4, 97),  // Control (right)
    (0xffe9, 56),  // Alt (left)
    (0xffea, 100), // Alt (right)
    (0xffeb, 125), // logo (left)
    (0xffec, 126), // logo (right)
];

/// Letters a to z on a US layout, as evdev codes.
const LETTERS: [u32; 26] = [
    30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45,
    21, 44,
];

/// The key a letter or digit is on, for shortcuts.
fn shortcut_key(c: char) -> Option<u32> {
    match c.to_ascii_lowercase() {
        l @ 'a'..='z' => Some(LETTERS[(l as u8 - b'a') as usize]),
        '0' => Some(11),
        d @ '1'..='9' => Some(u32::from(d as u8 - b'1') + 2),
        ' ' => Some(57),
        _ => None,
    }
}

/// The character a keysym types, if it is one: Latin-1 directly, the rest
/// of Unicode as 0x01000000 plus the code point.
fn character(keysym: u32) -> Option<char> {
    match keysym {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(keysym),
        0x0100_0000..=0x0110_ffff => char::from_u32(keysym - 0x0100_0000),
        _ => None,
    }
}

impl Modifiers {
    /// The input one KeyEvent makes.
    pub fn event(&mut self, keysym: u32, down: bool) -> Vec<InputEvent> {
        match keysym {
            0xffe3 | 0xffe4 => self.control = down,
            0xffe9 | 0xffea => self.alt = down,
            0xffeb | 0xffec => self.logo = down,
            _ => {}
        }
        if let Some(&(_, keycode)) = KEYS.iter().find(|(sym, _)| *sym == keysym) {
            return vec![InputEvent::Key {
                keycode,
                pressed: down,
            }];
        }
        let Some(c) = character(keysym) else {
            return Vec::new();
        };
        let shortcut = self.control || self.alt || self.logo;
        match shortcut.then(|| shortcut_key(c)).flatten() {
            Some(keycode) => vec![InputEvent::Key {
                keycode,
                pressed: down,
            }],
            None if down => vec![InputEvent::Text {
                text: c.to_string(),
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_are_text_and_shortcuts_are_keys() {
        let mut keys = Modifiers::default();
        assert_eq!(
            keys.event('A' as u32, true),
            vec![InputEvent::Text { text: "A".into() }]
        );
        assert!(keys.event('A' as u32, false).is_empty());
        assert_eq!(
            keys.event(0xff0d, true),
            vec![InputEvent::Key {
                keycode: 28,
                pressed: true
            }]
        );
        // Ctrl+C: Control, then the C key, not the text "c".
        keys.event(0xffe3, true);
        assert_eq!(
            keys.event('c' as u32, true),
            vec![InputEvent::Key {
                keycode: 46,
                pressed: true
            }]
        );
        keys.event(0xffe3, false);
        assert_eq!(
            keys.event(0x0100_20ac, true),
            vec![InputEvent::Text {
                text: "\u{20ac}".into()
            }]
        );
    }
}
