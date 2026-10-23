//! egui key events → bytes for the remote side (xterm conventions).

use egui::{Key, Modifiers};

/// xterm modifier parameter: 1 + shift + 2·alt + 4·ctrl.
fn param(m: Modifiers) -> u8 {
    1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.ctrl)
}

/// Bytes for a key press that is not plain text, or `None` if the key is not
/// for the terminal (e.g. Cmd+… on macOS).
pub fn encode(key: Key, m: Modifiers, app_cursor: bool) -> Option<Vec<u8>> {
    if m.mac_cmd {
        return None;
    }
    let p = param(m);
    let csi_letter = |c: char| -> Vec<u8> {
        if p > 1 {
            format!("\x1b[1;{p}{c}").into_bytes()
        } else if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if p > 1 {
            format!("\x1b[{n};{p}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let ss3 = |c: char| -> Vec<u8> {
        if p > 1 {
            format!("\x1b[1;{p}{c}").into_bytes()
        } else {
            format!("\x1bO{c}").into_bytes()
        }
    };
    let alt = |b: &[u8]| -> Vec<u8> {
        if m.alt {
            [b"\x1b".as_slice(), b].concat()
        } else {
            b.to_vec()
        }
    };
    Some(match key {
        Key::ArrowUp => csi_letter('A'),
        Key::ArrowDown => csi_letter('B'),
        Key::ArrowRight => csi_letter('C'),
        Key::ArrowLeft => csi_letter('D'),
        Key::Home => csi_letter('H'),
        Key::End => csi_letter('F'),
        Key::Insert => tilde(2),
        Key::Delete => tilde(3),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::F1 => ss3('P'),
        Key::F2 => ss3('Q'),
        Key::F3 => ss3('R'),
        Key::F4 => ss3('S'),
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
        Key::Enter => alt(b"\r"),
        Key::Tab if m.shift => b"\x1b[Z".to_vec(),
        Key::Tab => b"\t".to_vec(),
        Key::Escape => b"\x1b".to_vec(),
        Key::Backspace if m.ctrl => b"\x08".to_vec(),
        Key::Backspace => alt(b"\x7f"),
        _ if m.ctrl => alt(&[ctrl_byte(key)?]),
        _ => return None,
    })
}

/// Ctrl+<key> control byte.
fn ctrl_byte(key: Key) -> Option<u8> {
    let name = key.name();
    if name.len() == 1 {
        let c = name.as_bytes()[0];
        if c.is_ascii_uppercase() {
            return Some(c - b'A' + 1);
        }
    }
    Some(match key {
        Key::Space | Key::Num2 => 0,
        Key::OpenBracket | Key::Num3 => 0x1b,
        Key::Backslash | Key::Num4 => 0x1c,
        Key::CloseBracket | Key::Num5 => 0x1d,
        Key::Num6 => 0x1e,
        Key::Slash | Key::Minus | Key::Num7 => 0x1f,
        Key::Num8 => 0x7f,
        _ => return None,
    })
}

/// Text typed with Alt on Windows/Linux is sent ESC-prefixed (meta).
pub fn text(s: &str, m: Modifiers) -> Vec<u8> {
    if m.alt && !cfg!(target_os = "macos") {
        [b"\x1b".as_slice(), s.as_bytes()].concat()
    } else {
        s.as_bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes() {
        let none = Modifiers::NONE;
        assert_eq!(encode(Key::ArrowUp, none, false).unwrap(), b"\x1b[A");
        assert_eq!(encode(Key::ArrowUp, none, true).unwrap(), b"\x1bOA");
        assert_eq!(encode(Key::ArrowLeft, Modifiers::CTRL, true).unwrap(), b"\x1b[1;5D");
        assert_eq!(encode(Key::C, Modifiers::CTRL, false).unwrap(), [3]);
        assert_eq!(encode(Key::OpenBracket, Modifiers::CTRL, false).unwrap(), [0x1b]);
        assert_eq!(encode(Key::Delete, Modifiers::SHIFT, false).unwrap(), b"\x1b[3;2~");
        assert_eq!(encode(Key::F5, none, false).unwrap(), b"\x1b[15~");
        assert_eq!(encode(Key::Tab, Modifiers::SHIFT, false).unwrap(), b"\x1b[Z");
        assert_eq!(encode(Key::A, none, false), None);
        assert_eq!(encode(Key::C, Modifiers::MAC_CMD, false), None);
    }
}
