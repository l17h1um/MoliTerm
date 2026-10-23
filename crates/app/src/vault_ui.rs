//! Vault page (SPEC-9 §9.1): identities, saved passwords and known hosts.
//! Secrets are shown in clear only on request and only while unlocked.

use std::{
    collections::HashSet,
    sync::mpsc,
    time::{Duration, Instant},
};

use egui::{RichText, Stroke};
use moliterm_core::{
    identity::{self, Identity, Kind},
    known_hosts, sessions,
    theme::Theme,
    vault::{self, Vault},
};
use zeroize::Zeroizing;

use crate::{app::App, dialogs::primary, style::c};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Identities,
    Passwords,
    KnownHosts,
}

pub struct IdentityDraft {
    ident: Identity,
    password: Zeroizing<String>,
    key: Zeroizing<String>,
    passphrase: Zeroizing<String>,
    show: bool,
    error: Option<String>,
}

pub struct VaultPage {
    section: Section,
    search: String,
    /// Vault ids shown in clear text.
    revealed: HashSet<String>,
    draft: Option<IdentityDraft>,
    /// Saved password being changed: (vault id, new value).
    change: Option<(String, Zeroizing<String>)>,
    /// Second click deletes (id of the row waiting for it).
    confirm: Option<String>,
    key_pick: Option<mpsc::Receiver<std::path::PathBuf>>,
}

impl VaultPage {
    pub fn new() -> Self {
        Self {
            section: Section::Identities,
            search: String::new(),
            revealed: HashSet::new(),
            draft: None,
            change: None,
            confirm: None,
            key_pick: None,
        }
    }
}

/// Copied secrets are cleared from the clipboard after this, if still there.
const CLIP_TTL: Duration = Duration::from_secs(30);

impl App {
    /// Opens the Vault tab, or switches to it if it is already open.
    pub(crate) fn open_vault_page(&mut self) {
        if self.vault_page.is_none() {
            self.vault_page = Some(VaultPage::new());
        }
        match self.dock.find_tab_from(|t| matches!(t, crate::dock::Tab::Vault)) {
            Some(path) => {
                let _ = self.dock.set_active_tab(path);
                self.dock.set_focused_node_and_surface(egui_dock::NodePath {
                    surface: path.surface,
                    node: path.node,
                });
            }
            None => self.place(crate::dock::Tab::Vault, crate::layout::Place::Focused),
        }
    }

    fn secret(&self, id: &str) -> Option<Zeroizing<String>> {
        self.vault.get(&self.db, id).ok().flatten()
    }

    /// Copies a secret; it is wiped from the clipboard after 30 s if unchanged.
    fn copy_secret(&mut self, ctx: &egui::Context, text: &str) {
        ctx.copy_text(text.to_owned());
        self.clip_clear = Some((Instant::now() + CLIP_TTL, Zeroizing::new(text.to_owned())));
    }

    /// Runs every frame: clears a copied secret when its time is up.
    pub(crate) fn clear_clipboard_when_due(&mut self, ctx: &egui::Context) {
        let Some((due, text)) = &self.clip_clear else {
            return;
        };
        if Instant::now() < *due {
            ctx.request_repaint_after(due.saturating_duration_since(Instant::now()));
            return;
        }
        if let Ok(mut cb) = arboard::Clipboard::new()
            && cb.get_text().is_ok_and(|t| t == **text)
        {
            let _ = cb.set_text(String::new());
        }
        self.clip_clear = None;
    }

    /// Readable label of a vault entry id.
    fn entry_label(&self, id: &str) -> String {
        let t = |k: &str| self.i18n.t(k);
        let (base, kind) = id.rsplit_once(':').unwrap_or((id, ""));
        let what = match kind {
            "password" => t("vault-kind-password"),
            "passphrase" => t("vault-kind-passphrase"),
            k => k.to_owned(),
        };
        if let Some(sid) = base.strip_prefix("session:") {
            let name = self
                .sessions
                .iter()
                .find(|s| s.id == sid)
                .map_or_else(|| t("vault-deleted-session"), |s| s.name.clone());
            return format!("{name} · {what}");
        }
        if let Some(host) = base.strip_prefix("jump:") {
            return format!("{} {host} · {what}", t("vault-jump"));
        }
        if let Some(path) = base.strip_prefix("agentkey:") {
            let file = std::path::Path::new(path)
                .file_name()
                .map_or(path.to_owned(), |f| f.to_string_lossy().into_owned());
            return format!("{} {file} · {what}", t("vault-agent-key"));
        }
        id.to_owned()
    }

