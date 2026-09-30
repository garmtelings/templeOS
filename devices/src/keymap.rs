//! Host keyboard events to PS/2 scan code set 2, the form
//! [`crate::ps2::Ps2::keyboard_input`] takes.
//!
//! Windows raw input reports keys as scan code set 1 make codes plus E0/E1
//! prefix flags, which is what a PC keyboard sends through the 8042's
//! translation. [`KeyMapper`] turns those back into the set 2 bytes the
//! keyboard itself would send, following QEMU's `ps2_keyboard_event` for the
//! two odd keys:
//!
//! - Print Screen: make `E0 12 E0 7C`, break `E0 F0 7C E0 F0 12`.
//! - Pause: `E1 14 77 E1 F0 14 F0 77` on press, nothing on release.
//!
//! The "fake shift" events Windows reports around some E0 keys (E0 2A,
//! E0 36) are dropped: the keyboard doesn't send them on its own in set 2.

/// Set 2 code for each set 1 make code without a prefix (0 = no key).
const PLAIN: [u8; 0x59] = {
    let mut t = [0u8; 0x59];
    let pairs: [(usize, u8); 88] = [
        (0x01, 0x76), (0x02, 0x16), (0x03, 0x1e), (0x04, 0x26), (0x05, 0x25), (0x06, 0x2e),
        (0x07, 0x36), (0x08, 0x3d), (0x09, 0x3e), (0x0a, 0x46), (0x0b, 0x45), (0x0c, 0x4e),
        (0x0d, 0x55), (0x0e, 0x66), (0x0f, 0x0d), (0x10, 0x15), (0x11, 0x1d), (0x12, 0x24),
        (0x13, 0x2d), (0x14, 0x2c), (0x15, 0x35), (0x16, 0x3c), (0x17, 0x43), (0x18, 0x44),
        (0x19, 0x4d), (0x1a, 0x54), (0x1b, 0x5b), (0x1c, 0x5a), (0x1d, 0x14), (0x1e, 0x1c),
        (0x1f, 0x1b), (0x20, 0x23), (0x21, 0x2b), (0x22, 0x34), (0x23, 0x33), (0x24, 0x3b),
        (0x25, 0x42), (0x26, 0x4b), (0x27, 0x4c), (0x28, 0x52), (0x29, 0x0e), (0x2a, 0x12),
        (0x2b, 0x5d), (0x2c, 0x1a), (0x2d, 0x22), (0x2e, 0x21), (0x2f, 0x2a), (0x30, 0x32),
        (0x31, 0x31), (0x32, 0x3a), (0x33, 0x41), (0x34, 0x49), (0x35, 0x4a), (0x36, 0x59),
        (0x37, 0x7c), (0x38, 0x11), (0x39, 0x29), (0x3a, 0x58), (0x3b, 0x05), (0x3c, 0x06),
        (0x3d, 0x04), (0x3e, 0x0c), (0x3f, 0x03), (0x40, 0x0b), (0x41, 0x83), (0x42, 0x0a),
        (0x43, 0x01), (0x44, 0x09), (0x45, 0x77), (0x46, 0x7e), (0x47, 0x6c), (0x48, 0x75),
        (0x49, 0x7d), (0x4a, 0x7b), (0x4b, 0x6b), (0x4c, 0x73), (0x4d, 0x74), (0x4e, 0x79),
        (0x4f, 0x69), (0x50, 0x72), (0x51, 0x7a), (0x52, 0x70), (0x53, 0x71), (0x54, 0x84),
        (0x56, 0x61), (0x57, 0x78), (0x58, 0x07), (0x00, 0x00),
    ];
    let mut i = 0;
    while i < pairs.len() {
        t[pairs[i].0] = pairs[i].1;
        i += 1;
    }
    t
};

/// Set 2 code (after E0) for each E0-prefixed set 1 make code.
const EXTENDED: [(u8, u8); 17] = [
    (0x1c, 0x5a), // keypad Enter
    (0x1d, 0x14), // right Ctrl
    (0x35, 0x4a), // keypad /
    (0x38, 0x11), // right Alt
    (0x47, 0x6c), // Home
    (0x48, 0x75), // Up
    (0x49, 0x7d), // Page Up
    (0x4b, 0x6b), // Left
    (0x4d, 0x74), // Right
    (0x4f, 0x69), // End
    (0x50, 0x72), // Down
    (0x51, 0x7a), // Page Down
    (0x52, 0x70), // Insert
    (0x53, 0x71), // Delete
    (0x5b, 0x1f), // left Windows
    (0x5c, 0x27), // right Windows
    (0x5d, 0x2f), // Menu
];

