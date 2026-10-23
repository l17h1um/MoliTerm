use egui::{Align, Layout, RichText, Sense, Stroke, text::LayoutJob, vec2};
use moliterm_core::{
    settings::{CursorShape, RightClick, Settings, ToolbarMode},
    theme::Rgb,
    theme::Theme,
};

use crate::{
    i18n::I18n,
    icons::{self, Icon},
    style::c,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    General,
    Appearance,
    Terminal,
    Keyboard,
    Ssh,
    Agent,
    Vault,
    Sftp,
    Monitor,
    Import,
    Language,
    Advanced,
}

/// Section, its title key, and the labels inside it (used by the search box).
const SECTIONS: &[(Section, &str, &[&str])] = &[
    (
        Section::General,
        "settings-general",
        &["appearance-toolbar", "appearance-side-right", "rdp-mac-keys"],
    ),
    (
        Section::Appearance,
        "settings-appearance",
        &[
            "appearance-presets",
            "appearance-link",
            "appearance-fonts",
            "appearance-term-font",
            "appearance-ui-scale",
            "appearance-compact",
            "appearance-cursor",
        ],
    ),
    (
        Section::Terminal,
        "settings-terminal",
        &[
            "terminal-scrollback",
            "terminal-copy-on-select",
            "terminal-right-click",
            "terminal-paste-confirm",
            "terminal-highlight",
            "terminal-logging",
        ],
    ),
    (Section::Keyboard, "settings-keyboard", &["hk-hint"]),
    (
        Section::Ssh,
        "settings-ssh",
        &["ssh-known-hosts", "ssh-show-config", "keygen-title"],
    ),
    (Section::Agent, "settings-agent", &["agent-loaded", "agent-add"]),
    (
        Section::Vault,
        "settings-vault",
        &["vault-change", "vault-lock-title", "vault-lock-never"],
    ),
    (
        Section::Sftp,
        "settings-sftp",
        &["settings-sftp-parallel", "settings-sftp-hidden", "settings-sftp-follow"],
    ),
    (Section::Monitor, "settings-monitor", &["monitor-enabled"]),
    (
        Section::Import,
        "settings-import",
        &["import-mobaxterm", "import-putty-reg"],
    ),
    (Section::Language, "settings-language", &["language-label"]),
    (
        Section::Advanced,
        "settings-advanced",
        &["advanced-debug-log", "advanced-data-dir"],
    ),
];

pub struct Page {
    pub draft: Settings,
    section: Section,
    search: String,
    /// Shortcut id waiting for a key press.
    capture: Option<&'static str>,
    capture_error: Option<String>,
}

impl Page {
    pub fn new(draft: Settings) -> Self {
        Self {
            draft,
            section: Section::Appearance,
            search: String::new(),
            capture: None,
            capture_error: None,
        }
    }

    /// True while a shortcut is being recorded (app shortcuts are paused).
    pub fn capturing(&self) -> bool {
        self.capture.is_some()
    }
}

#[cfg(any(debug_assertions, feature = "devtools"))]
impl Page {
    pub fn dev_section(&mut self, key: &str) {
        let key = format!("settings-{key}");
        if let Some(s) = SECTIONS.iter().find(|s| s.1 == key) {
            self.section = s.0;
        }
    }
}

pub enum Outcome {
    Keep,
    Apply,
    Close,
}