    fn usage(&self, ident: &str) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.common.identity.as_deref() == Some(ident))
            .count()
            + self
                .folders
                .iter()
                .filter(|f| f.common.identity.as_deref() == Some(ident))
                .count()
    }

    pub(crate) fn vault_screen(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let mut page = self.vault_page.take().unwrap_or_else(VaultPage::new);
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let unlocked = self.vault.is_unlocked();
        let exists = vault::exists(&self.db);

        if let Some(path) = page.key_pick.as_ref().and_then(|rx| rx.try_recv().ok()) {
            page.key_pick = None;
            if let Some(d) = &mut page.draft {
                match std::fs::read_to_string(&path) {
                    Ok(text) => *d.key = text,
                    Err(e) => d.error = Some(format!("{}: {e}", path.display())),
                }
            }
        }

        egui::Panel::top("vault-head")
            .frame(egui::Frame::new().fill(c(theme.ui.panel)).inner_margin(12))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                    crate::icons::paint(ui.painter(), r, crate::icons::Icon::Lock, c(theme.ui.accent));
                    ui.label(RichText::new(t("vault-page")).heading());
                    ui.add_space(12.0);
                    let status = if !exists {
                        t("vault-status-none")
                    } else if unlocked {
                        i18n.t_args("vault-status-unlocked", &[("n", &Vault::count(&self.db).to_string())])
                    } else {
                        t("vault-status-locked")
                    };
                    ui.label(RichText::new(status).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !exists {
                            if ui.button(t("vault-create")).clicked() {
                                self.request_create_vault();
                            }
                        } else if unlocked {
                            if ui.button(t("vault-lock-now")).clicked() {
                                self.lock_vault();
                            }
                        } else if primary(ui, theme, t("vault-unlock")).clicked() {
                            self.request_unlock();
                        }
                        ui.add(
                            crate::style::field(&mut page.search)
                                .hint_text(t("vault-search"))
                                .desired_width(200.0),
                        );
                    });
                });
            });

        let n_ident = self.identities.len();
        let entries: Vec<String> = Vault::ids(&self.db)
            .into_iter()
            .filter(|id| !id.starts_with("identity:"))
            .collect();
        let ssh_kh = known_hosts::default_path().unwrap_or_default();
        let rdp_kh = moliterm_core::paths::data_dir().join("rdp_known_hosts");
        let ssh_hosts = known_hosts::list(&ssh_kh);
        let rdp_text = std::fs::read_to_string(&rdp_kh).unwrap_or_default();
        let rdp_hosts: Vec<(usize, String, String)> = rdp_text
            .lines()
            .enumerate()
            .filter_map(|(i, l)| l.split_once(' ').map(|(h, f)| (i, h.to_owned(), f.trim().to_owned())))
            .collect();

        egui::Panel::left("vault-sections")
            .exact_size((ui.ctx().content_rect().width() * 0.3).clamp(150.0, 240.0))
            .resizable(false)
            .frame(egui::Frame::new().fill(c(theme.ui.bg)).inner_margin(10))
            .show(ui, |ui| {
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::Min), |ui| {
                    for (sec, key, n) in [
                        (Section::Identities, "vault-identities", n_ident),
                        (Section::Passwords, "vault-passwords", entries.len()),
                        (
                            Section::KnownHosts,
                            "vault-known-hosts",
                            ssh_hosts.len() + rdp_hosts.len(),
                        ),
                    ] {
                        let b = egui::Button::selectable(page.section == sec, format!("{}  ({n})", t(key)))
                            .frame_when_inactive(false)
                            .min_size(egui::vec2(0.0, 30.0));
                        if ui.add(b).clicked() {
                            page.section = sec;
                            page.confirm = None;
                        }
                    }
                });
            });

        let q = page.search.to_lowercase();
        let hit = |s: &str| q.is_empty() || s.to_lowercase().contains(&q);
        egui::CentralPanel::default_margins()
            .frame(egui::Frame::new().fill(c(theme.ui.bg)).inner_margin(20))
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| match page.section {
                        Section::Identities => self.identities_section(ui, theme, &mut page, unlocked, &hit),
                        Section::Passwords => {
                            ui.label(RichText::new(t("vault-passwords-hint")).weak());
                            ui.add_space(8.0);
                            if entries.is_empty() {
                                ui.label(RichText::new(t("vault-empty")).weak());
                            }
                            for id in entries.iter() {
                                let label = self.entry_label(id);
                                if !hit(&label) {
                                    continue;
                                }
                                self.password_row(ui, theme, &mut page, unlocked, id, &label);
                            }
                        }
                        Section::KnownHosts => {
                            ui.label(RichText::new(format!("SSH · {}", ssh_kh.display())).small().weak());
                            if ssh_hosts.is_empty() {
                                ui.label(RichText::new(t("vault-empty")).weak());
                            }
                            let mut remove = None;
                            for e in &ssh_hosts {
                                let host = e.hosts.clone().unwrap_or_else(|| t("vault-hashed"));
                                if !hit(&host) {
                                    continue;
                                }
                                let key = format!("ssh:{}", e.line);
                                card(ui, theme, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(RichText::new(&host).strong());
                                        ui.label(RichText::new(&e.key_type).small().weak());
                                        ui.label(RichText::new(&e.fingerprint).small().monospace());
                                        if delete_button(ui, &mut page.confirm, &key, &i18n) {
                                            remove = Some(e.line);
                                        }
                                    });
                                });
                            }
                            if let Some(line) = remove
                                && let Err(e) = known_hosts::remove_line(&ssh_kh, line)
                            {
                                tracing::warn!("known_hosts: {e}");
                            }
                            ui.add_space(12.0);
                            ui.label(RichText::new(format!("RDP · {}", rdp_kh.display())).small().weak());
                            if rdp_hosts.is_empty() {
                                ui.label(RichText::new(t("vault-empty")).weak());
                            }
                            let mut remove = None;
                            for (line, host, fp) in &rdp_hosts {
                                if !hit(host) {
                                    continue;
                                }
                                let key = format!("rdp:{line}");
                                card(ui, theme, |ui| {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(RichText::new(host).strong());
                                        ui.label(RichText::new("RDP TLS").small().weak());
                                        ui.label(RichText::new(fp).small().monospace());
                                        if delete_button(ui, &mut page.confirm, &key, &i18n) {
                                            remove = Some(*line);
                                        }
                                    });
                                });
                            }
                            if let Some(line) = remove
                                && let Err(e) = known_hosts::remove_line(&rdp_kh, line)
                            {
                                tracing::warn!("rdp known hosts: {e}");
                            }
                        }
                    });
            });
        self.vault_page = Some(page);
    }

    fn identities_section(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        page: &mut VaultPage,
        unlocked: bool,
        hit: &dyn Fn(&str) -> bool,
    ) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        ui.label(RichText::new(t("vault-identities-hint")).weak());
        ui.add_space(8.0);
        if page.draft.is_none()
            && ui
                .add_enabled(unlocked, egui::Button::new(t("vault-identity-new")))
                .on_disabled_hover_text(t("vault-need-unlock"))
                .clicked()
        {
            page.draft = Some(IdentityDraft {
                ident: Identity {
                    id: sessions::new_id(),
                    sort: self.identities.len() as i64,
                    ..Default::default()
                },
                password: Default::default(),
                key: Default::default(),
                passphrase: Default::default(),
                show: false,
                error: None,
            });
        }
        if page.draft.is_some() {
            self.identity_form(ui, theme, page);
        }
        ui.add_space(8.0);
        if self.identities.is_empty() {
            ui.label(RichText::new(t("vault-empty")).weak());
        }
        let list = self.identities.clone();
        for ident in list {
            if !hit(&ident.name) && !hit(&ident.username) {
                continue;
            }
            let prefix = ident.secret_prefix();
            let secret_id = match ident.kind {
                Kind::Password => format!("{prefix}:password"),
                Kind::Key => format!("{prefix}:key"),
            };
            let shown = page.revealed.contains(&secret_id);
            let mut edit = false;
            let mut delete = false;
            let mut copy = None;
            card(ui, theme, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&ident.name).strong());
                    ui.label(RichText::new(&ident.username).monospace());
                    let kind = match ident.kind {
                        Kind::Password => t("vault-kind-password"),
                        Kind::Key => t("vault-kind-key"),
                    };
                    ui.label(RichText::new(kind).small().color(c(theme.ui.accent)));
                    let n = self.usage(&ident.id);
                    ui.label(
                        RichText::new(i18n.t_args("vault-used-by", &[("n", &n.to_string())]))
                            .small()
                            .weak(),
                    );
                });
                if !ident.public_key.is_empty() {
                    ui.label(RichText::new(&ident.public_key).small().monospace().weak());
                }
                if shown && let Some(s) = self.secret(&secret_id) {
                    ui.label(RichText::new(s.as_str()).monospace());
                }
                ui.horizontal(|ui| {
                    let eye = if shown { t("vault-hide") } else { t("vault-show") };
                    if ui.add_enabled(unlocked, egui::Button::new(eye)).clicked() {
                        if shown {
                            page.revealed.remove(&secret_id);
                        } else {
                            page.revealed.insert(secret_id.clone());
                        }
                    }
                    if ui.add_enabled(unlocked, egui::Button::new(t("vault-copy"))).clicked() {
                        copy = self.secret(&secret_id);
                    }
                    if !ident.public_key.is_empty() && ui.button(t("vault-copy-public")).clicked() {
                        ui.ctx().copy_text(ident.public_key.clone());
                    }
                    if ui.add_enabled(unlocked, egui::Button::new(t("tree-edit"))).clicked() {
                        edit = true;
                    }
                    delete = delete_button(ui, &mut page.confirm, &ident.id, &i18n);
                });
            });
            if let Some(s) = copy {
                self.copy_secret(ui.ctx(), &s);
            }
            if edit {
                page.draft = Some(IdentityDraft {
                    password: self.secret(&format!("{prefix}:password")).unwrap_or_default(),
                    key: self.secret(&format!("{prefix}:key")).unwrap_or_default(),
                    passphrase: self.secret(&format!("{prefix}:passphrase")).unwrap_or_default(),
                    ident: ident.clone(),
                    show: false,
                    error: None,
                });
            }
            if delete {
                if let Err(e) = identity::delete(&self.db, &ident.id) {
                    tracing::error!("delete identity: {e}");
                }
                if unlocked {
                    for k in ["password", "key", "passphrase"] {
                        let _ = self.vault.delete(&self.db, &format!("{prefix}:{k}"));
                    }
                }
                self.identities = identity::load(&self.db).unwrap_or_default();
            }
        }
    }

    fn identity_form(&mut self, ui: &mut egui::Ui, theme: &Theme, page: &mut VaultPage) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let Some(d) = page.draft.as_mut() else {
            return;
        };
        let mut save = false;
        let mut cancel = false;
        egui::Frame::new()
            .stroke(Stroke::new(1.0, c(theme.ui.accent)))
            .corner_radius(6)
            .inner_margin(12)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::Grid::new("ident-form")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label(t("tunnel-name"));
                        ui.add(crate::style::field(&mut d.ident.name).desired_width(300.0));
                        ui.end_row();
                        ui.label(t("editor-username"));
                        ui.add(
                            crate::style::field(&mut d.ident.username)
                                .hint_text(t("editor-rdp-user-hint"))
                                .desired_width(300.0),
                        );
                        ui.end_row();
                        ui.label(t("vault-identity-kind"));
                        ui.horizontal(|ui| {
                            ui.radio_value(&mut d.ident.kind, Kind::Password, t("vault-kind-password"));
                            ui.radio_value(&mut d.ident.kind, Kind::Key, t("vault-kind-key"));
                        });
                        ui.end_row();
                        match d.ident.kind {
                            Kind::Password => {
                                ui.label(t("vault-kind-password"));
                                ui.horizontal(|ui| {
                                    ui.add(
                                        crate::style::field(&mut *d.password)
                                            .password(!d.show)
                                            .desired_width(240.0),
                                    );
                                    ui.checkbox(&mut d.show, t("vault-show"));
                                });
                                ui.end_row();
                            }
                            Kind::Key => {
                                ui.label(t("vault-kind-key"));
                                ui.horizontal(|ui| {
                                    if ui.button(t("vault-key-import")).clicked() {
                                        let (tx, rx) = mpsc::channel();
                                        let dialog = rfd::AsyncFileDialog::new().pick_file();
                                        let wake = ui.ctx().clone();
                                        self.rt.spawn(async move {
                                            if let Some(f) = dialog.await {
                                                let _ = tx.send(f.path().to_path_buf());
                                                wake.request_repaint();
                                            }
                                        });
                                        page.key_pick = Some(rx);
                                    }
                                    ui.checkbox(&mut d.show, t("vault-show"));
                                });
                                ui.end_row();
                                ui.label("");
                                if d.show {
                                    ui.add(
                                        egui::TextEdit::multiline(&mut *d.key)
                                            .font(egui::TextStyle::Monospace)
                                            .desired_rows(4)
                                            .desired_width(420.0)
                                            .hint_text(t("vault-key-paste")),
                                    );
                                } else {
                                    let n = d.key.lines().count();
                                    ui.label(
                                        RichText::new(i18n.t_args("vault-key-lines", &[("n", &n.to_string())])).weak(),
                                    );
                                }
                                ui.end_row();
                                ui.label(t("vault-kind-passphrase"));
                                ui.add(
                                    crate::style::field(&mut *d.passphrase)
                                        .password(!d.show)
                                        .desired_width(240.0),
                                );
                                ui.end_row();
                            }
                        }
                    });
                if let Some(e) = &d.error {
                    ui.label(RichText::new(e).color(c(theme.ui.danger)));
                }
                ui.horizontal(|ui| {
                    if primary(ui, theme, t("editor-save")).clicked() {
                        save = true;
                    }
                    if ui.button(t("settings-cancel")).clicked() {
                        cancel = true;
                    }
                });
            });
        if cancel {
            page.draft = None;
            return;
        }
        if !save {
            return;
        }
        let Some(mut d) = page.draft.take() else {
            return;
        };
        if d.ident.name.trim().is_empty() {
            d.error = Some(t("macros-err-name"));
            page.draft = Some(d);
            return;
        }
        if d.ident.kind == Kind::Key {
            let pass = Some(d.passphrase.as_str()).filter(|p| !p.is_empty());
            match identity::public_key_of(&d.key, pass) {
                Some(pk) => d.ident.public_key = pk,
                None => {
                    d.error = Some(t("vault-key-bad"));
                    page.draft = Some(d);
                    return;
                }
            }
        } else {
            d.ident.public_key.clear();
        }
        let prefix = d.ident.secret_prefix();
        let put = |k: &str, v: &str| {
            let id = format!("{prefix}:{k}");
            let r = if v.is_empty() {
                self.vault.delete(&self.db, &id)
            } else {
                self.vault.put(&self.db, &id, v)
            };
            if let Err(e) = r {
                tracing::error!("vault: {e}");
            }
        };
        match d.ident.kind {
            Kind::Password => {
                put("password", &d.password);
                put("key", "");
                put("passphrase", "");
            }
            Kind::Key => {
                put("key", &d.key);
                put("passphrase", &d.passphrase);
                put("password", "");
            }
        }
        if let Err(e) = identity::save(&self.db, &d.ident) {
            tracing::error!("save identity: {e}");
        }
        self.identities = identity::load(&self.db).unwrap_or_default();
    }

    fn password_row(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        page: &mut VaultPage,
        unlocked: bool,
        id: &str,
        label: &str,
    ) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let shown = page.revealed.contains(id);
        let (mut copy, mut delete, mut make, mut save_change) = (None, false, false, false);
        card(ui, theme, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(label).strong());
                ui.label(RichText::new(id).small().weak());
            });
            if shown && let Some(s) = self.secret(id) {
                ui.label(RichText::new(s.as_str()).monospace());
            }
            if let Some((cid, value)) = &mut page.change
                && cid == id
            {
                ui.horizontal(|ui| {
                    ui.add(crate::style::field(&mut **value).password(true).desired_width(240.0));
                    if ui.button(t("editor-save")).clicked() {
                        save_change = true;
                    }
                });
            }
            ui.horizontal(|ui| {
                let eye = if shown { t("vault-hide") } else { t("vault-show") };
                if ui.add_enabled(unlocked, egui::Button::new(eye)).clicked() {
                    if shown {
                        page.revealed.remove(id);
                    } else {
                        page.revealed.insert(id.to_owned());
                    }
                }
                if ui.add_enabled(unlocked, egui::Button::new(t("vault-copy"))).clicked() {
                    copy = self.secret(id);
                }
                if ui.add_enabled(unlocked, egui::Button::new(t("vault-change"))).clicked() {
                    page.change = Some((id.to_owned(), Zeroizing::new(String::new())));
                }
                if id.starts_with("session:")
                    && id.ends_with(":password")
                    && ui
                        .add_enabled(unlocked, egui::Button::new(t("vault-make-identity")))
                        .on_hover_text(t("vault-make-identity-hint"))
                        .clicked()
                {
                    make = true;
                }
                delete = delete_button(ui, &mut page.confirm, id, &i18n);
            });
        });
        if let Some(s) = copy {
            self.copy_secret(ui.ctx(), &s);
        }
        if save_change
            && let Some((cid, value)) = page.change.take()
            && let Err(e) = self.vault.put(&self.db, &cid, &value)
        {
            tracing::error!("vault: {e}");
        }
        if delete {
            let _ = self.vault.delete(&self.db, id);
        }
        if make {
            self.make_identity_from(id);
        }
    }

    /// Saved session password → new identity used by that session.
    fn make_identity_from(&mut self, id: &str) {
        let Some(sid) = id.strip_prefix("session:").and_then(|r| r.strip_suffix(":password")) else {
            return;
        };
        let (Some(secret), Some(mut s)) = (self.secret(id), self.find_session(sid)) else {
            return;
        };
        let r = sessions::resolve(&s.common, &self.folders, s.folder.as_deref()).common;
        let ident = Identity {
            id: sessions::new_id(),
            name: s.name.clone(),
            sort: self.identities.len() as i64,
            username: r.username.unwrap_or_default(),
            kind: Kind::Password,
            public_key: String::new(),
        };
        if self
            .vault
            .put(&self.db, &format!("{}:password", ident.secret_prefix()), &secret)
            .is_err()
            || identity::save(&self.db, &ident).is_err()
        {
            return;
        }
        let _ = self.vault.delete(&self.db, id);
        s.common.identity = Some(ident.id.clone());
        self.save_session(&s);
        self.identities = identity::load(&self.db).unwrap_or_default();
    }
}

