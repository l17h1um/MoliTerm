//! Tab dock (egui_dock): tab titles, bodies and close handling.

use egui::{RichText, text::LayoutJob};
use egui_dock::{DockArea, TabViewer, tab_viewer::OnCloseResponse};
use moliterm_core::theme::Theme;

use crate::{
    app::App,
    desk_view::{DeskTab, Dx},
    i18n::I18n,
    style::c,
    term_view::{Cx, Req, TermTab},
};

pub enum Tab {
    Welcome,
    Term(Box<TermTab>),
    Desk(Box<DeskTab>),
    /// The Vault page as a tab (SPEC-9 §9.1); its state lives in `App::vault_page`.
    Vault,
}

/// Tab id of the (single) Vault tab.
pub const VAULT_TAB: u64 = u64::MAX - 1;

impl Tab {
    pub fn id(&self) -> Option<u64> {
        match self {
            Tab::Welcome => None,
            Tab::Term(t) => Some(t.id),
            Tab::Desk(d) => Some(d.id),
            Tab::Vault => Some(VAULT_TAB),
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Tab::Welcome | Tab::Vault => None,
            Tab::Term(t) => Some(&t.name),
            Tab::Desk(d) => Some(&d.name),
        }
    }

    /// Ends the tab's session (the tab itself is removed by the caller).
    pub fn close(&self) {
        match self {
            Tab::Welcome | Tab::Vault => {}
            Tab::Term(t) => {
                let _ = t.handle.cmd.send(moliterm_core::terminal::Cmd::Close);
            }
            Tab::Desk(d) => d.handle.send(moliterm_core::rdp::Input::Close),
        }
    }
}

struct Viewer<'a> {
    i18n: &'a I18n,
    /// Tabs whose body was drawn last frame = the active tab of each leaf
    /// (the tab bar is drawn before the bodies, so this frame isn't known yet).
    shown_before: &'a std::collections::HashSet<u64>,
    shown_now: &'a mut std::collections::HashSet<u64>,
    selected: &'a mut std::collections::HashSet<u64>,
    cx: Cx<'a>,
    dx: Dx<'a>,
    theme: &'a Theme,
    reqs: Vec<(u64, Req)>,
    /// Where the Vault tab's body is, when it is the active tab of its leaf.
    vault_rect: Option<egui::Rect>,
}

