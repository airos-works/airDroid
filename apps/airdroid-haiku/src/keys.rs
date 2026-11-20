//! Haiku raw key codes (keyboard positions) to Linux KEY_* codes, which is
//! what Android's input stack gets from the hwcomposer's evdev pipes.
//! Android applies its own keyboard layout to them.

/// Linux input event code for a Haiku raw key, if it has one.
pub fn linux_key(raw: i32) -> Option<u32> {
    Some(match raw {
        0x01 => 1,  // Esc
        0x02..=0x0b => 59 + (raw - 0x02) as u32, // F1..F10
        0x0c => 87, // F11
        0x0d => 88, // F12
        0x0e => 99, // Print / SysRq
        0x0f => 70, // Scroll Lock
        0x10 => 119, // Pause
        0x11 => 41, // `
        0x12..=0x1b => 2 + (raw - 0x12) as u32, // 1..0
        0x1c => 12, // -
        0x1d => 13, // =
        0x1e => 14, // Backspace
        0x1f => 110, // Insert
        0x20 => 102, // Home
        0x21 => 104, // Page Up
        0x22 => 69, // Num Lock
        0x23 => 98, // KP /
        0x24 => 55, // KP *
        0x25 => 74, // KP -
        0x26 => 15, // Tab
        0x27..=0x30 => [16, 17, 18, 19, 20, 21, 22, 23, 24, 25][(raw - 0x27) as usize], // Q..P
        0x31 => 26, // [
        0x32 => 27, // ]
        0x33 => 43, // backslash
        0x34 => 111, // Delete
        0x35 => 107, // End
        0x36 => 109, // Page Down
        0x37 => 71, // KP 7
        0x38 => 72, // KP 8
        0x39 => 73, // KP 9
        0x3a => 78, // KP +
        0x3b => 58, // Caps Lock
        0x3c..=0x44 => [30, 31, 32, 33, 34, 35, 36, 37, 38][(raw - 0x3c) as usize], // A..L
        0x45 => 39, // ;
        0x46 => 40, // '
        0x47 => 28, // Enter
        0x48 => 75, // KP 4
        0x49 => 76, // KP 5
        0x4a => 77, // KP 6
        0x4b => 42, // Left Shift
        0x4c..=0x52 => [44, 45, 46, 47, 48, 49, 50][(raw - 0x4c) as usize], // Z..M
        0x53 => 51, // ,
        0x54 => 52, // .
        0x55 => 53, // /
        0x56 => 54, // Right Shift
        0x57 => 103, // Up
        0x58 => 79, // KP 1
        0x59 => 80, // KP 2
        0x5a => 81, // KP 3
        0x5b => 96, // KP Enter
        0x5c => 29, // Left Control
        0x5d => 56, // Left Command (the Alt key)
        0x5e => 57, // Space
        0x5f => 100, // Right Command (AltGr)
        0x60 => 97, // Right Control
        0x61 => 105, // Left
        0x62 => 108, // Down
        0x63 => 106, // Right
        0x64 => 82, // KP 0
        0x65 => 83, // KP .
        0x66 => 125, // Left Option (Windows)
        0x67 => 126, // Right Option
        0x68 => 127, // Menu
        0x69 => 86, // 102nd key (<>)
        _ => return None,
    })
}

// Haiku modifier bits (InterfaceDefs.h) for the left/right keys.
const MODIFIER_KEYS: [(u32, u32); 8] = [
    (0x0100, 42),  // B_LEFT_SHIFT_KEY -> KEY_LEFTSHIFT
    (0x0200, 54),  // B_RIGHT_SHIFT_KEY
    (0x0400, 56),  // B_LEFT_COMMAND_KEY -> KEY_LEFTALT
    (0x0800, 100), // B_RIGHT_COMMAND_KEY -> KEY_RIGHTALT
    (0x1000, 29),  // B_LEFT_CONTROL_KEY
    (0x2000, 97),  // B_RIGHT_CONTROL_KEY
    (0x4000, 125), // B_LEFT_OPTION_KEY -> KEY_LEFTMETA
    (0x8000, 126), // B_RIGHT_OPTION_KEY
];
const B_CAPS_LOCK: u32 = 0x08;

/// Haiku reports modifier keys only as B_MODIFIERS_CHANGED: the key presses
/// and releases (Linux codes) that turn `old` into `new`. A Caps Lock toggle
/// is a press and release.
pub fn modifier_changes(old: u32, new: u32) -> Vec<(u32, bool)> {
    let mut out = Vec::new();
    for (bit, code) in MODIFIER_KEYS {
        match (old & bit != 0, new & bit != 0) {
            (false, true) => out.push((code, true)),
            (true, false) => out.push((code, false)),
            _ => {}
        }
    }
    if (old ^ new) & B_CAPS_LOCK != 0 {
        out.push((58, true));
        out.push((58, false));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_digits() {
        assert_eq!(linux_key(0x27), Some(16)); // Q
        assert_eq!(linux_key(0x3c), Some(30)); // A
        assert_eq!(linux_key(0x4c), Some(44)); // Z
        assert_eq!(linux_key(0x12), Some(2)); // 1
        assert_eq!(linux_key(0x1b), Some(11)); // 0
        assert_eq!(linux_key(0x0b), Some(68)); // F10
        assert_eq!(linux_key(0x47), Some(28)); // Enter
        assert_eq!(linux_key(0x200), None);
    }

    #[test]
    fn modifiers() {
        assert_eq!(modifier_changes(0, 0x0101), vec![(42, true)]);
        assert_eq!(modifier_changes(0x0101, 0), vec![(42, false)]);
        assert_eq!(modifier_changes(0, 0x08), vec![(58, true), (58, false)]);
    }
}
