//! App shortcuts (SPEC §4.6): one table, MobaXterm-like defaults per OS,
//! user overrides stored in settings as text ("Cmd+Shift+N").

use std::collections::BTreeMap;

use egui::{Key, KeyboardShortcut, Modifiers};

use crate::app::Action;

/// (id, label key, macOS default, Windows/Linux default, action)
pub const TABLE: &[(&str, &str, &str, &str, Action)] = &[
    (
        "new-session",
        "hk-new-session",
        "Cmd+Shift+N",
        "Ctrl+Shift+N",
        Action::NewSession,
    ),
    (
        "new-local-shell",
        "hk-new-local-shell",
        "Cmd+T",
        "Ctrl+Shift+T",
        Action::NewLocalShell(None),
    ),
    ("close-tab", "hk-close-tab", "Cmd+W", "Ctrl+Shift+W", Action::CloseTab),
    (
        "prev-tab",
        "hk-prev-tab",
        "Ctrl+Shift+Tab",
        "Ctrl+Shift+Tab",
        Action::PrevTab,
    ),
    ("next-tab", "hk-next-tab", "Ctrl+Tab", "Ctrl+Tab", Action::NextTab),
    ("tab-1", "hk-tab-1", "Cmd+1", "Alt+1", Action::GoToTab(0)),
    ("tab-2", "hk-tab-2", "Cmd+2", "Alt+2", Action::GoToTab(1)),
    ("tab-3", "hk-tab-3", "Cmd+3", "Alt+3", Action::GoToTab(2)),
    ("tab-4", "hk-tab-4", "Cmd+4", "Alt+4", Action::GoToTab(3)),
    ("tab-5", "hk-tab-5", "Cmd+5", "Alt+5", Action::GoToTab(4)),
    ("tab-6", "hk-tab-6", "Cmd+6", "Alt+6", Action::GoToTab(5)),
    ("tab-7", "hk-tab-7", "Cmd+7", "Alt+7", Action::GoToTab(6)),
    ("tab-8", "hk-tab-8", "Cmd+8", "Alt+8", Action::GoToTab(7)),
    ("tab-9", "hk-tab-9", "Cmd+9", "Alt+9", Action::GoToTab(8)),
    ("copy", "hk-copy", "Cmd+C", "Ctrl+Shift+C", Action::Copy),
    ("paste", "hk-paste", "Cmd+V", "Ctrl+Shift+V", Action::Paste),
    ("find", "hk-find", "Cmd+F", "Ctrl+Shift+F", Action::Find),
    (
        "split-down",
        "hk-split-down",
        "Cmd+Shift+D",
        "Ctrl+Shift+E",
        Action::SplitDown,
    ),
    (
        "split-right",
        "hk-split-right",
        "Cmd+D",
        "Ctrl+Shift+D",
        Action::SplitRight,
    ),
    (
        "to-window",
        "hk-to-window",
        "Cmd+Shift+Enter",
        "Ctrl+Shift+Enter",
        Action::ToWindow,
    ),
    (
        "multi-exec",
        "hk-multi-exec",
        "Cmd+Shift+M",
        "Ctrl+Shift+M",
        Action::MultiExec,
    ),
    (
        "toggle-side",
        "hk-toggle-side",
        "Cmd+B",
        "Ctrl+Shift+B",
        Action::ToggleSidePanel,
    ),
    ("font-bigger", "hk-font-bigger", "Cmd+=", "Ctrl+=", Action::FontBigger),
    (
        "font-smaller",
        "hk-font-smaller",
        "Cmd+-",
        "Ctrl+-",
        Action::FontSmaller,
    ),
    ("font-reset", "hk-font-reset", "Cmd+0", "Ctrl+0", Action::FontReset),
    ("fullscreen", "hk-fullscreen", "Cmd+Ctrl+F", "F11", Action::Fullscreen),
    ("settings", "hk-settings", "Cmd+,", "Ctrl+,", Action::OpenSettings),
];

const MAC: bool = cfg!(target_os = "macos");

pub fn default_for(id: &str) -> &'static str {
    TABLE
        .iter()
        .find(|r| r.0 == id)
        .map_or("", |r| if MAC { r.2 } else { r.3 })
}

/// Effective binding text for `id` (user override or default; "" = unbound).
pub fn binding<'a>(id: &str, overrides: &'a BTreeMap<String, String>) -> &'a str {
    overrides.get(id).map(String::as_str).unwrap_or_else(|| default_for(id))
}

fn key_from(name: &str) -> Option<Key> {
    match name {
        "=" => Some(Key::Equals),
        "-" => Some(Key::Minus),
        "," => Some(Key::Comma),
        "." => Some(Key::Period),
        "/" => Some(Key::Slash),
        _ => Key::from_name(name),
    }
}

