//! Questions from session tasks (host keys, passwords, 2FA prompts, vault
//! secrets) and the vault unlock/create dialogs (SPEC §5.1.7, §9.2, §9.3).

use std::{
    sync::{Arc, mpsc},
    time::{Duration, Instant, SystemTime},
};

use egui::{RichText, Stroke};
use moliterm_core::{
    Error,
    ssh::{self, Ask, HostKeyAnswer, PromptKind},
    theme::Theme,
    vault::{self, Key, Vault},
};
use tokio::sync::oneshot;
use zeroize::Zeroizing;

use crate::{
    app::App,
    dialogs::primary,
    icons::{self, Icon},
    style::c,
};

type Reply = oneshot::Sender<Option<Zeroizing<String>>>;
/// Result of deriving a new vault key on a worker thread.
pub type NewKey = mpsc::Receiver<Result<(Key, [u8; 16]), Error>>;

#[derive(Default)]
pub struct VaultUi {
    /// "Continue locked — ask for passwords" chosen in this run.
    pub continue_locked: bool,
    unlock: Option<Unlock>,
    create: Option<Create>,
    /// Secret reads waiting for the vault.
    waiting: Vec<(String, Reply)>,
    /// Secrets to write once the vault is open.
    to_store: Vec<(String, Zeroizing<String>)>,
    /// Text typed into the current ask dialog.
    fields: Vec<Zeroizing<String>>,
    save: bool,
}

#[derive(Default)]
struct Unlock {
    password: Zeroizing<String>,
    error: bool,
    busy: Option<mpsc::Receiver<Result<Key, Error>>>,
}

#[derive(Default)]
pub struct Create {
    pw1: Zeroizing<String>,
    pw2: Zeroizing<String>,
    error: Option<String>,
    busy: Option<NewKey>,
}

impl App {
    /// Sorts incoming questions: vault reads/writes are answered here, the rest
    /// wait for a dialog. Also runs vault auto-lock.
    pub(crate) fn pump_asks(&mut self, ctx: &egui::Context) {
        while let Ok(ask) = self.ask_rx.try_recv() {
            match ask {
                Ask::Secret { id, reply } => {
                    if self.vault.is_unlocked() {
                        let _ = reply.send(self.vault.get(&self.db, &id).ok().flatten());
                    } else if !vault::exists(&self.db) || self.vault_ui.continue_locked {
                        let _ = reply.send(None);
                    } else {
                        self.vault_ui.waiting.push((id, reply));
                        self.open_unlock();
                    }
                }
                Ask::StoreSecret { id, value } => {
                    if self.vault.is_unlocked() {
                        if let Err(e) = self.vault.put(&self.db, &id, &value) {
                            tracing::error!("vault put: {e}");
                        }
                    } else if !vault::exists(&self.db) {
                        self.vault_ui.to_store.push((id, value));
                        self.vault_ui.create.get_or_insert_default();
                    } else if !self.vault_ui.continue_locked {
                        self.vault_ui.to_store.push((id, value));
                        self.open_unlock();
                    }
                }
                Ask::HostKey { reply, .. } if self.dev_auto_answer => {
                    let _ = reply.send(HostKeyAnswer::Once);
                }
                Ask::Password { reply, .. } if self.dev_auto_answer => {
                    let _ = reply.send(Some((Zeroizing::new("test".into()), false)));
                }
                other => self.asks.push_back(other),
            }
        }
        // Drop questions whose session went away.
        self.asks.retain(|a| match a {
            Ask::HostKey { reply, .. } => !reply.is_closed(),
            Ask::Password { reply, .. } => !reply.is_closed(),
            Ask::KeyboardInteractive { reply, .. } => !reply.is_closed(),
            _ => true,
        });

        // Auto-lock after idle time or OS sleep (wall clock jumps ahead of the monotonic one).
        if ctx.input(|i| !i.events.is_empty()) {
            self.last_activity = Instant::now();
        }
        let (wall, mono) = self.last_tick;
        let slept = SystemTime::now()
            .duration_since(wall)
            .ok()
            .is_some_and(|d| d > mono.elapsed() + Duration::from_secs(30));
        self.last_tick = (SystemTime::now(), Instant::now());
        if self.vault.is_unlocked() {
            let s = &self.settings;
            let idle = Duration::from_secs(u64::from(s.vault_lock_minutes.max(1)) * 60);
            if (slept && s.vault_lock_on_sleep) || (s.vault_auto_lock && self.last_activity.elapsed() > idle) {
                self.lock_vault();
            } else if s.vault_auto_lock {
                ctx.request_repaint_after(Duration::from_secs(30));
            }
        }
    }

