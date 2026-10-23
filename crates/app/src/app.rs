use std::{
    collections::{HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock, mpsc},
    time::{Instant, SystemTime},
};

use egui_dock::DockState;
use moliterm_core::{
    db::Db,
    highlight::{Compiled, Rule},
    local::{self, Shell},
    sessions::{self, Folder, Session},
    settings::{RightClick, Settings},
    ssh::{self, AgentKeys, Ask},
    sshconfig::HostEntry,
    terminal::{self, SessionEvent},
    termlog::{LogOptions, SessionLog},
    theme::{self, TermColors, Theme},
    vault::Vault,
};

use crate::{
    dialogs::Dialog,
    dock::Tab,
    fonts,
    i18n::{self, I18n},
    settings_ui,
    style::{self, c},
    term_view::{self, Launch, Req, TermTab},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side {
    Sessions,
    Sftp,
    Tunnels,
    Macros,
}

/// Everything a menu item, toolbar button or shortcut can trigger.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    OpenSettings,
    ToggleSidePanel,
    ShowSide(Side),
    Fullscreen,
    Quit,
    /// Index into the detected shells; `None` = default shell.
    NewLocalShell(Option<usize>),
    NewSession,
    LockVault,
    Copy,
    Paste,
    SplitRight,
    SplitDown,
    /// Move the active tab into its own native window.
    ToWindow,
    OpenVault,
    MultiExec,
    CloseTab,
    NextTab,
    PrevTab,
    GoToTab(usize),
    Find,
    FontBigger,
    FontSmaller,
    FontReset,
    /// Index into the macro list.
    RunMacro(usize),
}

/// What is currently applied to the egui context; compared every frame
/// against the live settings so theme/language/font changes take effect at once.
#[derive(PartialEq, Default)]
struct Applied {
    theme: String,
    term_theme: String,
    zoom: f32,
    compact: bool,
    lang: &'static str,
    term_font: Option<String>,
}