fn key_name(key: Key) -> String {
    match key {
        Key::Equals => "=".into(),
        Key::Minus => "-".into(),
        Key::Comma => ",".into(),
        Key::Period => ".".into(),
        Key::Slash => "/".into(),
        k => k.name().to_owned(),
    }
}

/// "Cmd+Shift+N" → shortcut. `Cmd` is ⌘ on macOS and Ctrl elsewhere.
pub fn parse(text: &str) -> Option<KeyboardShortcut> {
    let parts: Vec<&str> = text.split('+').map(str::trim).collect();
    // A trailing "+" key ("Cmd++") splits into an empty last part.
    let (mods, key) = if text.ends_with("++") {
        (&parts[..parts.len() - 2], "+")
    } else {
        parts.split_last().map(|(k, m)| (m, *k))?
    };
    let mut m = Modifiers::NONE;
    for p in mods {
        match *p {
            "Cmd" => {
                m.command = true;
                m.mac_cmd = MAC;
                m.ctrl |= !MAC;
            }
            "Ctrl" => {
                m.ctrl = true;
                m.command |= !MAC;
            }
            "Alt" => m.alt = true,
            "Shift" => m.shift = true,
            _ => return None,
        }
    }
    Some(KeyboardShortcut::new(
        m,
        if key == "+" { Key::Plus } else { key_from(key)? },
    ))
}

/// Text form of a key press, using the same names `parse` reads.
pub fn format(m: Modifiers, key: Key) -> String {
    let mut out = String::new();
    if MAC && m.mac_cmd {
        out.push_str("Cmd+");
    }
    if m.ctrl {
        out.push_str("Ctrl+");
    }
    if m.alt {
        out.push_str("Alt+");
    }
    if m.shift {
        out.push_str("Shift+");
    }
    out + &key_name(key)
}

/// SPEC §4.6: plain Ctrl+<letter> always belongs to the terminal.
pub fn shadows_terminal(m: Modifiers, key: Key) -> bool {
    let letter = key.name().len() == 1 && key.name().as_bytes()[0].is_ascii_alphabetic();
    m.ctrl && !m.shift && !m.alt && !(MAC && m.mac_cmd) && letter
}

/// A key press matches `s` by its logical or physical key (extra Shift/Alt allowed).
pub fn matches(s: &KeyboardShortcut, key: Key, physical: Option<Key>, m: Modifiers) -> bool {
    (s.logical_key == key || physical == Some(s.logical_key)) && m.matches_logically(s.modifiers)
}

/// Shortcuts to check this frame, Shift variants first (egui ignores extra Shift).
pub fn active(overrides: &BTreeMap<String, String>) -> Vec<(KeyboardShortcut, Action)> {
    let mut v: Vec<(KeyboardShortcut, Action)> = TABLE
        .iter()
        .filter_map(|(id, _, _, _, action)| parse(binding(id, overrides)).map(|s| (s, *action)))
        .collect();
    // Cmd+= also fires on Cmd++ (the same key with Shift on most layouts).
    if let Some((s, a)) = v.iter().find(|(_, a)| *a == Action::FontBigger).copied()
        && s.logical_key == Key::Equals
    {
        v.push((KeyboardShortcut::new(s.modifiers, Key::Plus), a));
    }
    v.sort_by_key(|(s, _)| std::cmp::Reverse(s.modifiers.shift));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_format_roundtrip_and_rules() {
        for (id, ..) in TABLE {
            let text = default_for(id);
            let s = parse(text).unwrap_or_else(|| panic!("{id}: {text}"));
            assert!(!shadows_terminal(s.modifiers, s.logical_key), "{id}");
        }
        let s = parse("Ctrl+Shift+T").unwrap();
        assert_eq!(format(s.modifiers, s.logical_key), "Ctrl+Shift+T");
        assert!(shadows_terminal(Modifiers::CTRL, Key::R));
        assert!(!shadows_terminal(Modifiers::CTRL | Modifiers::SHIFT, Key::R));
        assert!(parse("Hyper+X").is_none());
        // Cmd+Shift+1 arrives as logical "!" with physical "1".
        let s = parse("Cmd+Shift+1").unwrap();
        assert!(matches(&s, Key::Exclamationmark, Some(Key::Num1), s.modifiers));
        assert!(!matches(&s, Key::Num2, Some(Key::Num2), s.modifiers));
        let mut o = BTreeMap::new();
        o.insert("find".to_owned(), String::new());
        assert!(!active(&o).iter().any(|(_, a)| *a == Action::Find));
    }
}
