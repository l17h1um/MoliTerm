//! Sessions panel (SPEC §4.2): folder tree with colors and counts, filter,
//! drag & drop, context menus, the live `~/.ssh/config` folder and local shells.
//! Also turns a saved session into a running tab.

use std::sync::{Arc, Mutex, mpsc};

use egui::{Color32, RichText, Sense, Stroke, vec2};
use moliterm_core::{
    identity::{self, Identity},
    known_hosts, rdp, serial,
    sessions::{self, Common, Folder, Protocol, Session},
    ssh::{self, AgentKind, AuthMethod, Hop, Notices},
    sshconfig::{self, HostEntry},
    telnet, terminal,
    theme::Theme,
};

use crate::{
    app::{Action, App},
    desk_view::DeskTab,
    dialogs::Dialog,
    dock::Tab,
    editor::Editor,
    icons::{self, Icon},
    layout::Place,
    style::c,
    term_view::{self, Launch, TermTab},
};

/// Polls `~/.ssh/config` every 3 s and sends the parsed hosts when it changes.
pub fn watch_ssh_config(rt: &tokio::runtime::Handle, ctx: egui::Context) -> mpsc::Receiver<Vec<HostEntry>> {
    let (tx, rx) = mpsc::channel();
    rt.spawn(async move {
        let Some(path) = sshconfig::default_path() else {
            return;
        };
        let mut last = None;
        loop {
            // ponytail: only the main file's mtime is watched; edits to Included files show up after the next main-file save.
            let stamp = tokio::fs::metadata(&path).await.and_then(|m| m.modified()).ok();
            if stamp != last {
                last = stamp;
                let p = path.clone();
                let hosts = tokio::task::spawn_blocking(move || sshconfig::load(&p))
                    .await
                    .unwrap_or_default();
                if tx.send(hosts).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    });
    rx
}

pub fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

pub const SSHCFG_PREFIX: &str = "sshcfg:";

/// A read-only session built from a `~/.ssh/config` host.
pub fn from_ssh_config(h: &HostEntry) -> Session {
    let key = h.identity_files.first().cloned();
    Session {
        id: format!("{SSHCFG_PREFIX}{}", h.alias),
        name: h.alias.clone(),
        protocol: Protocol::Ssh,
        host: h.hostname.clone().unwrap_or_else(|| h.alias.clone()),
        port: h.port.unwrap_or(22),
        common: Common {
            username: h.user.clone(),
            auth: Some(if key.is_some() {
                AuthMethod::Key
            } else {
                AuthMethod::Agent
            }),
            key_path: key,
            agent: Some(AgentKind::System),
            jump: h.proxy_jump.clone(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Drag & drop payload.
#[derive(Clone, PartialEq, Eq, Debug)]
enum Item {
    Folder(String),
    Session(String),
}

/// Opens a file in the OS text editor (macOS TextEdit or default, Linux xdg-open, Windows notepad).
pub fn open_in_editor(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-t").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("notepad").arg(path).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = r {
        tracing::warn!("open editor: {e}");
    }
}

fn badge_color(p: Protocol, theme: &Theme) -> Color32 {
    match p {
        Protocol::Ssh => c(theme.ui.success),
        Protocol::Rdp => c(theme.term.ansi[12]),
        Protocol::Telnet | Protocol::Raw => c(theme.ui.warning),
        Protocol::Serial => c(theme.term.ansi[13]),
        Protocol::Local => c(theme.ui.muted),
    }
}

pub fn parse_color(s: Option<&str>) -> Option<Color32> {
    let s = s?;
    let rgb = moliterm_core::theme::Rgb::try_from(s.to_owned()).ok()?;
    Some(c(rgb))
}

impl App {
    pub(crate) fn reload_sessions(&mut self) {
        match sessions::load(&self.db) {
            Ok((f, s)) => {
                self.folders = f;
                self.sessions = s;
            }
            Err(e) => tracing::error!("loading sessions: {e}"),
        }
    }

    /// Saved session, or a virtual one from `~/.ssh/config`.
    pub(crate) fn find_session(&self, id: &str) -> Option<Session> {
        if let Some(alias) = id.strip_prefix(SSHCFG_PREFIX) {
            return self.sshcfg.iter().find(|h| h.alias == alias).map(from_ssh_config);
        }
        self.sessions.iter().find(|s| s.id == id).cloned()
    }

    fn session_by_name(&self, name: &str) -> Option<Session> {
        self.sessions
            .iter()
            .find(|s| s.name == name)
            .cloned()
            .or_else(|| self.sshcfg.iter().find(|h| h.alias == name).map(from_ssh_config))
    }

    /// User name and vault prefix for a session: its identity (SPEC-9 §9.1)
    /// unless the session sets its own user name.
    pub(crate) fn login(&self, s: &Session, common: &Common) -> (Option<String>, String, Option<&Identity>) {
        let ident = common
            .identity
            .as_deref()
            .and_then(|id| self.identities.iter().find(|i| i.id == id));
        let own = s.common.username.clone().filter(|u| !u.is_empty());
        match ident {
            Some(i) => (
                own.or(Some(i.username.clone()).filter(|u| !u.is_empty()))
                    .or(common.username.clone()),
                i.secret_prefix(),
                Some(i),
            ),
            None => (common.username.clone(), format!("session:{}", s.id), None),
        }
    }

    fn hop(&self, s: &Session, common: &Common) -> Hop {
        let (user, secret_id, ident) = self.login(s, common);
        if let Some(i) = ident {
            let key = i.kind == identity::Kind::Key;
            return Hop {
                label: s.name.clone(),
                host: s.host.clone(),
                port: if s.port == 0 { 22 } else { s.port },
                user: user.filter(|u| !u.is_empty()).unwrap_or_else(local_user),
                auth: if key { AuthMethod::Key } else { AuthMethod::Password },
                key_path: key.then(|| identity::key_ref(&i.id)),
                agent: common.agent.unwrap_or_default(),
                secret_id,
            };
        }
        Hop {
            label: s.name.clone(),
            host: s.host.clone(),
            port: if s.port == 0 { 22 } else { s.port },
            user: common
                .username
                .clone()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(local_user),
            auth: common.auth.unwrap_or_default(),
            key_path: common.key_path.clone().filter(|k| !k.is_empty()),
            agent: common.agent.unwrap_or_default(),
            secret_id: format!("session:{}", s.id),
        }
    }

    /// `jump` = comma-separated saved session names, `~/.ssh/config` aliases or `[user@]host[:port]`.
    fn jump_hops(&self, spec: &str, target: &Hop, depth: u8) -> Vec<Hop> {
        let mut out = vec![];
        for item in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(s) = self.session_by_name(item) {
                let r = sessions::resolve(&s.common, &self.folders, s.folder.as_deref()).common;
                if depth < 4
                    && let Some(j) = &r.jump
                {
                    out.extend(self.jump_hops(j, target, depth + 1));
                }
                out.push(self.hop(&s, &r));
                continue;
            }
            let (user, rest) = match item.split_once('@') {
                Some((u, r)) => (u.to_owned(), r),
                None => (target.user.clone(), item),
            };
            let (host, port) = match rest.rsplit_once(':') {
                Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_owned(), p.parse().unwrap_or(22)),
                _ => (rest.to_owned(), 22),
            };
            out.push(Hop {
                label: item.to_owned(),
                secret_id: format!("jump:{host}"),
                host,
                port,
                user,
                ..target.clone()
            });
        }
        out
    }

    pub(crate) fn ssh_options(&self, s: &Session) -> ssh::Options {
        let r = sessions::resolve(&s.common, &self.folders, s.folder.as_deref()).common;
        let target = self.hop(s, &r);
        let jumps = r
            .jump
            .as_deref()
            .map(|j| self.jump_hops(j, &target, 0))
            .unwrap_or_default();
        ssh::Options {
            jumps,
            proxy: r.proxy.clone(),
            keepalive: r.keepalive.unwrap_or(true),
            reconnect: r.reconnect.unwrap_or(true),
            legacy: r.legacy.unwrap_or(false),
            after_login: r.after_login.clone(),
            x11: r
                .x11
                .unwrap_or(false)
                .then(|| (self.settings.x11_display.clone(), moliterm_core::x11::fake_cookie())),
            lang: match r.locale.as_deref().map(str::trim) {
                Some("off") => None,
                Some(l) if !l.is_empty() => Some(l.to_owned()),
                _ => Some(String::new()),
            },
            agent_forward: r.agent_forward.unwrap_or(false),
            known_hosts: known_hosts::default_path().unwrap_or_default(),
            notices: Notices {
                reconnected: self.i18n.t_args("ssh-reconnected", &[("time", "{time}")]),
                retrying: self.i18n.t_args(
                    "ssh-retrying",
                    &[("reason", "{reason}"), ("n", "{n}"), ("secs", "{secs}")],
                ),
                lang_refused: self.i18n.t_args("ssh-lang-refused", &[("lang", "{lang}")]),
            },
            target,
        }
    }

    pub(crate) fn open_session(&mut self, ctx: &egui::Context, id: &str) {
        self.open_session_at(ctx, id, Place::Focused);
    }

    /// Opens a saved session; returns the new tab id.
    pub(crate) fn open_session_at(&mut self, ctx: &egui::Context, id: &str, place: Place) -> Option<u64> {
        let s = self.find_session(id)?;
        if s.protocol == Protocol::Local {
            return self.open_local_at(ctx, None, place);
        }
        let tab = self.make_session_tab(ctx, s)?;
        let id = tab.id();
        self.place(tab, place);
        id
    }

    /// Starts a tab for a saved SSH / Telnet / raw / serial / RDP session.
    pub(crate) fn make_session_tab(&mut self, ctx: &egui::Context, s: Session) -> Option<Tab> {
        let r = sessions::resolve(&s.common, &self.folders, s.folder.as_deref()).common;
        let (user, secret_id, _) = self.login(&s, &r);
        if s.protocol == Protocol::Rdp {
            let ex: rdp::Extra = s.extra();
            let wake = ctx.clone();
            let (handle, link) = rdp::pipeline(Arc::new(move || wake.request_repaint()));
            let opts = rdp::Options {
                host: s.host.trim().to_owned(),
                port: s.port,
                user: user.unwrap_or_default(),
                domain: ex.domain,
                secret_id,
                known_hosts: moliterm_core::paths::data_dir().join("rdp_known_hosts"),
                size: (1280, 800),
            };
            let mut d = DeskTab::new(self.next_tab_id(), s.name.clone(), handle, link, opts, ex.scale);
            d.color = parse_color(r.color.as_deref());
            d.session_id = Some(s.id.clone());
            return Some(Tab::Desk(Box::new(d)));
        }
        let (proto, launch) = match s.protocol {
            Protocol::Ssh => (
                "SSH",
                Launch::Ssh {
                    options: Box::new(self.ssh_options(&s)),
                    live: Arc::new(Mutex::new(None)),
                },
            ),
            Protocol::Telnet | Protocol::Raw => {
                let ex: telnet::Extra = s.extra();
                let raw = s.protocol == Protocol::Raw;
                let login = ex.auto_login.then(|| telnet::AutoLogin {
                    user: user.clone().unwrap_or_default(),
                    secret_id: secret_id.clone(),
                    login_re: ex.login_prompt,
                    password_re: ex.password_prompt,
                });
                (
                    if raw { "RAW" } else { "TEL" },
                    Launch::Telnet(Box::new(telnet::Options {
                        host: s.host.trim().to_owned(),
                        port: s.port,
                        raw,
                        login,
                    })),
                )
            }
            Protocol::Serial => (
                "SER",
                Launch::Serial {
                    path: s.host.trim().to_owned(),
                    extra: s.extra(),
                },
            ),
            Protocol::Rdp | Protocol::Local => return None,
        };
        let wake = ctx.clone();
        let log = if r.log.unwrap_or(self.settings.log_sessions) {
            self.session_log(&s.name)
        } else {
            None
        };
        let (handle, wire) = terminal::pipeline(
            term_view::INITIAL_SIZE,
            self.settings.scrollback,
            self.palette.clone(),
            log,
            terminal::encoding(r.encoding.as_deref()),
            Arc::new(move || wake.request_repaint()),
        );
        let id = self.next_tab_id();
        let mut tab = TermTab::new(id, s.name.clone(), proto, handle, wire, launch);
        tab.color = parse_color(r.color.as_deref());
        tab.highlight = r.highlight.unwrap_or(true);
        tab.session_id = Some(s.id.clone());
        start(&self.rt, &self.ssh_ctx, &tab);
        if let Launch::Ssh { live, .. } = &tab.launch {
            if r.monitor.unwrap_or(true) && self.settings.monitor_enabled {
                let shared = Arc::new(moliterm_core::monitor::Shared::default());
                self.rt.spawn(moliterm_core::monitor::run(live.clone(), shared.clone()));
                tab.monitor = Some(shared);
            }
            self.tunnels_for_session(&s.id);
        }
        Some(Tab::Term(Box::new(tab)))
    }

    // ---- panel ----------------------------------------------------------

    pub(crate) fn sessions_panel(&mut self, ui: &mut egui::Ui, theme: &Theme, out: &mut Vec<Action>) {
        let muted = ui.visuals().weak_text_color();
        ui.horizontal(|ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new(self.i18n.t("panel-sessions"))
                    .small()
                    .strong()
                    .color(muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icons::button(ui, Icon::FolderPlus, 14.0, &self.i18n.t("tree-new-folder")).clicked() {
                    self.editor = Some(Editor::new_folder(None));
                }
                if icons::button(ui, Icon::Plus, 14.0, &self.i18n.t("tree-new-session")).clicked() {
                    out.push(Action::NewSession);
                }
            });
        });
        ui.add(
            crate::style::field(&mut self.filter)
                .hint_text(self.i18n.t("panel-filter-hint"))
                .desired_width(f32::INFINITY),
        );
        ui.add_space(6.0);
        let mut ev = TreeEvents::default();
        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            if self.filter.trim().is_empty() {
                self.folder_children(ui, None, theme, &mut ev);
                self.ssh_config_folder(ui, theme, &mut ev);
                self.local_shells_folder(ui, theme, out);
                // Drop on empty space → move to the root.
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
                if resp.dnd_hover_payload::<Item>().is_some() {
                    ui.painter()
                        .hline(rect.x_range(), rect.top(), Stroke::new(2.0, c(theme.ui.accent)));
                }
                if let Some(p) = resp.dnd_release_payload::<Item>() {
                    ev.drop = Some(((*p).clone(), None, None));
                }
            } else {
                let q = self.filter.trim().to_owned();
                let mut found: Vec<Session> = self
                    .sessions
                    .iter()
                    .filter(|s| sessions::matches(s, &self.folders, &q))
                    .cloned()
                    .collect();
                found.extend(
                    self.sshcfg
                        .iter()
                        .map(from_ssh_config)
                        .filter(|s| sessions::matches(s, &self.folders, &q)),
                );
                for s in &found {
                    self.session_row(ui, s, theme, 0.0, &mut ev);
                }
                if found.is_empty() {
                    ui.label(RichText::new(self.i18n.t("tree-no-match")).color(muted));
                }
            }
        });
        self.apply_tree_events(ui.ctx(), ev);
    }

    fn folder_children(&mut self, ui: &mut egui::Ui, parent: Option<&str>, theme: &Theme, ev: &mut TreeEvents) {
        let subfolders: Vec<Folder> = self
            .folders
            .iter()
            .filter(|f| f.parent.as_deref() == parent)
            .cloned()
            .collect();
        for f in subfolders {
            self.folder_row(ui, &f, theme, ev);
        }
        let list: Vec<Session> = self
            .sessions
            .iter()
            .filter(|s| s.folder.as_deref() == parent)
            .cloned()
            .collect();
        for s in &list {
            self.session_row(ui, s, theme, 0.0, ev);
        }
    }

    fn count_in(&self, folder: &str) -> usize {
        let direct = self
            .sessions
            .iter()
            .filter(|s| s.folder.as_deref() == Some(folder))
            .count();
        direct
            + self
                .folders
                .iter()
                .filter(|f| f.parent.as_deref() == Some(folder))
                .map(|f| self.count_in(&f.id))
                .sum::<usize>()
    }

    fn folder_row(&mut self, ui: &mut egui::Ui, f: &Folder, theme: &Theme, ev: &mut TreeEvents) {
        let color = sessions::resolve(&f.common, &self.folders, f.parent.as_deref())
            .common
            .color;
        let id = ui.make_persistent_id(("folder", &f.id));
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, true);
        let count = self.count_in(&f.id);
        let (_, header, _) = state
            .show_header(ui, |ui| {
                let dot = parse_color(color.as_deref()).unwrap_or(c(theme.ui.muted));
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, dot);
                let resp = ui.add(
                    egui::Label::new(&f.name)
                        .sense(Sense::click_and_drag())
                        .selectable(false),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(count.to_string())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
                resp
            })
            .body(|ui| self.folder_children(ui, Some(&f.id), theme, ev));
        let resp = header.inner;
        self.drag_and_drop(
            ui,
            &resp,
            Item::Folder(f.id.clone()),
            Some(f.id.clone()),
            None,
            theme,
            ev,
        );
        if resp.double_clicked() {
            ev.edit_folder = Some(f.id.clone());
        }
        resp.context_menu(|ui| {
            let t = |k: &str| self.i18n.t(k);
            if ui.button(t("tree-new-session-here")).clicked() {
                ev.new_session_in = Some(Some(f.id.clone()));
                ui.close();
            }
            if ui.button(t("tree-new-folder")).clicked() {
                ev.new_folder_in = Some(Some(f.id.clone()));
                ui.close();
            }
            if ui.button(t("tree-edit-folder")).clicked() {
                ev.edit_folder = Some(f.id.clone());
                ui.close();
            }
            if ui.button(t("tree-open-multi-exec")).clicked() {
                ev.multi_exec = Some(f.id.clone());
                ui.close();
            }
            ui.separator();
            if ui.button(t("tree-delete")).clicked() {
                ev.delete_folder = Some(f.id.clone());
                ui.close();
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn drag_and_drop(
        &self,
        ui: &egui::Ui,
        resp: &egui::Response,
        item: Item,
        into_folder: Option<String>,
        before: Option<String>,
        theme: &Theme,
        ev: &mut TreeEvents,
    ) {
        if resp.drag_started() {
            resp.dnd_set_drag_payload(item);
        }
        if resp.dnd_hover_payload::<Item>().is_some() {
            let y = if before.is_some() {
                resp.rect.top()
            } else {
                resp.rect.bottom()
            };
            ui.painter()
                .hline(resp.rect.x_range(), y, Stroke::new(2.0, c(theme.ui.accent)));
        }
        if let Some(p) = resp.dnd_release_payload::<Item>() {
            ev.drop = Some(((*p).clone(), Some(into_folder), before));
        }
    }

    fn session_row(&mut self, ui: &mut egui::Ui, s: &Session, theme: &Theme, indent: f32, ev: &mut TreeEvents) {
        let readonly = s.id.starts_with(SSHCFG_PREFIX);
        let selected = self.selected.contains(&s.id);
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click_and_drag());
        let v = ui.visuals();
        if selected {
            ui.painter().rect_filled(rect, 4.0, v.selection.bg_fill);
        } else if resp.hovered() {
            ui.painter().rect_filled(rect, 4.0, v.widgets.hovered.weak_bg_fill);
        }
        let fg = v.text_color();
        let icon = egui::Rect::from_min_size(rect.left_center() + vec2(indent + 6.0, -6.0), vec2(12.0, 12.0));
        let glyph = match s.protocol {
            Protocol::Rdp => Icon::Split,
            Protocol::Serial => Icon::Link,
            Protocol::Telnet | Protocol::Raw => Icon::Server,
            _ => Icon::Prompt,
        };
        icons::paint(ui.painter(), icon, glyph, v.weak_text_color());
        ui.painter().text(
            rect.left_center() + vec2(indent + 24.0, 0.0),
            egui::Align2::LEFT_CENTER,
            &s.name,
            egui::FontId::proportional(13.0),
            fg,
        );
        ui.painter().text(
            rect.right_center() - vec2(8.0, 0.0),
            egui::Align2::RIGHT_CENTER,
            s.protocol.tag(),
            egui::FontId::monospace(10.0),
            badge_color(s.protocol, theme),
        );
        let resp = resp.on_hover_text(format!("{}:{}", s.host, s.port));
        if !readonly {
            self.drag_and_drop(
                ui,
                &resp,
                Item::Session(s.id.clone()),
                s.folder.clone(),
                Some(s.id.clone()),
                theme,
                ev,
            );
        }
        if resp.clicked() {
            let multi = ui.input(|i| i.modifiers.command);
            if !multi {
                self.selected.clear();
            }
            if !self.selected.remove(&s.id) || !multi {
                self.selected.insert(s.id.clone());
            }
        }
        if resp.double_clicked() {
            ev.open = Some(s.id.clone());
        }
        resp.context_menu(|ui| {
            let t = |k: &str| self.i18n.t(k);
            if ui.button(t("tree-open")).clicked() {
                ev.open = Some(s.id.clone());
                ui.close();
            }
            if ui.button(t("tree-open-split")).clicked() {
                ev.open_at = Some((s.id.clone(), Place::Right));
                ui.close();
            }
            if ui.button(t("tree-open-window")).clicked() {
                ev.open_at = Some((s.id.clone(), Place::Window));
                ui.close();
            }
            ui.separator();
            if readonly {
                if ui.button(t("tree-edit-ssh-config")).clicked() {
                    if let Some(p) = sshconfig::default_path() {
                        open_in_editor(&p);
                    }
                    ui.close();
                }
                if ui.button(t("tree-copy-to-mine")).clicked() {
                    ev.copy_to_mine = Some(s.clone());
                    ui.close();
                }
            } else {
                if ui.button(t("tree-edit")).clicked() {
                    ev.edit = Some(s.id.clone());
                    ui.close();
                }
                if ui.button(t("tree-duplicate")).clicked() {
                    ev.duplicate = Some(s.id.clone());
                    ui.close();
                }
                if ui.button(t("tree-delete")).clicked() {
                    ev.delete = Some(s.id.clone());
                    ui.close();
                }
            }
            if ui.button(t("tree-copy-host")).clicked() {
                ui.ctx().copy_text(s.host.clone());
                ui.close();
            }
        });
    }

    fn ssh_config_folder(&mut self, ui: &mut egui::Ui, theme: &Theme, ev: &mut TreeEvents) {
        if !self.settings.ssh_config_folder || self.sshcfg.is_empty() {
            return;
        }
        let id = ui.make_persistent_id("sshcfg-folder");
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        let n = self.sshcfg.len();
        let list: Vec<Session> = self.sshcfg.iter().map(from_ssh_config).collect();
        state
            .show_header(ui, |ui| {
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, c(theme.ui.muted));
                ui.label("~/.ssh/config");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(n.to_string())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Lock, ui.visuals().weak_text_color());
                });
            })
            .body(|ui| {
                for s in &list {
                    self.session_row(ui, s, theme, 0.0, ev);
                }
            });
    }

    fn local_shells_folder(&mut self, ui: &mut egui::Ui, theme: &Theme, out: &mut Vec<Action>) {
        let id = ui.make_persistent_id("local-shells");
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        let n = self.shells.len();
        let title = self.i18n.t("folder-local-shells");
        state
            .show_header(ui, |ui| {
                let (r, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                ui.painter().rect_filled(r, 2.0, c(theme.ui.success));
                ui.label(title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(n.to_string())
                            .small()
                            .color(ui.visuals().weak_text_color()),
                    );
                });
            })
            .body(|ui| {
                for (i, sh) in self.shells.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), Sense::click());
                    if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
                    }
                    let icon = egui::Rect::from_min_size(rect.left_center() + vec2(6.0, -6.0), vec2(12.0, 12.0));
                    icons::paint(ui.painter(), icon, Icon::Prompt, ui.visuals().weak_text_color());
                    ui.painter().text(
                        rect.left_center() + vec2(24.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        &sh.name,
                        egui::FontId::proportional(13.0),
                        ui.visuals().text_color(),
                    );
                    ui.painter().text(
                        rect.right_center() - vec2(8.0, 0.0),
                        egui::Align2::RIGHT_CENTER,
                        "LOC",
                        egui::FontId::monospace(10.0),
                        c(theme.ui.muted),
                    );
                    if resp.on_hover_text(self.i18n.t("panel-open-hint")).double_clicked() {
                        out.push(Action::NewLocalShell(Some(i)));
                    }
                }
            });
    }

    fn apply_tree_events(&mut self, ctx: &egui::Context, ev: TreeEvents) {
        if let Some(id) = ev.open {
            self.open_session(ctx, &id);
        }
        if let Some((id, place)) = ev.open_at {
            self.open_session_at(ctx, &id, place);
        }
        if let Some(folder) = ev.multi_exec {
            self.multi_exec_folder(ctx, &folder);
        }
        if let Some(id) = ev.edit
            && let Some(s) = self.sessions.iter().find(|s| s.id == id)
        {
            self.editor = Some(Editor::edit_session(s.clone()));
        }
        if let Some(id) = ev.edit_folder
            && let Some(f) = self.folders.iter().find(|f| f.id == id)
        {
            self.editor = Some(Editor::edit_folder(f.clone()));
        }
        if let Some(parent) = ev.new_folder_in {
            self.editor = Some(Editor::new_folder(parent));
        }
        if let Some(folder) = ev.new_session_in {
            self.editor = Some(Editor::new_session(folder, &self.settings));
        }
        if let Some(id) = ev.duplicate
            && let Some(s) = self.sessions.iter().find(|s| s.id == id)
        {
            let mut copy = s.clone();
            copy.id = sessions::new_id();
            copy.name = self.i18n.t_args("tree-copy-name", &[("name", &s.name)]);
            self.save_session(&copy);
        }
        if let Some(mut s) = ev.copy_to_mine {
            s.id = sessions::new_id();
            self.save_session(&s);
        }
        if let Some(id) = ev.delete {
            if let Err(e) = sessions::delete_session(&self.db, &id) {
                tracing::error!("delete session: {e}");
            }
            self.reload_sessions();
        }
        if let Some(id) = ev.delete_folder
            && let Some(f) = self.folders.iter().find(|f| f.id == id)
        {
            self.dialog = Some(Dialog::DeleteFolder {
                id: id.clone(),
                name: f.name.clone(),
                count: self.count_in(&id),
            });
        }
        if let Some((item, into, before)) = ev.drop {
            self.move_item(item, into.flatten(), before);
        }
    }

    pub(crate) fn save_session(&mut self, s: &Session) {
        if let Err(e) = sessions::save_session(&self.db, s) {
            tracing::error!("save session: {e}");
        }
        self.reload_sessions();
    }

    pub(crate) fn save_folder(&mut self, f: &Folder) {
        if let Err(e) = sessions::save_folder(&self.db, f) {
            tracing::error!("save folder: {e}");
        }
        self.reload_sessions();
    }

    /// Moves a dragged session (or all selected ones) / folder into `folder`, before `before`.
    fn move_item(&mut self, item: Item, folder: Option<String>, before: Option<String>) {
        match item {
            Item::Folder(id) => {
                // Never into itself or a descendant.
                if folder.as_deref() == Some(&id)
                    || sessions::chain(&self.folders, folder.as_deref())
                        .iter()
                        .any(|f| f.id == id)
                {
                    return;
                }
                if let Some(mut f) = self.folders.iter().find(|f| f.id == id).cloned() {
                    f.parent = folder;
                    self.save_folder(&f);
                }
            }
            Item::Session(id) => {
                let mut moving: Vec<String> = if self.selected.contains(&id) {
                    self.selected.iter().cloned().collect()
                } else {
                    vec![id]
                };
                moving.retain(|m| !m.starts_with(SSHCFG_PREFIX));
                if before.as_ref().is_some_and(|b| moving.contains(b)) {
                    return;
                }
                let mut order: Vec<Session> = self
                    .sessions
                    .iter()
                    .filter(|s| s.folder == folder && !moving.contains(&s.id))
                    .cloned()
                    .collect();
                let at = before
                    .and_then(|b| order.iter().position(|s| s.id == b))
                    .unwrap_or(order.len());
                let moved: Vec<Session> = self
                    .sessions
                    .iter()
                    .filter(|s| moving.contains(&s.id))
                    .cloned()
                    .map(|mut s| {
                        s.folder = folder.clone();
                        s
                    })
                    .collect();
                order.splice(at..at, moved);
                for (i, s) in order.iter_mut().enumerate() {
                    s.sort = i as i64;
                    if let Err(e) = sessions::save_session(&self.db, s) {
                        tracing::error!("move session: {e}");
                    }
                }
                self.reload_sessions();
            }
        }
    }

    pub(crate) fn next_tab_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }
}