/// `custom` draws sections that need app state (vault, agent, SSH).
pub fn show(
    ui: &mut egui::Ui,
    page: &mut Page,
    i18n: &I18n,
    themes: &[Theme],
    fonts: Option<&(String, Vec<String>)>,
    current: &Theme,
    custom: &mut dyn FnMut(&mut egui::Ui, Section, &mut Settings),
) -> Outcome {
    let t = |id: &str| i18n.t(id);
    let mut outcome = Outcome::Keep;
    let panel = egui::Frame::new().fill(c(current.ui.panel));

    egui::Panel::top("settings-top")
        .frame(panel.inner_margin(egui::Margin::symmetric(16, 8)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                icons::paint(ui.painter(), r, Icon::Gear, ui.visuals().text_color());
                ui.label(RichText::new(t("settings-title")).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if icons::button(ui, Icon::Close, 14.0, &t("settings-cancel")).clicked() {
                        outcome = Outcome::Close;
                    }
                    ui.add(
                        crate::style::field(&mut page.search)
                            .hint_text(t("settings-search"))
                            .desired_width(280.0),
                    );
                });
            });
        });

    egui::Panel::bottom("settings-bottom")
        .frame(panel.inner_margin(egui::Margin::symmetric(16, 10)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.button(t("settings-reset")).clicked() {
                    reset_section(&mut page.draft, page.section);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let apply = egui::Button::new(RichText::new(t("settings-apply")).color(c(current.ui.on_accent)))
                        .fill(c(current.ui.accent));
                    if ui.add(apply).clicked() {
                        outcome = Outcome::Apply;
                    }
                    if ui.button(t("settings-cancel")).clicked() {
                        outcome = Outcome::Close;
                    }
                });
            });
        });

    // Narrower list in small windows; it scrolls when the window is short.
    let list_w = (ui.ctx().content_rect().width() * 0.3).clamp(150.0, 240.0);
    egui::Panel::left("settings-sections")
        .exact_size(list_w)
        .resizable(false)
        .frame(egui::Frame::new().fill(c(current.ui.bg)).inner_margin(10))
        .show(ui, |ui| {
            let q = page.search.to_lowercase();
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.with_layout(Layout::top_down_justified(Align::Min), |ui| {
                    for (section, key, items) in SECTIONS {
                        let visible = q.is_empty()
                            || std::iter::once(key)
                                .chain(items.iter())
                                .any(|k| i18n.t(k).to_lowercase().contains(&q));
                        let button = egui::Button::selectable(page.section == *section, t(key))
                            .frame_when_inactive(false)
                            .min_size(vec2(0.0, 30.0));
                        if visible && ui.add(button).clicked() {
                            page.section = *section;
                        }
                    }
                });
            });
        });

    egui::CentralPanel::default_margins()
        .frame(egui::Frame::new().fill(c(current.ui.bg)).inner_margin(24))
        .show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match page.section {
                Section::Appearance => appearance(ui, &mut page.draft, i18n, themes, fonts, current),
                Section::General => general(ui, &mut page.draft, i18n),
                Section::Terminal => terminal(ui, &mut page.draft, i18n),
                Section::Sftp => sftp(ui, &mut page.draft, i18n),
                Section::Keyboard => keyboard(ui, page, i18n),
                sec @ (Section::Vault | Section::Agent | Section::Ssh | Section::Import | Section::Advanced) => {
                    custom(ui, sec, &mut page.draft)
                }
                Section::Language => language(ui, &mut page.draft, i18n),
                Section::Monitor => {
                    ui.heading(t("settings-monitor"));
                    ui.add_space(10.0);
                    ui.checkbox(&mut page.draft.monitor_enabled, t("monitor-enabled"));
                    ui.label(RichText::new(t("monitor-hint")).small().weak());
                }
            });
        });

    outcome
}

fn reset_section(s: &mut Settings, section: Section) {
    let d = Settings::default();
    match section {
        Section::General => {
            s.toolbar = d.toolbar;
            s.side_panel_right = d.side_panel_right;
            s.restore = d.restore;
            s.rdp_mac_keys = d.rdp_mac_keys;
        }
        Section::Appearance => {
            *s = Settings {
                language: s.language.clone(),
                toolbar: s.toolbar,
                side_panel_right: s.side_panel_right,
                ..d
            }
        }
        Section::Language => s.language = d.language,
        Section::Monitor => s.monitor_enabled = d.monitor_enabled,
        Section::Advanced => s.debug_log = d.debug_log,
        Section::Keyboard => s.hotkeys = d.hotkeys,
        Section::Vault => {
            s.vault_lock_minutes = d.vault_lock_minutes;
            s.vault_auto_lock = d.vault_auto_lock;
            s.vault_lock_on_sleep = d.vault_lock_on_sleep;
        }
        Section::Sftp => {
            s.sftp_parallel = d.sftp_parallel;
            s.sftp_show_hidden = d.sftp_show_hidden;
            s.sftp_follow = d.sftp_follow;
        }
        Section::Ssh => {
            s.ssh_config_folder = d.ssh_config_folder;
            s.color_border = d.color_border;
            s.x11_display = d.x11_display;
        }
        Section::Terminal => {
            s.scrollback = d.scrollback;
            s.copy_on_select = d.copy_on_select;
            s.right_click = d.right_click;
            s.paste_confirm = d.paste_confirm;
            s.paste_delay_ms = d.paste_delay_ms;
            s.highlight = d.highlight;
            s.keyword_rules = d.keyword_rules;
            s.log_sessions = d.log_sessions;
            s.log_raw = d.log_raw;
            s.log_timestamps = d.log_timestamps;
            s.log_max_mb = d.log_max_mb;
        }
        _ => {}
    }
}

