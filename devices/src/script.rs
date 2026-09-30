//! Input scripts: the same steps drive templeos.exe (`--script`) and the
//! QEMU reference machine (tools/qemu-ref/qemu_trace.py `--script`), so the
//! two can be compared screenshot for screenshot (Phase 6).
//!
//! One step per line; `#` starts a comment.
//!
//! ```text
//! wait 2.5              # let the guest run this many seconds
//! type Dir;\n           # type text (US layout); escapes \n \t \\ \#
//! key ctrl+alt+x        # a chord of QEMU key names (QKeyCode), pressed in
//!                       # order and released in reverse
//! mouse 10 -5 1         # relative motion dx dy (screen: y down), buttons
//!                       # (bit 0 left, 1 right, 2 middle)
//! screenshot boot       # save the display as boot.ppm
//! ```
//!
//! Key names are QEMU's (`a`, `1`, `ret`, `spc`, `shift`, `ctrl_r`, `up`,
//! `f1`...), the same strings QMP's `input-send-event` takes.

use crate::keymap::{HostKey, KeyMapper};

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Wait(f64),
    /// Key events in order: (QEMU key name, pressed).
    Keys(Vec<(&'static str, bool)>),
    Mouse { dx: i32, dy: i32, buttons: u8 },
    Screenshot(String),
}

/// Parse a script. Errors name the line.
pub fn parse(text: &str) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        let err = |m: &str| format!("line {}: {m}: {raw}", n + 1);
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((&line, ""));
        let rest = rest.trim();
        steps.push(match cmd {
            "wait" => Step::Wait(rest.parse().map_err(|_| err("bad seconds"))?),
            "type" => {
                // `type` keeps its argument's leading/trailing spaces except
                // the one separating it from the command.
                let arg = raw.trim_start().strip_prefix("type").unwrap_or("");
                let arg = strip_comment(arg.strip_prefix(' ').unwrap_or(arg));
                Step::Keys(type_text(&unescape(arg.trim_end_matches(['\r']))).map_err(|e| err(&e))?)
            }
            "key" => {
                let names: Vec<&'static str> =
                    rest.split('+').map(|k| qcode(k.trim()).ok_or_else(|| err(&format!("unknown key {k}")))).collect::<Result<_, _>>()?;
                let mut ev: Vec<_> = names.iter().map(|&k| (k, true)).collect();
                ev.extend(names.iter().rev().map(|&k| (k, false)));
                Step::Keys(ev)
            }
            "mouse" => {
                let f: Vec<&str> = rest.split_whitespace().collect();
                if f.len() < 2 || f.len() > 3 {
                    return Err(err("expected: mouse dx dy [buttons]"));
                }
                Step::Mouse {
                    dx: f[0].parse().map_err(|_| err("bad dx"))?,
                    dy: f[1].parse().map_err(|_| err("bad dy"))?,
                    buttons: f.get(2).map_or(Ok(0), |b| b.parse()).map_err(|_| err("bad buttons"))?,
                }
            }
            "screenshot" => {
                if rest.is_empty() || rest.contains(['/', '\\']) {
                    return Err(err("screenshot needs a plain name"));
                }
                Step::Screenshot(rest.to_string())
            }
            _ => return Err(err("unknown command")),
        });
    }
    Ok(steps)
}

/// Drop a `#` comment (an escaped `\#` is kept).
fn strip_comment(s: &str) -> &str {
    let b = s.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'#' && (i == 0 || b[i - 1] != b'\\') {
            return &s[..i];
        }
    }
    s
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

/// Key events that type `text` on a US keyboard.
pub fn type_text(text: &str) -> Result<Vec<(&'static str, bool)>, String> {
    let mut ev = Vec::new();
    for c in text.chars() {
        let (key, shift) = char_key(c).ok_or_else(|| format!("can't type {c:?}"))?;
        if shift {
            ev.push(("shift", true));
        }
        ev.push((key, true));
        ev.push((key, false));
        if shift {
            ev.push(("shift", false));
        }
    }
    Ok(ev)
}