impl TabViewer for Viewer<'_> {
    type Tab = Tab;

    fn id(&mut self, tab: &mut Tab) -> egui::Id {
        match tab {
            Tab::Welcome => egui::Id::new("welcome"),
            Tab::Vault => egui::Id::new("vault-tab"),
            Tab::Term(t) => egui::Id::new(("term-tab", t.id)),
            Tab::Desk(d) => egui::Id::new(("desk-tab", d.id)),
        }
    }

    fn title(&mut self, tab: &mut Tab) -> egui::WidgetText {
        match tab {
            Tab::Welcome => self.i18n.t("tab-welcome").into(),
            Tab::Vault => RichText::new(self.i18n.t("vault-page"))
                .color(c(self.theme.ui.accent))
                .into(),
            Tab::Term(t) => {
                let u = &self.theme.ui;
                let dot = t.state_color(self.theme);
                let active = self.shown_before.contains(&t.id);
                let name = if t.is_dead() || !active { c(u.muted) } else { c(u.fg) };
                let mut job = LayoutJob::default();
                let fmt = |size: f32, color| egui::TextFormat::simple(egui::FontId::proportional(size), color);
                if self.selected.contains(&t.id) {
                    job.append("✓ ", 0.0, fmt(12.0, c(u.accent)));
                }
                job.append("● ", 0.0, fmt(10.0, dot));
                job.append(t.proto, 0.0, fmt(10.0, c(u.muted)));
                job.append(&t.name, 6.0, fmt(13.0, name));
                job.into()
            }
            Tab::Desk(d) => {
                let u = &self.theme.ui;
                let mut job = LayoutJob::default();
                let fmt = |size: f32, color| egui::TextFormat::simple(egui::FontId::proportional(size), color);
                job.append("● ", 0.0, fmt(10.0, d.state_color(self.theme)));
                job.append("RDP", 0.0, fmt(10.0, c(u.muted)));
                let active = self.shown_before.contains(&d.id);
                let name = if d.is_dead() || !active { c(u.muted) } else { c(u.fg) };
                job.append(&d.name, 6.0, fmt(13.0, name));
                job.into()
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Tab) {
        match tab {
            Tab::Vault => {
                // Drawn by the app right after the dock, over this area.
                let rect = ui.available_rect_before_wrap();
                ui.allocate_rect(rect, egui::Sense::hover());
                self.vault_rect = Some(rect);
                self.shown_now.insert(VAULT_TAB);
            }
            Tab::Welcome => {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.35);
                    ui.heading(self.i18n.t("welcome-title"));
                    ui.add_space(6.0);
                    ui.label(RichText::new(self.i18n.t("welcome-body")).color(ui.visuals().weak_text_color()));
                });
            }
            Tab::Term(t) => {
                self.shown_now.insert(t.id);
                let reqs = t.ui(ui, &self.cx);
                self.reqs.extend(reqs.into_iter().map(|r| (t.id, r)));
            }
            Tab::Desk(d) => {
                self.shown_now.insert(d.id);
                let reqs = d.ui(ui, &self.dx);
                self.reqs.extend(reqs.into_iter().map(|r| (d.id, r)));
            }
        }
    }

    fn on_tab_button(&mut self, tab: &mut Tab, response: &egui::Response) {
        // Accent bar on top of each leaf's active tab: "you are here".
        if let Some(id) = tab.id()
            && self.shown_before.contains(&id)
        {
            let r = response.rect;
            response.ctx.layer_painter(response.layer_id).rect_filled(
                egui::Rect::from_min_max(r.left_top(), egui::pos2(r.right(), r.top() + 2.5)),
                0.0,
                c(self.theme.ui.accent),
            );
        }
        if let Tab::Desk(d) = tab
            && response.middle_clicked()
        {
            self.reqs.push((d.id, Req::Close));
        }
        if let Tab::Term(t) = tab {
            // Cmd/Ctrl-click selects tabs as multi-exec targets (SPEC §8.1).
            if response.clicked() && response.ctx.input(|i| i.modifiers.command) && !self.selected.remove(&t.id) {
                self.selected.insert(t.id);
            }
            if response.middle_clicked() {
                self.reqs.push((t.id, Req::Close));
            }
            let tip = [t.title.as_deref(), t.cwd.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join("\n");
            if !tip.is_empty() {
                response.clone().on_hover_text(tip);
            }
        }
    }

    fn is_closeable(&self, tab: &Tab) -> bool {
        !matches!(tab, Tab::Welcome)
    }

    fn on_close(&mut self, tab: &mut Tab) -> OnCloseResponse {
        if let Some(id) = tab.id() {
            // Let the app remove it (sends Close to the session, restores Welcome).
            self.reqs.push((id, Req::Close));
        }
        OnCloseResponse::Ignore
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, tab: &mut Tab, _path: egui_dock::NodePath) {
        // Quick-connect tabs (no saved session) can be saved.
        let quick = match tab {
            Tab::Term(t) => t.session_id.is_none() && !matches!(t.launch, crate::term_view::Launch::Local(_)),
            Tab::Desk(d) => d.session_id.is_none(),
            Tab::Welcome | Tab::Vault => false,
        };
        if quick
            && let Some(id) = tab.id()
            && ui.button(self.i18n.t("tab-save-session")).clicked()
        {
            self.reqs.push((id, Req::SaveAsSession));
        }
        if let Tab::Desk(d) = tab {
            if d.is_dead() && ui.button(self.i18n.t("term-reconnect")).clicked() {
                self.reqs.push((d.id, Req::Reconnect));
            }
            if ui.button(self.i18n.t("tab-move-window")).clicked() {
                self.reqs.push((d.id, Req::MoveToWindow));
            }
            if ui.button(self.i18n.t("menu-close-tab")).clicked() {
                self.reqs.push((d.id, Req::Close));
            }
        }
        if let Tab::Term(t) = tab {
            if t.is_dead() && ui.button(self.i18n.t("term-reconnect")).clicked() {
                self.reqs.push((t.id, Req::Reconnect));
            }
            if ui.button(self.i18n.t("tab-move-window")).clicked() {
                self.reqs.push((t.id, Req::MoveToWindow));
            }
            if ui.button(self.i18n.t("term-save-scrollback")).clicked() {
                self.reqs.push((t.id, Req::SaveScrollback(t.scrollback_text())));
            }
            if ui.button(self.i18n.t("menu-close-tab")).clicked() {
                self.reqs.push((t.id, Req::Close));
            }
        }
    }
}

