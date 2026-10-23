//! SFTP side panel (SPEC §6), bound to the active SSH tab and its connection.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use egui::{RichText, Sense, Stroke, vec2};
use moliterm_core::{
    remote::{self, Entry, Remote},
    ssh::SshHandle,
    theme::Theme,
    transfer::{Dir, OnConflict, Queue, State},
};

use crate::{
    app::App,
    dialogs::primary,
    icons::{self, Icon},
    style::c,
    term_view::{Launch, TermTab},
};

/// Files chosen in a native dialog, delivered back to the UI thread.
pub enum Picked {
    Upload(u64, Vec<PathBuf>),
    /// Target folder and (remote path, size) of each selected file.
    Download(u64, PathBuf, Vec<(String, u64)>),
}

/// A remote file operation started from a dialog.
type Op = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

enum Ev {
    Opened(Result<Arc<Remote>, String>),
    Listed(String, Result<Vec<Entry>, String>),
    Done(Result<(), String>),
    EditReady {
        remote: String,
        local: PathBuf,
        stop: Arc<AtomicBool>,
    },
    Uploaded(String),
    Hook(Result<String, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Name,
    Size,
    Perms,
}

enum FileDialog {
    NewFolder(String),
    Rename { from: String, to: String },
    Delete(Vec<(String, bool)>),
    Chmod { path: String, mode: u32, octal: String },
}

#[derive(PartialEq, Eq)]
enum Offer {
    Waiting,
    Show,
    Done,
}

pub struct SftpTab {
    tx: mpsc::Sender<Ev>,
    rx: mpsc::Receiver<Ev>,
    remote: Option<Arc<Remote>>,
    opening: bool,
    path: String,
    path_edit: String,
    entries: Vec<Entry>,
    loading: bool,
    error: Option<String>,
    selected: HashSet<String>,
    sort: (Sort, bool),
    follow: bool,
    show_hidden: bool,
    queue: Queue,
    edits: Vec<(String, PathBuf, Arc<AtomicBool>)>,
    last_cwd: Option<String>,
    offer: Offer,
    connected_at: Option<Instant>,
    dialog: Option<FileDialog>,
    conflict_all: bool,
    notice: Option<String>,
    speeds: HashMap<u64, (Instant, u64, f64)>,
}

impl SftpTab {
    fn new(follow: bool, show_hidden: bool) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            remote: None,
            opening: false,
            path: String::new(),
            path_edit: String::new(),
            entries: vec![],
            loading: false,
            error: None,
            selected: HashSet::new(),
            sort: (Sort::Name, true),
            follow,
            show_hidden,
            queue: Queue::default(),
            edits: vec![],
            last_cwd: None,
            offer: Offer::Waiting,
            connected_at: None,
            dialog: None,
            conflict_all: false,
            notice: None,
            speeds: HashMap::new(),
        }
    }

    /// Stops watchers, cancels transfers and removes temp files of external edits.
    pub fn shutdown(&mut self, tab_id: u64) {
        for (_, _, stop) in &self.edits {
            stop.store(true, Ordering::Relaxed);
        }
        self.queue.cancel_all();
        let _ = std::fs::remove_dir_all(edit_dir(tab_id));
    }
}

fn edit_dir(tab_id: u64) -> PathBuf {
    std::env::temp_dir()
        .join(format!("moliterm-edit-{}", std::process::id()))
        .join(tab_id.to_string())
}

/// Opens a file with the OS default application.
pub fn open_with_os(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(path)
        .spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = r {
        tracing::warn!("open file: {e}");
    }
}

fn perms_text(mode: u32, dir: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if dir { 'd' } else { '-' });
    for (bit, ch) in [
        (0o400, 'r'),
        (0o200, 'w'),
        (0o100, 'x'),
        (0o40, 'r'),
        (0o20, 'w'),
        (0o10, 'x'),
        (0o4, 'r'),
        (0o2, 'w'),
        (0o1, 'x'),
    ] {
        s.push(if mode & bit != 0 { ch } else { '-' });
    }
    s
}

pub fn human_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

const HOOK_BASH: &str = "# MoliTerm: report the current folder (OSC 7) so the SFTP panel can follow it\n_moliterm_osc7() { printf '\\033]7;file://%s%s\\033\\\\' \"${HOSTNAME:-$(hostname)}\" \"$PWD\"; }\nPROMPT_COMMAND=\"_moliterm_osc7${PROMPT_COMMAND:+;$PROMPT_COMMAND}\"";
const HOOK_ZSH: &str = "# MoliTerm: report the current folder (OSC 7) so the SFTP panel can follow it\n_moliterm_osc7() { printf '\\033]7;file://%s%s\\033\\\\' \"$HOST\" \"$PWD\"; }\nautoload -Uz add-zsh-hook && add-zsh-hook precmd _moliterm_osc7";

/// The parts of the active tab the panel needs.
struct Bound {
    id: u64,
    live: Option<SshHandle>,
    cwd: Option<String>,
    connected: bool,
    target: String,
    host: String,
}

