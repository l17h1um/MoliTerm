//! Macros side panel, editor, recording and running.

use egui::{RichText, Stroke};
use moliterm_core::{
    macros::{self, Macro, Step},
    sessions,
    terminal::{Cmd, encode},
    theme::Theme,
};

use crate::{
    app::App,
    dialogs::primary,
    icons::{self, Icon},
    style::c,
};

#[derive(Default)]
pub struct Macros {
    pub list: Vec<Macro>,
    pub editor: Option<Macro>,
    /// Tab whose typed input is being recorded.
    pub recording: Option<u64>,
    /// The editor is waiting for the shortcut keys.
    pub capturing: bool,
    capture_error: Option<String>,
}

impl App {
    pub(crate) fn load_macros(&mut self) {
        self.macros.list = macros::load(&self.db).unwrap_or_else(|e| {
            tracing::error!("loading macros: {e}");
            vec![]
        });
    }

    /// Runs macro `i` in the multi-exec targets, or else the active tab.
    pub(crate) fn run_macro(&mut self, i: usize) {
        let Some(m) = self.macros.list.get(i) else {
            return;
        };
        let steps = macros::parse(&m.body);
        let targets: Vec<u64> = match &self.multi {
            Some(me) => me.tabs.iter().copied().filter(|id| !me.excluded.contains(id)).collect(),
            None => self.active_term_id().into_iter().collect(),
        };
        let sinks: Vec<_> = self
            .terms_ref()
            .filter(|t| targets.contains(&t.id) && !t.is_dead())
            .map(|t| (t.handle.cmd.clone(), t.handle.encoding))
            .collect();
        for (cmd, enc) in sinks {
            let steps = steps.clone();
            self.rt.spawn(async move {
                for s in steps {
                    match s {
                        Step::Send(bytes) => {
                            if cmd.send(Cmd::Input(encode(enc, bytes))).is_err() {
                                return;
                            }
                        }
                        Step::Sleep(ms) => tokio::time::sleep(std::time::Duration::from_millis(ms)).await,
                    }
                }
            });
        }
    }

    fn toggle_recording(&mut self) {
        if let Some(id) = self.macros.recording.take() {
            let keys = self.term(id).and_then(|t| t.recording.take()).unwrap_or_default();
            self.macros.editor = Some(Macro {
                id: sessions::new_id(),
                body: macros::from_keys(&keys),
                ..Default::default()
            });
        } else if let Some(t) = self.active_term() {
            t.recording = Some(vec![]);
            self.macros.recording = Some(t.id);
        }
    }