    fn open_unlock(&mut self) {
        if self.vault_ui.unlock.is_none() {
            self.vault_ui.unlock = Some(Unlock::default());
        }
    }

    /// Settings › Vault "Unlock" button.
    pub(crate) fn request_unlock(&mut self) {
        self.vault_ui.continue_locked = false;
        self.open_unlock();
    }

    pub(crate) fn request_create_vault(&mut self) {
        self.vault_ui.create.get_or_insert_default();
    }

    pub(crate) fn lock_vault(&mut self) {
        self.vault.lock();
        if let Ok(mut keys) = self.agent_keys.lock() {
            keys.clear();
        }
        tracing::info!("vault locked");
    }

    fn vault_opened(&mut self, v: Vault) {
        self.vault = v;
        self.vault_ui.continue_locked = false;
        for (id, reply) in std::mem::take(&mut self.vault_ui.waiting) {
            let _ = reply.send(self.vault.get(&self.db, &id).ok().flatten());
        }
        for (id, value) in std::mem::take(&mut self.vault_ui.to_store) {
            if let Err(e) = self.vault.put(&self.db, &id, &value) {
                tracing::error!("vault put: {e}");
            }
        }
        self.last_activity = Instant::now();
        self.load_agent_keys();
    }

    /// Loads every configured key file into the built-in agent (prompts for
    /// passphrases that are not in the vault).
    pub(crate) fn load_agent_keys(&mut self) {
        for path in self.settings.agent_key_paths.clone() {
            self.load_agent_key(path);
        }
    }

    fn load_agent_key(&self, path: String) {
        let ctx = self.ssh_ctx.clone();
        let hop = ssh::Hop {
            label: path.clone(),
            host: String::new(),
            port: 0,
            user: String::new(),
            auth: ssh::AuthMethod::Key,
            key_path: Some(path.clone()),
            agent: ssh::AgentKind::BuiltIn,
            secret_id: format!("agentkey:{path}"),
        };
        self.rt.spawn(async move {
            match ssh::load_key(&path, &hop, &ctx).await {
                Ok(key) => {
                    if let Ok(mut keys) = ctx.agent_keys.lock()
                        && !keys.iter().any(|k| k.public_key() == key.public_key())
                    {
                        keys.push(Arc::new(key));
                    }
                    (ctx.wake)();
                }
                Err(f) => tracing::warn!("agent key {path}: {}", f.msg),
            }
        });
    }

    pub(crate) fn add_agent_key(&mut self, path: String) {
        if !self.settings.agent_key_paths.contains(&path) {
            let mut s = self.settings.clone();
            s.agent_key_paths.push(path.clone());
            self.save_settings(s);
        }
        if self.vault.is_unlocked() || !vault::exists(&self.db) {
            self.load_agent_key(path);
        }
    }

    pub(crate) fn remove_agent_key(&mut self, index: usize) {
        let mut s = self.settings.clone();
        if index < s.agent_key_paths.len() {
            s.agent_key_paths.remove(index);
            self.save_settings(s);
        }
        // Reload the rest so the removed key disappears from memory.
        if let Ok(mut keys) = self.agent_keys.lock() {
            keys.clear();
        }
        if self.vault.is_unlocked() || !vault::exists(&self.db) {
            self.load_agent_keys();
        }
    }

    // ---- dialogs --------------------------------------------------------