/// The key (and whether Shift is needed) for a character on a US layout.
fn char_key(c: char) -> Option<(&'static str, bool)> {
    const PLAIN: &str = "`1234567890-=qwertyuiop[]\\asdfghjkl;'zxcvbnm,./";
    const SHIFTED: &str = "~!@#$%^&*()_+QWERTYUIOP{}|ASDFGHJKL:\"ZXCVBNM<>?";
    const KEYS: [&str; 47] = [
        "grave_accent", "1", "2", "3", "4", "5", "6", "7", "8", "9", "0", "minus", "equal", "q", "w", "e", "r",
        "t", "y", "u", "i", "o", "p", "bracket_left", "bracket_right", "backslash", "a", "s", "d", "f", "g",
        "h", "j", "k", "l", "semicolon", "apostrophe", "z", "x", "c", "v", "b", "n", "m", "comma", "dot",
        "slash",
    ];
    match c {
        ' ' => return Some(("spc", false)),
        '\n' => return Some(("ret", false)),
        '\t' => return Some(("tab", false)),
        _ => {}
    }
    if let Some(i) = PLAIN.chars().position(|p| p == c) {
        return Some((KEYS[i], false));
    }
    SHIFTED.chars().position(|p| p == c).map(|i| (KEYS[i], true))
}

/// QEMU key names this module knows, with the host key (set 1 make code
/// and E0 prefix) each one is.
const QCODES: &[(&str, u8, bool)] = &[
    ("esc", 0x01, false), ("1", 0x02, false), ("2", 0x03, false), ("3", 0x04, false), ("4", 0x05, false),
    ("5", 0x06, false), ("6", 0x07, false), ("7", 0x08, false), ("8", 0x09, false), ("9", 0x0a, false),
    ("0", 0x0b, false), ("minus", 0x0c, false), ("equal", 0x0d, false), ("backspace", 0x0e, false),
    ("tab", 0x0f, false), ("q", 0x10, false), ("w", 0x11, false), ("e", 0x12, false), ("r", 0x13, false),
    ("t", 0x14, false), ("y", 0x15, false), ("u", 0x16, false), ("i", 0x17, false), ("o", 0x18, false),
    ("p", 0x19, false), ("bracket_left", 0x1a, false), ("bracket_right", 0x1b, false), ("ret", 0x1c, false),
    ("ctrl", 0x1d, false), ("a", 0x1e, false), ("s", 0x1f, false), ("d", 0x20, false), ("f", 0x21, false),
    ("g", 0x22, false), ("h", 0x23, false), ("j", 0x24, false), ("k", 0x25, false), ("l", 0x26, false),
    ("semicolon", 0x27, false), ("apostrophe", 0x28, false), ("grave_accent", 0x29, false),
    ("shift", 0x2a, false), ("backslash", 0x2b, false), ("z", 0x2c, false), ("x", 0x2d, false),
    ("c", 0x2e, false), ("v", 0x2f, false), ("b", 0x30, false), ("n", 0x31, false), ("m", 0x32, false),
    ("comma", 0x33, false), ("dot", 0x34, false), ("slash", 0x35, false), ("shift_r", 0x36, false),
    ("kp_multiply", 0x37, false), ("alt", 0x38, false), ("spc", 0x39, false), ("caps_lock", 0x3a, false),
    ("f1", 0x3b, false), ("f2", 0x3c, false), ("f3", 0x3d, false), ("f4", 0x3e, false), ("f5", 0x3f, false),
    ("f6", 0x40, false), ("f7", 0x41, false), ("f8", 0x42, false), ("f9", 0x43, false), ("f10", 0x44, false),
    ("num_lock", 0x45, false), ("scroll_lock", 0x46, false), ("kp_7", 0x47, false), ("kp_8", 0x48, false),
    ("kp_9", 0x49, false), ("kp_subtract", 0x4a, false), ("kp_4", 0x4b, false), ("kp_5", 0x4c, false),
    ("kp_6", 0x4d, false), ("kp_add", 0x4e, false), ("kp_1", 0x4f, false), ("kp_2", 0x50, false),
    ("kp_3", 0x51, false), ("kp_0", 0x52, false), ("kp_decimal", 0x53, false), ("less", 0x56, false),
    ("f11", 0x57, false), ("f12", 0x58, false), ("kp_enter", 0x1c, true), ("ctrl_r", 0x1d, true),
    ("kp_divide", 0x35, true), ("alt_r", 0x38, true), ("home", 0x47, true), ("up", 0x48, true),
    ("pgup", 0x49, true), ("left", 0x4b, true), ("right", 0x4d, true), ("end", 0x4f, true),
    ("down", 0x50, true), ("pgdn", 0x51, true), ("insert", 0x52, true), ("delete", 0x53, true),
    ("meta_l", 0x5b, true), ("meta_r", 0x5c, true), ("compose", 0x5d, true), ("print", 0x37, true),
];

