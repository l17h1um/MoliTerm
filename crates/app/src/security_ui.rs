//! Settings sections that need app state: Vault, SSH agent, SSH & keys.

use std::sync::mpsc;

use egui::RichText;
use moliterm_core::{
    known_hosts,
    settings::Settings,
    theme::Theme,
    vault::{self, Vault},
};
use zeroize::Zeroizing;

use crate::{app::App, dialogs::Dialog, settings_ui::Section, style::c};

#[derive(Default)]
pub struct ChangePassword {
    pw1: Zeroizing<String>,
    pw2: Zeroizing<String>,
    msg: Option<Result<String, String>>,
    busy: Option<crate::asks::NewKey>,
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

impl App {
    pub(crate) fn security_section(
        &mut self,
        ui: &mut egui::Ui,
        section: Section,
        draft: &mut Settings,
        theme: &Theme,
    ) {
        match section {
            Section::Vault => self.vault_section(ui, draft, theme),
            Section::Agent => self.agent_section(ui, theme),
            Section::Ssh => self.ssh_section(ui, draft),
            Section::Import => self.import_section(ui, theme),
            Section::Advanced => self.advanced_section(ui, draft),
            _ => {}
        }
    }

    fn vault_section(&mut self, ui: &mut egui::Ui, draft: &mut Settings, theme: &Theme) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        ui.heading(t("settings-vault"));
        ui.label(RichText::new(t("vault-about")).weak());
        ui.add_space(10.0);
        let exists = vault::exists(&self.db);
        let status = if !exists {
            t("vault-status-none")
        } else if self.vault.is_unlocked() {
            i18n.t_args("vault-status-unlocked", &[("n", &Vault::count(&self.db).to_string())])
        } else {
            t("vault-status-locked")
        };
        ui.label(RichText::new(status).strong());
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if !exists {
                if ui.button(t("vault-create")).clicked() {
                    self.request_create_vault();
                }
            } else if self.vault.is_unlocked() {
                if ui.button(t("vault-lock-now")).clicked() {
                    self.lock_vault();
                }
            } else if ui.button(t("vault-unlock")).clicked() {
                self.request_unlock();
            }
        });
        caption(ui, t("vault-lock-title"));
        ui.radio_value(&mut draft.vault_auto_lock, false, t("vault-lock-never"));
        ui.horizontal(|ui| {
            ui.radio_value(&mut draft.vault_auto_lock, true, t("vault-lock-after"));
            ui.add_enabled(
                draft.vault_auto_lock,
                egui::DragValue::new(&mut draft.vault_lock_minutes).range(1..=1440),
            );
            ui.label(t("vault-lock-minutes"));
        });
        ui.checkbox(&mut draft.vault_lock_on_sleep, t("vault-lock-sleep"));
        ui.label(RichText::new(t("vault-no-recovery")).color(c(theme.ui.warning)));

        if exists && self.vault.is_unlocked() {
            caption(ui, t("vault-change").to_uppercase());
            let ch = &mut self.change_pw;
            if let Some(r) = ch.busy.as_ref().and_then(|rx| rx.try_recv().ok()) {
                ch.busy = None;
                ch.msg = Some(
                    match r.and_then(|(key, salt)| self.vault.change_key(&self.db, key, &salt)) {
                        Ok(()) => Ok(i18n.t("vault-changed")),
                        Err(e) => Err(e.to_string()),
                    },
                );
            }
            egui::Grid::new("chpw")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label(t("vault-new-master"));
                    ui.add(crate::style::field(&mut *ch.pw1).password(true).desired_width(260.0));
                    ui.end_row();
                    ui.label(t("vault-repeat"));
                    ui.add(crate::style::field(&mut *ch.pw2).password(true).desired_width(260.0));
                    ui.end_row();
                });
            ui.horizontal(|ui| {
                if ch.busy.is_some() {
                    ui.spinner();
                } else if ui.button(t("vault-change")).clicked() {
                    if ch.pw1.len() < 8 {
                        ch.msg = Some(Err(i18n.t("vault-too-short")));
                    } else if *ch.pw1 != *ch.pw2 {
                        ch.msg = Some(Err(i18n.t("vault-mismatch")));
                    } else {
                        let (tx, rx) = mpsc::channel();
                        let pw = ch.pw1.clone();
                        let wake = ui.ctx().clone();
                        std::thread::spawn(move || {
                            let _ = tx.send(vault::new_key(&pw));
                            wake.request_repaint();
                        });
                        ch.busy = Some(rx);
                        ch.pw1 = Default::default();
                        ch.pw2 = Default::default();
                    }
                }
                match &ch.msg {
                    Some(Ok(m)) => {
                        ui.label(RichText::new(m).color(c(theme.ui.success)));
                    }
                    Some(Err(m)) => {
                        ui.label(RichText::new(m).color(c(theme.ui.danger)));
                    }
                    None => {}
                }
            });
        }
    }

    fn agent_section(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        ui.heading(t("settings-agent"));
        ui.label(RichText::new(t("agent-about")).weak());
        ui.add_space(10.0);
        let loaded: Vec<(String, String)> = self
            .agent_keys
            .lock()
            .map(|k| {
                k.iter()
                    .map(|k| (known_hosts::fingerprint(k.public_key()), k.comment().to_string()))
                    .collect()
            })
            .unwrap_or_default();
        caption(ui, t("agent-loaded").to_uppercase());
        if loaded.is_empty() {
            ui.label(RichText::new(t("agent-none")).weak());
        }
        for (fp, comment) in &loaded {
            ui.label(RichText::new(format!("{fp}  {comment}")).monospace());
        }
        caption(ui, t("agent-files").to_uppercase());
        let mut remove = None;
        for (i, p) in self.settings.agent_key_paths.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(p).monospace());
                if ui.small_button(t("terminal-rule-remove")).clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some(i) = remove {
            self.remove_agent_key(i);
        }
        if ui.button(t("agent-add")).clicked() {
            let file = rfd::AsyncFileDialog::new().pick_file();
            let tx = self.agent_pick.0.clone();
            let wake = ui.ctx().clone();
            self.rt.spawn(async move {
                if let Some(f) = file.await {
                    let _ = tx.send(f.path().to_string_lossy().into_owned());
                    wake.request_repaint();
                }
            });
        }
        if let Some(path) = self.agent_pick.1.try_iter().last() {
            self.add_agent_key(path);
        }
        if vault::exists(&self.db) && !self.vault.is_unlocked() {
            ui.label(RichText::new(t("agent-locked")).color(c(theme.ui.warning)));
        }
    }

    fn ssh_section(&mut self, ui: &mut egui::Ui, draft: &mut Settings) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        ui.heading(t("settings-ssh"));
        ui.add_space(10.0);
        ui.label(t("ssh-known-hosts"));
        let kh = known_hosts::default_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        ui.label(RichText::new(kh).monospace());
        ui.add_space(8.0);
        ui.checkbox(&mut draft.ssh_config_folder, t("ssh-show-config"));
        ui.checkbox(&mut draft.color_border, t("ssh-color-border"));
        caption(ui, t("x11-title"));
        ui.horizontal(|ui| {
            ui.label(t("x11-display"));
            ui.add(
                crate::style::field(&mut draft.x11_display)
                    .hint_text(std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into()))
                    .desired_width(260.0)
                    .font(egui::TextStyle::Monospace),
            );
        });
        ui.label(RichText::new(t("x11-hint")).small().weak());
        caption(ui, t("keygen-title").to_uppercase());
        if ui.button(t("keygen-open")).clicked() {
            self.dialog = Some(Dialog::KeyGen(Default::default()));
        }
    }
}