    pub(crate) fn ask_dialogs(&mut self, ctx: &egui::Context, theme: &Theme) {
        if self.vault_dialogs(ctx, theme) {
            return;
        }
        let Some(ask) = self.asks.pop_front() else {
            return;
        };
        let t = |k: &str| self.i18n.t(k);
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(22);
        match ask {
            Ask::HostKey {
                host,
                port,
                key_type,
                fingerprint,
                changed,
                reply,
            } => {
                let mut answer = None;
                let danger = c(theme.ui.danger);
                let frame = if changed.is_some() {
                    frame.stroke(Stroke::new(1.5, danger))
                } else {
                    frame
                };
                egui::Modal::new("hostkey".into()).frame(frame).show(ctx, |ui| {
                    crate::style::fit_width(ui, 640.0);
                    let hp = format!("{host}:{port}");
                    ui.horizontal(|ui| {
                        if changed.is_some() {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(22.0, 22.0), egui::Sense::hover());
                            icons::paint(ui.painter(), r, Icon::Warning, danger);
                        }
                        let title = if changed.is_some() {
                            "hostkey-changed-title"
                        } else {
                            "hostkey-unknown-title"
                        };
                        ui.label(RichText::new(self.i18n.t_args(title, &[("host", &host)])).heading());
                    });
                    ui.add_space(6.0);
                    let body = if changed.is_some() {
                        "hostkey-changed-body"
                    } else {
                        "hostkey-unknown-body"
                    };
                    ui.label(self.i18n.t_args(body, &[("hostport", &hp)]));
                    ui.add_space(8.0);
                    egui::Frame::new()
                        .fill(c(theme.ui.bg))
                        .inner_margin(10)
                        .corner_radius(6)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::Grid::new("fp")
                                .num_columns(2)
                                .spacing([16.0, 4.0])
                                .show(ui, |ui| {
                                    if let Some(saved) = &changed {
                                        ui.label(RichText::new(t("hostkey-saved")).weak());
                                        ui.label(RichText::new(format!("{key_type} {saved}")).monospace());
                                        ui.end_row();
                                        ui.label(RichText::new(t("hostkey-presented")).weak());
                                        ui.label(
                                            RichText::new(format!("{key_type} {fingerprint}"))
                                                .monospace()
                                                .color(danger),
                                        );
                                    } else {
                                        ui.label(RichText::new(t("hostkey-fingerprint")).weak());
                                        ui.label(RichText::new(format!("{key_type} {fingerprint}")).monospace());
                                    }
                                    ui.end_row();
                                });
                        });
                    ui.add_space(14.0);
                    ui.horizontal(|ui| {
                        if changed.is_some() {
                            if primary(ui, theme, t("hostkey-disconnect")).clicked() {
                                answer = Some(HostKeyAnswer::Reject);
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let accept = egui::Button::new(RichText::new(t("hostkey-accept-update")).color(danger))
                                    .stroke(Stroke::new(1.0, danger));
                                if ui.add(accept).clicked() {
                                    answer = Some(HostKeyAnswer::Save);
                                }
                                if ui.button(t("hostkey-once")).clicked() {
                                    answer = Some(HostKeyAnswer::Once);
                                }
                            });
                        } else {
                            if ui.button(t("settings-cancel")).clicked() {
                                answer = Some(HostKeyAnswer::Reject);
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if primary(ui, theme, t("hostkey-trust")).clicked() {
                                    answer = Some(HostKeyAnswer::Save);
                                }
                                if ui.button(t("hostkey-once")).clicked() {
                                    answer = Some(HostKeyAnswer::Once);
                                }
                            });
                        }
                    });
                });
                match answer {
                    Some(a) => {
                        let _ = reply.send(a);
                    }
                    None => self.asks.push_front(Ask::HostKey {
                        host,
                        port,
                        key_type,
                        fingerprint,
                        changed,
                        reply,
                    }),
                }
            }
            Ask::Password { title, kind, reply } => {
                if self.vault_ui.fields.is_empty() {
                    self.vault_ui.fields = vec![Zeroizing::new(String::new())];
                    self.vault_ui.save = false;
                }
                let mut done: Option<Option<(Zeroizing<String>, bool)>> = None;
                let prompt = match &kind {
                    PromptKind::Password(uh) => self.i18n.t_args("prompt-password", &[("target", uh)]),
                    PromptKind::Passphrase(p) => self.i18n.t_args("prompt-passphrase", &[("path", p)]),
                };
                egui::Modal::new("password".into()).frame(frame).show(ctx, |ui| {
                    crate::style::fit_width(ui, 440.0);
                    ui.label(RichText::new(&title).heading());
                    ui.add_space(4.0);
                    ui.label(&prompt);
                    ui.add_space(6.0);
                    let edit = ui.add(
                        crate::style::field(&mut *self.vault_ui.fields[0])
                            .password(true)
                            .desired_width(f32::INFINITY),
                    );
                    edit.request_focus();
                    let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                    ui.checkbox(&mut self.vault_ui.save, t("prompt-save-vault"));
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("settings-cancel")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            done = Some(None);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if primary(ui, theme, t("prompt-ok")).clicked() || enter {
                                done = Some(Some((self.vault_ui.fields[0].clone(), self.vault_ui.save)));
                            }
                        });
                    });
                });
                match done {
                    Some(answer) => {
                        self.vault_ui.fields.clear();
                        let _ = reply.send(answer);
                    }
                    None => self.asks.push_front(Ask::Password { title, kind, reply }),
                }
            }
            Ask::KeyboardInteractive {
                title,
                instructions,
                prompts,
                reply,
            } => {
                if self.vault_ui.fields.len() != prompts.len() {
                    self.vault_ui.fields = prompts.iter().map(|_| Zeroizing::new(String::new())).collect();
                }
                let mut done: Option<Option<Vec<Zeroizing<String>>>> = None;
                egui::Modal::new("kbd-int".into()).frame(frame).show(ctx, |ui| {
                    crate::style::fit_width(ui, 440.0);
                    ui.label(RichText::new(&title).heading());
                    if !instructions.is_empty() {
                        ui.label(&instructions);
                    }
                    ui.add_space(6.0);
                    let mut enter = false;
                    for (i, (prompt, echo)) in prompts.iter().enumerate() {
                        ui.label(prompt);
                        let edit = ui.add(
                            crate::style::field(&mut *self.vault_ui.fields[i])
                                .password(!echo)
                                .desired_width(f32::INFINITY),
                        );
                        if i == 0 && !edit.has_focus() && self.vault_ui.fields[0].is_empty() {
                            edit.request_focus();
                        }
                        enter |= ui.input(|i| i.key_pressed(egui::Key::Enter));
                    }
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("settings-cancel")).clicked() {
                            done = Some(None);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if primary(ui, theme, t("prompt-ok")).clicked() || enter {
                                done = Some(Some(self.vault_ui.fields.clone()));
                            }
                        });
                    });
                });
                match done {
                    Some(answer) => {
                        self.vault_ui.fields.clear();
                        let _ = reply.send(answer);
                    }
                    None => self.asks.push_front(Ask::KeyboardInteractive {
                        title,
                        instructions,
                        prompts,
                        reply,
                    }),
                }
            }
            other => self.asks.push_front(other),
        }
    }

    /// Unlock / create vault dialogs. Returns true while one is shown.
    fn vault_dialogs(&mut self, ctx: &egui::Context, theme: &Theme) -> bool {
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(22);
        if let Some(mut u) = self.vault_ui.unlock.take() {
            if let Some(r) = u.busy.as_ref().and_then(|rx| rx.try_recv().ok()) {
                u.busy = None;
                match r {
                    Ok(key) => {
                        self.vault_opened(Vault::unlocked(key));
                        return false;
                    }
                    Err(_) => u.error = true,
                }
            }
            let mut cont_locked = false;
            let count = Vault::count(&self.db).to_string();
            let minutes = self.settings.vault_lock_minutes.to_string();
            let i18n = &self.i18n;
            egui::Modal::new("unlock-vault".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 520.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Lock, c(theme.ui.accent));
                    ui.label(RichText::new(i18n.t("vault-unlock-title")).heading());
                });
                ui.add_space(6.0);
                ui.label(i18n.t_args("vault-unlock-body", &[("n", &count)]));
                ui.add_space(8.0);
                ui.label(i18n.t("vault-master"));
                let edit = ui.add_enabled(
                    u.busy.is_none(),
                    crate::style::field(&mut *u.password)
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                if u.busy.is_none() {
                    edit.request_focus();
                }
                let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                if u.error {
                    ui.label(RichText::new(i18n.t("vault-wrong")).color(c(theme.ui.danger)));
                }
                let note = if self.settings.vault_auto_lock {
                    i18n.t_args("vault-autolock-note", &[("n", &minutes)])
                } else {
                    i18n.t("vault-autolock-never-note")
                };
                ui.label(RichText::new(note).small().weak());
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.link(i18n.t("vault-continue-locked")).clicked() {
                        cont_locked = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if u.busy.is_some() {
                            ui.spinner();
                        } else if (primary(ui, theme, i18n.t("vault-unlock")).clicked() || enter)
                            && !u.password.is_empty()
                        {
                            match vault::Unlocker::load(&self.db) {
                                Ok(unlocker) => {
                                    let (tx, rx) = mpsc::channel();
                                    let pw = u.password.clone();
                                    let wake = ui.ctx().clone();
                                    std::thread::spawn(move || {
                                        let _ = tx.send(unlocker.unlock(&pw));
                                        wake.request_repaint();
                                    });
                                    u.busy = Some(rx);
                                    u.error = false;
                                }
                                Err(e) => tracing::error!("vault header: {e}"),
                            }
                        }
                    });
                });
            });
            if cont_locked {
                self.vault_ui.continue_locked = true;
                for (_, reply) in std::mem::take(&mut self.vault_ui.waiting) {
                    let _ = reply.send(None);
                }
                self.vault_ui.to_store.clear();
                return false;
            }
            self.vault_ui.unlock = Some(u);
            return true;
        }
        if let Some(mut cr) = self.vault_ui.create.take() {
            if let Some(r) = cr.busy.as_ref().and_then(|rx| rx.try_recv().ok()) {
                cr.busy = None;
                match r.and_then(|(key, salt)| Vault::create(&self.db, key, &salt)) {
                    Ok(v) => {
                        self.vault_opened(v);
                        return false;
                    }
                    Err(e) => cr.error = Some(e.to_string()),
                }
            }
            let mut cancel = false;
            let i18n = &self.i18n;
            egui::Modal::new("create-vault".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 520.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Lock, c(theme.ui.accent));
                    ui.label(RichText::new(i18n.t("vault-create-title")).heading());
                });
                ui.add_space(6.0);
                ui.label(i18n.t("vault-create-body"));
                ui.label(RichText::new(i18n.t("vault-no-recovery")).color(c(theme.ui.warning)));
                ui.add_space(8.0);
                ui.label(i18n.t("vault-master"));
                ui.add(
                    crate::style::field(&mut *cr.pw1)
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                ui.label(i18n.t("vault-repeat"));
                ui.add(
                    crate::style::field(&mut *cr.pw2)
                        .password(true)
                        .desired_width(f32::INFINITY),
                );
                if let Some(e) = &cr.error {
                    ui.label(RichText::new(e).color(c(theme.ui.danger)));
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.button(i18n.t("settings-cancel")).clicked() {
                        cancel = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if cr.busy.is_some() {
                            ui.spinner();
                        } else if primary(ui, theme, i18n.t("vault-create")).clicked()
                            || ui.input(|i| i.key_pressed(egui::Key::Enter))
                        {
                            if cr.pw1.len() < 8 {
                                cr.error = Some(i18n.t("vault-too-short"));
                            } else if *cr.pw1 != *cr.pw2 {
                                cr.error = Some(i18n.t("vault-mismatch"));
                            } else {
                                let (tx, rx) = mpsc::channel();
                                let pw = cr.pw1.clone();
                                let wake = ui.ctx().clone();
                                std::thread::spawn(move || {
                                    let _ = tx.send(vault::new_key(&pw));
                                    wake.request_repaint();
                                });
                                cr.busy = Some(rx);
                            }
                        }
                    });
                });
            });
            if cancel {
                self.vault_ui.to_store.clear();
                return false;
            }
            self.vault_ui.create = Some(cr);
            return true;
        }
        false
    }
}