fn caption(ui: &mut egui::Ui, text: String) {
    ui.add_space(14.0);
    ui.label(
        RichText::new(text)
            .small()
            .strong()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(4.0);
}

fn general(ui: &mut egui::Ui, s: &mut Settings, i18n: &I18n) {
    let t = |id: &str| i18n.t(id);
    ui.heading(t("settings-general"));
    caption(ui, t("appearance-toolbar"));
    for (mode, key) in [
        (ToolbarMode::Full, "appearance-toolbar-full"),
        (ToolbarMode::IconsOnly, "appearance-toolbar-icons"),
        (ToolbarMode::Hidden, "appearance-toolbar-hidden"),
    ] {
        ui.radio_value(&mut s.toolbar, mode, t(key));
    }
    ui.add_space(10.0);
    ui.checkbox(&mut s.side_panel_right, t("appearance-side-right"));
    caption(ui, t("restore-setting"));
    for (mode, key) in [
        (moliterm_core::settings::RestoreMode::Ask, "restore-ask"),
        (moliterm_core::settings::RestoreMode::Always, "restore-always"),
        (moliterm_core::settings::RestoreMode::Never, "restore-never"),
    ] {
        ui.radio_value(&mut s.restore, mode, t(key));
    }
    if cfg!(target_os = "macos") {
        caption(ui, "RDP".into());
        ui.checkbox(&mut s.rdp_mac_keys, t("rdp-mac-keys"));
    }
}

fn terminal(ui: &mut egui::Ui, s: &mut Settings, i18n: &I18n) {
    let t = |id: &str| i18n.t(id);
    ui.heading(t("settings-terminal"));
    ui.add_space(10.0);
    egui::Grid::new("terminal-grid")
        .num_columns(2)
        .spacing([16.0, 8.0])
        .show(ui, |ui| {
            ui.label(t("terminal-scrollback"));
            ui.add(
                egui::DragValue::new(&mut s.scrollback)
                    .range(100..=1_000_000)
                    .speed(100),
            );
            ui.end_row();
            ui.label(t("terminal-right-click"));
            ui.horizontal(|ui| {
                for (v, key) in [
                    (RightClick::Ask, "terminal-rclick-ask"),
                    (RightClick::Paste, "terminal-rclick-paste"),
                    (RightClick::Menu, "terminal-rclick-menu"),
                ] {
                    ui.radio_value(&mut s.right_click, v, t(key));
                }
            });
            ui.end_row();
            ui.label(t("terminal-paste-delay"));
            ui.add(egui::DragValue::new(&mut s.paste_delay_ms).range(0..=5000));
            ui.end_row();
        });
    ui.checkbox(&mut s.copy_on_select, t("terminal-copy-on-select"));
    ui.checkbox(&mut s.paste_confirm, t("terminal-paste-confirm"));

    caption(ui, t("terminal-highlight").to_uppercase());
    ui.checkbox(&mut s.highlight, t("terminal-highlight"));
    let mut remove = None;
    egui::Grid::new("rules")
        .num_columns(5)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            ui.label("");
            ui.label(t("terminal-rule-pattern"));
            ui.label(t("terminal-rule-color"));
            ui.label(t("terminal-rule-case"));
            ui.end_row();
            for (i, rule) in s.keyword_rules.iter_mut().enumerate() {
                ui.checkbox(&mut rule.enabled, "");
                let bad = regex::Regex::new(&rule.pattern).is_err();
                ui.add(
                    crate::style::field(&mut rule.pattern)
                        .desired_width(280.0)
                        .font(egui::TextStyle::Monospace)
                        .text_color_opt(bad.then_some(ui.visuals().error_fg_color)),
                );
                let Rgb(r, g, b) = Rgb::try_from(rule.fg.clone()).unwrap_or(Rgb(255, 255, 255));
                let mut rgb = [r, g, b];
                if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
                    rule.fg = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
                }
                ui.checkbox(&mut rule.case_insensitive, "");
                if ui.small_button(t("terminal-rule-remove")).clicked() {
                    remove = Some(i);
                }
                ui.end_row();
            }
        });
    if let Some(i) = remove {
        s.keyword_rules.remove(i);
    }
    if ui.button(t("terminal-rule-add")).clicked() {
        s.keyword_rules.push(moliterm_core::highlight::Rule {
            pattern: String::new(),
            fg: "#59c2ff".into(),
            bg: String::new(),
            case_insensitive: true,
            enabled: true,
        });
    }

    caption(ui, t("terminal-logging").to_uppercase());
    ui.checkbox(&mut s.log_sessions, t("terminal-log-on"));
    ui.add_enabled_ui(s.log_sessions, |ui| {
        ui.checkbox(&mut s.log_raw, t("terminal-log-raw"));
        ui.checkbox(&mut s.log_timestamps, t("terminal-log-timestamps"));
        ui.horizontal(|ui| {
            ui.label(t("terminal-log-rotate"));
            ui.add(egui::DragValue::new(&mut s.log_max_mb).range(0..=10_000));
        });
    });
}

