//! Window chrome: menu bar (Windows/Linux), toolbar, side rail, side panel, status bar.

use egui::{RichText, Sense, vec2};
use moliterm_core::{settings::ToolbarMode, theme::Theme};

use crate::{
    app::{Action, App, Side},
    icons::{self, Icon},
    settings_ui,
    style::c,
};

impl App {
    pub(crate) fn chrome(&mut self, ui: &mut egui::Ui, theme: &Theme, actions: &mut Vec<Action>) {
        let live = self.settings.clone();
        #[cfg(not(target_os = "macos"))]
        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui, actions));
        if live.toolbar != ToolbarMode::Hidden {
            egui::Panel::top("toolbar")
                .frame(egui::Frame::new().fill(c(theme.ui.panel)).inner_margin(6))
                .show(ui, |ui| self.toolbar(ui, live.toolbar, actions));
        }
        egui::Panel::bottom("status")
            .exact_size(26.0)
            .frame(
                egui::Frame::new()
                    .fill(c(theme.ui.panel))
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.status_bar(ui, theme));
        let rail = if live.side_panel_right {
            egui::Panel::right("rail")
        } else {
            egui::Panel::left("rail")
        };
        rail.exact_size(64.0)
            .resizable(false)
            .frame(egui::Frame::new().fill(c(theme.ui.bg)))
            .show(ui, |ui| self.rail(ui, actions));
        if let Some(side) = self.side {
            // One panel id = one width for every section, so switching doesn't jump.
            let (id, width) = ("side-panel", 360.0);
            let panel = if live.side_panel_right {
                egui::Panel::right(id)
            } else {
                egui::Panel::left(id)
            };
            // At most 45 % of the window, so small windows keep room for the tabs.
            // Never below the default: the first frames can report a tiny window, and
            // egui keeps the clamped width afterwards.
            let max = (ui.ctx().content_rect().width() * 0.45).max(width);
            panel
                .resizable(true)
                .default_size(f32::min(width, max))
                .min_size(180.0)
                .max_size(max)
                .frame(egui::Frame::new().fill(c(theme.ui.panel)).inner_margin(10))
                .show(ui, |ui| {
                    // egui sizes a panel to its contents; fill it so every section keeps the same width.
                    ui.set_min_width(ui.available_width());
                    self.side_panel(ui, side, theme, actions)
                });
        }
    }

    pub(crate) fn settings_screen(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let Some(mut page) = self.settings_page.take() else {
            return;
        };
        // Cheap copies so the closure below may borrow `self` mutably.
        let i18n = self.i18n.clone();
        let themes = self.themes.clone();
        let fonts = self
            .fonts
            .as_ref()
            .map(|f| (f.default_mono.clone(), f.monospace.clone()));
        let outcome = settings_ui::show(
            ui,
            &mut page,
            &i18n,
            &themes,
            fonts.as_ref(),
            theme,
            &mut |ui, sec, draft| {
                self.security_section(ui, sec, draft, theme);
            },
        );
        match outcome {
            settings_ui::Outcome::Keep => self.settings_page = Some(page),
            settings_ui::Outcome::Apply => {
                let draft = page.draft.clone();
                self.save_settings(draft);
            }
            settings_ui::Outcome::Close => {}
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn menu_bar(&self, ui: &mut egui::Ui, out: &mut Vec<Action>) {
        let t = |id: &str| self.i18n.t(id);
        let item = |ui: &mut egui::Ui, out: &mut Vec<Action>, key: &str, action: Action| {
            if ui.button(t(key)).clicked() {
                out.push(action);
            }
        };
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button(t("menu-terminal"), |ui| {
                item(ui, out, "menu-new-local-shell", Action::NewLocalShell(None));
                item(ui, out, "menu-find", Action::Find);
                item(ui, out, "menu-close-tab", Action::CloseTab);
                ui.separator();
                item(ui, out, "menu-quit", Action::Quit);
            });
            ui.menu_button(t("menu-sessions"), |ui| {
                item(ui, out, "menu-new-session", Action::NewSession);
                item(ui, out, "menu-lock-vault", Action::LockVault);
            });
            ui.menu_button(t("menu-view"), |ui| {
                item(ui, out, "menu-toggle-side-panel", Action::ToggleSidePanel);
                item(ui, out, "hk-split-right", Action::SplitRight);
                item(ui, out, "hk-split-down", Action::SplitDown);
                item(ui, out, "menu-font-bigger", Action::FontBigger);
                item(ui, out, "menu-font-smaller", Action::FontSmaller);
                item(ui, out, "menu-font-reset", Action::FontReset);
                item(ui, out, "menu-fullscreen", Action::Fullscreen);
            });
            ui.menu_button(t("menu-tools"), |ui| {
                item(ui, out, "menu-multi-exec", Action::MultiExec);
                item(ui, out, "menu-tunnels", Action::ShowSide(Side::Tunnels));
                item(ui, out, "menu-macros", Action::ShowSide(Side::Macros));
                item(ui, out, "rail-vault", Action::OpenVault);
            });
            ui.menu_button(t("menu-settings"), |ui| {
                item(ui, out, "menu-open-settings", Action::OpenSettings);
            });
            ui.menu_button(t("menu-help"), |ui| {
                ui.label(format!("{} {}", t("app-name"), env!("CARGO_PKG_VERSION")));
            });
        });
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, mode: ToolbarMode, out: &mut Vec<Action>) {
        let items = [
            (Icon::Plus, "toolbar-session", Some(Action::NewSession)),
            (Icon::Prompt, "toolbar-local-shell", Some(Action::NewLocalShell(None))),
            (Icon::Split, "toolbar-split", Some(Action::SplitRight)),
            (Icon::Window, "toolbar-to-window", Some(Action::ToWindow)),
            (Icon::Broadcast, "toolbar-multi-exec", Some(Action::MultiExec)),
            (Icon::Gear, "toolbar-settings", Some(Action::OpenSettings)),
        ];
        // Labels only when they fit next to quick connect (small windows / big zoom → icons, SPEC-9 §9.2).
        let with_labels = mode == ToolbarMode::Full && ui.available_width() >= items.len() as f32 * 78.0 + 300.0;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (icon, key, action) in items {
                let label = self.i18n.t(key);
                if tool_button(ui, icon, &label, with_labels, action.is_some()).clicked()
                    && let Some(a) = action
                {
                    out.push(a);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.quick_connect_ui(ui);
            });
        });
    }

    fn rail(&self, ui: &mut egui::Ui, out: &mut Vec<Action>) {
        ui.add_space(6.0);
        for (side, icon, key) in [
            (Side::Sessions, Icon::Server, "rail-sessions"),
            (Side::Sftp, Icon::Folder, "rail-sftp"),
            (Side::Tunnels, Icon::Link, "rail-tunnels"),
            (Side::Macros, Icon::Play, "rail-macros"),
        ] {
            let selected = self.side == Some(side);
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::click());
            let v = ui.visuals();
            if selected || resp.hovered() {
                ui.painter()
                    .rect_filled(rect.shrink2(vec2(5.0, 2.0)), 6.0, v.widgets.inactive.bg_fill);
            }
            let fg = if selected {
                v.strong_text_color()
            } else {
                v.weak_text_color()
            };
            let icon_rect = egui::Rect::from_center_size(rect.center() - vec2(0.0, 9.0), vec2(18.0, 18.0));
            icons::paint(ui.painter(), icon_rect, icon, fg);
            ui.painter().text(
                rect.center() + vec2(0.0, 13.0),
                egui::Align2::CENTER_CENTER,
                self.i18n.t(key),
                egui::FontId::proportional(11.0),
                fg,
            );
            if resp.clicked() {
                out.push(Action::ShowSide(side));
            }
        }
        // The vault is a page of its own, not a side panel.
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::click());
        let v = ui.visuals();
        if resp.hovered() {
            ui.painter()
                .rect_filled(rect.shrink2(vec2(5.0, 2.0)), 6.0, v.widgets.inactive.bg_fill);
        }
        let fg = v.weak_text_color();
        let icon_rect = egui::Rect::from_center_size(rect.center() - vec2(0.0, 9.0), vec2(18.0, 18.0));
        icons::paint(ui.painter(), icon_rect, Icon::Lock, fg);
        ui.painter().text(
            rect.center() + vec2(0.0, 13.0),
            egui::Align2::CENTER_CENTER,
            self.i18n.t("rail-vault"),
            egui::FontId::proportional(11.0),
            fg,
        );
        if resp.clicked() {
            out.push(Action::OpenVault);
        }
    }

    fn side_panel(&mut self, ui: &mut egui::Ui, side: Side, theme: &Theme, out: &mut Vec<Action>) {
        match side {
            Side::Sessions => self.sessions_panel(ui, theme, out),
            Side::Sftp => self.sftp_panel(ui, theme),
            Side::Tunnels => self.tunnels_panel(ui, theme),
            Side::Macros => self.macros_panel(ui, theme),
        }
    }

    fn status_bar(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        if let Some(d) = self.active_desk() {
            let (dot, info, size) = (d.state_color(theme), d.info.clone(), d.remote_size());
            ui.horizontal_centered(|ui| {
                let small = |s: String| RichText::new(s).small();
                let muted = ui.visuals().weak_text_color();
                ui.label(small("●".into()).color(dot));
                if let Some((target, detail)) = info {
                    ui.label(small(target));
                    ui.add_space(12.0);
                    ui.label(small(detail).color(muted));
                }
                if size.0 > 0 {
                    ui.add_space(12.0);
                    ui.label(small(format!("{}×{}", size.0, size.1)).color(muted));
                }
            });
            return;
        }
        let active = self.active_term().map(|t| {
            (
                t.state_color(theme),
                t.name.clone(),
                t.size,
                t.cwd.clone(),
                t.info.clone(),
                t.proto,
                t.handle.encoding.map_or("UTF-8", |e| e.name()),
                t.monitor.clone(),
            )
        });
        let i18n = &self.i18n;
        ui.horizontal_centered(|ui| {
            let muted = ui.visuals().weak_text_color();
            let small = |s: String| RichText::new(s).small();
            match active {
                Some((dot, name, size, cwd, info, proto, encoding, monitor)) => {
                    ui.label(small("●".into()).color(dot));
                    match info {
                        Some((target, detail)) => {
                            ui.label(small(target));
                            ui.add_space(12.0);
                            ui.label(small(detail).color(muted));
                        }
                        None => {
                            ui.label(small(name));
                            ui.add_space(12.0);
                            let kind = if proto == "LOC" {
                                i18n.t("status-local")
                            } else {
                                proto.to_owned()
                            };
                            ui.label(small(kind).color(muted));
                        }
                    }
                    ui.add_space(12.0);
                    ui.label(small(encoding.into()).color(muted));
                    ui.add_space(12.0);
                    ui.label(small(format!("{}×{}", size.cols, size.rows)).color(muted));
                    if let Some(cwd) = cwd {
                        ui.add_space(12.0);
                        ui.label(small(cwd).color(muted));
                    }
                    if let Some(m) = monitor {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_space(8.0);
                            monitor_line(ui, &m, i18n);
                        });
                    }
                }
                None => {
                    ui.label(small("●".into()).color(muted));
                    ui.label(small(i18n.t("status-idle")).color(muted));
                }
            }
        });
    }
}