/// The canonical `&'static str` for a QEMU key name, if known.
pub fn qcode(name: &str) -> Option<&'static str> {
    QCODES.iter().find(|q| q.0 == name).map(|q| q.0)
}

/// The host key a QEMU key name stands for.
pub fn host_key(name: &str) -> Option<HostKey> {
    QCODES.iter().find(|q| q.0 == name).map(|q| HostKey { code: q.1, e0: q.2 })
}

/// Set 2 bytes for a sequence of key events (what the guest's keyboard
/// sends), through the same translator the window uses.
pub fn keys_to_set2(events: &[(&str, bool)]) -> Vec<Vec<u8>> {
    let mut km = KeyMapper::new();
    events
        .iter()
        .filter_map(|&(name, pressed)| {
            let k = host_key(name)?;
            let b = km.translate(k.code, k.e0, false, pressed);
            (!b.is_empty()).then_some(b)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command() {
        let s = parse(
            "# a test\nwait 1.5\ntype Hi!\\n\nkey ctrl+alt+x   # kill\nmouse 3 -4 1\nmouse 1 2\nscreenshot a1\n",
        )
        .unwrap();
        assert_eq!(s[0], Step::Wait(1.5));
        assert_eq!(
            s[1],
            Step::Keys(vec![
                ("shift", true),
                ("h", true),
                ("h", false),
                ("shift", false),
                ("i", true),
                ("i", false),
                ("shift", true),
                ("1", true),
                ("1", false),
                ("shift", false),
                ("ret", true),
                ("ret", false),
            ])
        );
        assert_eq!(
            s[2],
            Step::Keys(vec![("ctrl", true), ("alt", true), ("x", true), ("x", false), ("alt", false), ("ctrl", false)])
        );
        assert_eq!(s[3], Step::Mouse { dx: 3, dy: -4, buttons: 1 });
        assert_eq!(s[4], Step::Mouse { dx: 1, dy: 2, buttons: 0 });
        assert_eq!(s[5], Step::Screenshot("a1".into()));
        assert!(parse("key ctrl+nope").unwrap_err().contains("unknown key nope"));
        assert!(parse("dance").is_err());
        assert!(parse("screenshot ../x").is_err());
    }

    #[test]
    fn type_keeps_spaces_and_escaped_hashes() {
        let s = parse("type  a \\# b").unwrap();
        let Step::Keys(ev) = &s[0] else { panic!() };
        let typed: Vec<&str> = ev.iter().filter(|e| e.1).map(|e| e.0).collect();
        assert_eq!(typed, ["spc", "a", "spc", "shift", "3", "spc", "b"]);
    }

    #[test]
    fn every_printable_ascii_character_is_typable() {
        for c in (0x20u8..0x7f).map(char::from) {
            let ev = type_text(&c.to_string()).unwrap();
            assert!(!keys_to_set2(&ev).is_empty(), "{c:?}");
        }
    }

    #[test]
    fn qcodes_map_to_the_keys_qemu_sends() {
        // Spot checks against QEMU's qcode -> set 2 tables.
        assert_eq!(keys_to_set2(&[("a", true), ("a", false)]), [vec![0x1c], vec![0xf0, 0x1c]]);
        assert_eq!(keys_to_set2(&[("up", true)]), [vec![0xe0, 0x75]]);
        assert_eq!(keys_to_set2(&[("f7", true)]), [vec![0x83]]);
        assert_eq!(keys_to_set2(&[("print", true)]), [vec![0xe0, 0x12, 0xe0, 0x7c]]);
    }
}

#[cfg(test)]
mod script_files {
    #[test]
    fn repository_scripts_parse() {
        for text in [
            include_str!("../../tests/scripts/cd_smoke.script"),
            include_str!("../../tests/scripts/demos.script"),
        ] {
            super::parse(text).unwrap();
        }
    }
}