pub struct App {
    pub(crate) db: Db,
    pub(crate) data_dir: PathBuf,
    /// Lives for the whole process (see `main`), so blocking PTY reads never delay exit.
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) settings: Settings,
    pub(crate) themes: Vec<Theme>,
    pub(crate) i18n: std::rc::Rc<I18n>,
    fonts_rx: Option<mpsc::Receiver<fonts::Loaded>>,
    pub(crate) fonts: Option<fonts::Loaded>,
    /// Fonts handed to egui this frame; usable from the next frame on.
    fonts_pending: Option<fonts::Loaded>,
    applied: Applied,
    pub(crate) palette: Arc<RwLock<TermColors>>,
    pub(crate) rules: (Vec<Rule>, Compiled),
    pub(crate) dock: DockState<Tab>,
    pub(crate) next_id: u64,
    /// Detached native windows (SPEC §4.4).
    pub(crate) windows: Vec<crate::layout::Win>,
    pub(crate) window_seq: u64,
    /// True while a detached window is being drawn (its dock is swapped in).
    pub(crate) in_window: bool,
    /// Multi-exec of the current window (swapped like the dock for detached windows).
    pub(crate) multi: Option<crate::multiexec::MultiExec>,
    pub(crate) me_picker: Option<crate::multiexec::Picker>,
    pub(crate) jobs: Option<crate::multiexec::Jobs>,
    /// Tabs Cmd/Ctrl-clicked as multi-exec targets.
    pub(crate) selected_tabs: HashSet<u64>,
    pub(crate) risky_confirm_ui: bool,
    pub(crate) ctx_wake: moliterm_core::terminal::Waker,
    pub(crate) shells: Vec<Shell>,
    shells_rx: Option<mpsc::Receiver<Vec<Shell>>>,
    pub(crate) change_pw: crate::security_ui::ChangePassword,
    pub(crate) agent_pick: (mpsc::Sender<String>, mpsc::Receiver<String>),
    pub(crate) side: Option<Side>,
    last_side: Side,
    pub(crate) filter: String,
    pub(crate) settings_page: Option<settings_ui::Page>,
    pub(crate) dialog: Option<Dialog>,
    actions: mpsc::Receiver<Action>,
    pub(crate) folders: Vec<Folder>,
    pub(crate) sessions: Vec<Session>,
    pub(crate) sshcfg: Vec<HostEntry>,
    sshcfg_rx: mpsc::Receiver<Vec<HostEntry>>,
    pub(crate) selected: HashSet<String>,
    pub(crate) editor: Option<crate::editor::Editor>,
    pub(crate) tunnels: crate::tunnels_ui::Tunnels,
    pub(crate) identities: Vec<moliterm_core::identity::Identity>,
    pub(crate) vault_page: Option<crate::vault_ui::VaultPage>,
    /// Toolbar quick-connect text, and the unsaved sessions of tabs opened from it.
    pub(crate) quick: String,
    pub(crate) quick_sessions: std::collections::HashMap<u64, Session>,
    /// Tab waiting for "Type password from vault" (SPEC-9 §9.6).
    pub(crate) type_password: Option<u64>,
    /// A copied secret and when to wipe it from the clipboard.
    pub(crate) clip_clear: Option<(Instant, zeroize::Zeroizing<String>)>,
    pub(crate) macros: crate::macros_ui::Macros,
    /// Tab full screen of the current window, since when (SPEC-9 §9.4).
    pub(crate) focus: Option<Instant>,
    /// Tabs drawn (= active in their leaf) last frame / this frame, for the tab bar highlight.
    pub(crate) tabs_shown: (HashSet<u64>, HashSet<u64>),
    pub(crate) import: crate::import_ui::ImportUi,
    pub(crate) vault: Vault,
    pub(crate) vault_ui: crate::asks::VaultUi,
    /// Questions from session tasks waiting for a dialog.
    pub(crate) asks: VecDeque<Ask>,
    pub(crate) ask_rx: tokio::sync::mpsc::UnboundedReceiver<Ask>,
    pub(crate) ssh_ctx: ssh::Ctx,
    pub(crate) agent_keys: AgentKeys,
    /// SFTP panel state per SSH tab id.
    pub(crate) sftp: std::collections::HashMap<u64, crate::sftp_ui::SftpTab>,
    pub(crate) sftp_pick: (
        mpsc::Sender<crate::sftp_ui::Picked>,
        mpsc::Receiver<crate::sftp_ui::Picked>,
    ),
    pub(crate) last_activity: Instant,
    /// Debug builds: answer host-key/password questions automatically.
    pub(crate) dev_auto_answer: bool,
    #[cfg(any(debug_assertions, feature = "devtools"))]
    ctx_for_dev: egui::Context,
    /// Wall clock vs monotonic clock, to notice OS sleep.
    pub(crate) last_tick: (SystemTime, Instant),
    #[cfg(target_os = "macos")]
    menu: crate::native_menu::NativeMenu,
    #[cfg(any(debug_assertions, feature = "devtools"))]
    devshot: Option<crate::devshot::DevShot>,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        rt: tokio::runtime::Handle,
        db: Db,
        settings: Settings,
        data_dir: PathBuf,
    ) -> Self {
        let ctx = cc.egui_ctx.clone();
        let (tx, actions) = mpsc::channel();
        let i18n = std::rc::Rc::new(I18n::new(i18n::resolve(&settings.language)));
        #[cfg(target_os = "macos")]
        let menu = crate::native_menu::NativeMenu::new(&i18n, ctx.clone(), tx);
        #[cfg(not(target_os = "macos"))]
        drop(tx);
        let themes = theme::load_all(&data_dir.join("themes"));
        let palette = Arc::new(RwLock::new(themes[0].term.clone()));
        let (ask_tx, ask_rx) = tokio::sync::mpsc::unbounded_channel();
        let agent_keys: AgentKeys = Arc::new(Mutex::new(vec![]));
        let wake_ctx = ctx.clone();
        let ssh_ctx = ssh::Ctx {
            ask: ask_tx,
            agent_keys: agent_keys.clone(),
            wake: Arc::new(move || wake_ctx.request_repaint()),
        };
        let (folders, sessions) = sessions::load(&db).unwrap_or_else(|e| {
            tracing::error!("loading sessions: {e}");
            (vec![], vec![])
        });
        let sshcfg_rx = crate::sessions_ui::watch_ssh_config(&rt, ctx.clone());
        let (shells_tx, shells_rx) = mpsc::channel();
        let wake = ctx.clone();
        std::thread::spawn(move || {
            let _ = shells_tx.send(local::detect());
            wake.request_repaint();
        });
        let app = Self {
            fonts_rx: Some(fonts::spawn(ctx, None, settings.term_font.clone())),
            applied: Applied {
                term_font: Some(settings.term_font.clone()),
                lang: i18n.lang,
                ..Default::default()
            },
            rules: (settings.keyword_rules.clone(), Compiled::new(&settings.keyword_rules)),
            rt,
            themes,
            palette,
            i18n,
            db,
            data_dir,
            settings,
            fonts: None,
            fonts_pending: None,
            dock: DockState::new(vec![Tab::Welcome]),
            next_id: 1,
            windows: vec![],
            window_seq: 0,
            in_window: false,
            multi: None,
            me_picker: None,
            jobs: None,
            selected_tabs: HashSet::new(),
            risky_confirm_ui: true,
            ctx_wake: {
                let c = cc.egui_ctx.clone();
                Arc::new(move || c.request_repaint())
            },
            shells: vec![],
            shells_rx: Some(shells_rx),
            change_pw: Default::default(),
            agent_pick: mpsc::channel(),
            side: Some(Side::Sessions),
            last_side: Side::Sessions,
            filter: String::new(),
            settings_page: None,
            dialog: None,
            actions,
            folders,
            sessions,
            sshcfg: vec![],
            sshcfg_rx,
            selected: HashSet::new(),
            editor: None,
            tunnels: Default::default(),
            identities: vec![],
            vault_page: None,
            quick: String::new(),
            quick_sessions: Default::default(),
            type_password: None,
            clip_clear: None,
            macros: Default::default(),
            focus: None,
            tabs_shown: Default::default(),
            import: Default::default(),
            vault: Vault::default(),
            vault_ui: Default::default(),
            asks: VecDeque::new(),
            ask_rx,
            ssh_ctx,
            agent_keys,
            sftp: Default::default(),
            sftp_pick: mpsc::channel(),
            last_activity: Instant::now(),
            dev_auto_answer: false,
            #[cfg(any(debug_assertions, feature = "devtools"))]
            ctx_for_dev: cc.egui_ctx.clone(),
            last_tick: (SystemTime::now(), Instant::now()),
            #[cfg(target_os = "macos")]
            menu,
            #[cfg(any(debug_assertions, feature = "devtools"))]
            devshot: crate::devshot::DevShot::from_env(),
        };
        let mut app = app;
        app.risky_confirm_ui = app.settings.risky_confirm;
        let marker = app.data_dir.join("logs").join("last-crash.txt");
        if marker.exists() {
            let _ = std::fs::remove_file(&marker);
            let logs = app.data_dir.join("logs").display().to_string();
            app.dialog = Some(Dialog::Info(app.i18n.t_args("crash-last-time", &[("path", &logs)])));
        }
        app.identities = moliterm_core::identity::load(&app.db).unwrap_or_default();
        app.load_tunnels();
        app.load_macros();
        app.autostart_tunnels();
        app.offer_restore(&cc.egui_ctx);
        #[cfg(any(debug_assertions, feature = "devtools"))]
        app.dev_actions();
        app
    }

    /// Debug builds: `MOLITERM_SHOT_ACTIONS=settings,section:general,theme:Monokai,lang:ru,side:sftp,right,shell`.
    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn dev_actions(&mut self) {
        let Ok(list) = std::env::var("MOLITERM_SHOT_ACTIONS") else {
            return;
        };
        for a in list.split(',') {
            match a.split_once(':').unwrap_or((a, "")) {
                ("theme", v) => self.settings.theme = v.into(),
                ("lang", v) => self.settings.language = v.into(),
                ("right", _) => self.settings.side_panel_right = true,
                ("icons", _) => self.settings.toolbar = moliterm_core::settings::ToolbarMode::IconsOnly,
                ("side", "sftp") => self.side = Some(Side::Sftp),
                ("side", "tunnels") => self.side = Some(Side::Tunnels),
                ("side", "macros") => self.side = Some(Side::Macros),
                ("side", "none") => self.side = None,
                ("settings", _) => self.run(&egui::Context::default(), Action::OpenSettings),
                ("shell", cmd) => {
                    self.shells = local::detect();
                    self.open_local(&egui::Context::default(), None);
                    if let Some(t) = self.terms().next() {
                        t.handle.send(format!("{cmd}\n"));
                    }
                }
                ("find", q) => {
                    if let Some(t) = self.active_term() {
                        t.toggle_find();
                        if let Some(f) = &mut t.find {
                            f.query = q.into();
                        }
                    }
                }
                ("paste", _) => {
                    if let Some(id) = self.active_term_id() {
                        self.dialog = Some(Dialog::Paste {
                            tab: id,
                            targets: vec![id],
                            text: "cd /etc/nginx\nsudo cp nginx.conf nginx.conf.bak\nsudo nginx -t\nsudo systemctl reload nginx".into(),
                            by_line: false,
                            no_ask: false,
                        });
                    }
                }
                ("demo", _) => self.seed_demo(),
                ("tabs", n) => {
                    self.shells = local::detect();
                    for _ in 0..n.parse().unwrap_or(1) {
                        self.open_local(&self.ctx_for_dev.clone(), None);
                    }
                }
                ("multi", _) => {
                    let ids = self.terms_ref().map(|t| t.id).collect();
                    self.enter_multi_exec(ids);
                    if let Some(m) = &mut self.multi
                        && let Some(first) = m.tabs.first().copied()
                    {
                        m.excluded.insert(first);
                    }
                }
                ("line", _) => {
                    if let Some(m) = &mut self.multi {
                        m.mode = crate::multiexec::Mode::Line;
                    }
                }
                ("split", _) => self.split_active(&self.ctx_for_dev.clone(), crate::layout::Place::Right),
                ("picker", _) => self.toggle_multi_exec(),
                ("cwd", dir) => {
                    if let Some(t) = self.active_term() {
                        t.cwd = Some(dir.into());
                    }
                }
                // `rdp:127.0.0.1:3390` — ad-hoc RDP session (docker xrdp, test / test).
                // `tunnel:<session name>` — a demo -D tunnel through that session, started.
                ("tunnel", name) => {
                    if let Some(s) = self.sessions.iter().find(|s| s.name == name).cloned() {
                        let t = moliterm_core::tunnel::Tunnel {
                            id: sessions::new_id(),
                            name: "socks".into(),
                            kind: moliterm_core::tunnel::Kind::Dynamic,
                            session_id: s.id,
                            listen_port: 22299,
                            ..Default::default()
                        };
                        self.save_tunnel(&t);
                        self.start_tunnel(&t.id);
                    }
                }
                ("runmacro", _) => self.run_macro(0),
                ("newsession", _) => self.run(&self.ctx_for_dev.clone(), Action::NewSession),
                ("quick", text) => {
                    self.dev_auto_answer = true;
                    self.quick_connect(&self.ctx_for_dev.clone(), text.into(), vec![]);
                }
                // Vault with password "test", one identity and one saved password; opens the page.
                ("vaultdemo", _) => {
                    if let Ok((key, salt)) = moliterm_core::vault::new_key("test")
                        && let Ok(v) = Vault::create(&self.db, key, &salt)
                    {
                        self.vault = v;
                        let ident = moliterm_core::identity::Identity {
                            id: sessions::new_id(),
                            name: "prod-admin".into(),
                            username: "CORP\\admin".into(),
                            ..Default::default()
                        };
                        let _ = self
                            .vault
                            .put(&self.db, &format!("{}:password", ident.secret_prefix()), "S3cret!");
                        let _ = moliterm_core::identity::save(&self.db, &ident);
                        if let Some(s) = self.sessions.first() {
                            let _ = self
                                .vault
                                .put(&self.db, &format!("session:{}:password", s.id), "hunter2");
                        }
                        self.identities = moliterm_core::identity::load(&self.db).unwrap_or_default();
                    }
                    self.open_vault_page();
                }
                // `size:720x480`, `zoom:1.5` — check small windows.
                ("size", wh) => {
                    if let Some((w, h)) = wh.split_once('x')
                        && let (Ok(w), Ok(h)) = (w.parse::<f32>(), h.parse::<f32>())
                    {
                        self.ctx_for_dev
                            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
                    }
                }
                ("zoom", z) => self.settings.ui_scale = z.parse().unwrap_or(1.0),
                ("focus", _) => self.toggle_focus(&self.ctx_for_dev.clone()),
                ("macro", _) => {
                    let m = moliterm_core::macros::Macro {
                        id: sessions::new_id(),
                        name: "Update packages".into(),
                        body: "echo first\n{sleep 500}echo moli$((40+2))\n".into(),
                        hotkey: "Cmd+Shift+1".into(),
                        ..Default::default()
                    };
                    let _ = moliterm_core::macros::save(&self.db, &m);
                    self.load_macros();
                    self.macros.editor = Some(m);
                }
                ("rdp", target) => {
                    self.dev_auto_answer = true;
                    let (host, port) = target.rsplit_once(':').unwrap_or((target, "3389"));
                    let s = Session {
                        id: "dev-rdp".into(),
                        name: "xrdp".into(),
                        protocol: sessions::Protocol::Rdp,
                        host: host.into(),
                        port: port.parse().unwrap_or(3389),
                        common: sessions::Common {
                            username: Some("test".into()),
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    self.sessions.push(s);
                    self.open_session(&self.ctx_for_dev.clone(), "dev-rdp");
                }
                // `ssh:127.0.0.1:2299` — ad-hoc session against `dev_sshd`; trusts the key once, password "test".
                ("ssh", target) => {
                    self.dev_auto_answer = true;
                    let (user, target) = target.split_once('@').unwrap_or(("admin", target));
                    let (host, port) = target.rsplit_once(':').unwrap_or((target, "22"));
                    let s = Session {
                        id: "dev".into(),
                        name: "dev_sshd".into(),
                        host: host.into(),
                        port: port.parse().unwrap_or(22),
                        common: sessions::Common {
                            username: Some(user.into()),
                            reconnect: Some(false),
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    self.sessions.push(s);
                    self.open_session(&self.ctx_for_dev.clone(), "dev");
                }
                ("edit", _) => {
                    if let Some(s) = self.sessions.iter().find(|s| s.name == "web-01") {
                        self.editor = Some(crate::editor::Editor::edit_session(s.clone()));
                    }
                }
                ("edit-as", proto) => {
                    if let Some(s) = self.sessions.iter().find(|s| s.name == "web-01") {
                        let mut s = s.clone();
                        s.protocol = match proto {
                            "telnet" => sessions::Protocol::Telnet,
                            _ => sessions::Protocol::Serial,
                        };
                        s.extra.insert("auto_login".into(), true.into());
                        self.editor = Some(crate::editor::Editor::edit_session(s));
                    }
                }
                ("hostkey", _) => {
                    let (tx, _rx) = tokio::sync::oneshot::channel();
                    std::mem::forget(_rx);
                    self.asks.push_back(Ask::HostKey {
                        host: "web-02".into(),
                        port: 22,
                        key_type: "ssh-ed25519".into(),
                        fingerprint: "SHA256:Qa71ZbN3cR8mK2wD5hLp0sVt4yXe9fUj6iGo1nTqMvA".into(),
                        changed: Some("SHA256:3fK9pQx7Lm2vT0aWcE8yHn4RzB1uJd6sGk5oVi9XtYw".into()),
                        reply: tx,
                    });
                }
                ("unlock", _) => {
                    if !moliterm_core::vault::exists(&self.db)
                        && let Ok((k, salt)) = moliterm_core::vault::new_key("demo-password")
                    {
                        let _ = Vault::create(&self.db, k, &salt);
                    }
                    self.request_unlock();
                }
                ("section", v) => {
                    if let Some(p) = &mut self.settings_page {
                        p.dev_section(v);
                    }
                }
                _ => {}
            }
        }
    }

    /// Debug builds: the mockup's session tree.
    #[cfg(any(debug_assertions, feature = "devtools"))]
    fn seed_demo(&mut self) {
        if !self.folders.is_empty() {
            return;
        }
        let folder =
            |name: &str, parent: Option<&Folder>, color: Option<&str>, user: Option<&str>, jump: Option<&str>| Folder {
                id: sessions::new_id(),
                parent: parent.map(|p| p.id.clone()),
                name: name.into(),
                sort: 0,
                common: sessions::Common {
                    color: color.map(Into::into),
                    username: user.map(Into::into),
                    jump: jump.map(Into::into),
                    ..Default::default()
                },
            };
        let prod = folder("Production", None, Some("#f07178"), Some("admin"), Some("bastion.prod"));
        let web = folder("web", Some(&prod), None, None, None);
        let staging = folder("Staging", None, Some("#e6b450"), None, None);
        let network = folder("Network", None, Some("#59c2ff"), None, None);
        for f in [&prod, &web, &staging, &network] {
            let _ = sessions::save_folder(&self.db, f);
        }
        let list = [
            ("web-01", &web, sessions::Protocol::Ssh, "10.20.1.11"),
            ("web-02", &web, sessions::Protocol::Ssh, "10.20.1.12"),
            ("web-03", &web, sessions::Protocol::Ssh, "10.20.1.13"),
            ("db-primary", &prod, sessions::Protocol::Ssh, "10.20.2.10"),
            ("win-ad-01", &prod, sessions::Protocol::Rdp, "10.20.3.5"),
            ("stage-app", &staging, sessions::Protocol::Ssh, "10.30.1.5"),
            ("stage-win", &staging, sessions::Protocol::Rdp, "10.30.3.5"),
            ("legacy-sw", &staging, sessions::Protocol::Telnet, "10.30.9.2"),
            ("core-sw-01", &network, sessions::Protocol::Telnet, "10.0.0.1"),
            ("edge-rtr-01", &network, sessions::Protocol::Ssh, "10.0.0.254"),
        ];
        for (i, (name, f, proto, host)) in list.into_iter().enumerate() {
            let s = Session {
                id: sessions::new_id(),
                folder: Some(f.id.clone()),
                name: name.into(),
                sort: i as i64,
                protocol: proto,
                host: host.into(),
                port: proto.default_port(),
                ..Default::default()
            };
            let _ = sessions::save_session(&self.db, &s);
        }
        self.reload_sessions();
    }

    pub fn theme(&self, name: &str) -> &Theme {
        self.themes.iter().find(|t| t.name == name).unwrap_or(&self.themes[0])
    }

    /// Settings as currently shown: the unsaved draft while Settings is open.
    pub(crate) fn live(&self) -> &Settings {
        self.settings_page.as_ref().map_or(&self.settings, |p| &p.draft)
    }

    /// Name of the UI theme to show now (follows OS light/dark if enabled).
    fn ui_theme_name(&self, ctx: &egui::Context) -> String {
        let s = self.live();
        if s.follow_os {
            return match ctx.system_theme() {
                Some(egui::Theme::Light) => s.os_light_theme.clone(),
                _ => s.os_dark_theme.clone(),
            };
        }
        s.theme.clone()
    }

    pub(crate) fn ui_theme(&self) -> &Theme {
        self.theme(&self.applied.theme)
    }

    pub(crate) fn term_theme(&self) -> &Theme {
        self.theme(&self.applied.term_theme)
    }

    fn sync_applied(&mut self, ctx: &egui::Context) {
        if let Some(loaded) = self.fonts_pending.take() {
            self.fonts = Some(loaded);
        }
        if let Some(loaded) = self.fonts_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            ctx.set_fonts(loaded.defs.clone());
            self.fonts_pending = Some(loaded);
            self.fonts_rx = None;
        }
        if let Some(hosts) = self.sshcfg_rx.try_iter().last() {
            self.sshcfg = hosts;
        }
        if let Some(shells) = self.shells_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            self.shells = shells;
            self.shells_rx = None;
        }
        if self.rules.0 != self.live().keyword_rules {
            let rules = self.live().keyword_rules.clone();
            self.rules = (rules.clone(), Compiled::new(&rules));
        }
        let s = self.live().clone();
        let ui_theme = self.ui_theme_name(ctx);
        let want = Applied {
            term_theme: if s.link_themes {
                ui_theme.clone()
            } else {
                s.terminal_theme.clone()
            },
            theme: ui_theme,
            zoom: s.ui_scale,
            compact: s.compact,
            lang: i18n::resolve(&s.language),
            term_font: Some(s.term_font.clone()),
        };
        if want == self.applied {
            return;
        }
        if want.lang != self.applied.lang {
            self.i18n = std::rc::Rc::new(I18n::new(want.lang));
            #[cfg(target_os = "macos")]
            self.menu.rebuild(&self.i18n);
        }
        if want.term_font != self.applied.term_font
            && let Some(f) = &self.fonts
        {
            self.fonts_rx = Some(fonts::spawn(ctx.clone(), Some(Arc::clone(&f.db)), s.term_font.clone()));
        }
        if let Ok(mut p) = self.palette.write() {
            *p = self.theme(&want.term_theme).term.clone();
        }
        style::apply(ctx, self.theme(&want.theme), want.zoom, want.compact);
        self.applied = want;
    }

    fn shortcuts(&self, ctx: &egui::Context) -> Vec<Action> {
        if self.settings_page.as_ref().is_some_and(|p| p.capturing()) || self.macros.capturing {
            return vec![];
        }
        let mut table = crate::hotkeys::active(&self.settings.hotkeys);
        table.extend(
            self.macros
                .list
                .iter()
                .enumerate()
                .filter_map(|(i, m)| crate::hotkeys::parse(&m.hotkey).map(|k| (k, Action::RunMacro(i)))),
        );
        // Match the physical key too: with Shift ("1" → "!") or a non-Latin
        // layout (Cmd+T → Cmd+"е") the logical key differs from the shortcut.
        ctx.input_mut(|i| {
            let mut out = vec![];
            i.events.retain(|e| {
                let egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    modifiers,
                    ..
                } = e
                else {
                    return true;
                };
                let hit = table
                    .iter()
                    .find(|(s, _)| crate::hotkeys::matches(s, *key, *physical_key, *modifiers));
                match hit {
                    Some((_, a)) => {
                        out.push(*a);
                        false
                    }
                    None => true,
                }
            });
            out
        })
    }

    pub(crate) fn run(&mut self, ctx: &egui::Context, action: Action) {
        match action {
            Action::OpenSettings => {
                if self.settings_page.is_none() {
                    self.settings_page = Some(settings_ui::Page::new(self.settings.clone()));
                }
            }
            Action::ToggleSidePanel => {
                self.side = match self.side {
                    Some(_) => None,
                    None => Some(self.last_side),
                }
            }
            Action::ShowSide(s) => {
                self.side = if self.side == Some(s) { None } else { Some(s) };
                self.last_side = s;
            }
            Action::Fullscreen => self.toggle_focus(ctx),
            Action::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::NewLocalShell(i) => self.open_local(ctx, i),
            Action::NewSession => self.editor = Some(crate::editor::Editor::new_session(None, &self.settings)),
            Action::LockVault => self.lock_vault(),
            Action::Copy => {
                if let Some(text) = self.active_term().and_then(|t| t.selection_text()) {
                    ctx.copy_text(text);
                }
            }
            Action::Paste => {
                if let Some(id) = self.active_term_id() {
                    self.handle_paste_clipboard(id);
                }
            }
            Action::SplitRight => self.split_active(ctx, crate::layout::Place::Right),
            Action::SplitDown => self.split_active(ctx, crate::layout::Place::Below),
            Action::MultiExec => self.toggle_multi_exec(),
            Action::CloseTab => {
                if let Some(id) = self.active_term_id() {
                    self.close_tab(id);
                }
            }
            Action::NextTab | Action::PrevTab | Action::GoToTab(_) => self.switch_tab(action),
            Action::Find => {
                if let Some(t) = self.active_term() {
                    t.toggle_find();
                }
            }
            Action::RunMacro(i) => self.run_macro(i),
            Action::OpenVault => self.open_vault_page(),
            Action::ToWindow => {
                if let Some(id) = self.active_tab_id() {
                    self.move_to_window(id);
                }
            }
            Action::FontBigger | Action::FontSmaller | Action::FontReset => {
                let mut s = self.settings.clone();
                s.term_font_size = match action {
                    Action::FontBigger => (s.term_font_size + 1.0).min(72.0),
                    Action::FontSmaller => (s.term_font_size - 1.0).max(6.0),
                    _ => Settings::default().term_font_size,
                };
                self.save_settings(s);
            }
        }
    }

    /// One window's content (main or detached): chrome, dock, shortcuts.
    pub(crate) fn window_frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let mut actions = vec![];
        if self.dialog.is_none() && self.editor.is_none() {
            actions.extend(self.shortcuts(&ctx));
        }
        let theme = self.ui_theme().clone();
        // Left OS full screen some other way (green button, Esc on macOS): leave tab full screen too.
        if self
            .focus
            .is_some_and(|t| t.elapsed() > std::time::Duration::from_millis(1500))
            && ctx.input(|i| i.viewport().fullscreen) == Some(false)
        {
            self.focus = None;
        }
        if self.focus.is_none() {
            self.chrome(ui, &theme, &mut actions);
        }
        // Background tabs too: state dot, title, cwd, OSC 52 stay current.
        let mut reqs = vec![];
        for (_, t) in self.dock.iter_all_tabs_mut() {
            match t {
                Tab::Term(t) => {
                    let mut r = vec![];
                    t.poll_events(&mut r);
                    reqs.extend(r.into_iter().map(|x| (t.id, x)));
                }
                Tab::Desk(d) => d.poll_events(),
                Tab::Welcome | Tab::Vault => {}
            }
        }
        if self.focus.is_some() {
            reqs.extend(self.focus_ui(ui, &theme));
        } else {
            reqs.extend(self.dock_ui(ui, &theme));
        }
        self.handle_reqs(&ctx, reqs);
        for a in actions {
            self.run(&ctx, a);
        }
    }

    pub(crate) fn save_settings(&mut self, new: Settings) {
        if let Err(e) = new.save(&self.settings, &self.db) {
            tracing::error!("saving settings: {e}");
        }
        self.settings = new;
    }

    // ---- tabs -----------------------------------------------------------

    pub(crate) fn terms(&mut self) -> impl Iterator<Item = &mut TermTab> {
        self.dock.iter_all_tabs_mut().filter_map(|(_, t)| match t {
            Tab::Term(t) => Some(&mut **t),
            _ => None,
        })
    }

    pub(crate) fn terms_ref(&self) -> impl Iterator<Item = &TermTab> {
        self.dock.iter_all_tabs().filter_map(|(_, t)| match t {
            Tab::Term(t) => Some(&**t),
            _ => None,
        })
    }

    pub(crate) fn term(&mut self, id: u64) -> Option<&mut TermTab> {
        self.terms().find(|t| t.id == id)
    }

    /// Active tab of the focused leaf, or of the first leaf before anything got focus.
    pub(crate) fn active_term(&mut self) -> Option<&mut TermTab> {
        let path = self
            .dock
            .focused_leaf()
            .or_else(|| self.dock.iter_leaves().next().map(|(p, _)| p))?;
        let leaf = self.dock.leaf_mut(path).ok()?;
        match leaf.tabs.get_mut(leaf.active.0) {
            Some(Tab::Term(t)) => Some(t),
            _ => None,
        }
    }

    pub(crate) fn desk(&mut self, id: u64) -> Option<&mut crate::desk_view::DeskTab> {
        self.dock.iter_all_tabs_mut().find_map(|(_, t)| match t {
            Tab::Desk(d) if d.id == id => Some(&mut **d),
            _ => None,
        })
    }

    /// Active desktop tab of the focused leaf.
    pub(crate) fn active_desk(&mut self) -> Option<&mut crate::desk_view::DeskTab> {
        let path = self
            .dock
            .focused_leaf()
            .or_else(|| self.dock.iter_leaves().next().map(|(p, _)| p))?;
        let leaf = self.dock.leaf_mut(path).ok()?;
        match leaf.tabs.get_mut(leaf.active.0) {
            Some(Tab::Desk(d)) => Some(d),
            _ => None,
        }
    }

    /// Tab full screen on/off for the window being drawn.
    fn toggle_focus(&mut self, ctx: &egui::Context) {
        self.focus = match self.focus {
            Some(_) => None,
            None => Some(Instant::now()),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.focus.is_some()));
    }

    /// Id of the active tab (terminal or desktop) of the focused leaf.
    pub(crate) fn active_tab_id(&mut self) -> Option<u64> {
        let path = self
            .dock
            .focused_leaf()
            .or_else(|| self.dock.iter_leaves().next().map(|(p, _)| p))?;
        let leaf = self.dock.leaf(path).ok()?;
        leaf.tabs.get(leaf.active.0).and_then(Tab::id)
    }

    pub(crate) fn active_term_id(&mut self) -> Option<u64> {
        self.active_term().map(|t| t.id)
    }

    pub(crate) fn session_log(&self, name: &str) -> Option<SessionLog> {
        let s = &self.settings;
        if !s.log_sessions {
            return None;
        }
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let opts = LogOptions {
            raw: s.log_raw,
            timestamps: s.log_timestamps,
            max_bytes: u64::from(s.log_max_mb) * 1024 * 1024,
        };
        SessionLog::open(self.data_dir.join("logs").join(safe), opts)
            .map_err(|e| tracing::warn!("session log: {e}"))
            .ok()
    }

    pub(crate) fn open_local(&mut self, ctx: &egui::Context, index: Option<usize>) {
        self.open_local_at(ctx, index, crate::layout::Place::Focused);
    }

    pub(crate) fn open_local_at(
        &mut self,
        ctx: &egui::Context,
        index: Option<usize>,
        place: crate::layout::Place,
    ) -> Option<u64> {
        let shell = self
            .shells
            .get(index.unwrap_or(0))
            .cloned()
            .or_else(|| local::detect().into_iter().next())?;
        let tab = self.make_local_tab(ctx, shell);
        let id = tab.id;
        self.place(Tab::Term(Box::new(tab)), place);
        Some(id)
    }

    pub(crate) fn make_local_tab(&mut self, ctx: &egui::Context, shell: Shell) -> TermTab {
        let wake_ctx = ctx.clone();
        let (handle, wire) = terminal::pipeline(
            term_view::INITIAL_SIZE,
            self.settings.scrollback,
            self.palette.clone(),
            self.session_log(&shell.name),
            None,
            Arc::new(move || wake_ctx.request_repaint()),
        );
        let tab = TermTab::new(
            self.next_tab_id(),
            shell.name.clone(),
            "LOC",
            handle,
            wire.clone(),
            Launch::Local(shell.clone()),
        );
        if let Err(e) = local::spawn(&self.rt, &shell, None, tab.size, wire.clone()) {
            wire.event(SessionEvent::Disconnected(e.to_string()));
        }
        tab
    }

    fn reconnect(&mut self, id: u64) {
        let rt = self.rt.clone();
        let ctx = self.ssh_ctx.clone();
        let note = self.i18n.t_args("term-restarted", &[("time", &chrono_now())]);
        if let Some(d) = self.desk(id) {
            if d.pending_start || d.is_dead() {
                if !d.pending_start {
                    d.state = term_view::State::Connecting;
                }
                d.pending_start = false;
                rt.spawn(moliterm_core::rdp::run(d.opts.clone(), ctx, d.link.clone()));
            }
            return;
        }
        let Some(t) = self.term(id) else {
            return;
        };
        if !t.is_dead() {
            return;
        }
        t.wire.notice(&note);
        t.state = term_view::State::Connecting;
        match &t.launch {
            Launch::Local(shell) => {
                if let Err(e) = local::spawn(&rt, shell, t.cwd.clone().map(PathBuf::from), t.size, t.wire.clone()) {
                    t.wire.event(SessionEvent::Disconnected(e.to_string()));
                }
            }
            _ => crate::sessions_ui::start(&rt, &ctx, t),
        }
    }

    pub(crate) fn close_tab(&mut self, id: u64) {
        let in_window = self.in_window;
        if let Some(path) = self.dock.find_tab_from(|t| t.id() == Some(id))
            && let Some(t) = self.dock.remove_tab(path)
        {
            t.close();
        }
        self.quick_sessions.remove(&id);
        if id == crate::dock::VAULT_TAB {
            // Forget revealed secrets and drafts with the tab.
            self.vault_page = None;
        }
        self.drop_sftp(id);
        if self.dock.iter_all_tabs().next().is_none() && !in_window {
            self.dock.push_to_first_leaf(Tab::Welcome);
        }
    }

    fn switch_tab(&mut self, action: Action) {
        let Some(path) = self.dock.focused_leaf() else {
            return;
        };
        let Ok(leaf) = self.dock.leaf_mut(path) else {
            return;
        };
        let n = leaf.tabs.len();
        if n == 0 {
            return;
        }
        let cur = leaf.active.0;
        let next = match action {
            Action::NextTab => (cur + 1) % n,
            Action::PrevTab => (cur + n - 1) % n,
            Action::GoToTab(i) if i < n => i,
            _ => return,
        };
        leaf.active = egui_dock::TabIndex(next);
    }

    fn handle_reqs(&mut self, ctx: &egui::Context, reqs: Vec<(u64, Req)>) {
        for (id, req) in reqs {
            match req {
                Req::Paste(text) => self.paste(id, text),
                Req::PasteClipboard => self.handle_paste_clipboard(id),
                Req::AskRightClick => self.dialog = Some(Dialog::RightClick { tab: id }),
                Req::Osc52(text) => match self.term(id).and_then(|t| t.osc52) {
                    Some(true) => ctx.copy_text(text),
                    Some(false) => {}
                    None => self.dialog = Some(Dialog::Osc52 { tab: id, text }),
                },
                Req::OpenUrl(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
                Req::SaveScrollback(text) => {
                    let name = self.term(id).map(|t| t.name.clone()).unwrap_or_default();
                    let file = rfd::AsyncFileDialog::new()
                        .set_file_name(format!("{name}.txt"))
                        .save_file();
                    self.rt.spawn(async move {
                        if let Some(handle) = file.await
                            && let Err(e) = tokio::fs::write(handle.path(), text).await
                        {
                            tracing::warn!("save scrollback: {e}");
                        }
                    });
                }
                Req::Reconnect => self.reconnect(id),
                Req::MoveToWindow => self.move_to_window(id),
                Req::Close => self.close_tab(id),
                Req::Fullscreen => self.toggle_focus(ctx),
                Req::SaveAsSession => {
                    if let Some(s) = self.quick_sessions.get(&id).cloned() {
                        self.editor = Some(crate::editor::Editor::new_from(s));
                    }
                }
                Req::TypePassword => self.type_password = Some(id),
                Req::EditSession => {
                    let sid = self.dock.iter_all_tabs().find_map(|(_, t)| match t {
                        Tab::Term(x) if x.id == id => x.session_id.clone(),
                        Tab::Desk(x) if x.id == id => x.session_id.clone(),
                        _ => None,
                    });
                    if let Some(s) = sid.and_then(|sid| self.sessions.iter().find(|s| s.id == sid).cloned()) {
                        self.editor = Some(crate::editor::Editor::edit_session(s));
                    }
                }
            }
        }
    }

    /// Multi-line pastes go through the confirmation dialog (SPEC §7.4). In
    /// multi-exec live mode the paste goes to every included tile.
    pub(crate) fn paste(&mut self, id: u64, text: String) {
        let ask = self.settings.paste_confirm;
        let targets: Vec<u64> = match &self.multi {
            Some(m) if m.mode == crate::multiexec::Mode::Live && m.tabs.contains(&id) && !m.excluded.contains(&id) => {
                m.tabs.iter().copied().filter(|t| !m.excluded.contains(t)).collect()
            }
            _ => vec![id],
        };
        let no_ask = self.term(id).is_some_and(|t| t.paste_no_ask);
        let multi_line = text.trim_end_matches(['\n', '\r']).contains('\n');
        if (ask && !no_ask && multi_line) || targets.len() > 1 {
            self.dialog = Some(Dialog::Paste {
                tab: id,
                targets,
                text,
                by_line: false,
                no_ask: false,
            });
        } else if let Some(t) = self.term(id) {
            t.paste_now(&text);
        }
    }

    pub(crate) fn handle_paste_clipboard(&mut self, id: u64) {
        if let Some(text) = clipboard_text() {
            self.paste(id, text);
        }
    }

    pub(crate) fn set_right_click(&mut self, choice: RightClick) {
        let mut s = self.settings.clone();
        s.right_click = choice;
        self.save_settings(s);
    }
}