impl App {
    /// Toolbar quick connect (SPEC-9 §9.6): Enter opens an unsaved tab.
    fn quick_connect_ui(&mut self, ui: &mut egui::Ui) {
        let recent: Vec<String> = self
            .db
            .meta_get("quick_recent")
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();
        let mut go = None;
        if !recent.is_empty() {
            ui.menu_button("▾", |ui| {
                for r in &recent {
                    if ui.button(r).clicked() {
                        go = Some(r.clone());
                        ui.close();
                    }
                }
            })
            .response
            .on_hover_text(self.i18n.t("quick-recent"));
        }
        let resp = ui.add(
            crate::style::field(&mut self.quick)
                .hint_text(self.i18n.t("quick-hint"))
                .desired_width(ui.available_width().min(240.0)),
        );
        resp.clone().on_hover_text(self.i18n.t("quick-tip"));
        if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !self.quick.trim().is_empty() {
            go = Some(self.quick.trim().to_owned());
        }
        let Some(text) = go else {
            return;
        };
        self.quick.clear();
        let ctx = ui.ctx().clone();
        self.quick_connect(&ctx, text, recent);
    }

    pub(crate) fn quick_connect(&mut self, ctx: &egui::Context, text: String, recent: Vec<String>) {
        let Some(s) = moliterm_core::sessions::parse_quick(&text) else {
            self.dialog = Some(crate::dialogs::Dialog::Info(self.i18n.t("quick-bad")));
            return;
        };
        let mut recent = recent;
        recent.retain(|r| *r != text);
        recent.insert(0, text);
        recent.truncate(10);
        if let Ok(j) = serde_json::to_string(&recent) {
            let _ = self.db.meta_set("quick_recent", &j);
        }
        if let Some(mut tab) = self.make_session_tab(ctx, s.clone()) {
            match &mut tab {
                crate::dock::Tab::Term(t) => t.session_id = None,
                crate::dock::Tab::Desk(d) => d.session_id = None,
                crate::dock::Tab::Welcome | crate::dock::Tab::Vault => {}
            }
            if let Some(id) = tab.id() {
                self.quick_sessions.insert(id, s);
            }
            self.place(tab, crate::layout::Place::Focused);
        }
    }
}