/// A key as the host reports it: set 1 make code and prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostKey {
    pub code: u8,
    pub e0: bool,
}

impl HostKey {
    /// The right Ctrl key, the VMM's host key.
    pub const RIGHT_CTRL: HostKey = HostKey { code: 0x1d, e0: true };
}

/// Stateful translator (Pause arrives from Windows as two events).
#[derive(Default)]
pub struct KeyMapper {
    pause_pending: bool,
}

impl KeyMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set 2 bytes for one host key event, or an empty vector when the event
    /// produces nothing (unknown key, fake shift, Pause release, the first
    /// half of Pause). `e1` is the E1 prefix flag, which only Pause uses.
    pub fn translate(&mut self, code: u8, e0: bool, e1: bool, pressed: bool) -> Vec<u8> {
        if e1 {
            // Windows: Pause is E1 1D followed by a plain 45.
            self.pause_pending = code == 0x1d;
            return Vec::new();
        }
        if std::mem::take(&mut self.pause_pending) && code == 0x45 && !e0 {
            return if pressed { vec![0xe1, 0x14, 0x77, 0xe1, 0xf0, 0x14, 0xf0, 0x77] } else { Vec::new() };
        }
        if e0 {
            return match code {
                // Fake shifts around E0 keys.
                0x2a | 0x36 | 0xaa | 0xb6 => Vec::new(),
                0x37 if pressed => vec![0xe0, 0x12, 0xe0, 0x7c],
                0x37 => vec![0xe0, 0xf0, 0x7c, 0xe0, 0xf0, 0x12],
                // Windows reports Pause with Ctrl held as E0 46 (Break).
                0x46 if pressed => vec![0xe0, 0x7e, 0xe0, 0xf0, 0x7e],
                0x46 => Vec::new(),
                _ => match EXTENDED.iter().find(|(c, _)| *c == code) {
                    Some(&(_, s2)) if pressed => vec![0xe0, s2],
                    Some(&(_, s2)) => vec![0xe0, 0xf0, s2],
                    None => Vec::new(),
                },
            };
        }
        match PLAIN.get(usize::from(code)).copied() {
            Some(0) | None => Vec::new(),
            Some(s2) if pressed => vec![s2],
            Some(s2) => vec![0xf0, s2],
        }
    }
}

/// What a host key combination asks the frontend to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostAction {
    None,
    ReleaseMouse,
    ToggleFullscreen,
}

const LEFT_CTRL: HostKey = HostKey { code: 0x1d, e0: false };
const ENTER: u8 = 0x1c;
const F: u8 = 0x21;

/// The VMM's host keys, decided before a key reaches the guest:
///
/// - **Right Ctrl** (VirtualBox style): pressed and released alone it
///   releases the mouse; with F or Enter it toggles fullscreen. Right Ctrl
///   and the keys pressed with it never reach the guest.
/// - **Ctrl+Alt** (QEMU style, for keyboards without a Right Ctrl): left
///   Ctrl and either Alt pressed together and released without another key
///   release the mouse; Ctrl+Alt+Enter toggles fullscreen. Ctrl and Alt still
///   go to the guest (TempleOS's Ctrl+Alt+letter shortcuts keep working); only
///   the Enter of Ctrl+Alt+Enter is kept from it, a combination TempleOS
///   doesn't bind.
#[derive(Default)]
pub struct HostKeys {
    host_down: bool,
    host_used: bool,
    /// Keys used in a host combination: their release is swallowed too.
    consumed: std::collections::HashSet<HostKey>,
    ctrl: bool,
    /// Held Alt keys: bit 0 left, bit 1 right.
    alts: u8,
    /// Another key was pressed while Ctrl+Alt were held.
    chord_used: bool,
}