pub(crate) fn chrono_now() -> String {
    moliterm_core::termlog::local_time()
        .get(11..16)
        .unwrap_or_default()
        .to_owned()
}

fn clipboard_text() -> Option<String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.get_text())
        .map_err(|e| tracing::debug!("clipboard: {e}"))
        .ok()
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        static FIRST: std::sync::Once = std::sync::Once::new();
        FIRST.call_once(|| {
            if let Some(t) = crate::STARTED.get() {
                tracing::info!("first frame after {} ms", t.elapsed().as_millis());
            }
        });
        #[cfg(any(debug_assertions, feature = "devtools"))]
        if let Some(d) = &mut self.devshot {
            d.tick(&ctx);
        }
        self.sync_applied(&ctx);
        self.capture_macro_hotkey(&ctx);
        self.tabs_shown.0 = std::mem::take(&mut self.tabs_shown.1);

        let menu_actions: Vec<Action> = self.actions.try_iter().collect();
        let theme = self.ui_theme().clone();
        self.clear_clipboard_when_due(&ctx);
        if self.settings_page.is_some() {
            self.settings_screen(ui, &theme);
        } else {
            self.window_frame(ui);
        }
        self.pump_asks(&ctx);
        self.pump_sftp(&ctx);
        self.editor_ui(&ctx, &theme);
        self.tunnel_editor_ui(&ctx, &theme);
        self.macro_editor_ui(&ctx, &theme);
        self.type_password_ui(&ctx, &theme);
        self.dialogs(&ctx, &theme);
        self.multi_exec_picker(&ctx, &theme);
        self.jobs_window(&ctx, &theme);
        self.ask_dialogs(&ctx, &theme);
        for a in menu_actions {
            self.run(&ctx, a);
        }
        self.windows_ui(&ctx);
        // The tab bar highlights last frame's active tabs; redraw once when they change.
        if self.tabs_shown.0 != self.tabs_shown.1 {
            ctx.request_repaint();
        }
    }

    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        self.save_layout();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        c(self.ui_theme().ui.bg).to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all_tunnels();
        let ids: Vec<u64> = self.sftp.keys().copied().collect();
        for id in ids {
            self.drop_sftp(id);
        }
        for (_, t) in self.dock.iter_all_tabs() {
            t.close();
        }
        for w in &self.windows {
            for (_, t) in w.dock.iter_all_tabs() {
                t.close();
            }
        }
    }
}
