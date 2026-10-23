//! Modal dialogs: multi-line paste (SPEC §7.4), right-click choice (§7.3), OSC 52 (§7.1).

use egui::{RichText, Stroke};
use moliterm_core::{settings::RightClick, theme::Theme};

use crate::{app::App, style::c};

#[derive(Default)]
pub struct KeyGen {
    rsa: bool,
    comment: String,
    passphrase: String,
    path: String,
    busy: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    result: Option<Result<String, String>>,
}

pub enum Dialog {
    Info(String),
    Risky {
        line: String,
        job: bool,
        n: usize,
    },
    Restore {
        tabs: Vec<crate::layout::TabRef>,
        checked: Vec<bool>,
        saved: Box<crate::layout::Saved>,
        remember: bool,
    },
    DeleteFolder {
        id: String,
        name: String,
        count: usize,
    },
    KeyGen(KeyGen),
    Paste {
        tab: u64,
        /// Tabs that receive the paste (several in multi-exec).
        targets: Vec<u64>,
        text: String,
        by_line: bool,
        no_ask: bool,
    },
    RightClick {
        tab: u64,
    },
    Osc52 {
        tab: u64,
        text: String,
    },
}

/// Accent-filled primary button.
pub fn primary(ui: &mut egui::Ui, theme: &Theme, label: String) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(label).color(c(theme.ui.on_accent)).strong()).fill(c(theme.ui.accent)))
}

