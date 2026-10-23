//! Tunnels side panel, tunnel editor and the running tunnel tasks (SPEC §5.6).

use std::{collections::HashMap, sync::Arc, sync::atomic::Ordering};

use egui::{RichText, Stroke};
use moliterm_core::{
    monitor::bytes,
    sessions::{self, Protocol},
    theme::Theme,
    tunnel::{self, Kind, State, Stats, Tunnel},
};
use tokio::sync::watch;

use crate::{
    app::App,
    dialogs::primary,
    icons::{self, Icon},
    style::c,
};

pub struct Run {
    pub stats: Arc<Stats>,
    stop: watch::Sender<bool>,
}

#[derive(Default)]
pub struct Tunnels {
    pub list: Vec<Tunnel>,
    pub runs: HashMap<String, Run>,
    pub editor: Option<TunnelEditor>,
}

pub struct TunnelEditor {
    t: Tunnel,
    is_new: bool,
    /// The user confirmed listening on a non-loopback address.
    exposed_ok: bool,
}

impl TunnelEditor {
    pub fn new(session_id: String) -> Self {
        Self {
            t: Tunnel {
                id: sessions::new_id(),
                session_id,
                ..Default::default()
            },
            is_new: true,
            exposed_ok: false,
        }
    }
}

impl Tunnels {
    pub fn running(&self, id: &str) -> bool {
        self.runs
            .get(id)
            .is_some_and(|r| matches!(r.stats.state(), State::Starting | State::Up))
    }
}

impl App {
    pub(crate) fn load_tunnels(&mut self) {
        self.tunnels.list = tunnel::load(&self.db).unwrap_or_else(|e| {
            tracing::error!("loading tunnels: {e}");
            vec![]
        });
    }

    /// Autostart tunnels (SPEC §5.6), called once at launch.
    pub(crate) fn autostart_tunnels(&mut self) {
        let ids: Vec<String> = self
            .tunnels
            .list
            .iter()
            .filter(|t| t.autostart)
            .map(|t| t.id.clone())
            .collect();
        for id in ids {
            self.start_tunnel(&id);
        }
    }

    /// Tunnels marked "start with this session" when the session opens.
    pub(crate) fn tunnels_for_session(&mut self, session_id: &str) {
        let ids: Vec<String> = self
            .tunnels
            .list
            .iter()
            .filter(|t| t.with_session && t.session_id == session_id)
            .map(|t| t.id.clone())
            .collect();
        for id in ids {
            if !self.tunnels.running(&id) {
                self.start_tunnel(&id);
            }
        }
    }

    pub(crate) fn start_tunnel(&mut self, id: &str) {
        let Some(t) = self.tunnels.list.iter().find(|t| t.id == id).cloned() else {
            return;
        };
        let stats = Arc::new(Stats::default());
        let (stop, rx) = watch::channel(false);
        match self
            .sessions
            .iter()
            .find(|s| s.id == t.session_id && s.protocol == Protocol::Ssh)
            .cloned()
        {
            Some(s) => {
                let opts = self.ssh_options(&s);
                self.rt
                    .spawn(tunnel::run(t, opts, self.ssh_ctx.clone(), stats.clone(), rx));
            }
            None => {
                if let Ok(mut st) = stats.state.lock() {
                    *st = State::Error(self.i18n.t("tunnel-err-session"));
                }
            }
        }
        self.tunnels.runs.insert(id.to_owned(), Run { stats, stop });
    }

    pub(crate) fn stop_tunnel(&mut self, id: &str) {
        if let Some(r) = self.tunnels.runs.get(id) {
            let _ = r.stop.send(true);
        }
    }

    pub(crate) fn stop_all_tunnels(&self) {
        for r in self.tunnels.runs.values() {
            let _ = r.stop.send(true);
        }
    }

    pub(crate) fn save_tunnel(&mut self, t: &Tunnel) {
        if let Err(e) = tunnel::save(&self.db, t) {
            tracing::error!("save tunnel: {e}");
        }
        self.load_tunnels();
    }

    fn delete_tunnel(&mut self, id: &str) {
        self.stop_tunnel(id);
        self.tunnels.runs.remove(id);
        if let Err(e) = tunnel::delete(&self.db, id) {
            tracing::error!("delete tunnel: {e}");
        }
        self.load_tunnels();
    }

