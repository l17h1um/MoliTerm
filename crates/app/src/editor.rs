//! Session / folder editor (SPEC §4.3, mockup "Edit session").

use std::sync::mpsc;

use egui::{RichText, Stroke, vec2};
use moliterm_core::{
    serial,
    sessions::{self, Common, Folder, Protocol, Session},
    settings::Settings,
    ssh::{self, AuthMethod},
    telnet,
    theme::Theme,
};

use crate::{
    app::App,
    dialogs::primary,
    icons::{self, Icon},
    sessions_ui::parse_color,
    style::c,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum SubTab {
    Advanced,
    Terminal,
    Tunnels,
    Sftp,
    Appearance,
}

pub enum Target {
    Session(Session),
    Folder(Folder),
}

enum Test {
    Running(mpsc::Receiver<Result<String, String>>),
    Done(Result<String, String>),
}

pub struct Editor {
    target: Target,
    is_new: bool,
    tab: SubTab,
    tags: String,
    keep_in_agent: bool,
    test: Option<Test>,
    pick: Option<mpsc::Receiver<std::path::PathBuf>>,
    error: Option<String>,
    /// Detected serial ports, listed once per editor.
    ports: Option<Vec<String>>,
    /// From the Tunnels sub-tab: tunnels to save, "New tunnel" clicked.
    tunnel_req: (Vec<moliterm_core::tunnel::Tunnel>, bool),
}

/// Environment palette (SPEC §4.2): prod, staging, network, local.
pub const PALETTE: [&str; 4] = ["#f07178", "#e6b450", "#59c2ff", "#7fd962"];

impl Editor {
    fn new(target: Target, is_new: bool) -> Self {
        let tags = match &target {
            Target::Session(s) => s.tags.join(", "),
            Target::Folder(_) => String::new(),
        };
        Self {
            target,
            is_new,
            tab: SubTab::Advanced,
            tags,
            keep_in_agent: false,
            test: None,
            pick: None,
            error: None,
            ports: None,
            tunnel_req: Default::default(),
        }
    }

    pub fn new_session(folder: Option<String>, _settings: &Settings) -> Self {
        Self::new(
            Target::Session(Session {
                id: sessions::new_id(),
                folder,
                port: 22,
                sort: i64::MAX / 2,
                ..Default::default()
            }),
            true,
        )
    }

    /// A quick-connect session, to be saved as new.
    pub fn new_from(s: Session) -> Self {
        Self::new(Target::Session(s), true)
    }

    pub fn edit_session(mut s: Session) -> Self {
        // RDP: the old separate Domain field becomes `DOMAIN\user` (SPEC-9 §9.5).
        if s.protocol == Protocol::Rdp {
            let mut ex: moliterm_core::rdp::Extra = s.extra();
            let domain = ex.domain.trim().to_owned();
            // An inherited user name keeps the old field (still honoured when connecting).
            if !domain.is_empty()
                && let Some(u) = s.common.username.as_mut()
            {
                if !u.contains(['\\', '@']) {
                    *u = format!("{domain}\\{u}");
                }
                ex.domain.clear();
                s.set_extra(&ex);
            }
        }
        Self::new(Target::Session(s), false)
    }

    pub fn new_folder(parent: Option<String>) -> Self {
        Self::new(
            Target::Folder(Folder {
                id: sessions::new_id(),
                parent,
                ..Default::default()
            }),
            true,
        )
    }

    pub fn edit_folder(f: Folder) -> Self {
        Self::new(Target::Folder(f), false)
    }

    pub fn set_key_path(&mut self, path: String) {
        if let Target::Session(s) = &mut self.target {
            s.common.key_path = Some(path);
            s.common.auth = Some(AuthMethod::Key);
        }
    }
}

/// Empty strings mean "not set".
fn normalize(c: &mut Common) {
    for f in [
        &mut c.username,
        &mut c.key_path,
        &mut c.jump,
        &mut c.after_login,
        &mut c.encoding,
        &mut c.locale,
        &mut c.sftp_dir,
    ] {
        if f.as_deref().is_some_and(|v| v.trim().is_empty()) {
            *f = None;
        }
    }
}

fn telnet_fields(ui: &mut egui::Ui, s: &mut Session, t: &dyn Fn(&str) -> String, half: f32) {
    caption(ui, t("editor-telnet"));
    let mut raw = s.protocol == Protocol::Raw;
    if ui.checkbox(&mut raw, t("editor-raw")).changed() {
        s.protocol = if raw { Protocol::Raw } else { Protocol::Telnet };
    }
    if raw {
        return;
    }
    let mut ex: telnet::Extra = s.extra();
    let before = ex.clone();
    ui.checkbox(&mut ex.auto_login, t("editor-auto-login"))
        .on_hover_text(t("editor-auto-login-hint"));
    if ex.auto_login {
        egui::Grid::new("telnet")
            .num_columns(2)
            .min_col_width(half)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.label(t("editor-login-prompt"));
                ui.label(t("editor-password-prompt"));
                ui.end_row();
                for re in [&mut ex.login_prompt, &mut ex.password_prompt] {
                    let bad = regex::Regex::new(re).is_err();
                    let mut f = crate::style::field(re)
                        .desired_width(half)
                        .font(egui::TextStyle::Monospace);
                    if bad {
                        f = f.text_color(ui.visuals().error_fg_color);
                    }
                    ui.add(f);
                }
                ui.end_row();
            });
    }
    if ex != before {
        s.set_extra(&ex);
    }
}