fn bound(t: &TermTab) -> Option<Bound> {
    let Launch::Ssh { live, options } = &t.launch else {
        return None;
    };
    Some(Bound {
        id: t.id,
        live: live.lock().ok().and_then(|l| l.clone()),
        cwd: t.cwd.clone(),
        connected: matches!(t.state, crate::term_view::State::Connected),
        target: format!("{}@{}", options.target.user, options.target.label),
        host: options.target.host.clone(),
    })
}

impl App {
    /// Keeps transfers running for every tab, also when the panel is hidden.
    pub(crate) fn pump_sftp(&mut self, ctx: &egui::Context) {
        let parallel = self.settings.sftp_parallel as usize;
        let wake_ctx = ctx.clone();
        let wake: moliterm_core::terminal::Waker = Arc::new(move || wake_ctx.request_repaint());
        for s in self.sftp.values_mut() {
            if let Some(r) = &s.remote {
                s.queue.pump(r, parallel, &self.rt, &wake);
            }
            if s.queue.running() > 0 {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
    }

    pub(crate) fn sftp_panel(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let muted = ui.visuals().weak_text_color();
        let Some(b) = self.active_term().and_then(|t| bound(t)) else {
            ui.add_space(8.0);
            ui.label(RichText::new(self.i18n.t("panel-sftp")).small().strong().color(muted));
            ui.add_space(8.0);
            ui.label(RichText::new(self.i18n.t("sftp-none")).color(muted));
            return;
        };
        let (follow, hidden) = (self.settings.sftp_follow, self.settings.sftp_show_hidden);
        let start_dir = self
            .active_term()
            .and_then(|t| t.session_id.clone())
            .and_then(|id| self.sessions.iter().find(|s| s.id == id).cloned())
            .and_then(|s| {
                moliterm_core::sessions::resolve(&s.common, &self.folders, s.folder.as_deref())
                    .common
                    .sftp_dir
            })
            .filter(|d| !d.trim().is_empty());
        let s = self.sftp.entry(b.id).or_insert_with(|| SftpTab::new(follow, hidden));
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);

        // Connection & events.
        if b.connected && s.connected_at.is_none() {
            s.connected_at = Some(Instant::now());
        }
        if !b.connected {
            s.remote = None;
            s.opening = false;
            s.connected_at = None;
        }
        if s.remote.is_none()
            && !s.opening
            && let Some(h) = b.live.clone()
        {
            s.opening = true;
            let tx = s.tx.clone();
            let wake = ui.ctx().clone();
            self.rt.spawn(async move {
                let _ = tx.send(Ev::Opened(Remote::open(&h).await.map(Arc::new)));
                wake.request_repaint();
            });
        }
        let mut navigate: Option<String> = None;
        let mut refresh = false;
        while let Ok(ev) = s.rx.try_recv() {
            match ev {
                Ev::Opened(Ok(r)) => {
                    s.opening = false;
                    let home_tx = s.tx.clone();
                    let rr = r.clone();
                    let wake = ui.ctx().clone();
                    s.remote = Some(r);
                    if s.path.is_empty() {
                        let start = start_dir.clone();
                        self.rt.spawn(async move {
                            let home = match start {
                                Some(d) => d,
                                None => rr.home().await.unwrap_or_else(|_| "/".into()),
                            };
                            let list = rr.list(&home).await;
                            let _ = home_tx.send(Ev::Listed(home, list));
                            wake.request_repaint();
                        });
                        s.loading = true;
                    } else {
                        refresh = true;
                    }
                }
                Ev::Opened(Err(e)) => {
                    s.opening = false;
                    s.error = Some(e);
                }
                Ev::Listed(path, Ok(list)) => {
                    s.loading = false;
                    s.error = None;
                    if path != s.path {
                        s.selected.clear();
                    }
                    s.path_edit = path.clone();
                    s.path = path;
                    s.entries = list;
                }
                Ev::Listed(_, Err(e)) => {
                    s.loading = false;
                    s.error = Some(e);
                    s.path_edit = s.path.clone();
                }
                Ev::Done(r) => {
                    if let Err(e) = r {
                        s.error = Some(e);
                    }
                    refresh = true;
                }
                Ev::EditReady { remote, local, stop } => {
                    open_with_os(&local);
                    s.edits.push((remote, local, stop));
                }
                Ev::Uploaded(name) => s.notice = Some(i18n.t_args("sftp-uploaded", &[("name", &name)])),
                Ev::Hook(r) => {
                    s.notice = Some(match r {
                        Ok(file) => i18n.t_args("sftp-hook-done", &[("file", &file)]),
                        Err(e) => e,
                    })
                }
            }
        }
        // Edits still alive? (watchers stop themselves on error)
        s.edits.retain(|(_, _, stop)| !stop.load(Ordering::Relaxed));

        // Follow the terminal folder (OSC 7).
        if b.cwd.is_some() {
            s.offer = Offer::Done;
        }
        if s.offer == Offer::Waiting
            && s.follow
            && !self.settings.osc7_no_ask.contains(&b.host)
            && s.connected_at.is_some_and(|t| t.elapsed() > Duration::from_secs(10))
        {
            s.offer = Offer::Show;
        }

        let Some(remote) = s.remote.clone() else {
            ui.add_space(8.0);
            ui.label(RichText::new(t("panel-sftp")).small().strong().color(muted));
            ui.add_space(8.0);
            if let Some(e) = &s.error {
                ui.label(RichText::new(e).color(c(theme.ui.danger)));
            } else {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new(t("sftp-connecting")).color(muted));
                });
            }
            return;
        };

        // Follow only once the remote is open, so a cwd seen earlier is not lost.
        if s.follow && b.cwd != s.last_cwd && !s.loading {
            s.last_cwd = b.cwd.clone();
            if let Some(cwd) = &b.cwd {
                navigate = Some(cwd.clone());
            }
        }

        // Header.
        ui.horizontal(|ui| {
            ui.label(RichText::new(t("panel-sftp")).small().strong().color(muted));
            ui.label(RichText::new(&b.target).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (text, color) = if remote.is_scp() {
                    (t("sftp-badge-scp"), c(theme.ui.warning))
                } else {
                    ("SFTP".to_owned(), c(theme.ui.accent))
                };
                egui::Frame::new()
                    .fill(color.gamma_multiply(0.15))
                    .corner_radius(4)
                    .inner_margin(egui::Margin::symmetric(6, 1))
                    .show(ui, |ui| {
                        ui.label(RichText::new(text).small().monospace().color(color));
                    });
            });
        });

        // Toolbar.
        let mut upload = false;
        let mut download = false;
        ui.horizontal(|ui| {
            if icons::button(ui, Icon::ArrowUp, 14.0, &t("sftp-parent")).clicked() {
                navigate = Some(remote::parent(&s.path));
            }
            if icons::button(ui, Icon::Home, 14.0, &t("sftp-home")).clicked() {
                navigate = Some(String::new());
            }
            if icons::button(ui, Icon::Refresh, 14.0, &t("sftp-refresh")).clicked() {
                refresh = true;
            }
            ui.separator();
            if icons::button(ui, Icon::Upload, 14.0, &t("sftp-upload")).clicked() {
                upload = true;
            }
            if icons::button(ui, Icon::Download, 14.0, &t("sftp-download")).clicked() {
                download = true;
            }
            if icons::button(ui, Icon::FolderPlus, 14.0, &t("sftp-new-folder")).clicked() {
                s.dialog = Some(FileDialog::NewFolder(String::new()));
            }
            let hidden = ui
                .selectable_label(s.show_hidden, RichText::new(".*").monospace())
                .on_hover_text(t("sftp-hidden"));
            if hidden.clicked() {
                s.show_hidden = !s.show_hidden;
            }
        });
        let path_resp = ui.add(
            crate::style::field(&mut s.path_edit)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY),
        );
        if path_resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            navigate = Some(s.path_edit.clone());
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut s.follow, t("sftp-follow"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(t("sftp-via-osc7")).small().color(muted));
            });
        });

        // OSC 7 hook offer (SPEC §6.2): show the exact lines, write only on consent.
        if s.offer == Offer::Show {
            egui::Frame::new()
                .fill(c(theme.ui.surface))
                .corner_radius(6)
                .inner_margin(8)
                .show(ui, |ui| {
                    ui.label(RichText::new(t("sftp-hook-title")).strong());
                    ui.label(RichText::new(t("sftp-hook-body")).small());
                    ui.collapsing(t("sftp-hook-show"), |ui| {
                        ui.label(RichText::new("~/.bashrc").small().strong());
                        ui.label(RichText::new(HOOK_BASH).monospace().small());
                        ui.label(RichText::new("~/.zshrc").small().strong());
                        ui.label(RichText::new(HOOK_ZSH).monospace().small());
                    });
                    ui.horizontal(|ui| {
                        if primary(ui, theme, t("sftp-hook-enable")).clicked() {
                            s.offer = Offer::Done;
                            if let Some(h) = b.live.clone() {
                                let tx = s.tx.clone();
                                let wake = ui.ctx().clone();
                                self.rt.spawn(async move {
                                    let _ = tx.send(Ev::Hook(install_hook(&h).await));
                                    wake.request_repaint();
                                });
                            }
                        }
                        if ui.button(t("sftp-hook-later")).clicked() {
                            s.offer = Offer::Done;
                        }
                        if ui.button(t("sftp-hook-never")).clicked() {
                            s.offer = Offer::Done;
                            let mut st = self.settings.clone();
                            st.osc7_no_ask.push(b.host.clone());
                            if let Err(e) = st.save(&self.settings, &self.db) {
                                tracing::error!("saving settings: {e}");
                            }
                            self.settings = st;
                        }
                    });
                });
        }
        if let Some(n) = s.notice.clone() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(n).small().color(c(theme.ui.success)));
                if ui.small_button("×").clicked() {
                    s.notice = None;
                }
            });
        }
        if let Some(e) = &s.error {
            ui.label(RichText::new(e).small().color(c(theme.ui.danger)));
        }

        // Transfers at the bottom.
        let transfers_h = if s.queue.items.is_empty() {
            0.0
        } else {
            (40.0 * s.queue.items.len().min(4) as f32).min(170.0)
        };

        // File list.
        ui.separator();
        // Column headers, laid out like the rows below (name | size | perms).
        let (hrect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
        let cols = [
            (
                Sort::Name,
                "sftp-col-name",
                egui::Rect::from_min_max(hrect.min, egui::pos2(hrect.right() - 170.0, hrect.bottom())),
            ),
            (
                Sort::Size,
                "sftp-col-size",
                egui::Rect::from_min_max(
                    egui::pos2(hrect.right() - 170.0, hrect.top()),
                    egui::pos2(hrect.right() - 88.0, hrect.bottom()),
                ),
            ),
            (
                Sort::Perms,
                "sftp-col-perms",
                egui::Rect::from_min_max(egui::pos2(hrect.right() - 84.0, hrect.top()), hrect.max),
            ),
        ];
        for (col, key, rect) in cols {
            let arrow = if s.sort.0 == col {
                if s.sort.1 { " ↑" } else { " ↓" }
            } else {
                ""
            };
            let align = if col == Sort::Size {
                egui::Align2::RIGHT_CENTER
            } else {
                egui::Align2::LEFT_CENTER
            };
            let pos = if col == Sort::Size {
                rect.right_center()
            } else {
                rect.left_center() + vec2(if col == Sort::Name { 22.0 } else { 0.0 }, 0.0)
            };
            ui.painter().text(
                pos,
                align,
                format!("{}{arrow}", t(key)),
                egui::FontId::proportional(10.0),
                muted,
            );
            if ui
                .interact(rect, ui.id().with(("sftp-col", key)), Sense::click())
                .clicked()
            {
                s.sort = if s.sort.0 == col { (col, !s.sort.1) } else { (col, true) };
            }
        }
        let mut entries: Vec<Entry> = s
            .entries
            .iter()
            .filter(|e| s.show_hidden || !e.name.starts_with('.'))
            .cloned()
            .collect();
        let (col, asc) = s.sort;
        entries.sort_by(|a, b| {
            let ord = match col {
                Sort::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
                Sort::Size => a.size.cmp(&b.size),
                Sort::Perms => a.perms.cmp(&b.perms),
            };
            b.is_dir.cmp(&a.is_dir).then(if asc { ord } else { ord.reverse() })
        });
        let mut open_entry: Option<Entry> = None;
        let mut edit_entry: Option<Entry> = None;
        let dropping = ui.input(|i| !i.raw.hovered_files.is_empty());
        // Leave room for the transfers header (~34 px) and its list.
        let list_h = (ui.available_height() - transfers_h - 40.0).max(80.0);
        let list_resp = egui::ScrollArea::vertical()
            .id_salt("sftp-list")
            .max_height(list_h)
            .min_scrolled_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if s.path != "/" {
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                    icons::paint(
                        ui.painter(),
                        egui::Rect::from_min_size(rect.left_center() + vec2(2.0, -7.0), vec2(14.0, 14.0)),
                        Icon::Folder,
                        c(theme.term.ansi[12]),
                    );
                    ui.painter().text(
                        rect.left_center() + vec2(22.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        "..",
                        egui::FontId::proportional(13.0),
                        ui.visuals().text_color(),
                    );
                    if resp.double_clicked() {
                        navigate = Some(remote::parent(&s.path));
                    }
                }
                for e in &entries {
                    let full = remote::join(&s.path, &e.name);
                    let selected = s.selected.contains(&e.name);
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                    if selected {
                        ui.painter().rect_filled(rect, 3.0, ui.visuals().selection.bg_fill);
                    } else if resp.hovered() {
                        ui.painter()
                            .rect_filled(rect, 3.0, ui.visuals().widgets.hovered.weak_bg_fill);
                    }
                    let (icon, color) = if e.is_dir {
                        (Icon::Folder, c(theme.term.ansi[12]))
                    } else {
                        (Icon::File, muted)
                    };
                    icons::paint(
                        ui.painter(),
                        egui::Rect::from_min_size(rect.left_center() + vec2(2.0, -7.0), vec2(14.0, 14.0)),
                        icon,
                        color,
                    );
                    let name_color = if e.is_dir {
                        c(theme.term.ansi[12])
                    } else if e.name.starts_with('.') {
                        muted
                    } else {
                        ui.visuals().text_color()
                    };
                    let editing = s.edits.iter().any(|(r, ..)| r == &full);
                    let name_w = rect.width() - 170.0;
                    let galley = ui.painter().layout(
                        e.name.clone(),
                        egui::FontId::proportional(13.0),
                        name_color,
                        f32::INFINITY,
                    );
                    let clipped =
                        egui::Rect::from_min_size(rect.min + vec2(22.0, 0.0), vec2(name_w.max(20.0), rect.height()));
                    ui.painter().with_clip_rect(clipped).galley(
                        rect.left_center() + vec2(22.0, -galley.size().y / 2.0),
                        galley,
                        name_color,
                    );
                    if editing {
                        let badge = t("sftp-editing");
                        let pos = rect.left_center() + vec2(26.0 + (name_w * 0.45).min(e.name.len() as f32 * 7.5), 0.0);
                        let g =
                            ui.painter()
                                .layout_no_wrap(badge, egui::FontId::proportional(10.0), c(theme.ui.warning));
                        let r = egui::Rect::from_min_size(
                            pos - vec2(0.0, g.size().y / 2.0 + 1.0),
                            g.size() + vec2(8.0, 2.0),
                        );
                        ui.painter()
                            .rect_filled(r, 3.0, c(theme.ui.warning).gamma_multiply(0.18));
                        ui.painter().galley(r.min + vec2(4.0, 1.0), g, c(theme.ui.warning));
                    }
                    if !e.is_dir {
                        ui.painter().text(
                            rect.right_center() - vec2(92.0, 0.0),
                            egui::Align2::RIGHT_CENTER,
                            human_size(e.size),
                            egui::FontId::monospace(11.0),
                            muted,
                        );
                    }
                    ui.painter().text(
                        rect.right_center() - vec2(84.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        perms_text(e.perms, e.is_dir),
                        egui::FontId::monospace(11.0),
                        muted,
                    );
                    let tip = e
                        .mtime
                        .and_then(|m| chrono::DateTime::from_timestamp(m, 0))
                        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string());
                    let resp = match tip {
                        Some(tip) => resp.on_hover_text(format!("{}\n{tip}", e.name)),
                        None => resp,
                    };
                    if resp.clicked() {
                        let multi = ui.input(|i| i.modifiers.command || i.modifiers.shift);
                        if !multi {
                            s.selected.clear();
                        }
                        if !s.selected.remove(&e.name) || !multi {
                            s.selected.insert(e.name.clone());
                        }
                    }
                    if resp.double_clicked() {
                        if e.is_dir {
                            open_entry = Some(e.clone());
                        } else {
                            edit_entry = Some(e.clone());
                        }
                    }
                    resp.context_menu(|ui| {
                        if !s.selected.contains(&e.name) {
                            s.selected.clear();
                            s.selected.insert(e.name.clone());
                        }
                        if e.is_dir {
                            if ui.button(t("sftp-open")).clicked() {
                                open_entry = Some(e.clone());
                                ui.close();
                            }
                        } else if ui.button(t("sftp-edit")).clicked() {
                            edit_entry = Some(e.clone());
                            ui.close();
                        }
                        if ui.button(t("sftp-download")).clicked() {
                            download = true;
                            ui.close();
                        }
                        if ui.button(t("sftp-rename")).clicked() {
                            s.dialog = Some(FileDialog::Rename {
                                from: e.name.clone(),
                                to: e.name.clone(),
                            });
                            ui.close();
                        }
                        if ui.button(t("sftp-chmod")).clicked() {
                            s.dialog = Some(FileDialog::Chmod {
                                path: full.clone(),
                                mode: e.perms,
                                octal: format!("{:o}", e.perms),
                            });
                            ui.close();
                        }
                        if ui.button(t("sftp-copy-path")).clicked() {
                            ui.ctx().copy_text(full.clone());
                            ui.close();
                        }
                        ui.separator();
                        if ui.button(t("tree-delete")).clicked() {
                            let list = entries
                                .iter()
                                .filter(|x| s.selected.contains(&x.name))
                                .map(|x| (remote::join(&s.path, &x.name), x.is_dir && !x.is_link))
                                .collect();
                            s.dialog = Some(FileDialog::Delete(list));
                            ui.close();
                        }
                    });
                }
                if s.loading {
                    ui.spinner();
                }
            });
        // Drag files in from the OS.
        let list_rect = list_resp.inner_rect;
        let pointer_in = ui
            .input(|i| i.pointer.hover_pos())
            .is_some_and(|p| list_rect.contains(p));
        if dropping {
            ui.painter().rect_stroke(
                list_rect,
                6.0,
                Stroke::new(2.0, c(theme.ui.accent)),
                egui::StrokeKind::Inside,
            );
            ui.painter().text(
                list_rect.center(),
                egui::Align2::CENTER_CENTER,
                i18n.t_args("sftp-drop", &[("path", &s.path)]),
                egui::FontId::proportional(14.0),
                c(theme.ui.accent),
            );
        }
        let dropped: Vec<PathBuf> = ui.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        if !dropped.is_empty() && (pointer_in || dropping || self.side == Some(crate::app::Side::Sftp)) {
            for p in dropped {
                enqueue_upload(s, p);
            }
        }

        // Transfers.
        ui.separator();
        let running = s.queue.items.len();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(i18n.t_args("sftp-transfers", &[("n", &running.to_string())]))
                    .small()
                    .strong()
                    .color(muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button(t("sftp-clear-done")).clicked() {
                    s.queue.clear_done();
                }
            });
        });
        enum Act {
            Pause,
            Resume,
            Cancel,
        }
        let mut action: Option<(u64, Act)> = None;
        egui::ScrollArea::vertical()
            .id_salt("sftp-queue")
            .max_height(transfers_h)
            .show(ui, |ui| {
                for tr in &s.queue.items {
                    let done = tr.done.load(Ordering::Relaxed);
                    let st = tr.state();
                    let now = Instant::now();
                    let speed = {
                        let e = s.speeds.entry(tr.id).or_insert((now, done, 0.0));
                        let dt = now.duration_since(e.0).as_secs_f64();
                        if dt > 0.5 {
                            let v = (done.saturating_sub(e.1)) as f64 / dt;
                            *e = (now, done, e.2 * 0.5 + v * 0.5);
                        }
                        e.2
                    };
                    ui.horizontal(|ui| {
                        let arrow = if tr.dir == Dir::Up {
                            Icon::Upload
                        } else {
                            Icon::Download
                        };
                        let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                        icons::paint(ui.painter(), r, arrow, muted);
                        ui.label(RichText::new(tr.name()).small()).on_hover_text(format!(
                            "{}\n{}",
                            tr.remote,
                            tr.local.display()
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            match &st {
                                State::Running if ui.small_button("❚❚").on_hover_text(t("sftp-pause")).clicked() => {
                                    action = Some((tr.id, Act::Pause));
                                }
                                State::Paused | State::Failed(_)
                                    if ui.small_button("▶").on_hover_text(t("sftp-resume")).clicked() =>
                                {
                                    action = Some((tr.id, Act::Resume));
                                }
                                _ => {}
                            }
                            if !matches!(st, State::Done | State::Skipped)
                                && ui.small_button("×").on_hover_text(t("sftp-cancel")).clicked()
                            {
                                action = Some((tr.id, Act::Cancel));
                            }
                        });
                    });
                    let frac = if tr.total == 0 {
                        1.0
                    } else {
                        done as f32 / tr.total as f32
                    };
                    let label = match &st {
                        State::Running => format!(
                            "{} / {} · {}/s",
                            human_size(done),
                            human_size(tr.total),
                            human_size(speed as u64)
                        ),
                        State::Queued => t("sftp-queued"),
                        State::Paused => format!("{} · {}", t("sftp-paused"), human_size(done)),
                        State::Done => t("sftp-done"),
                        State::Skipped => t("sftp-skipped"),
                        State::Failed(e) => e.clone(),
                        State::Conflict(_) => t("sftp-conflict"),
                    };
                    ui.add(egui::ProgressBar::new(frac.clamp(0.0, 1.0)).desired_height(4.0));
                    ui.label(RichText::new(label).small().color(if matches!(st, State::Failed(_)) {
                        c(theme.ui.danger)
                    } else {
                        muted
                    }));
                }
            });
        if let Some((id, what)) = action {
            match what {
                Act::Pause => s.queue.pause(id),
                Act::Resume => s.queue.resume(id),
                Act::Cancel => s.queue.cancel(id),
            }
        }

        // Pending work.
        if let Some(e) = open_entry {
            navigate = Some(remote::join(&s.path, &e.name));
        }
        if let Some(e) = edit_entry {
            let full = remote::join(&s.path, &e.name);
            if !s.edits.iter().any(|(r, ..)| r == &full) {
                start_edit(&self.rt, remote.clone(), full, b.id, s.tx.clone(), ui.ctx().clone());
            }
        }
        if upload {
            let tx_files = self.sftp_pick.0.clone();
            let file = rfd::AsyncFileDialog::new().pick_files();
            let wake = ui.ctx().clone();
            let tab = b.id;
            self.rt.spawn(async move {
                if let Some(files) = file.await {
                    let _ = tx_files.send(Picked::Upload(
                        tab,
                        files.iter().map(|f| f.path().to_path_buf()).collect(),
                    ));
                    wake.request_repaint();
                }
            });
        }
        if download {
            let names: Vec<(String, u64)> = s
                .entries
                .iter()
                .filter(|e| s.selected.contains(&e.name) && !e.is_dir)
                .map(|e| (remote::join(&s.path, &e.name), e.size))
                .collect();
            if !names.is_empty() {
                let tx_files = self.sftp_pick.0.clone();
                let folder = rfd::AsyncFileDialog::new().pick_folder();
                let wake = ui.ctx().clone();
                let tab = b.id;
                self.rt.spawn(async move {
                    if let Some(dir) = folder.await {
                        let _ = tx_files.send(Picked::Download(tab, dir.path().to_path_buf(), names));
                        wake.request_repaint();
                    }
                });
            }
        }
        let (tx, cur_path) = (s.tx.clone(), s.path.clone());
        if navigate.is_some() {
            s.loading = true;
        }
        while let Ok(picked) = self.sftp_pick.1.try_recv() {
            match picked {
                Picked::Upload(tab, files) => {
                    if let Some(st) = self.sftp.get_mut(&tab) {
                        files.into_iter().for_each(|p| enqueue_upload(st, p));
                    }
                }
                Picked::Download(tab, dir, files) => {
                    if let Some(st) = self.sftp.get_mut(&tab) {
                        for (remote_path, size) in files {
                            let name = remote_path.rsplit('/').next().unwrap_or(&remote_path).to_owned();
                            st.queue
                                .push(Dir::Down, dir.join(name), remote_path, size, OnConflict::Ask);
                        }
                    }
                }
            }
        }
        self.file_dialogs(ui.ctx(), b.id, theme);
        if let Some(p) = navigate {
            let (r, wake) = (remote.clone(), ui.ctx().clone());
            self.rt.spawn(async move {
                let path = if p.is_empty() {
                    r.home().await.unwrap_or_else(|_| "/".into())
                } else {
                    p
                };
                let list = r.list(&path).await;
                let _ = tx.send(Ev::Listed(path, list));
                wake.request_repaint();
            });
        } else if refresh {
            let (r, wake, path) = (remote.clone(), ui.ctx().clone(), cur_path);
            self.rt.spawn(async move {
                let list = r.list(&path).await;
                let _ = tx.send(Ev::Listed(path, list));
                wake.request_repaint();
            });
        }
    }

    fn file_dialogs(&mut self, ctx: &egui::Context, tab: u64, theme: &Theme) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let Some(s) = self.sftp.get_mut(&tab) else { return };
        let Some(remote) = s.remote.clone() else { return };
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .stroke(Stroke::new(1.0, c(theme.ui.border)))
            .inner_margin(20);
        let mut run: Option<Op> = None;
        let mut close = false;

        // Transfer conflicts come first.
        if let Some(tr) = s.queue.items.iter().find(|x| matches!(x.state(), State::Conflict(_))) {
            let State::Conflict(existing) = tr.state() else { return };
            let (id, total, name) = (tr.id, tr.total, tr.name());
            let mut choice = None;
            egui::Modal::new("sftp-conflict".into()).frame(frame).show(ctx, |ui| {
                crate::style::fit_width(ui, 420.0);
                ui.label(RichText::new(i18n.t_args("sftp-conflict-title", &[("name", &name)])).heading());
                ui.label(i18n.t_args(
                    "sftp-conflict-body",
                    &[("existing", &human_size(existing)), ("total", &human_size(total))],
                ));
                ui.checkbox(&mut s.conflict_all, t("sftp-apply-all"));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if existing < total && primary(ui, theme, t("sftp-resume")).clicked() {
                        choice = Some(OnConflict::Resume);
                    }
                    if ui.button(t("sftp-overwrite")).clicked() {
                        choice = Some(OnConflict::Overwrite);
                    }
                    if ui.button(t("sftp-rename-new")).clicked() {
                        choice = Some(OnConflict::Rename);
                    }
                    if ui.button(t("sftp-skip")).clicked() {
                        choice = Some(OnConflict::Skip);
                    }
                });
            });
            if let Some(ch) = choice {
                let all = s.conflict_all;
                s.queue.decide(id, ch, all);
            }
            return;
        }

        let Some(dialog) = &mut s.dialog else { return };
        let path = s.path.clone();
        egui::Modal::new("sftp-file-dialog".into())
            .frame(frame)
            .show(ctx, |ui| {
                crate::style::fit_width(ui, 400.0);
                match dialog {
                    FileDialog::NewFolder(name) => {
                        ui.label(RichText::new(t("sftp-new-folder")).heading());
                        let r = ui.add(crate::style::field(name).desired_width(f32::INFINITY));
                        r.request_focus();
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            if ui.button(t("settings-cancel")).clicked() {
                                close = true;
                            }
                            if (primary(ui, theme, t("prompt-ok")).clicked() || enter) && !name.trim().is_empty() {
                                let (r, p) = (remote.clone(), remote::join(&path, name.trim()));
                                run = Some(Box::pin(async move { r.mkdir(&p).await }));
                            }
                        });
                    }
                    FileDialog::Rename { from, to } => {
                        ui.label(RichText::new(t("sftp-rename")).heading());
                        let r = ui.add(crate::style::field(to).desired_width(f32::INFINITY));
                        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        ui.horizontal(|ui| {
                            if ui.button(t("settings-cancel")).clicked() {
                                close = true;
                            }
                            if (primary(ui, theme, t("prompt-ok")).clicked() || enter) && !to.trim().is_empty() {
                                let (r, a, b) = (
                                    remote.clone(),
                                    remote::join(&path, from),
                                    remote::join(&path, to.trim()),
                                );
                                run = Some(Box::pin(async move { r.rename(&a, &b).await }));
                            }
                        });
                    }
                    FileDialog::Delete(list) => {
                        ui.label(
                            RichText::new(i18n.t_args("sftp-delete-title", &[("n", &list.len().to_string())]))
                                .heading(),
                        );
                        for (p, _) in list.iter().take(8) {
                            ui.label(RichText::new(p).monospace().small());
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button(t("settings-cancel")).clicked() {
                                close = true;
                            }
                            let danger =
                                egui::Button::new(RichText::new(t("tree-delete")).color(c(theme.ui.on_accent)))
                                    .fill(c(theme.ui.danger));
                            if ui.add(danger).clicked() {
                                let (r, items) = (remote.clone(), list.clone());
                                run = Some(Box::pin(async move {
                                    for (p, dir) in items {
                                        r.remove(&p, dir).await?;
                                    }
                                    Ok(())
                                }));
                            }
                        });
                    }
                    FileDialog::Chmod { path: p, mode, octal } => {
                        ui.label(RichText::new(t("sftp-chmod")).heading());
                        ui.label(RichText::new(p.as_str()).monospace().small());
                        egui::Grid::new("chmod").show(ui, |ui| {
                            ui.label("");
                            for k in ["sftp-read", "sftp-write", "sftp-exec"] {
                                ui.label(t(k));
                            }
                            ui.end_row();
                            for (row, key) in [(6u32, "sftp-owner"), (3, "sftp-group"), (0, "sftp-others")] {
                                ui.label(t(key));
                                for bit in [4u32, 2, 1] {
                                    let mask = bit << row;
                                    let mut on = *mode & mask != 0;
                                    if ui.checkbox(&mut on, "").changed() {
                                        *mode = if on { *mode | mask } else { *mode & !mask };
                                        *octal = format!("{:o}", *mode);
                                    }
                                }
                                ui.end_row();
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(t("sftp-octal"));
                            if ui
                                .add(
                                    crate::style::field(octal)
                                        .desired_width(60.0)
                                        .font(egui::TextStyle::Monospace),
                                )
                                .changed()
                                && let Ok(m) = u32::from_str_radix(octal.trim(), 8)
                            {
                                *mode = m & 0o7777;
                            }
                        });
                        ui.horizontal(|ui| {
                            if ui.button(t("settings-cancel")).clicked() {
                                close = true;
                            }
                            if primary(ui, theme, t("prompt-ok")).clicked() {
                                let (r, p, m) = (remote.clone(), p.clone(), *mode);
                                run = Some(Box::pin(async move { r.chmod(&p, m).await }));
                            }
                        });
                    }
                }
            });
        if let Some(fut) = run {
            let (tx, wake) = (s.tx.clone(), ctx.clone());
            self.rt.spawn(async move {
                let _ = tx.send(Ev::Done(fut.await));
                wake.request_repaint();
            });
            close = true;
        }
        if close {
            s.dialog = None;
        }
    }

    pub(crate) fn drop_sftp(&mut self, tab: u64) {
        if let Some(mut s) = self.sftp.remove(&tab) {
            s.shutdown(tab);
        }
    }
}