    pub(crate) fn macros_panel(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let muted = ui.visuals().weak_text_color();
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let mut record = false;
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(RichText::new(t("panel-macros")).small().strong().color(muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icons::button(ui, Icon::Plus, 14.0, &t("macros-new")).clicked() {
                    self.macros.editor = Some(Macro {
                        id: sessions::new_id(),
                        ..Default::default()
                    });
                }
            });
        });
        ui.add_space(6.0);
        let recording = self.macros.recording.is_some();
        let label = if recording {
            t("macros-stop-record")
        } else {
            t("macros-record")
        };
        let btn = egui::Button::new(RichText::new(label).color(if recording {
            c(theme.ui.danger)
        } else {
            ui.visuals().text_color()
        }));
        if ui
            .add_enabled(recording || self.active_term_id().is_some(), btn)
            .on_hover_text(t("macros-record-hint"))
            .clicked()
        {
            record = true;
        }
        ui.add_space(6.0);
        if self.macros.list.is_empty() {
            ui.label(RichText::new(t("macros-empty")).color(muted));
        }
        let (mut run, mut edit, mut delete) = (None, None, None);
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            for (i, m) in self.macros.list.iter().enumerate() {
                // The card is clickable (double-click, context menu) but registered
                // before its children, so buttons inside still get their clicks.
                let resp = ui
                    .scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), |ui| {
                        egui::Frame::new()
                            .stroke(Stroke::new(1.0, c(theme.ui.border)))
                            .corner_radius(6)
                            .inner_margin(egui::Margin::symmetric(10, 6))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(&m.name).strong());
                                    if !m.hotkey.is_empty() {
                                        ui.label(RichText::new(&m.hotkey).small().monospace().color(muted));
                                    }
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button(t("macros-run")).clicked() {
                                            run = Some(i);
                                        }
                                    });
                                });
                                let preview: String = m.body.lines().next().unwrap_or("").chars().take(40).collect();
                                ui.label(RichText::new(preview).small().monospace().color(muted));
                            })
                    })
                    .response;
                if resp.double_clicked() {
                    edit = Some(m.clone());
                }
                resp.context_menu(|ui| {
                    if ui.button(t("tree-edit")).clicked() {
                        edit = Some(m.clone());
                        ui.close();
                    }
                    if ui.button(t("tree-delete")).clicked() {
                        delete = Some(m.id.clone());
                        ui.close();
                    }
                });
                ui.add_space(6.0);
            }
        });
        if record {
            self.toggle_recording();
        }
        if let Some(i) = run {
            self.run_macro(i);
        }
        if edit.is_some() {
            self.macros.editor = edit;
        }
        if let Some(id) = delete {
            if let Err(e) = macros::delete(&self.db, &id) {
                tracing::error!("delete macro: {e}");
            }
            self.load_macros();
        }
    }

    /// While the macro editor waits for a shortcut, the next key press is taken
    /// here — at the start of the frame, before terminals or app shortcuts see it.
    pub(crate) fn capture_macro_hotkey(&mut self, ctx: &egui::Context) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let Some(m) = self.macros.editor.as_mut() else {
            self.macros.capturing = false;
            return;
        };
        let hk = &mut m.hotkey;
        if self.macros.capturing {
            // Next key press (with its modifiers) becomes the shortcut.
            let pressed = ctx.input(|i| {
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
            if let Some((key, mods)) = pressed {
                let f_key = key.name().starts_with('F') && key.name().len() > 1;
                if key == egui::Key::Escape && mods.is_none() {
                    self.macros.capturing = false;
                } else if mods.is_none() && matches!(key, egui::Key::Backspace | egui::Key::Delete) {
                    hk.clear();
                    self.macros.capturing = false;
                } else if !(mods.command || mods.ctrl || mods.alt || mods.mac_cmd) && !f_key {
                    // Shift+letter would steal normal typing.
                    self.macros.capture_error = Some(t("macros-err-modifier"));
                } else if crate::hotkeys::shadows_terminal(mods, key) {
                    self.macros.capture_error = Some(t("hk-err-terminal"));
                } else {
                    *hk = crate::hotkeys::format(mods, key);
                    self.macros.capturing = false;
                    self.macros.capture_error = None;
                }
                ctx.input_mut(|i| i.events.clear());
            }
        }
    }

    pub(crate) fn macro_editor_ui(&mut self, ctx: &egui::Context, theme: &Theme) {
        let Some(mut m) = self.macros.editor.take() else {
            return;
        };
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(24);
        let (mut save, mut close) = (false, false);
        let resp = egui::Modal::new("macro-editor".into()).frame(frame).show(ctx, |ui| {
            crate::style::fit_width(ui, 560.0);
            ui.heading(t("macros-editor"));
            ui.add_space(10.0);
            egui::Grid::new("macro-grid")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    ui.label(t("tunnel-name"));
                    ui.add(crate::style::field(&mut m.name).desired_width(360.0));
                    ui.end_row();
                    ui.label(t("macros-hotkey"));
                    ui.horizontal(|ui| {
                        let capturing = self.macros.capturing;
                        let label = if capturing {
                            t("hk-press")
                        } else if m.hotkey.is_empty() {
                            t("macros-hotkey-set")
                        } else {
                            m.hotkey.clone()
                        };
                        let btn = egui::Button::new(RichText::new(label).monospace())
                            .selected(capturing)
                            .min_size(egui::vec2(200.0, 0.0));
                        if ui.add(btn).on_hover_text(t("hk-hint")).clicked() {
                            self.macros.capturing = !capturing;
                            self.macros.capture_error = None;
                        }
                        if !m.hotkey.is_empty() && !capturing && ui.button(t("macros-hotkey-clear")).clicked() {
                            m.hotkey.clear();
                        }
                    });
                    ui.end_row();
                });
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::multiline(&mut m.body)
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(8)
                    .desired_width(f32::INFINITY),
            );
            ui.label(RichText::new(t("macros-syntax")).small().weak());
            let hk = m.hotkey.trim();
            let error = if let Some(e) = &self.macros.capture_error {
                Some(e.clone())
            } else if m.name.trim().is_empty() {
                Some(t("macros-err-name"))
            } else if !hk.is_empty() {
                match crate::hotkeys::parse(hk) {
                    None => Some(t("macros-err-hotkey")),
                    Some(k) if crate::hotkeys::shadows_terminal(k.modifiers, k.logical_key) => {
                        Some(t("hk-err-terminal"))
                    }
                    Some(k) => {
                        let text = crate::hotkeys::format(k.modifiers, k.logical_key);
                        crate::hotkeys::TABLE
                            .iter()
                            .find(|r| crate::hotkeys::binding(r.0, &self.settings.hotkeys) == text)
                            .map(|r| i18n.t_args("hk-err-used", &[("action", &i18n.t(r.1))]))
                    }
                }
            } else {
                None
            };
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if let Some(e) = &error {
                    ui.label(RichText::new(e).small().color(c(theme.ui.danger)));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add_enabled_ui(error.is_none(), |ui| primary(ui, theme, t("editor-save")))
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
        if save || close || resp.should_close() {
            self.macros.capturing = false;
            self.macros.capture_error = None;
        }
        if save {
            m.hotkey = m.hotkey.trim().to_owned();
            if !self.macros.list.iter().any(|x| x.id == m.id) {
                m.sort = self.macros.list.len() as i64;
            }
            if let Err(e) = macros::save(&self.db, &m) {
                tracing::error!("save macro: {e}");
            }
            self.load_macros();
        } else if !close && !resp.should_close() {
            self.macros.editor = Some(m);
        }
    }
}