impl App {
    /// "Type password from vault…" picker: this session's password first, then
    /// identities with a password. Typed without Enter.
    pub(crate) fn type_password_ui(&mut self, ctx: &egui::Context, theme: &Theme) {
        let Some(tab) = self.type_password else {
            return;
        };
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let session = self.term(tab).and_then(|x| x.session_id.clone());
        let mut options: Vec<(String, String)> = vec![];
        if let Some(sid) = &session {
            let id = format!("session:{sid}:password");
            if Vault::ids(&self.db).contains(&id) {
                options.push((t("type-pw-session"), id));
            }
        }
        for i in self.identities.iter().filter(|i| i.kind == Kind::Password) {
            options.push((
                format!("{} · {}", i.name, i.username),
                format!("{}:password", i.secret_prefix()),
            ));
        }
        let unlocked = self.vault.is_unlocked();
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(20);
        let mut pick = None;
        let mut close = false;
        let resp = egui::Modal::new("type-password".into()).frame(frame).show(ctx, |ui| {
            crate::style::fit_width(ui, 420.0);
            ui.heading(t("term-type-password"));
            ui.label(RichText::new(t("type-pw-hint")).small().weak());
            ui.add_space(8.0);
            if !unlocked {
                if primary(ui, theme, t("vault-unlock")).clicked() {
                    self.request_unlock();
                }
            } else if options.is_empty() {
                ui.label(RichText::new(t("type-pw-none")).weak());
            }
            for (label, id) in &options {
                if ui.add_enabled(unlocked, egui::Button::new(label)).clicked() {
                    pick = Some(id.clone());
                }
            }
            ui.add_space(8.0);
            if ui.button(t("settings-cancel")).clicked() {
                close = true;
            }
        });
        if let Some(id) = pick
            && let Some(secret) = self.secret(&id)
            && let Some(term) = self.term(tab)
        {
            term.send_direct(secret.as_bytes().to_vec());
            close = true;
        }
        if close || resp.should_close() {
            self.type_password = None;
        }
    }
}

fn card(ui: &mut egui::Ui, theme: &Theme, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .stroke(Stroke::new(1.0, c(theme.ui.border)))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
    ui.add_space(6.0);
}

/// Delete button that asks for a second click ("Delete?") first.
fn delete_button(ui: &mut egui::Ui, confirm: &mut Option<String>, key: &str, i18n: &crate::i18n::I18n) -> bool {
    if confirm.as_deref() == Some(key) {
        let b = egui::Button::new(RichText::new(i18n.t("vault-delete-confirm")).color(ui.visuals().error_fg_color));
        if ui.add(b).clicked() {
            *confirm = None;
            return true;
        }
    } else if ui.button(i18n.t("tree-delete")).clicked() {
        *confirm = Some(key.to_owned());
    }
    false
}
