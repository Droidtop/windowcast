//! PC keyboard scan codes (set 1, with the E0 "extended" prefix) and the
//! protocol's evdev key codes. For the main block (codes 1 to 88) the two
//! are the same numbers; extended keys map through the table. Windows key
//! messages, SendInput and RDP all speak scan codes.

/// Extended (E0) scan codes and their evdev key codes.
const EXTENDED: &[(u16, u32)] = &[
    (0x1c, 96),  // keypad Enter
    (0x1d, 97),  // right Ctrl
    (0x35, 98),  // keypad /
    (0x37, 99),  // Print Screen
    (0x38, 100), // right Alt
    (0x47, 102), // Home
    (0x48, 103), // Up
    (0x49, 104), // Page Up
    (0x4b, 105), // Left
    (0x4d, 106), // Right
    (0x4f, 107), // End
    (0x50, 108), // Down
    (0x51, 109), // Page Down
    (0x52, 110), // Insert
    (0x53, 111), // Delete
    (0x5b, 125), // left Windows
    (0x5c, 126), // right Windows
    (0x5d, 127), // Menu
];

/// The evdev key code for a scan code and its E0 flag.
pub fn evdev_from_scan(scan: u16, extended: bool) -> Option<u32> {
    if !extended {
        return (1..=88).contains(&scan).then_some(u32::from(scan));
    }
    EXTENDED.iter().find(|(s, _)| *s == scan).map(|(_, e)| *e)
}

/// The scan code and E0 flag for an evdev key code.
pub fn scan_from_evdev(keycode: u32) -> Option<(u16, bool)> {
    if (1..=88).contains(&keycode) {
        return Some((keycode as u16, false));
    }
    EXTENDED
        .iter()
        .find(|(_, e)| *e == keycode)
        .map(|(s, _)| (*s, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_block_keys_keep_their_number_and_arrows_are_extended() {
        assert_eq!(scan_from_evdev(30), Some((0x1e, false))); // A
        assert_eq!(scan_from_evdev(28), Some((0x1c, false))); // Enter
        assert_eq!(scan_from_evdev(103), Some((0x48, true))); // Up
        assert_eq!(scan_from_evdev(111), Some((0x53, true))); // Delete
        assert_eq!(scan_from_evdev(500), None);
        assert_eq!(evdev_from_scan(0x1c, true), Some(96));
        assert_eq!(evdev_from_scan(0x60, true), None);
    }

    #[test]
    fn every_key_maps_back() {
        for keycode in (1..=88).chain(EXTENDED.iter().map(|(_, e)| *e)) {
            let (scan, extended) = scan_from_evdev(keycode).unwrap();
            assert_eq!(evdev_from_scan(scan, extended), Some(keycode));
        }
    }
}