#[derive(Default)]
struct TreeEvents {
    open: Option<String>,
    open_at: Option<(String, Place)>,
    /// Folder id or tag for multi-exec.
    multi_exec: Option<String>,
    edit: Option<String>,
    edit_folder: Option<String>,
    duplicate: Option<String>,
    delete: Option<String>,
    delete_folder: Option<String>,
    copy_to_mine: Option<Session>,
    new_folder_in: Option<Option<String>>,
    new_session_in: Option<Option<String>>,
    /// (item, target folder (None = keep None/root), insert before session)
    drop: Option<(Item, Option<Option<String>>, Option<String>)>,
}

/// (Re)starts the transport of a session tab.
pub(crate) fn start(rt: &tokio::runtime::Handle, ctx: &ssh::Ctx, t: &TermTab) {
    match &t.launch {
        Launch::Local(_) => {}
        Launch::Ssh { options, live } => {
            rt.spawn(ssh::run(
                (**options).clone(),
                ctx.clone(),
                t.wire.clone(),
                t.size,
                live.clone(),
            ));
        }
        Launch::Telnet(o) => {
            t.wire.drain();
            rt.spawn(telnet::run((**o).clone(), ctx.clone(), t.wire.clone(), t.size));
        }
        Launch::Serial { path, extra } => serial::spawn(rt, path.clone(), extra.clone(), t.wire.clone()),
    }
}
