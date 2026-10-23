use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, db::Db, highlight};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ToolbarMode {
    Full,
    IconsOnly,
    Hidden,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    Block,
    Beam,
    Underline,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum RightClick {
    /// Not chosen yet: the first right-click asks.
    Ask,
    Paste,
    Menu,
}

/// Reopen last run's tabs at start (SPEC §4.5).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMode {
    Ask,
    Always,
    Never,
}

/// App settings. Each field is one row in the `settings` table; missing rows
/// fall back to the defaults below.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Settings {
    pub theme: String,
    pub link_themes: bool,
    pub terminal_theme: String,
    pub follow_os: bool,
    pub os_dark_theme: String,
    pub os_light_theme: String,
    /// "system", "en" or "ru".
    pub language: String,
    pub ui_scale: f32,
    pub compact: bool,
    pub toolbar: ToolbarMode,
    pub side_panel_right: bool,
    /// Empty = OS default monospace.
    pub term_font: String,
    pub term_font_size: f32,
    pub cursor: CursorShape,
    pub cursor_blink: bool,
    pub scrollback: usize,
    pub copy_on_select: bool,
    pub right_click: RightClick,
    pub paste_confirm: bool,
    pub paste_delay_ms: u32,
    pub highlight: bool,
    pub keyword_rules: Vec<highlight::Rule>,
    pub log_sessions: bool,
    pub log_raw: bool,
    pub log_timestamps: bool,
    /// Rotate session logs after this many MB; 0 = never.
    pub log_max_mb: u32,
    /// 2px border in the session color around colored sessions.
    pub color_border: bool,
    pub vault_lock_minutes: u32,
    /// Key files loaded into the built-in agent while the vault is unlocked.
    pub agent_key_paths: Vec<String>,
    /// Show the read-only `~/.ssh/config` folder.
    pub ssh_config_folder: bool,
    /// Parallel SFTP transfers per connection.
    pub sftp_parallel: u32,
    pub sftp_show_hidden: bool,
    pub sftp_follow: bool,
    /// Hosts where the OSC 7 hook offer must not be shown again.
    pub osc7_no_ask: Vec<String>,
    pub restore: RestoreMode,
    /// Shortcut overrides: action id → "Cmd+Shift+N" ("" = unbound).
    pub hotkeys: std::collections::BTreeMap<String, String>,
    /// Multi-exec: ask before sending risky commands (SPEC §8.6).
    pub risky_confirm: bool,
    /// Multi-exec jobs: parallel hosts and delay between starts.
    pub job_parallel: u32,
    pub job_delay_ms: u32,
    /// RDP on macOS: Cmd acts as Ctrl ("Mac-friendly" keys, SPEC §5.8).
    pub rdp_mac_keys: bool,
    /// Remote system monitor in the status bar for SSH tabs (SPEC §5.7).
    pub monitor_enabled: bool,
    /// Local X display for X11 forwarding; empty = `$DISPLAY`, else `:0`.
    pub x11_display: String,
    /// App log at `debug` level (applies after restart).
    pub debug_log: bool,
    /// Lock the vault after `vault_lock_minutes` idle; off = once per start (SPEC-9 §9.1).
    pub vault_auto_lock: bool,
    pub vault_lock_on_sleep: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "Ayu Dark".into(),
            link_themes: true,
            terminal_theme: "Ayu Dark".into(),
            follow_os: false,
            os_dark_theme: "Ayu Dark".into(),
            os_light_theme: "Ayu Light".into(),
            language: "system".into(),
            ui_scale: 1.0,
            compact: false,
            toolbar: ToolbarMode::Full,
            side_panel_right: false,
            term_font: String::new(),
            term_font_size: 13.0,
            cursor: CursorShape::Block,
            cursor_blink: false,
            scrollback: 10_000,
            copy_on_select: true,
            right_click: RightClick::Ask,
            paste_confirm: true,
            paste_delay_ms: 200,
            highlight: true,
            keyword_rules: highlight::default_rules(),
            log_sessions: false,
            log_raw: false,
            log_timestamps: false,
            log_max_mb: 0,
            color_border: false,
            vault_lock_minutes: 15,
            agent_key_paths: vec![],
            ssh_config_folder: true,
            sftp_parallel: 3,
            sftp_show_hidden: false,
            sftp_follow: true,
            osc7_no_ask: vec![],
            restore: RestoreMode::Ask,
            hotkeys: Default::default(),
            risky_confirm: true,
            job_parallel: 10,
            job_delay_ms: 0,
            rdp_mac_keys: true,
            monitor_enabled: true,
            x11_display: String::new(),
            debug_log: false,
            vault_auto_lock: true,
            vault_lock_on_sleep: true,
        }
    }
}

impl Settings {
    pub fn load(db: &Db) -> Result<Self, Error> {
        let Value::Object(mut map) = serde_json::to_value(Self::default())? else {
            return Ok(Self::default());
        };
        map.extend(db.settings_map()?);
        // A row with a wrong type (e.g. after a downgrade) must not wipe the rest.
        Ok(serde_json::from_value(Value::Object(map.clone())).unwrap_or_else(|_| {
            let mut s = Self::default();
            for (k, v) in map {
                s.try_set(&k, v);
            }
            s
        }))
    }

    fn try_set(&mut self, key: &str, value: Value) {
        let Ok(Value::Object(mut current)) = serde_json::to_value(&*self) else {
            return;
        };
        current.insert(key.to_owned(), value);
        if let Ok(s) = serde_json::from_value(Value::Object(current)) {
            *self = s;
        }
    }

    /// Writes only the fields that differ from `old`.
    pub fn save(&self, old: &Self, db: &Db) -> Result<(), Error> {
        let (Value::Object(new), Value::Object(old)) = (serde_json::to_value(self)?, serde_json::to_value(old)?) else {
            return Ok(());
        };
        for (k, v) in new {
            if old.get(&k) != Some(&v) {
                db.set_setting(&k, &v)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_and_bad_row() {
        let db = Db::open_in_memory().unwrap();
        let s = Settings {
            theme: "Monokai".into(),
            ui_scale: 1.25,
            ..Default::default()
        };
        s.save(&Settings::default(), &db).unwrap();
        db.set_setting("compact", &Value::from("not a bool")).unwrap();
        let loaded = Settings::load(&db).unwrap();
        assert_eq!(loaded.theme, "Monokai");
        assert_eq!(loaded.ui_scale, 1.25);
        assert!(!loaded.compact);
    }
}