fn keyboard(ui: &mut egui::Ui, page: &mut Page, i18n: &I18n) {
    use crate::hotkeys;
    let t = |id: &str| i18n.t(id);
    ui.heading(t("settings-keyboard"));
    ui.label(RichText::new(t("hk-hint")).color(ui.visuals().weak_text_color()));
    ui.add_space(10.0);

    // Record the next key press for the shortcut being edited.
    if let Some(id) = page.capture {
        let pressed = ui.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    modifiers,
                    ..
                } => Some((physical_key.unwrap_or(*key), *modifiers)),
                _ => None,
            })
        });
        if let Some((key, m)) = pressed {
            let clear = m.is_none() && matches!(key, egui::Key::Backspace | egui::Key::Delete);
            if key == egui::Key::Escape && m.is_none() {
                page.capture = None;
            } else if clear {
                page.draft.hotkeys.insert(id.to_owned(), String::new());
                page.capture = None;
            } else if hotkeys::shadows_terminal(m, key) {
                page.capture_error = Some(t("hk-err-terminal"));
            } else {
                let text = hotkeys::format(m, key);
                let clash = hotkeys::TABLE
                    .iter()
                    .find(|(other, ..)| *other != id && hotkeys::binding(other, &page.draft.hotkeys) == text);
                match clash {
                    Some((_, label, ..)) => {
                        page.capture_error = Some(i18n.t_args("hk-err-used", &[("action", &t(label))]))
                    }
                    None => {
                        if text == hotkeys::default_for(id) {
                            page.draft.hotkeys.remove(id);
                        } else {
                            page.draft.hotkeys.insert(id.to_owned(), text);
                        }
                        page.capture = None;
                        page.capture_error = None;
                    }
                }
            }
            ui.input_mut(|i| i.events.clear());
        }
    }

    egui::Grid::new("hotkeys")
        .num_columns(3)
        .striped(true)
        .spacing([24.0, 4.0])
        .show(ui, |ui| {
            for (id, label, ..) in hotkeys::TABLE {
                ui.label(t(label));
                let current = hotkeys::binding(id, &page.draft.hotkeys);
                let text = if page.capture == Some(*id) {
                    t("hk-press")
                } else if current.is_empty() {
                    t("hk-none")
                } else {
                    current.to_owned()
                };
                if ui
                    .add(egui::Button::new(RichText::new(text).monospace()).min_size(egui::vec2(180.0, 0.0)))
                    .clicked()
                {
                    page.capture = Some(id);
                    page.capture_error = None;
                }
                if page.draft.hotkeys.contains_key(*id) {
                    if ui.small_button(t("hk-reset")).clicked() {
                        page.draft.hotkeys.remove(*id);
                    }
                } else {
                    ui.label("");
                }
                ui.end_row();
            }
            ui.label(t("hk-reconnect"));
            ui.label(RichText::new("R").monospace());
            ui.label("");
            ui.end_row();
        });
    if let Some(e) = &page.capture_error {
        ui.label(RichText::new(e).color(ui.visuals().error_fg_color));
    }
}

fn sftp(ui: &mut egui::Ui, s: &mut Settings, i18n: &I18n) {
    let t = |id: &str| i18n.t(id);
    ui.heading(t("settings-sftp"));
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.label(t("settings-sftp-parallel"));
        ui.add(egui::DragValue::new(&mut s.sftp_parallel).range(1..=8));
    });
    ui.checkbox(&mut s.sftp_show_hidden, t("settings-sftp-hidden"));
    ui.checkbox(&mut s.sftp_follow, t("settings-sftp-follow"));
}