    pub(crate) fn tunnels_panel(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let muted = ui.visuals().weak_text_color();
        let t = |k: &str| self.i18n.t(k);
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(t("panel-tunnels")).small().strong().color(muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icons::button(ui, Icon::Plus, 14.0, &t("tunnels-new")).clicked() {
                    let first_ssh = self
                        .sessions
                        .iter()
                        .find(|s| s.protocol == Protocol::Ssh)
                        .map(|s| s.id.clone())
                        .unwrap_or_default();
                    self.tunnels.editor = Some(TunnelEditor::new(first_ssh));
                }
            });
        });
        ui.add_space(6.0);
        if self.tunnels.list.is_empty() {
            ui.label(RichText::new(t("tunnels-empty")).color(muted));
            return;
        }
        let mut toggle = None;
        let mut edit = None;
        let mut delete = None;
        let mut any_running = false;
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            for tun in &self.tunnels.list {
                let stats = self.tunnels.runs.get(&tun.id).map(|r| r.stats.clone());
                let state = stats.as_ref().map(|s| s.state()).unwrap_or_default();
                let running = matches!(state, State::Starting | State::Up);
                any_running |= running;
                let dot = match state {
                    State::Up => c(theme.ui.success),
                    State::Starting => c(theme.ui.warning),
                    State::Error(_) => c(theme.ui.danger),
                    State::Stopped => muted,
                };
                // The card is clickable (double-click, context menu) but registered
                // before its children, so buttons inside still get their clicks.
                let resp = ui
                    .scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                        egui::Frame::new()
                            .stroke(Stroke::new(1.0, c(theme.ui.border)))
                            .corner_radius(6)
                            .inner_margin(egui::Margin::symmetric(10, 8))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("●").color(dot));
                                    let badge = match tun.kind {
                                        Kind::Local => "L",
                                        Kind::Remote => "R",
                                        Kind::Dynamic => "D",
                                    };
                                    ui.label(RichText::new(badge).small().strong().color(c(theme.ui.accent)));
                                    ui.label(RichText::new(&tun.name).strong());
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        let label = if running { t("tunnel-stop") } else { t("tunnel-start") };
                                        if ui.small_button(label).clicked() {
                                            toggle = Some((tun.id.clone(), running));
                                        }
                                    });
                                });
                                ui.label(RichText::new(tun.route()).monospace().small());
                                let via = self
                                    .sessions
                                    .iter()
                                    .find(|s| s.id == tun.session_id)
                                    .map_or("?", |s| s.name.as_str());
                                let mut line = self.i18n.t_args("tunnel-via", &[("session", via)]);
                                if let Some(s) = &stats {
                                    let (rx, tx) = (s.rx.load(Ordering::Relaxed), s.tx.load(Ordering::Relaxed));
                                    line.push_str(&format!(" · ↓ {} ↑ {}", bytes(rx as f64), bytes(tx as f64)));
                                    let n = s.conns.load(Ordering::Relaxed);
                                    if n > 0 {
                                        line.push_str(" · ");
                                        line.push_str(&self.i18n.t_args("tunnel-conns", &[("n", &n.to_string())]));
                                    }
                                }
                                ui.label(RichText::new(line).small().color(muted));
                                if let State::Error(e) = &state {
                                    ui.label(RichText::new(e).small().color(c(theme.ui.danger)));
                                }
                            })
                    })
                    .response;
                if resp.double_clicked() {
                    edit = Some(tun.clone());
                }
                resp.context_menu(|ui| {
                    if ui.button(t("tree-edit")).clicked() {
                        edit = Some(tun.clone());
                        ui.close();
                    }
                    if ui.button(t("tree-delete")).clicked() {
                        delete = Some(tun.id.clone());
                        ui.close();
                    }
                });
                ui.add_space(6.0);
            }
        });
        if any_running {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
        if let Some((id, running)) = toggle {
            if running {
                self.stop_tunnel(&id);
            } else {
                self.start_tunnel(&id);
            }
        }
        if let Some(tun) = edit {
            self.tunnels.editor = Some(TunnelEditor {
                exposed_ok: tun.exposed(),
                t: tun,
                is_new: false,
            });
        }
        if let Some(id) = delete {
            self.delete_tunnel(&id);
        }
    }

    pub(crate) fn tunnel_editor_ui(&mut self, ctx: &egui::Context, theme: &Theme) {
        let Some(mut ed) = self.tunnels.editor.take() else {
            return;
        };
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(24);
        let ssh: Vec<(String, String)> = self
            .sessions
            .iter()
            .filter(|s| s.protocol == Protocol::Ssh)
            .map(|s| (s.id.clone(), s.name.clone()))
            .collect();
        let mut close = false;
        let mut save = false;
        let resp = egui::Modal::new("tunnel-editor".into()).frame(frame).show(ctx, |ui| {
            crate::style::fit_width(ui, 520.0);
            let tun = &mut ed.t;
            ui.heading(if ed.is_new {
                t("tunnel-editor-new")
            } else {
                t("tunnel-editor-edit")
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                for (k, key) in [
                    (Kind::Local, "tunnel-kind-local"),
                    (Kind::Remote, "tunnel-kind-remote"),
                    (Kind::Dynamic, "tunnel-kind-dynamic"),
                ] {
                    ui.selectable_value(&mut tun.kind, k, t(key));
                }
            });
            let hint = match tun.kind {
                Kind::Local => "tunnel-kind-local-hint",
                Kind::Remote => "tunnel-kind-remote-hint",
                Kind::Dynamic => "tunnel-kind-dynamic-hint",
            };
            ui.label(RichText::new(t(hint)).small().weak());
            ui.add_space(8.0);
            egui::Grid::new("tunnel-grid")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label(t("tunnel-name"));
                    let route = tun.route();
                    ui.add(crate::style::field(&mut tun.name).hint_text(route).desired_width(300.0));
                    ui.end_row();
                    ui.label(t("tunnel-session"));
                    let current = ssh
                        .iter()
                        .find(|(id, _)| *id == tun.session_id)
                        .map_or_else(|| t("tunnel-session-none"), |(_, n)| n.clone());
                    egui::ComboBox::from_id_salt("tunnel-session")
                        .selected_text(current)
                        .width(300.0)
                        .show_ui(ui, |ui| {
                            for (id, name) in &ssh {
                                ui.selectable_value(&mut tun.session_id, id.clone(), name);
                            }
                        });
                    ui.end_row();
                    ui.label(if tun.kind == Kind::Remote {
                        t("tunnel-bind-remote")
                    } else {
                        t("tunnel-bind")
                    });
                    ui.horizontal(|ui| {
                        ui.add(crate::style::field(&mut tun.bind).desired_width(180.0));
                        ui.label(":");
                        ui.add(egui::DragValue::new(&mut tun.listen_port).range(0..=65535));
                    });
                    ui.end_row();
                    if tun.kind != Kind::Dynamic {
                        ui.label(t("tunnel-target"));
                        ui.horizontal(|ui| {
                            ui.add(crate::style::field(&mut tun.target_host).desired_width(180.0));
                            ui.label(":");
                            ui.add(egui::DragValue::new(&mut tun.target_port).range(0..=65535));
                        });
                        ui.end_row();
                    }
                });
            if tun.exposed() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new(i18n.t_args("tunnel-exposed", &[("addr", tun.bind.trim())]))
                        .color(c(theme.ui.warning)),
                );
                ui.checkbox(&mut ed.exposed_ok, t("tunnel-exposed-ok"));
            }
            ui.add_space(6.0);
            ui.checkbox(&mut tun.autostart, t("tunnel-autostart"));
            ui.checkbox(&mut tun.with_session, t("tunnel-with-session"));
            ui.checkbox(&mut tun.reconnect, t("tunnel-reconnect"));
            let error = if tun.session_id.is_empty() || !ssh.iter().any(|(id, _)| *id == tun.session_id) {
                Some(t("tunnel-err-session"))
            } else if tun.listen_port == 0 {
                Some(t("tunnel-err-port"))
            } else if tun.kind != Kind::Dynamic && (tun.target_host.trim().is_empty() || tun.target_port == 0) {
                Some(t("tunnel-err-target"))
            } else {
                None
            };
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if let Some(e) = &error {
                    ui.label(RichText::new(e).small().color(c(theme.ui.danger)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok = error.is_none() && (!tun.exposed() || ed.exposed_ok);
                    if ui
                        .add_enabled_ui(ok, |ui| primary(ui, theme, t("editor-save")))
                        .inner
                        .clicked()
                    {
                        save = true;
                    }
                    if ui.button(t("settings-cancel")).clicked() {
                        close = true;
                    }
                });
            });
        });
        if resp.should_close() {
            close = true;
        }
        if save {
            let mut tun = ed.t;
            if tun.name.trim().is_empty() {
                tun.name = tun.route();
            }
            if ed.is_new {
                tun.sort = self.tunnels.list.len() as i64;
            }
            let was_running = self.tunnels.running(&tun.id);
            self.save_tunnel(&tun);
            if was_running {
                // Apply the new settings.
                self.stop_tunnel(&tun.id);
                self.start_tunnel(&tun.id);
            }
        } else if !close {
            self.tunnels.editor = Some(ed);
        }
    }

    /// Session editor › Tunnels: the tunnels that use this session. Returns
    /// tunnels to save and whether "New tunnel" was clicked.
    pub(crate) fn session_tunnels_tab(&self, ui: &mut egui::Ui, session_id: &str) -> (Vec<Tunnel>, bool) {
        let t = |k: &str| self.i18n.t(k);
        ui.label(RichText::new(t("editor-tunnels-hint")).small().weak());
        ui.add_space(6.0);
        let mut changed = vec![];
        let mine: Vec<&Tunnel> = self
            .tunnels
            .list
            .iter()
            .filter(|x| x.session_id == session_id)
            .collect();
        if mine.is_empty() {
            ui.label(RichText::new(t("editor-tunnels-empty")).weak());
        }
        for tun in mine {
            ui.horizontal(|ui| {
                let mut on = tun.with_session;
                if ui.checkbox(&mut on, "").changed() {
                    let mut x = tun.clone();
                    x.with_session = on;
                    changed.push(x);
                }
                ui.label(RichText::new(&tun.name).strong());
                ui.label(RichText::new(tun.route()).monospace().small().weak());
            });
        }
        ui.label(RichText::new(t("tunnel-with-session")).small().weak());
        ui.add_space(6.0);
        let new = ui.button(t("tunnels-new")).clicked();
        (changed, new)
    }
}