#[cfg(test)]
mod tests {
    /// A clickable card must not swallow clicks on buttons inside it.
    fn click_button_in_card(new_way: bool) -> bool {
        use std::cell::Cell;
        let ctx = egui::Context::default();
        let button_rect = Cell::new(egui::Rect::NOTHING);
        let clicked = Cell::new(false);
        let frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let inner = |ui: &mut egui::Ui| {
                    egui::Frame::new().inner_margin(10).show(ui, |ui| {
                        let b = ui.button("Run");
                        button_rect.set(b.rect);
                        clicked.set(clicked.get() | b.clicked());
                    })
                };
                if new_way {
                    let _ = ui.scope_builder(egui::UiBuilder::new().sense(egui::Sense::click()), inner);
                } else {
                    let _ = inner(ui).response.interact(egui::Sense::click());
                }
            });
            out.textures_delta.clear();
        };
        frame(vec![]);
        let p = button_rect.get().center();
        frame(vec![egui::Event::PointerMoved(p)]);
        let press = |pressed| egui::Event::PointerButton {
            pos: p,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: Default::default(),
        };
        frame(vec![press(true)]);
        frame(vec![press(false)]);
        clicked.get()
    }

    #[test]
    fn card_buttons_get_clicks() {
        assert!(
            !click_button_in_card(false),
            "old layout was expected to swallow the click"
        );
        assert!(click_button_in_card(true));
    }
}