fn serial_fields(ui: &mut egui::Ui, s: &mut Session, ports: &[String], t: &dyn Fn(&str) -> String, half: f32) {
    use serial::{Flow, LineEnding, Parity};
    caption(ui, t("editor-serial"));
    let mut ex: serial::Extra = s.extra();
    let before = ex.clone();
    egui::Grid::new("serial")
        .num_columns(2)
        .min_col_width(half)
        .spacing([24.0, 6.0])
        .show(ui, |ui| {
            ui.label(t("editor-serial-port"));
            ui.label(t("editor-baud"));
            ui.end_row();
            ui.horizontal(|ui| {
                ui.add(
                    crate::style::field(&mut s.host)
                        .hint_text(if cfg!(windows) { "COM3" } else { "/dev/ttyUSB0" })
                        .desired_width(half - 110.0)
                        .font(egui::TextStyle::Monospace),
                );
                egui::ComboBox::from_id_salt("ports")
                    .selected_text(t("editor-serial-pick"))
                    .width(100.0)
                    .show_ui(ui, |ui| {
                        if ports.is_empty() {
                            ui.label(RichText::new(t("editor-serial-none")).weak());
                        }
                        for p in ports {
                            if ui.selectable_label(&s.host == p, p).clicked() {
                                s.host = p.clone();
                            }
                        }
                    });
            });
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("baud")
                    .selected_text(ex.baud.to_string())
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for b in serial::BAUDS {
                            ui.selectable_value(&mut ex.baud, b, b.to_string());
                        }
                    });
                ui.add(egui::DragValue::new(&mut ex.baud).range(50..=4_000_000));
            });
            ui.end_row();
            ui.label(t("editor-frame"));
            ui.label(t("editor-flow"));
            ui.end_row();
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("bits")
                    .selected_text(ex.data_bits.to_string())
                    .width(50.0)
                    .show_ui(ui, |ui| {
                        for b in [5, 6, 7, 8] {
                            ui.selectable_value(&mut ex.data_bits, b, b.to_string());
                        }
                    });
                egui::ComboBox::from_id_salt("parity")
                    .selected_text(t(match ex.parity {
                        Parity::None => "parity-none",
                        Parity::Odd => "parity-odd",
                        Parity::Even => "parity-even",
                    }))
                    .width(90.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut ex.parity, Parity::None, t("parity-none"));
                        ui.selectable_value(&mut ex.parity, Parity::Odd, t("parity-odd"));
                        ui.selectable_value(&mut ex.parity, Parity::Even, t("parity-even"));
                    });
                egui::ComboBox::from_id_salt("stop")
                    .selected_text(ex.stop_bits.to_string())
                    .width(50.0)
                    .show_ui(ui, |ui| {
                        for b in [1, 2] {
                            ui.selectable_value(&mut ex.stop_bits, b, b.to_string());
                        }
                    });
                ui.label(RichText::new(ex.summary()).weak().monospace());
            });
            let flow = |f: Flow| match f {
                Flow::None => t("flow-none"),
                Flow::RtsCts => "RTS/CTS".into(),
                Flow::XonXoff => "XON/XOFF".into(),
            };
            egui::ComboBox::from_id_salt("flow")
                .selected_text(flow(ex.flow))
                .width(half)
                .show_ui(ui, |ui| {
                    for f in [Flow::None, Flow::RtsCts, Flow::XonXoff] {
                        ui.selectable_value(&mut ex.flow, f, flow(f));
                    }
                });
            ui.end_row();
            ui.label(t("editor-line-ending"));
            ui.end_row();
            ui.horizontal(|ui| {
                for (l, name) in [
                    (LineEnding::Cr, "CR"),
                    (LineEnding::Lf, "LF"),
                    (LineEnding::CrLf, "CR LF"),
                ] {
                    ui.radio_value(&mut ex.line_ending, l, name);
                }
            });
            ui.checkbox(&mut ex.local_echo, t("editor-local-echo"));
            ui.end_row();
        });
    if ex != before {
        s.set_extra(&ex);
    }
}

enum Outcome {
    Keep,
    Close,
    Save { connect: bool },
}

fn caption(ui: &mut egui::Ui, text: String) {
    ui.add_space(10.0);
    ui.label(
        RichText::new(text)
            .small()
            .strong()
            .color(ui.visuals().weak_text_color()),
    );
    ui.add_space(2.0);
}

/// A text field that may inherit from a folder: shows the inherited value with
/// "from <Folder>" and an Override button, or an editable own value.
fn inherit_text(
    ui: &mut egui::Ui,
    own: &mut Option<String>,
    inherited: Option<(String, String)>,
    t: &dyn Fn(&str) -> String,
    width: f32,
) {
    match (&own, inherited) {
        (None, Some((value, from))) => {
            egui::Frame::new()
                .stroke(Stroke::new(1.0, ui.visuals().widgets.inactive.bg_stroke.color))
                .corner_radius(4)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.set_width(width - 18.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&value).color(ui.visuals().weak_text_color()));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(t("editor-override")).clicked() {
                                *own = Some(value.clone());
                            }
                            ui.label(
                                RichText::new(format!("{} {from}", t("editor-from")))
                                    .small()
                                    .color(ui.visuals().weak_text_color()),
                            );
                        });
                    });
                });
        }
        (_, inherited) => {
            ui.horizontal(|ui| {
                let buf = own.get_or_insert_with(String::new);
                ui.add(crate::style::field(buf).desired_width(width - if inherited.is_some() { 80.0 } else { 0.0 }));
                if inherited.is_some() && ui.small_button(t("editor-inherit")).clicked() {
                    *own = None;
                }
            });
        }
    }
}