impl HostKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Classify one key event: whether the guest should get it, and what
    /// the frontend should do.
    pub fn key(&mut self, key: HostKey, e1: bool, pressed: bool) -> (bool, HostAction) {
        if e1 {
            // Pause's first half: never a host key, and not Ctrl either.
            if pressed && self.chord() {
                self.chord_used = true;
            }
            return (true, HostAction::None);
        }
        if key == HostKey::RIGHT_CTRL {
            if pressed {
                if !self.host_down {
                    self.host_down = true;
                    self.host_used = false;
                }
                return (false, HostAction::None);
            }
            self.host_down = false;
            let action = if self.host_used { HostAction::None } else { HostAction::ReleaseMouse };
            return (false, action);
        }
        if self.host_down && pressed {
            self.host_used = true;
            self.consumed.insert(key);
            let action = match key.code {
                F if !key.e0 => HostAction::ToggleFullscreen,
                ENTER => HostAction::ToggleFullscreen,
                _ => HostAction::None,
            };
            return (false, action);
        }
        if !pressed && self.consumed.remove(&key) {
            return (false, HostAction::None);
        }

        let is_ctrl = key == LEFT_CTRL;
        let is_alt = key.code == 0x38;
        let was_chord = self.chord();
        if is_ctrl || is_alt {
            if is_ctrl {
                self.ctrl = pressed;
            } else {
                let bit = if key.e0 { 2 } else { 1 };
                self.alts = if pressed { self.alts | bit } else { self.alts & !bit };
            }
            if pressed && self.chord() && !was_chord {
                self.chord_used = false;
            }
            if !pressed && was_chord && !std::mem::replace(&mut self.chord_used, true) {
                return (true, HostAction::ReleaseMouse);
            }
            return (true, HostAction::None);
        }
        if pressed && was_chord {
            self.chord_used = true;
            if key.code == ENTER {
                self.consumed.insert(key);
                return (false, HostAction::ToggleFullscreen);
            }
        }
        (true, HostAction::None)
    }

    /// Ctrl and an Alt are both held.
    fn chord(&self) -> bool {
        self.ctrl && self.alts != 0
    }

    /// Forget all held keys (the window lost focus; the frontend releases
    /// the guest's keys itself).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ps2::set2_to_set1;

    #[test]
    fn every_key_round_trips_through_the_8042_translation() {
        // What the guest reads with translation on must be the set 1 code
        // the host reported: make as is, break with bit 7 set.
        let mut m = KeyMapper::new();
        let mut n = 0;
        for code in 1u8..0x59 {
            let make = m.translate(code, false, false, true);
            if make.is_empty() {
                continue;
            }
            n += 1;
            assert_eq!(set2_to_set1(&make), [code], "make {code:#x}");
            assert_eq!(set2_to_set1(&m.translate(code, false, false, false)), [code | 0x80], "break {code:#x}");
        }
        assert_eq!(n, 87);
        for &(code, _) in EXTENDED.iter() {
            assert_eq!(set2_to_set1(&m.translate(code, true, false, true)), [0xe0, code]);
            assert_eq!(set2_to_set1(&m.translate(code, true, false, false)), [0xe0, code | 0x80]);
        }
    }

    #[test]
    fn letters_and_arrows() {
        let mut m = KeyMapper::new();
        assert_eq!(m.translate(0x1e, false, false, true), [0x1c]); // A
        assert_eq!(m.translate(0x1e, false, false, false), [0xf0, 0x1c]);
        assert_eq!(m.translate(0x48, true, false, true), [0xe0, 0x75]); // Up
        assert_eq!(m.translate(0x48, false, false, true), [0x75]); // keypad 8
    }

    #[test]
    fn print_screen_and_fake_shifts() {
        let mut m = KeyMapper::new();
        assert!(m.translate(0x2a, true, false, true).is_empty());
        assert_eq!(m.translate(0x37, true, false, true), [0xe0, 0x12, 0xe0, 0x7c]);
        assert_eq!(m.translate(0x37, true, false, false), [0xe0, 0xf0, 0x7c, 0xe0, 0xf0, 0x12]);
        assert_eq!(set2_to_set1(&[0xe0, 0x12, 0xe0, 0x7c]), [0xe0, 0x2a, 0xe0, 0x37]);
    }

    const A: HostKey = HostKey { code: 0x1e, e0: false };
    const X: HostKey = HostKey { code: 0x2d, e0: false };
    const LALT: HostKey = HostKey { code: 0x38, e0: false };
    const RALT: HostKey = HostKey { code: 0x38, e0: true };
    const KEY_F: HostKey = HostKey { code: 0x21, e0: false };
    const KEY_ENTER: HostKey = HostKey { code: 0x1c, e0: false };

    fn press(h: &mut HostKeys, k: HostKey) -> (bool, HostAction) {
        h.key(k, false, true)
    }

    fn release(h: &mut HostKeys, k: HostKey) -> (bool, HostAction) {
        h.key(k, false, false)
    }

    #[test]
    fn right_ctrl_alone_releases_the_mouse() {
        let mut h = HostKeys::new();
        assert_eq!(press(&mut h, HostKey::RIGHT_CTRL), (false, HostAction::None));
        assert_eq!(press(&mut h, HostKey::RIGHT_CTRL), (false, HostAction::None), "typematic repeat");
        assert_eq!(release(&mut h, HostKey::RIGHT_CTRL), (false, HostAction::ReleaseMouse));
    }

    #[test]
    fn right_ctrl_combinations_stay_on_the_host() {
        let mut h = HostKeys::new();
        press(&mut h, HostKey::RIGHT_CTRL);
        assert_eq!(press(&mut h, KEY_F), (false, HostAction::ToggleFullscreen));
        assert_eq!(release(&mut h, HostKey::RIGHT_CTRL), (false, HostAction::None));
        assert_eq!(release(&mut h, KEY_F), (false, HostAction::None), "F's release swallowed too");
        assert_eq!(press(&mut h, KEY_F), (true, HostAction::None));
    }

    #[test]
    fn ctrl_alt_tap_releases_the_mouse_and_still_reaches_the_guest() {
        let mut h = HostKeys::new();
        assert_eq!(press(&mut h, LEFT_CTRL), (true, HostAction::None));
        assert_eq!(press(&mut h, LALT), (true, HostAction::None));
        assert_eq!(press(&mut h, LALT), (true, HostAction::None), "typematic repeat");
        assert_eq!(release(&mut h, LALT), (true, HostAction::ReleaseMouse));
        assert_eq!(release(&mut h, LEFT_CTRL), (true, HostAction::None), "only once");
        // Alt first, right Alt, works the same.
        press(&mut h, RALT);
        press(&mut h, LEFT_CTRL);
        assert_eq!(release(&mut h, LEFT_CTRL), (true, HostAction::ReleaseMouse));
        release(&mut h, RALT);
    }

    #[test]
    fn templeos_ctrl_alt_shortcuts_are_not_host_keys() {
        let mut h = HostKeys::new();
        press(&mut h, LEFT_CTRL);
        press(&mut h, LALT);
        assert_eq!(press(&mut h, X), (true, HostAction::None), "Ctrl+Alt+X kills a task");
        assert_eq!(release(&mut h, X), (true, HostAction::None));
        assert_eq!(release(&mut h, LALT), (true, HostAction::None));
        assert_eq!(release(&mut h, LEFT_CTRL), (true, HostAction::None));
        // Ctrl alone, Alt alone, Ctrl+A: nothing for the host.
        press(&mut h, LEFT_CTRL);
        assert_eq!(press(&mut h, A), (true, HostAction::None));
        assert_eq!(release(&mut h, LEFT_CTRL), (true, HostAction::None));
    }

    #[test]
    fn ctrl_alt_enter_toggles_fullscreen() {
        let mut h = HostKeys::new();
        press(&mut h, LEFT_CTRL);
        press(&mut h, LALT);
        assert_eq!(press(&mut h, KEY_ENTER), (false, HostAction::ToggleFullscreen));
        assert_eq!(release(&mut h, KEY_ENTER), (false, HostAction::None));
        assert_eq!(release(&mut h, LALT), (true, HostAction::None));
        release(&mut h, LEFT_CTRL);
        assert_eq!(press(&mut h, KEY_ENTER), (true, HostAction::None));
    }

    #[test]
    fn pause_is_one_sequence_on_press() {
        let mut m = KeyMapper::new();
        assert!(m.translate(0x1d, false, true, true).is_empty());
        let seq = m.translate(0x45, false, false, true);
        assert_eq!(seq, [0xe1, 0x14, 0x77, 0xe1, 0xf0, 0x14, 0xf0, 0x77]);
        assert_eq!(set2_to_set1(&seq), [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]);
        assert!(m.translate(0x1d, false, true, false).is_empty());
        assert!(m.translate(0x45, false, false, false).is_empty());
        // A plain 45 afterwards is Num Lock again.
        assert_eq!(m.translate(0x45, false, false, true), [0x77]);
    }
}
