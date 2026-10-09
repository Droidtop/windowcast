//! Windows keyboard scan codes to the protocol's evdev key codes. For the
//! main block (set 1 codes 1 to 88) the two are the same numbers; keys with
//! the E0 prefix map through the table.

/// `scan` is the set 1 scan code from a key message, `extended` its E0
/// flag (bit 24 of lParam).
pub fn evdev(scan: u32, extended: bool) -> Option<u32> {
    if !extended {
        return (1..=88).contains(&scan).then_some(scan);
    }
    Some(match scan {
        0x1c => 96,  // keypad Enter
        0x1d => 97,  // right Ctrl
        0x35 => 98,  // keypad /
        0x37 => 99,  // Print Screen
        0x38 => 100, // right Alt
        0x47 => 102, // Home
        0x48 => 103, // Up
        0x49 => 104, // Page Up
        0x4b => 105, // Left
        0x4d => 106, // Right
        0x4f => 107, // End
        0x50 => 108, // Down
        0x51 => 109, // Page Down
        0x52 => 110, // Insert
        0x53 => 111, // Delete
        0x5b => 125, // left Windows
        0x5c => 126, // right Windows
        0x5d => 127, // Menu
        _ => return None,
    })
}