/// A checkbox over an inheritable bool; unset shows the inherited/default value.
/// Identity choice (SPEC-9 §9.1). `Some("")` = explicitly none (overrides a
/// folder's identity). Returns the effective identity id.
fn identity_picker(
    ui: &mut egui::Ui,
    own: &mut Option<String>,
    inherited: Option<(String, String)>,
    identities: &[moliterm_core::identity::Identity],
    t: &dyn Fn(&str) -> String,
) -> Option<String> {
    let effective = match own.as_deref() {
        Some("") => None,
        Some(id) => Some(id.to_owned()),
        None => inherited.as_ref().map(|(id, _)| id.clone()),
    };
    let label = |id: &str| {
        identities
            .iter()
            .find(|i| i.id == id)
            .map(|i| format!("{} · {}", i.name, i.username))
            .unwrap_or_else(|| t("identity-missing"))
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(t("editor-identity"));
        egui::ComboBox::from_id_salt("identity")
            .selected_text(effective.as_deref().map_or_else(|| t("identity-none"), label))
            .width(260.0)
            .show_ui(ui, |ui| {
                if ui.selectable_label(effective.is_none(), t("identity-none")).clicked() {
                    *own = if inherited.is_some() { Some(String::new()) } else { None };
                }
                for i in identities {
                    let text = format!("{} · {}", i.name, i.username);
                    if ui
                        .selectable_label(effective.as_deref() == Some(i.id.as_str()), text)
                        .clicked()
                    {
                        *own = Some(i.id.clone());
                    }
                }
            });
        if own.is_none()
            && let Some((_, from)) = &inherited
        {
            ui.label(RichText::new(format!("{} {from}", t("editor-from"))).small().weak());
        }
    });
    effective.filter(|id| identities.iter().any(|i| &i.id == id))
}

fn inherit_bool(ui: &mut egui::Ui, own: &mut Option<bool>, effective: bool, label: String) {
    let mut v = own.unwrap_or(effective);
    if ui.checkbox(&mut v, label).changed() {
        *own = Some(v);
    }
}

impl App {
    pub(crate) fn editor_ui(&mut self, ctx: &egui::Context, theme: &Theme) {
        let Some(mut ed) = self.editor.take() else {
            return;
        };
        if let Some(path) = ed.pick.as_ref().and_then(|rx| rx.try_recv().ok()) {
            ed.set_key_path(path.to_string_lossy().into_owned());
            ed.pick = None;
        }
        if let Some(Test::Running(rx)) = &ed.test
            && let Ok(r) = rx.try_recv()
        {
            ed.test = Some(Test::Done(r));
        }
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(24);
        let mut outcome = Outcome::Keep;
        let resp = egui::Modal::new("session-editor".into()).frame(frame).show(ctx, |ui| {
            crate::style::fit_width(ui, 820.0);
            outcome = match &mut ed.target {
                Target::Session(_) => self.session_form(ui, &mut ed, theme),
                Target::Folder(_) => self.folder_form(ui, &mut ed, theme),
            };
        });
        let (changed, new_tunnel) = std::mem::take(&mut ed.tunnel_req);
        for x in changed {
            self.save_tunnel(&x);
        }
        if new_tunnel && let Target::Session(s) = &ed.target {
            self.tunnels.editor = Some(crate::tunnels_ui::TunnelEditor::new(s.id.clone()));
        }
        if resp.should_close() && matches!(outcome, Outcome::Keep) {
            outcome = Outcome::Close;
        }
        match outcome {
            Outcome::Keep => self.editor = Some(ed),
            Outcome::Close => {}
            Outcome::Save { connect } => match ed.target {
                Target::Session(mut s) => {
                    normalize(&mut s.common);
                    s.tags = ed
                        .tags
                        .split(',')
                        .map(|t| t.trim().to_owned())
                        .filter(|t| !t.is_empty())
                        .collect();
                    if ed.is_new {
                        s.sort = self.sessions.iter().filter(|x| x.folder == s.folder).count() as i64;
                    }
                    if ed.keep_in_agent
                        && let Some(k) = s.common.key_path.clone()
                    {
                        self.add_agent_key(k);
                    }
                    self.save_session(&s);
                    if connect {
                        self.open_session(ctx, &s.id);
                    }
                }
                Target::Folder(mut f) => {
                    normalize(&mut f.common);
                    self.save_folder(&f);
                }
            },
        }
    }

    fn validate(&self, s: &Session) -> Option<String> {
        if s.name.trim().is_empty() {
            return Some(self.i18n.t("editor-err-name"));
        }
        if s.protocol == Protocol::Serial && s.host.trim().is_empty() {
            return Some(self.i18n.t("editor-err-serial-port"));
        }
        if matches!(
            s.protocol,
            Protocol::Ssh | Protocol::Telnet | Protocol::Raw | Protocol::Rdp
        ) {
            if s.host.trim().is_empty() {
                return Some(self.i18n.t("editor-err-host"));
            }
            if s.port == 0 {
                return Some(self.i18n.t("editor-err-port"));
            }
        }
        None
    }