/// `CPU 12% · RAM 3.1/16 GB · / 45% · ↓1.2 MB/s ↑30 KB/s · load 0.52 · up 3d 4h`
fn monitor_line(ui: &mut egui::Ui, m: &moliterm_core::monitor::Shared, i18n: &crate::i18n::I18n) {
    use moliterm_core::monitor::bytes;
    let muted = ui.visuals().weak_text_color();
    let small = |s: String| RichText::new(s).small().color(muted);
    if m.unavailable.load(std::sync::atomic::Ordering::Relaxed) {
        ui.label(small("—".into())).on_hover_text(i18n.t("monitor-unavailable"));
        return;
    }
    let Some(x) = m.metrics.lock().ok().and_then(|g| g.clone()) else {
        return;
    };
    let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    let (d, h, min) = (x.uptime / 86400, x.uptime / 3600 % 24, x.uptime / 60 % 60);
    let up = match (d, h) {
        (0, 0) => format!("{min}m"),
        (0, _) => format!("{h}h {min}m"),
        _ => format!("{d}d {h}h"),
    };
    let mut parts = vec![
        format!("CPU {:.0}%", x.cpu),
        format!("RAM {:.1}/{:.0} GB", gb(x.mem_used), gb(x.mem_total)),
        format!("/ {:.0}%", x.disk_pct),
        format!("↓{}/s ↑{}/s", bytes(x.rx_rate), bytes(x.tx_rate)),
    ];
    if let Some(l) = x.load {
        parts.push(format!("load {l:.2}"));
    }
    parts.push(i18n.t_args("monitor-up", &[("t", &up)]));
    ui.label(small(parts.join(" · ")));
    ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
}

/// Big icon + label button (MobaXterm style); icon only with a tooltip in compact mode.
fn tool_button(ui: &mut egui::Ui, icon: Icon, label: &str, with_label: bool, enabled: bool) -> egui::Response {
    let size = if with_label { vec2(76.0, 50.0) } else { vec2(34.0, 30.0) };
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    let v = ui.visuals();
    let fg = if enabled { v.text_color() } else { v.weak_text_color() };
    if enabled && resp.hovered() {
        ui.painter().rect_filled(rect, 6.0, v.widgets.hovered.bg_fill);
    }
    let icon_color = if enabled && icon == Icon::Plus {
        v.hyperlink_color
    } else {
        fg
    };
    if with_label {
        let icon_rect = egui::Rect::from_center_size(rect.center() - vec2(0.0, 9.0), vec2(20.0, 20.0));
        icons::paint(ui.painter(), icon_rect, icon, icon_color);
        ui.painter().text(
            rect.center() + vec2(0.0, 14.0),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::proportional(12.0),
            fg,
        );
        resp
    } else {
        icons::paint(ui.painter(), rect.shrink(7.0), icon, icon_color);
        resp.on_hover_text(label)
    }
}