fn language(ui: &mut egui::Ui, s: &mut Settings, i18n: &I18n) {
    ui.heading(i18n.t("settings-language"));
    caption(ui, i18n.t("language-label"));
    for (code, key) in [
        ("system", "language-system"),
        ("en", "language-en"),
        ("ru", "language-ru"),
    ] {
        ui.radio_value(&mut s.language, code.to_owned(), i18n.t(key));
    }
}

fn theme_combo(ui: &mut egui::Ui, id: &str, value: &mut String, themes: &[Theme]) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.as_str())
        .width(200.0)
        .show_ui(ui, |ui| {
            for th in themes {
                ui.selectable_value(value, th.name.clone(), &th.name);
            }
        });
}

fn appearance(
    ui: &mut egui::Ui,
    s: &mut Settings,
    i18n: &I18n,
    themes: &[Theme],
    fonts: Option<&(String, Vec<String>)>,
    current: &Theme,
) {
    let t = |id: &str| i18n.t(id);
    let preview_w = 360.0;
    let wide = ui.available_width() > 900.0;
    ui.horizontal_top(|ui| {
        ui.vertical(|ui| {
            if wide {
                ui.set_max_width(ui.available_width() - preview_w - 24.0);
            }
            ui.heading(t("settings-appearance"));
            ui.label(RichText::new(t("appearance-hint")).color(ui.visuals().weak_text_color()));

            caption(ui, t("appearance-presets"));
            ui.horizontal_wrapped(|ui| {
                for th in themes {
                    let selected = if s.follow_os {
                        &th.name == if th.dark { &s.os_dark_theme } else { &s.os_light_theme }
                    } else {
                        th.name == s.theme
                    };
                    let sub = t(if th.dark { "appearance-dark" } else { "appearance-light" });
                    if theme_card(ui, th, &sub, selected, current).clicked() {
                        if s.follow_os {
                            if th.dark {
                                s.os_dark_theme = th.name.clone();
                            } else {
                                s.os_light_theme = th.name.clone();
                            }
                        } else {
                            s.theme = th.name.clone();
                        }
                        if s.link_themes {
                            s.terminal_theme = th.name.clone();
                        }
                    }
                }
            });
            ui.add_space(8.0);
            ui.checkbox(&mut s.link_themes, t("appearance-link"));
            if s.link_themes {
                s.terminal_theme = s.theme.clone();
            } else {
                ui.horizontal(|ui| {
                    ui.label(t("appearance-terminal-theme"));
                    theme_combo(ui, "term-theme", &mut s.terminal_theme, themes);
                });
            }
            let follow = i18n.t_args(
                "appearance-follow-os",
                &[("dark", &s.os_dark_theme), ("light", &s.os_light_theme)],
            );
            ui.checkbox(&mut s.follow_os, follow);
            if s.follow_os {
                ui.horizontal(|ui| {
                    ui.label(t("appearance-os-dark"));
                    theme_combo(ui, "os-dark", &mut s.os_dark_theme, themes);
                    ui.label(t("appearance-os-light"));
                    theme_combo(ui, "os-light", &mut s.os_light_theme, themes);
                });
            }

            caption(ui, t("appearance-fonts"));
            egui::Grid::new("fonts")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label(t("appearance-term-font"));
                    ui.label(t("appearance-font-size"));
                    ui.end_row();
                    let default_name = fonts.map_or("", |f| f.0.as_str());
                    let system = i18n.t_args("appearance-system-mono", &[("name", default_name)]);
                    let shown = if s.term_font.is_empty() {
                        system.clone()
                    } else {
                        s.term_font.clone()
                    };
                    egui::ComboBox::from_id_salt("term-font")
                        .selected_text(shown)
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut s.term_font, String::new(), system);
                            for name in fonts.map(|f| f.1.as_slice()).unwrap_or_default() {
                                ui.selectable_value(&mut s.term_font, name.clone(), name);
                            }
                        });
                    ui.add(egui::DragValue::new(&mut s.term_font_size).range(6.0..=72.0).speed(0.2));
                    ui.end_row();

                    ui.label(t("appearance-ui-scale"));
                    ui.label("");
                    ui.end_row();
                    let pct =
                        |v: f32| i18n.t_args("appearance-scale-value", &[("percent", &format!("{:.0}", v * 100.0))]);
                    egui::ComboBox::from_id_salt("ui-scale")
                        .selected_text(pct(s.ui_scale))
                        .width(260.0)
                        .show_ui(ui, |ui| {
                            for step in 3..=8 {
                                let v = step as f32 * 0.25;
                                ui.selectable_value(&mut s.ui_scale, v, pct(v));
                            }
                        });
                    ui.checkbox(&mut s.compact, t("appearance-compact"));
                    ui.end_row();
                });

            caption(ui, t("appearance-cursor"));
            ui.horizontal(|ui| {
                for (shape, key) in [
                    (CursorShape::Block, "appearance-cursor-block"),
                    (CursorShape::Beam, "appearance-cursor-beam"),
                    (CursorShape::Underline, "appearance-cursor-underline"),
                ] {
                    ui.selectable_value(&mut s.cursor, shape, t(key));
                }
                ui.add_space(12.0);
                ui.checkbox(&mut s.cursor_blink, t("appearance-blink"));
            });
        });
        if wide {
            ui.add_space(24.0);
            ui.vertical(|ui| {
                ui.set_width(preview_w);
                let term = themes.iter().find(|x| x.name == s.terminal_theme).unwrap_or(current);
                caption(
                    ui,
                    i18n.t_args("appearance-preview", &[("name", &term.name.to_uppercase())]),
                );
                preview(ui, term, s);
            });
        }
    });
}