    fn session_form(&mut self, ui: &mut egui::Ui, ed: &mut Editor, theme: &Theme) -> Outcome {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let Target::Session(s) = &mut ed.target else {
            return Outcome::Keep;
        };
        let title = if ed.is_new {
            t("editor-new-session")
        } else {
            i18n.t_args("editor-edit-session", &[("name", &s.name)])
        };
        let mut outcome = Outcome::Keep;
        ui.horizontal(|ui| {
            ui.heading(title);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if icons::button(ui, Icon::Close, 14.0, &t("settings-cancel")).clicked() {
                    outcome = Outcome::Close;
                }
            });
        });
        ui.add_space(12.0);
        // Fixed body height (scrolls inside): the dialog keeps its size when the
        // protocol or sub-tab changes, and fits small windows; footer stays visible.
        let body_h = crate::style::fit_height(ui, 190.0).min(560.0);
        egui::ScrollArea::vertical()
            .max_height(body_h)
            .min_scrolled_height(body_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // Protocol picker.
                ui.horizontal_wrapped(|ui| {
                    for (p, icon, key) in [
                        (Some(Protocol::Ssh), Icon::Prompt, "proto-ssh"),
                        (Some(Protocol::Telnet), Icon::Server, "proto-telnet"),
                        (Some(Protocol::Serial), Icon::Link, "proto-serial"),
                        (Some(Protocol::Rdp), Icon::Split, "proto-rdp"),
                        (Some(Protocol::Local), Icon::Prompt, "proto-local"),
                        (None, Icon::Split, "proto-vnc"),
                    ] {
                        let selected =
                            p == Some(s.protocol) || (p == Some(Protocol::Telnet) && s.protocol == Protocol::Raw);
                        let (rect, resp) = ui.allocate_exact_size(vec2(118.0, 74.0), egui::Sense::click());
                        let stroke = if selected {
                            Stroke::new(2.0, c(theme.ui.accent))
                        } else {
                            Stroke::new(1.0, c(theme.ui.border))
                        };
                        ui.painter()
                            .rect(rect, 8.0, c(theme.ui.panel), stroke, egui::StrokeKind::Inside);
                        let color = match (p.is_some(), selected) {
                            (false, _) => c(theme.ui.muted).gamma_multiply(0.6),
                            (_, true) => c(theme.ui.accent),
                            _ => c(theme.ui.fg),
                        };
                        icons::paint(
                            ui.painter(),
                            egui::Rect::from_center_size(rect.center() - vec2(0.0, 12.0), vec2(22.0, 22.0)),
                            icon,
                            color,
                        );
                        ui.painter().text(
                            rect.center() + vec2(0.0, 18.0),
                            egui::Align2::CENTER_CENTER,
                            t(key),
                            egui::FontId::proportional(13.0),
                            color,
                        );
                        if let Some(p) = p
                            && resp.clicked()
                            && p != s.protocol
                        {
                            if s.port == 0 || s.port == s.protocol.default_port() {
                                s.port = p.default_port();
                            }
                            s.protocol = p;
                        }
                    }
                });

                let inherited = sessions::resolve(&Common::default(), &self.folders, s.folder.as_deref());
                let inh = |field: &str, value: Option<String>| -> Option<(String, String)> {
                    Some((value?, inherited.from.get(field)?.clone()))
                };
                let half = ((ui.available_width() - 24.0) / 2.0).clamp(150.0, 380.0);

                caption(ui, t("editor-connection"));
                egui::Grid::new("conn")
                    .num_columns(2)
                    .min_col_width(half)
                    .spacing([24.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(t("editor-name"));
                        ui.label(t("editor-folder"));
                        ui.end_row();
                        ui.add(crate::style::field(&mut s.name).desired_width(half));
                        let current = if s.folder.is_some() {
                            sessions::path(&self.folders, s.folder.as_deref())
                        } else {
                            t("editor-no-folder")
                        };
                        egui::ComboBox::from_id_salt("folder")
                            .selected_text(current)
                            .width(half)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut s.folder, None, t("editor-no-folder"));
                                for f in &self.folders {
                                    ui.selectable_value(
                                        &mut s.folder,
                                        Some(f.id.clone()),
                                        sessions::path(&self.folders, Some(&f.id)),
                                    );
                                }
                            });
                        ui.end_row();
                        if s.protocol != Protocol::Local && s.protocol != Protocol::Serial {
                            ui.label(t("editor-host"));
                            ui.label(t("editor-port"));
                            ui.end_row();
                            ui.add(
                                crate::style::field(&mut s.host)
                                    .desired_width(half)
                                    .font(egui::TextStyle::Monospace),
                            );
                            ui.add(egui::DragValue::new(&mut s.port).range(1..=65535));
                            ui.end_row();
                        }
                        if s.protocol == Protocol::Rdp {
                            let mut ex: moliterm_core::rdp::Extra = s.extra();
                            let before = ex.clone();
                            ui.label(t("editor-username"));
                            ui.end_row();
                            inherit_text(
                                ui,
                                &mut s.common.username,
                                inh("username", inherited.common.username.clone()),
                                &t,
                                half,
                            );
                            ui.label(RichText::new(t("editor-rdp-user-hint")).small().weak());
                            ui.end_row();
                            ui.label(t("editor-rdp-display"));
                            ui.end_row();
                            ui.horizontal(|ui| {
                                use moliterm_core::rdp::Scale;
                                for (v, key) in [
                                    (Scale::Resize, "rdp-resize"),
                                    (Scale::Fit, "rdp-fit"),
                                    (Scale::Actual, "rdp-actual"),
                                ] {
                                    ui.radio_value(&mut ex.scale, v, t(key));
                                }
                            });
                            ui.end_row();
                            if ex != before {
                                s.set_extra(&ex);
                            }
                        }
                        if matches!(s.protocol, Protocol::Telnet | Protocol::Raw) {
                            ui.label(t("editor-username"));
                            ui.end_row();
                            inherit_text(
                                ui,
                                &mut s.common.username,
                                inh("username", inherited.common.username.clone()),
                                &t,
                                half,
                            );
                            ui.end_row();
                        }
                        if s.protocol == Protocol::Ssh {
                            ui.label(t("editor-username"));
                            ui.label(t("editor-jump"));
                            ui.end_row();
                            inherit_text(
                                ui,
                                &mut s.common.username,
                                inh("username", inherited.common.username.clone()),
                                &t,
                                half,
                            );
                            inherit_text(
                                ui,
                                &mut s.common.jump,
                                inh("jump", inherited.common.jump.clone()),
                                &t,
                                half,
                            );
                            ui.end_row();
                        }
                    });

                if matches!(s.protocol, Protocol::Ssh | Protocol::Rdp | Protocol::Telnet) {
                    caption(ui, t("editor-auth"));
                }
                let ident_inh = inherited
                    .common
                    .identity
                    .clone()
                    .filter(|i| !i.is_empty())
                    .zip(inherited.from.get("identity").cloned());
                let ident = if matches!(s.protocol, Protocol::Ssh | Protocol::Rdp | Protocol::Telnet) {
                    identity_picker(ui, &mut s.common.identity, ident_inh, &self.identities, &t)
                } else {
                    None
                };
                if ident.is_some() {
                    ui.label(RichText::new(t("editor-identity-note")).small().weak());
                }
                if s.protocol == Protocol::Ssh && ident.is_none() {
                    let effective = s.common.auth.or(inherited.common.auth).unwrap_or_default();
                    ui.horizontal_wrapped(|ui| {
                        for (m, key) in [
                            (AuthMethod::Password, "auth-password"),
                            (AuthMethod::Key, "auth-key"),
                            (AuthMethod::Agent, "auth-agent"),
                            (AuthMethod::KeyboardInteractive, "auth-kbd"),
                        ] {
                            if ui.radio(effective == m, t(key)).clicked() {
                                s.common.auth = Some(m);
                            }
                        }
                        if let Some(from) = inherited.from.get("auth")
                            && s.common.auth.is_none()
                        {
                            ui.label(RichText::new(format!("{} {from}", t("editor-from"))).small().weak());
                        }
                    });
                    if effective == AuthMethod::Key {
                        ui.add_space(4.0);
                        ui.label(t("editor-key-file"));
                        ui.horizontal_wrapped(|ui| {
                            inherit_text(
                                ui,
                                &mut s.common.key_path,
                                inh("key_path", inherited.common.key_path.clone()),
                                &t,
                                (ui.available_width() - 260.0).clamp(200.0, 520.0),
                            );
                            if ui.button(t("editor-browse")).clicked() {
                                let (tx, rx) = mpsc::channel();
                                let dialog = rfd::AsyncFileDialog::new().pick_file();
                                let wake = ui.ctx().clone();
                                self.rt.spawn(async move {
                                    if let Some(f) = dialog.await {
                                        let _ = tx.send(f.path().to_path_buf());
                                        wake.request_repaint();
                                    }
                                });
                                ed.pick = Some(rx);
                            }
                            if ui.button(t("editor-generate-key")).clicked() {
                                self.dialog = Some(crate::dialogs::Dialog::KeyGen(Default::default()));
                            }
                        });
                        ui.checkbox(&mut ed.keep_in_agent, t("editor-keep-in-agent"));
                    }
                }

                if matches!(s.protocol, Protocol::Telnet | Protocol::Raw) {
                    telnet_fields(ui, s, &t, half);
                }
                if s.protocol == Protocol::Serial {
                    let ports = ed.ports.get_or_insert_with(serial::ports);
                    serial_fields(ui, s, ports, &t, half);
                }

                // Sub-tabs.
                let is_ssh = s.protocol == Protocol::Ssh;
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    for (tab, key) in [
                        (SubTab::Advanced, "editor-tab-advanced"),
                        (SubTab::Terminal, "editor-tab-terminal"),
                        (SubTab::Tunnels, "editor-tab-tunnels"),
                        (SubTab::Sftp, "editor-tab-sftp"),
                        (SubTab::Appearance, "editor-tab-appearance"),
                    ] {
                        if !is_ssh && matches!(tab, SubTab::Tunnels | SubTab::Sftp) {
                            if ed.tab == tab {
                                ed.tab = SubTab::Advanced;
                            }
                            continue;
                        }
                        ui.selectable_value(&mut ed.tab, tab, t(key));
                    }
                });
                ui.separator();
                let ic = &inherited.common;
                match ed.tab {
                    SubTab::Advanced => {
                        egui::Grid::new("adv")
                            .num_columns(2)
                            .min_col_width(half)
                            .spacing([24.0, 6.0])
                            .show(ui, |ui| {
                                if is_ssh {
                                    ui.label(t("editor-after-login"));
                                }
                                ui.label(t("editor-encoding"));
                                ui.end_row();
                                if is_ssh {
                                    let al = s.common.after_login.get_or_insert_with(String::new);
                                    ui.add(
                                        crate::style::field(al)
                                            .hint_text(t("editor-after-login-hint"))
                                            .desired_width(half)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                }
                                let enc = s
                                    .common
                                    .encoding
                                    .clone()
                                    .or(ic.encoding.clone())
                                    .unwrap_or_else(|| "UTF-8".into());
                                egui::ComboBox::from_id_salt("enc")
                                    .selected_text(&enc)
                                    .width(half)
                                    .show_ui(ui, |ui| {
                                        for e in
                                            ["UTF-8", "Windows-1251", "KOI8-R", "CP866", "ISO-8859-1", "ISO-8859-5"]
                                        {
                                            if ui.selectable_label(enc == e, e).clicked() {
                                                s.common.encoding = Some(e.into());
                                            }
                                        }
                                    });
                                ui.end_row();
                                if !is_ssh {
                                    return;
                                }
                                {
                                    ui.label(t("editor-locale"));
                                    ui.end_row();
                                    let lc = s.common.locale.get_or_insert_with(String::new);
                                    ui.add(
                                        crate::style::field(lc)
                                            .hint_text(ic.locale.clone().unwrap_or_else(|| t("editor-locale-hint")))
                                            .desired_width(half)
                                            .font(egui::TextStyle::Monospace),
                                    );
                                    ui.end_row();
                                }
                            });
                        if is_ssh {
                            // Two columns when there is room; long labels don't wrap.
                            let wide = ui.available_width() >= 760.0;
                            let c = &mut s.common;
                            let flags = [
                                (&mut c.keepalive, ic.keepalive.unwrap_or(true), t("editor-keepalive")),
                                (&mut c.reconnect, ic.reconnect.unwrap_or(true), t("editor-reconnect")),
                                (&mut c.legacy, ic.legacy.unwrap_or(false), t("editor-legacy")),
                                (&mut c.monitor, ic.monitor.unwrap_or(true), t("editor-monitor")),
                                (
                                    &mut c.agent_forward,
                                    ic.agent_forward.unwrap_or(false),
                                    t("editor-agent-forward"),
                                ),
                                (&mut c.x11, ic.x11.unwrap_or(false), t("editor-x11")),
                            ];
                            if wide {
                                egui::Grid::new("adv-flags")
                                    .num_columns(2)
                                    .spacing([24.0, 6.0])
                                    .show(ui, |ui| {
                                        for (i, (own, effective, label)) in flags.into_iter().enumerate() {
                                            inherit_bool(ui, own, effective, label);
                                            if i % 2 == 1 {
                                                ui.end_row();
                                            }
                                        }
                                    });
                            } else {
                                // One per line, labels wrap.
                                for (own, effective, label) in flags {
                                    inherit_bool(ui, own, effective, label);
                                }
                            }
                        }
                        ui.add_space(4.0);
                        if is_ssh {
                            ui.horizontal(|ui| {
                                ui.label(t("editor-proxy"));
                                let mut kind = match &s.common.proxy {
                                    None => 0,
                                    Some(ssh::Proxy::Socks5 { .. }) => 1,
                                    Some(ssh::Proxy::Http { .. }) => 2,
                                };
                                let before = kind;
                                egui::ComboBox::from_id_salt("proxy")
                                    .selected_text(
                                        [t("editor-proxy-none"), "SOCKS5".into(), "HTTP CONNECT".into()][kind].clone(),
                                    )
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut kind, 0, t("editor-proxy-none"));
                                        ui.selectable_value(&mut kind, 1, "SOCKS5");
                                        ui.selectable_value(&mut kind, 2, "HTTP CONNECT");
                                    });
                                if kind != before {
                                    let (h, p) = match &s.common.proxy {
                                        Some(ssh::Proxy::Socks5 { host, port } | ssh::Proxy::Http { host, port }) => {
                                            (host.clone(), *port)
                                        }
                                        None => ("127.0.0.1".into(), 1080),
                                    };
                                    s.common.proxy = match kind {
                                        1 => Some(ssh::Proxy::Socks5 { host: h, port: p }),
                                        2 => Some(ssh::Proxy::Http { host: h, port: p }),
                                        _ => None,
                                    };
                                }
                                if let Some(ssh::Proxy::Socks5 { host, port } | ssh::Proxy::Http { host, port }) =
                                    &mut s.common.proxy
                                {
                                    ui.add(crate::style::field(host).desired_width(200.0));
                                    ui.add(egui::DragValue::new(port).range(1..=65535));
                                }
                            });
                        }
                    }
                    SubTab::Terminal => {
                        inherit_bool(
                            ui,
                            &mut s.common.highlight,
                            ic.highlight.unwrap_or(true),
                            t("terminal-highlight"),
                        );
                        inherit_bool(
                            ui,
                            &mut s.common.log,
                            ic.log.unwrap_or(self.settings.log_sessions),
                            t("terminal-log-on"),
                        );
                    }
                    SubTab::Tunnels => ed.tunnel_req = self.session_tunnels_tab(ui, &s.id),
                    SubTab::Sftp => {
                        ui.label(t("editor-sftp-dir"));
                        inherit_text(
                            ui,
                            &mut s.common.sftp_dir,
                            inh("sftp_dir", inherited.common.sftp_dir.clone()),
                            &t,
                            half,
                        );
                        ui.label(RichText::new(t("editor-sftp-dir-hint")).small().weak());
                    }
                    SubTab::Appearance => {
                        ui.label(t("editor-color"));
                        color_picker(ui, &mut s.common.color, ic.color.as_deref(), &t);
                        ui.add_space(6.0);
                        ui.label(t("editor-tags"));
                        ui.add(
                            crate::style::field(&mut ed.tags)
                                .hint_text(t("editor-tags-hint"))
                                .desired_width(half),
                        );
                    }
                }
            });

        // Footer.
        ui.add_space(14.0);
        let error = self.validate(s);
        let draft = s.clone();
        let mut left = |ui: &mut egui::Ui| {
            let can_test = matches!(
                draft.protocol,
                Protocol::Ssh | Protocol::Telnet | Protocol::Raw | Protocol::Rdp
            ) && error.is_none();
            if ui
                .add_enabled(
                    can_test && !matches!(ed.test, Some(Test::Running(_))),
                    egui::Button::new(t("editor-test")),
                )
                .clicked()
            {
                ed.test = Some(Test::Running(self.test_connection(&draft, ui.ctx())));
            }
            match &ed.test {
                Some(Test::Running(_)) => {
                    ui.spinner();
                }
                Some(Test::Done(Ok(msg))) => {
                    ui.label(RichText::new(format!("✓ {msg}")).color(c(theme.ui.success)));
                }
                Some(Test::Done(Err(msg))) => {
                    ui.label(RichText::new(msg).color(c(theme.ui.danger)));
                }
                None => {}
            }
            if let Some(e) = &error {
                ui.label(RichText::new(e).color(c(theme.ui.danger)));
            }
        };
        let mut right = |ui: &mut egui::Ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_enabled_ui(error.is_none(), |ui| {
                    if ui.button(t("editor-save-connect")).clicked() {
                        outcome = Outcome::Save { connect: true };
                    }
                    if primary(ui, theme, t("editor-save")).clicked() {
                        outcome = Outcome::Save { connect: false };
                    }
                });
                if ui.button(t("settings-cancel")).clicked() {
                    outcome = Outcome::Close;
                }
            });
        };
        // Narrow windows: test + messages on their own (wrapping) line, then the
        // buttons, wrapping too when even they don't fit.
        if ui.available_width() < 760.0 {
            ui.horizontal_wrapped(&mut left);
            if ui.available_width() < 470.0 {
                ui.horizontal_wrapped(|ui| {
                    if ui.button(t("settings-cancel")).clicked() {
                        outcome = Outcome::Close;
                    }
                    ui.add_enabled_ui(error.is_none(), |ui| {
                        if primary(ui, theme, t("editor-save")).clicked() {
                            outcome = Outcome::Save { connect: false };
                        }
                        if ui.button(t("editor-save-connect")).clicked() {
                            outcome = Outcome::Save { connect: true };
                        }
                    });
                });
            } else {
                ui.horizontal(&mut right);
            }
        } else {
            ui.horizontal(|ui| {
                left(ui);
                right(ui);
            });
        }
        if let Some(e) = &ed.error {
            ui.label(RichText::new(e).color(c(theme.ui.danger)));
        }
        outcome
    }

    fn folder_form(&mut self, ui: &mut egui::Ui, ed: &mut Editor, theme: &Theme) -> Outcome {
        let i18n = &self.i18n;
        let t = |k: &str| i18n.t(k);
        let Target::Folder(f) = &mut ed.target else {
            return Outcome::Keep;
        };
        let mut outcome = Outcome::Keep;
        ui.heading(if ed.is_new {
            t("editor-new-folder")
        } else {
            i18n.t_args("editor-edit-folder", &[("name", &f.name)])
        });
        let inherited = sessions::resolve(&Common::default(), &self.folders, f.parent.as_deref());
        let inh = |field: &str, value: Option<String>| -> Option<(String, String)> {
            Some((value?, inherited.from.get(field)?.clone()))
        };
        let half = ((ui.available_width() - 24.0) / 2.0).clamp(150.0, 380.0);
        egui::Grid::new("folder")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.label(t("editor-name"));
                ui.label(t("editor-parent"));
                ui.end_row();
                ui.add(crate::style::field(&mut f.name).desired_width(half));
                let current = if f.parent.is_some() {
                    sessions::path(&self.folders, f.parent.as_deref())
                } else {
                    t("editor-no-folder")
                };
                egui::ComboBox::from_id_salt("parent")
                    .selected_text(current)
                    .width(half)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut f.parent, None, t("editor-no-folder"));
                        for other in &self.folders {
                            let inside = sessions::chain(&self.folders, Some(&other.id))
                                .iter()
                                .any(|x| x.id == f.id);
                            if !inside {
                                ui.selectable_value(
                                    &mut f.parent,
                                    Some(other.id.clone()),
                                    sessions::path(&self.folders, Some(&other.id)),
                                );
                            }
                        }
                    });
                ui.end_row();
            });
        caption(ui, t("editor-folder-defaults"));
        egui::Grid::new("folder-defaults")
            .num_columns(2)
            .spacing([24.0, 6.0])
            .show(ui, |ui| {
                ui.label(t("editor-username"));
                ui.label(t("editor-jump"));
                ui.end_row();
                inherit_text(
                    ui,
                    &mut f.common.username,
                    inh("username", inherited.common.username.clone()),
                    &t,
                    half,
                );
                inherit_text(
                    ui,
                    &mut f.common.jump,
                    inh("jump", inherited.common.jump.clone()),
                    &t,
                    half,
                );
                ui.end_row();
                ui.label(t("editor-key-file"));
                ui.label(t("editor-after-login"));
                ui.end_row();
                inherit_text(
                    ui,
                    &mut f.common.key_path,
                    inh("key_path", inherited.common.key_path.clone()),
                    &t,
                    half,
                );
                inherit_text(
                    ui,
                    &mut f.common.after_login,
                    inh("after_login", inherited.common.after_login.clone()),
                    &t,
                    half,
                );
                ui.end_row();
            });
        let ident_inh = inherited
            .common
            .identity
            .clone()
            .filter(|i| !i.is_empty())
            .zip(inherited.from.get("identity").cloned());
        identity_picker(ui, &mut f.common.identity, ident_inh, &self.identities, &t);
        ui.horizontal(|ui| {
            let effective = f.common.auth.or(inherited.common.auth);
            for (m, key) in [
                (AuthMethod::Password, "auth-password"),
                (AuthMethod::Key, "auth-key"),
                (AuthMethod::Agent, "auth-agent"),
                (AuthMethod::KeyboardInteractive, "auth-kbd"),
            ] {
                if ui.radio(effective == Some(m), t(key)).clicked() {
                    f.common.auth = Some(m);
                }
            }
        });
        let ic = &inherited.common;
        inherit_bool(
            ui,
            &mut f.common.monitor,
            ic.monitor.unwrap_or(true),
            t("editor-monitor"),
        );
        inherit_bool(ui, &mut f.common.legacy, ic.legacy.unwrap_or(false), t("editor-legacy"));
        ui.label(t("editor-color"));
        color_picker(ui, &mut f.common.color, ic.color.as_deref(), &t);
        ui.add_space(14.0);
        let ok = !f.name.trim().is_empty();
        ui.horizontal(|ui| {
            if !ok {
                ui.label(RichText::new(t("editor-err-name")).color(c(theme.ui.danger)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add_enabled(ok, egui::Button::new(t("editor-save"))).clicked() {
                    outcome = Outcome::Save { connect: false };
                }
                if ui.button(t("settings-cancel")).clicked() {
                    outcome = Outcome::Close;
                }
            });
        });
        outcome
    }

    /// Connects and authenticates with the draft settings, then disconnects.
    fn test_connection(&self, draft: &Session, ctx: &egui::Context) -> mpsc::Receiver<Result<String, String>> {
        let (tx, rx) = mpsc::channel();
        let opts = self.ssh_options(draft);
        let sctx = self.ssh_ctx.clone();
        let wake = ctx.clone();
        let ok = self.i18n.t("editor-test-ok");
        if draft.protocol != Protocol::Ssh {
            let (host, port) = (draft.host.trim().to_owned(), draft.port);
            self.rt.spawn(async move {
                let connect = tokio::net::TcpStream::connect((host.as_str(), port));
                let r = match tokio::time::timeout(std::time::Duration::from_secs(10), connect).await {
                    Ok(Ok(_)) => Ok(format!("{ok} · TCP")),
                    Ok(Err(e)) => Err(format!("{host}:{port}: {e}")),
                    Err(_) => Err(format!("{host}:{port}: timeout")),
                };
                let _ = tx.send(r);
                wake.request_repaint();
            });
            return rx;
        }
        self.rt.spawn(async move {
            let r = match ssh::connect(&opts, &sctx).await {
                Ok((h, key)) => {
                    let _ = h.disconnect(russh::Disconnect::ByApplication, "", "en").await;
                    Ok(format!("{ok} · {key}"))
                }
                Err(f) => Err(f.msg),
            };
            let _ = tx.send(r);
            wake.request_repaint();
        });
        rx
    }
}

fn color_picker(ui: &mut egui::Ui, own: &mut Option<String>, inherited: Option<&str>, t: &dyn Fn(&str) -> String) {
    ui.horizontal(|ui| {
        let current = own.clone().or(inherited.map(String::from));
        let current = current.as_deref();
        for hex in PALETTE {
            let color = parse_color(Some(hex)).unwrap_or_default();
            let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), egui::Sense::click());
            ui.painter().rect_filled(r, 4.0, color);
            if current == Some(hex) {
                ui.painter().rect_stroke(
                    r.expand(2.0),
                    5.0,
                    Stroke::new(2.0, ui.visuals().text_color()),
                    egui::StrokeKind::Outside,
                );
            }
            if resp.clicked() {
                *own = Some(hex.into());
            }
        }
        let mut rgb = parse_color(current).map_or([128, 128, 128], |c| [c.r(), c.g(), c.b()]);
        if egui::color_picker::color_edit_button_srgb(ui, &mut rgb).changed() {
            *own = Some(format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]));
        }
        if ui.small_button(t("editor-color-none")).clicked() {
            *own = None;
        }
    });
}