fn enqueue_upload(s: &mut SftpTab, local: PathBuf) {
    let Ok(meta) = std::fs::metadata(&local) else { return };
    if !meta.is_file() {
        return;
    }
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    s.queue.push(
        Dir::Up,
        local,
        remote::join(&s.path, &name),
        meta.len(),
        OnConflict::Ask,
    );
}

/// Downloads to a temp file, then uploads it again whenever it is saved.
fn start_edit(
    rt: &tokio::runtime::Handle,
    remote: Arc<Remote>,
    path: String,
    tab: u64,
    tx: mpsc::Sender<Ev>,
    wake: egui::Context,
) {
    rt.spawn(async move {
        let name = path.rsplit('/').next().unwrap_or("file").to_owned();
        let dir = edit_dir(tab).join(format!("{:x}", hash_str(&path)));
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            let _ = tx.send(Ev::Done(Err(e.to_string())));
            return;
        }
        let local = dir.join(&name);
        let (done, cancel) = (Default::default(), AtomicBool::new(false));
        if let Err(e) = remote.download(&path, &local, 0, &done, &cancel).await {
            let _ = tx.send(Ev::Done(Err(e)));
            wake.request_repaint();
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let _ = tx.send(Ev::EditReady {
            remote: path.clone(),
            local: local.clone(),
            stop: stop.clone(),
        });
        wake.request_repaint();
        let modified = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let mut last = modified(&local);
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let now = modified(&local);
            if now.is_some() && now != last {
                last = now;
                // Wait until the editor finished writing.
                tokio::time::sleep(Duration::from_millis(300)).await;
                let done = Default::default();
                match remote.upload(&local, &path, 0, &done, &AtomicBool::new(false)).await {
                    Ok(()) => {
                        let _ = tx.send(Ev::Uploaded(name.clone()));
                    }
                    Err(e) => {
                        let _ = tx.send(Ev::Done(Err(e)));
                    }
                }
                wake.request_repaint();
            }
        }
    });
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Appends the OSC 7 hook to the remote shell's rc file (only after consent).
async fn install_hook(h: &SshHandle) -> Result<String, String> {
    let (out, _) = remote::exec(h, "echo $SHELL").await?;
    let shell = String::from_utf8_lossy(&out);
    let (file, hook) = if shell.trim().ends_with("zsh") {
        ("~/.zshrc", HOOK_ZSH)
    } else {
        ("~/.bashrc", HOOK_BASH)
    };
    let cmd = format!("printf '\\n%s\\n' {} >> {file}", remote::quote(hook));
    let (out, status) = remote::exec(h, &cmd).await?;
    if status == 0 {
        Ok(file.to_owned())
    } else {
        Err(String::from_utf8_lossy(&out).into_owned())
    }
}