impl App {
    pub(crate) fn dock_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Vec<(u64, Req)> {
        let mut style = egui_dock::Style::from_egui(ui.style());
        let u = &theme.ui;
        style.tab_bar.bg_fill = c(u.panel);
        style.tab_bar.hline_color = c(u.border);
        style.tab.active.bg_fill = c(u.bg);
        style.tab.focused.bg_fill = c(u.bg);
        style.tab.active.text_color = c(u.fg);
        style.tab.focused.text_color = c(u.fg);
        style.tab.inactive.text_color = c(u.muted);
        // The active tab has no bottom line, so it joins its content.
        style.tab.hline_below_active_tab_name = false;
        style.tab.inactive.bg_fill = c(u.panel);
        style.tab.hovered.bg_fill = c(u.bg).gamma_multiply(0.8);
        style.tab.tab_body.bg_fill = c(self.term_theme().term.bg);
        let term_theme = self.term_theme().clone();
        let settings = self.settings.clone();
        if self.multi.is_some() {
            let settings = self.settings.clone();
            let mut reqs = vec![];
            egui::CentralPanel::no_frame().show(ui, |ui| {
                reqs = self.multi_exec_ui(ui, theme, &settings);
            });
            return reqs;
        }
        let mut viewer = Viewer {
            i18n: &self.i18n,
            shown_before: &self.tabs_shown.0,
            shown_now: &mut self.tabs_shown.1,
            selected: &mut self.selected_tabs,
            cx: Cx {
                settings: &settings,
                palette: &term_theme.term,
                ui_theme: theme,
                rules: &self.rules.1,
                i18n: &self.i18n,
                styled_fonts: self.fonts.as_ref().is_some_and(|f| f.styled),
            },
            dx: Dx {
                i18n: &self.i18n,
                theme,
                mac_keys: settings.rdp_mac_keys,
                color_border: settings.color_border,
                toolbar: true,
            },
            theme,
            reqs: vec![],
            vault_rect: None,
        };
        let mut vault_rect = None;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            DockArea::new(&mut self.dock)
                .style(style)
                .show_leaf_collapse_buttons(false)
                .show_leaf_close_all_buttons(false)
                .show_inside(ui, &mut viewer);
            vault_rect = viewer.vault_rect;
        });
        let reqs = viewer.reqs;
        // Drawn after the dock, over the Vault tab's body (it needs the whole app).
        if let Some(rect) = vault_rect {
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.set_clip_rect(rect);
                self.vault_screen(ui, theme);
            });
        }
        self.detach_floating();
        reqs
    }

    /// Tab full screen (SPEC-9 §9.4): only the active tab's content, plus a
    /// bar that appears at the top edge.
    pub(crate) fn focus_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) -> Vec<(u64, Req)> {
        let term_theme = self.term_theme().clone();
        let settings = self.settings.clone();
        let i18n = self.i18n.clone();
        let styled = self.fonts.as_ref().is_some_and(|f| f.styled);
        let path = self
            .dock
            .focused_leaf()
            .or_else(|| self.dock.iter_leaves().next().map(|(p, _)| p));
        let mut reqs = vec![];
        let mut name = String::new();
        let mut desk_id = None;
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::new().fill(c(term_theme.term.bg)))
            .show(ui, |ui| {
                let Some(leaf) = path.and_then(|p| self.dock.leaf_mut(p).ok()) else {
                    return;
                };
                let active = leaf.active.0;
                match leaf.tabs.get_mut(active) {
                    Some(Tab::Term(t)) => {
                        name = t.name.clone();
                        self.tabs_shown.1.insert(t.id);
                        let cx = Cx {
                            settings: &settings,
                            palette: &term_theme.term,
                            ui_theme: theme,
                            rules: &self.rules.1,
                            i18n: &i18n,
                            styled_fonts: styled,
                        };
                        reqs.extend(t.ui(ui, &cx).into_iter().map(|r| (t.id, r)));
                    }
                    Some(Tab::Desk(d)) => {
                        name = d.name.clone();
                        desk_id = Some(d.id);
                        self.tabs_shown.1.insert(d.id);
                        let dx = Dx {
                            i18n: &i18n,
                            theme,
                            mac_keys: settings.rdp_mac_keys,
                            color_border: false,
                            toolbar: false,
                        };
                        reqs.extend(d.ui(ui, &dx).into_iter().map(|r| (d.id, r)));
                    }
                    _ => {}
                }
            });
        // Floating bar when the pointer is near the top edge.
        let near_top = ui.input(|i| i.pointer.hover_pos().is_some_and(|p| p.y < 8.0));
        let bar_id = egui::Id::new("focus-bar");
        let was_open = ui.ctx().data(|d| d.get_temp::<bool>(bar_id).unwrap_or(false));
        if near_top || was_open {
            let mut hovered = false;
            let t = |k: &str| i18n.t(k);
            egui::Area::new(bar_id)
                .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 4.0))
                .order(egui::Order::Foreground)
                .show(ui.ctx(), |ui| {
                    let r = egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&name).strong());
                            ui.separator();
                            if let Some(id) = desk_id
                                && ui.button(t("rdp-ctrl-alt-del")).clicked()
                                && let Some(Tab::Desk(d)) =
                                    self.dock.iter_all_tabs().map(|(_, t)| t).find(|t| t.id() == Some(id))
                            {
                                d.handle.send(moliterm_core::rdp::Input::CtrlAltDel);
                            }
                            if ui.button(t("focus-exit")).clicked() {
                                reqs.push((0, Req::Fullscreen));
                            }
                        });
                    });
                    hovered = r.response.contains_pointer();
                });
            ui.ctx().data_mut(|d| d.insert_temp(bar_id, near_top || hovered));
        }
        reqs
    }
}