impl App {
    pub(crate) fn dialogs(&mut self, ctx: &egui::Context, theme: &Theme) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        let tab_id = match &dialog {
            Dialog::Paste { tab, .. } | Dialog::RightClick { tab } | Dialog::Osc52 { tab, .. } => *tab,
            _ => 0,
        };
        let name = self.term(tab_id).map(|t| t.name.clone()).unwrap_or_default();
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(20);
        let mut keep = true;
        match &mut dialog {
            Dialog::Paste {
                tab,
                targets,
                text,
                by_line,
                no_ask,
            } => {
                let lines: Vec<&str> = text.lines().collect();
                let delay = self.settings.paste_delay_ms;
                let mut go = false;
                let resp = egui::Modal::new("paste-dialog".into()).frame(frame).show(ctx, |ui| {
                    crate::style::fit_width(ui, 560.0);
                    let n = lines.len().to_string();
                    let title = if targets.len() > 1 {
                        self.i18n
                            .t_args("paste-title-multi", &[("n", &n), ("k", &targets.len().to_string())])
                    } else {
                        self.i18n.t_args("paste-title", &[("n", &n), ("tab", &name)])
                    };
                    ui.label(RichText::new(title).heading());
                    ui.add_space(8.0);
                    egui::Frame::new()
                        .fill(c(theme.ui.bg))
                        .inner_margin(10)
                        .corner_radius(6)
                        .show(ui, |ui| {
                            egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                for (i, l) in lines.iter().enumerate().take(500) {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(format!("{:>3}", i + 1))
                                                .monospace()
                                                .color(c(theme.ui.muted)),
                                        );
                                        ui.label(RichText::new(*l).monospace());
                                    });
                                }
                            });
                        });
                    ui.add_space(8.0);
                    ui.radio_value(by_line, false, t("paste-as-is"));
                    let ms = delay.to_string();
                    ui.radio_value(by_line, true, self.i18n.t_args("paste-by-line", &[("ms", &ms)]));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.checkbox(no_ask, t("paste-dont-ask"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if primary(ui, theme, t("paste-ok")).clicked()
                                || ui.input(|i| i.key_pressed(egui::Key::Enter))
                            {
                                go = true;
                            }
                            if ui.button(t("settings-cancel")).clicked() {
                                keep = false;
                            }
                        });
                    });
                });
                if resp.should_close() {
                    keep = false;
                }
                if go {
                    keep = false;
                    let (id, by_line, no_ask) = (*tab, *by_line, *no_ask);
                    let text = std::mem::take(text);
                    let rt = self.rt.clone();
                    if let Some(t) = self.term(id) {
                        t.paste_no_ask |= no_ask;
                    }
                    for target in targets.clone() {
                        if let Some(t) = self.term(target) {
                            if by_line {
                                t.paste_lines(&rt, &text, delay);
                            } else {
                                t.paste_now(&text);
                            }
                        }
                    }
                }
            }
            Dialog::RightClick { tab } => {
                let mut choice = None;
                let resp = egui::Modal::new("right-click-dialog".into())
                    .frame(frame)
                    .show(ctx, |ui| {
                        crate::style::fit_width(ui, 420.0);
                        ui.label(RichText::new(t("rclick-title")).heading());
                        ui.add_space(6.0);
                        ui.label(RichText::new(t("rclick-body")).color(c(theme.ui.muted)));
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            if primary(ui, theme, t("rclick-paste")).clicked() {
                                choice = Some(RightClick::Paste);
                            }
                            if ui.button(t("rclick-menu")).clicked() {
                                choice = Some(RightClick::Menu);
                            }
                        });
                    });
                if resp.should_close() {
                    keep = false;
                }
                if let Some(ch) = choice {
                    keep = false;
                    self.set_right_click(ch);
                    let id = *tab;
                    let pointer = ctx.input(|i| i.pointer.latest_pos()).unwrap_or_default();
                    match ch {
                        RightClick::Paste => self.handle_paste_clipboard(id),
                        _ => {
                            if let Some(t) = self.term(id) {
                                t.open_menu(pointer);
                            }
                        }
                    }
                }
            }
            Dialog::Info(_)
            | Dialog::DeleteFolder { .. }
            | Dialog::KeyGen(_)
            | Dialog::Risky { .. }
            | Dialog::Restore { .. } => {}
            Dialog::Osc52 { tab, text } => {
                let mut answer = None;
                let resp = egui::Modal::new("osc52-dialog".into()).frame(frame).show(ctx, |ui| {
                    crate::style::fit_width(ui, 420.0);
                    ui.label(RichText::new(self.i18n.t_args("osc52-title", &[("tab", &name)])).heading());
                    ui.add_space(6.0);
                    ui.label(RichText::new(t("osc52-body")).color(c(theme.ui.muted)));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if primary(ui, theme, t("osc52-allow")).clicked() {
                            answer = Some(true);
                        }
                        if ui.button(t("osc52-deny")).clicked() {
                            answer = Some(false);
                        }
                    });
                });
                if resp.should_close() {
                    answer = Some(false);
                }
                if let Some(a) = answer {
                    keep = false;
                    if a {
                        ctx.copy_text(std::mem::take(text));
                    }
                    let id = *tab;
                    if let Some(t) = self.term(id) {
                        t.osc52 = Some(a);
                    }
                }
            }
        }
        if let Dialog::Risky { line, job, n } = &dialog {
            let mut send = false;
            egui::Modal::new("risky".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 460.0);
                ui.label(RichText::new(self.i18n.t_args("risky-title", &[("n", &n.to_string())])).heading());
                ui.add_space(6.0);
                ui.label(RichText::new(line.as_str()).monospace().color(c(theme.ui.danger)));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if primary(ui, theme, t("settings-cancel")).clicked() {
                        keep = false;
                    }
                    let danger = egui::Button::new(
                        RichText::new(self.i18n.t_args("risky-send", &[("n", &n.to_string())]))
                            .color(c(theme.ui.on_accent)),
                    )
                    .fill(c(theme.ui.danger));
                    if ui.add(danger).clicked() {
                        send = true;
                    }
                });
            });
            if send {
                let (line, job) = (line.clone(), *job);
                self.dialog = None;
                self.send_line(line, job, true);
                return;
            }
        }
        if let Dialog::Restore {
            tabs,
            checked,
            saved,
            remember,
        } = &mut dialog
        {
            let mut reopen = false;
            egui::Modal::new("restore".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 560.0);
                ui.label(RichText::new(self.i18n.t_args("restore-title", &[("n", &tabs.len().to_string())])).heading());
                ui.add_space(8.0);
                egui::Grid::new("restore-grid")
                    .num_columns(2)
                    .spacing([40.0, 6.0])
                    .show(ui, |ui| {
                        for (i, tab) in tabs.iter().enumerate() {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut checked[i], &tab.name);
                                ui.label(RichText::new(&tab.proto).small().color(c(theme.ui.muted)));
                            });
                            if i % 2 == 1 {
                                ui.end_row();
                            }
                        }
                    });
                ui.add_space(6.0);
                ui.label(RichText::new(t("restore-note")).small().color(c(theme.ui.muted)));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    ui.checkbox(remember, t("restore-remember"));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let n = checked.iter().filter(|c| **c).count();
                        if primary(ui, theme, self.i18n.t_args("restore-ok", &[("n", &n.to_string())])).clicked() {
                            reopen = true;
                        }
                        if ui.button(t("restore-not-now")).clicked() {
                            keep = false;
                            if *remember {
                                let mut st = self.settings.clone();
                                st.restore = moliterm_core::settings::RestoreMode::Never;
                                self.save_settings(st);
                            }
                        }
                    });
                });
            });
            if reopen {
                keep = false;
                if *remember {
                    let mut st = self.settings.clone();
                    st.restore = moliterm_core::settings::RestoreMode::Always;
                    self.save_settings(st);
                }
                let keep_set = checked
                    .iter()
                    .enumerate()
                    .filter(|(_, c)| **c)
                    .map(|(i, _)| i)
                    .collect();
                let saved = std::mem::replace(
                    saved,
                    Box::new(crate::layout::Saved {
                        main: egui_dock::DockState::new(vec![]),
                        windows: vec![],
                    }),
                );
                self.reopen(ctx, *saved, &keep_set);
            }
        }
        if let Dialog::Info(msg) = &dialog {
            egui::Modal::new("info-dialog".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 420.0);
                ui.label(msg);
                ui.add_space(10.0);
                if primary(ui, theme, t("prompt-ok")).clicked() {
                    keep = false;
                }
            });
        }
        if let Dialog::DeleteFolder { id, name, count } = &dialog {
            let mut delete = false;
            let resp = egui::Modal::new("delete-folder".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 420.0);
                let n = count.to_string();
                ui.label(RichText::new(self.i18n.t_args("delete-folder-title", &[("name", name)])).heading());
                ui.label(self.i18n.t_args("delete-folder-body", &[("n", &n)]));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(t("settings-cancel")).clicked() {
                        keep = false;
                    }
                    let danger = egui::Button::new(RichText::new(t("tree-delete")).color(c(theme.ui.on_accent)))
                        .fill(c(theme.ui.danger));
                    if ui.add(danger).clicked() {
                        delete = true;
                    }
                });
            });
            if resp.should_close() {
                keep = false;
            }
            if delete {
                keep = false;
                if let Err(e) = moliterm_core::sessions::delete_folder(&self.db, id, &self.folders) {
                    tracing::error!("delete folder: {e}");
                }
                self.reload_sessions();
            }
        }
        if let Dialog::KeyGen(kg) = &mut dialog {
            if kg.path.is_empty() {
                kg.path = moliterm_core::sshconfig::expand_home("~/.ssh/id_ed25519_moliterm");
                kg.comment = format!("{}@moliterm", crate::sessions_ui::local_user());
            }
            if let Some(r) = kg.busy.as_ref().and_then(|rx| rx.try_recv().ok()) {
                kg.busy = None;
                if r.is_ok()
                    && let Some(ed) = &mut self.editor
                {
                    ed.set_key_path(kg.path.clone());
                }
                kg.result = Some(r);
            }
            let resp = egui::Modal::new("keygen".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 520.0);
                ui.label(RichText::new(t("keygen-title")).heading());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let was = kg.rsa;
                    ui.radio_value(&mut kg.rsa, false, "Ed25519");
                    ui.radio_value(&mut kg.rsa, true, "RSA 4096");
                    if was != kg.rsa {
                        let (from, to) = if kg.rsa {
                            ("id_ed25519", "id_rsa")
                        } else {
                            ("id_rsa", "id_ed25519")
                        };
                        kg.path = kg.path.replace(from, to);
                    }
                });
                egui::Grid::new("kg")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(t("keygen-path"));
                        ui.add(crate::style::field(&mut kg.path).desired_width(360.0));
                        ui.end_row();
                        ui.label(t("keygen-comment"));
                        ui.add(crate::style::field(&mut kg.comment).desired_width(360.0));
                        ui.end_row();
                        ui.label(t("keygen-passphrase"));
                        ui.add(
                            crate::style::field(&mut kg.passphrase)
                                .password(true)
                                .desired_width(360.0),
                        );
                        ui.end_row();
                    });
                match &kg.result {
                    Some(Ok(fp)) => {
                        ui.label(
                            RichText::new(self.i18n.t_args("keygen-done", &[("fp", fp)])).color(c(theme.ui.success)),
                        );
                    }
                    Some(Err(e)) => {
                        ui.label(RichText::new(e).color(c(theme.ui.danger)));
                    }
                    None => {}
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(t(if matches!(kg.result, Some(Ok(_))) {
                            "prompt-ok"
                        } else {
                            "settings-cancel"
                        }))
                        .clicked()
                    {
                        keep = false;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if kg.busy.is_some() {
                            ui.spinner();
                        } else if !matches!(kg.result, Some(Ok(_)))
                            && primary(ui, theme, t("keygen-generate")).clicked()
                        {
                            let (tx, rx) = std::sync::mpsc::channel();
                            let kind = if kg.rsa {
                                moliterm_core::keys::KeyType::Rsa4096
                            } else {
                                moliterm_core::keys::KeyType::Ed25519
                            };
                            let (comment, pass) = (kg.comment.clone(), kg.passphrase.clone());
                            let path = std::path::PathBuf::from(moliterm_core::sshconfig::expand_home(&kg.path));
                            let wake = ui.ctx().clone();
                            std::thread::spawn(move || {
                                let r = moliterm_core::keys::generate(kind, &comment, &pass, &path)
                                    .map_err(|e| e.to_string());
                                let _ = tx.send(r);
                                wake.request_repaint();
                            });
                            kg.busy = Some(rx);
                            kg.result = None;
                        }
                    });
                });
            });
            if resp.should_close() && kg.busy.is_none() {
                keep = false;
            }
        }
        if keep && self.dialog.is_none() {
            self.dialog = Some(dialog);
        }
    }
}