fn theme_card(ui: &mut egui::Ui, th: &Theme, sub: &str, selected: bool, current: &Theme) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(200.0, 92.0), Sense::click());
    let p = ui.painter();
    let stroke = if selected {
        Stroke::new(2.0, c(current.ui.accent))
    } else {
        Stroke::new(1.0, c(current.ui.border))
    };
    p.rect(rect, 8.0, c(th.ui.surface), stroke, egui::StrokeKind::Inside);
    let swatches = [
        th.ui.accent,
        th.term.ansi[10],
        th.term.ansi[12],
        th.term.ansi[9],
        th.term.ansi[13],
    ];
    for (i, sw) in swatches.iter().enumerate() {
        let r = egui::Rect::from_min_size(rect.min + vec2(14.0 + i as f32 * 26.0, 14.0), vec2(20.0, 20.0));
        p.rect_filled(r, 4.0, c(*sw));
    }
    p.text(
        rect.min + vec2(14.0, 48.0),
        egui::Align2::LEFT_TOP,
        &th.name,
        egui::FontId::proportional(15.0),
        c(th.ui.fg),
    );
    p.text(
        rect.min + vec2(14.0, 68.0),
        egui::Align2::LEFT_TOP,
        sub,
        egui::FontId::proportional(12.0),
        c(th.ui.muted),
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Fake terminal output painted in the terminal palette and font.
fn preview(ui: &mut egui::Ui, th: &Theme, s: &Settings) {
    let a = &th.term.ansi;
    let fg = th.term.fg;
    let lines: &[&[(&str, moliterm_core::theme::Rgb)]] = &[
        &[("admin@web-01", a[2]), (":~$ ", a[4]), ("ls -la", fg)],
        &[("drwxr-xr-x  ", a[8]), (".config", a[12])],
        &[("-rwxr-xr-x  ", a[8]), ("deploy.sh", a[10])],
        &[("lrwxrwxrwx  ", a[8]), ("current", a[14]), (" -> releases/42", fg)],
        &[("admin@web-01", a[2]), (":~$ ", a[4]), ("git status -s", fg)],
        &[(" M ", a[3]), ("nginx/site.conf", fg)],
        &[("?? ", a[1]), ("backup.tar.gz", fg)],
        &[("[warn]", a[11]), (" disk usage above 80% on /var", fg)],
        &[("admin@web-01", a[2]), (":~$ ", a[4])],
    ];
    let font = egui::FontId::monospace(s.term_font_size);
    egui::Frame::new()
        .fill(c(th.term.bg))
        .stroke(Stroke::new(1.0, c(th.ui.border)))
        .corner_radius(8)
        .inner_margin(14)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for line in lines {
                let mut job = LayoutJob::default();
                for (text, color) in *line {
                    job.append(text, 0.0, egui::TextFormat::simple(font.clone(), c(*color)));
                }
                ui.label(job);
            }
            let (r, _) = ui.allocate_exact_size(vec2(s.term_font_size * 0.6, s.term_font_size * 1.2), Sense::hover());
            let cursor = c(th.term.cursor);
            match s.cursor {
                CursorShape::Block => ui.painter().rect_filled(r, 0.0, cursor),
                CursorShape::Beam => ui.painter().rect_filled(r.with_max_x(r.min.x + 2.0), 0.0, cursor),
                CursorShape::Underline => ui.painter().rect_filled(r.with_min_y(r.max.y - 2.0), 0.0, cursor),
            }
        });
}
