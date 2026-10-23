//! Multi-exec (SPEC §8): one input to many terminal tabs of the current window.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use egui::{Rect, RichText, Stroke, UiBuilder, pos2, vec2};
use moliterm_core::{remote, sessions, theme::Theme};

use crate::{
    app::App,
    dialogs::{Dialog, primary},
    dock::Tab,
    icons::{self, Icon},
    style::c,
    term_view::{Cx, Launch, Req, TermTab},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Live,
    Line,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Grid {
    Auto,
    Two,
    Three,
    Columns,
}

pub struct MultiExec {
    pub tabs: Vec<u64>,
    pub excluded: HashSet<u64>,
    pub mode: Mode,
    pub grid: Grid,
    line: String,
    history: Vec<String>,
    hist_pos: Option<usize>,
}

/// Target choice shown before entering (SPEC §8.1).
pub struct Picker {
    pub choice: usize,
    pub folder: Option<String>,
    pub tag: Option<String>,
}

pub enum JobState {
    Waiting,
    Running(Instant),
    Done { result: remote::ExecResult, took: Duration },
    Skipped(String),
    Failed(String),
}

pub struct JobRow {
    pub host: String,
    pub state: JobState,
}

pub struct Jobs {
    pub cmd: String,
    pub rows: Arc<Mutex<Vec<JobRow>>>,
    pub selected: Option<usize>,
    pub failed_only: bool,
    pub open: bool,
}

/// SPEC §8.6.
pub fn is_risky(cmd: &str) -> bool {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"rm -rf|reboot|shutdown|halt|poweroff|mkfs|dd\s|:\(\)\{").ok())
        .as_ref()
        .is_some_and(|re| re.is_match(cmd))
}

/// `{name} {host} {user} {port} {folder}` for one tab.
fn substitute(line: &str, vars: &[(&str, String)]) -> String {
    vars.iter()
        .fold(line.to_owned(), |acc, (k, v)| acc.replace(&format!("{{{k}}}"), v))
}

impl App {
    pub(crate) fn toggle_multi_exec(&mut self) {
        if self.multi.is_some() {
            self.exit_multi_exec();
        } else {
            self.me_picker = Some(Picker {
                choice: 0,
                folder: None,
                tag: None,
            });
        }
    }

    fn window_term_ids(&self) -> Vec<u64> {
        self.dock
            .iter_all_tabs()
            .filter_map(|(_, t)| match t {
                Tab::Term(t) => Some(t.id),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn enter_multi_exec(&mut self, tabs: Vec<u64>) {
        if tabs.is_empty() {
            return;
        }
        for t in self.terms() {
            if tabs.contains(&t.id) {
                t.capture = Some(vec![]);
            }
        }
        self.multi = Some(MultiExec {
            tabs,
            excluded: HashSet::new(),
            mode: Mode::Live,
            grid: Grid::Auto,
            line: String::new(),
            history: vec![],
            hist_pos: None,
        });
    }

    pub(crate) fn exit_multi_exec(&mut self) {
        self.multi = None;
        for t in self.terms() {
            t.capture = None;
        }
    }

    /// Opens every session of a folder (recursively) or with a tag, reusing
    /// open tabs, and enters multi-exec with them.
    pub(crate) fn multi_exec_folder(&mut self, ctx: &egui::Context, folder_or_tag: &str) {
        let ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| {
                s.tags.iter().any(|t| t == folder_or_tag)
                    || sessions::chain(&self.folders, s.folder.as_deref())
                        .iter()
                        .any(|f| f.id == folder_or_tag)
            })
            .map(|s| s.id.clone())
            .collect();
        let mut tabs = vec![];
        for id in ids {
            let open = self
                .terms()
                .find(|t| t.session_id.as_deref() == Some(&id))
                .map(|t| t.id);
            match open {
                Some(t) => tabs.push(t),
                None => tabs.extend(self.open_session_at(ctx, &id, crate::layout::Place::Focused)),
            }
        }
        self.exit_multi_exec();
        self.enter_multi_exec(tabs);
    }

    fn vars(&self, t: &TermTab) -> Vec<(&'static str, String)> {
        let (host, user, port) = match &t.launch {
            Launch::Ssh { options, .. } => (
                options.target.host.clone(),
                options.target.user.clone(),
                options.target.port.to_string(),
            ),
            Launch::Telnet(o) => (o.host.clone(), String::new(), o.port.to_string()),
            Launch::Serial { path, .. } => (path.clone(), String::new(), String::new()),
            Launch::Local(_) => ("localhost".into(), crate::sessions_ui::local_user(), String::new()),
        };
        let folder = t
            .session_id
            .as_ref()
            .and_then(|id| self.sessions.iter().find(|s| &s.id == id))
            .map(|s| sessions::path(&self.folders, s.folder.as_deref()))
            .unwrap_or_default();
        vec![
            ("name", t.name.clone()),
            ("host", host),
            ("user", user),
            ("port", port),
            ("folder", folder),
        ]
    }

    /// Command-line mode: send to every included tile (risky commands ask first).
    pub(crate) fn send_line(&mut self, line: String, job: bool, confirmed: bool) {
        let Some(m) = &self.multi else { return };
        let targets: Vec<u64> = m.tabs.iter().copied().filter(|id| !m.excluded.contains(id)).collect();
        if !confirmed && self.settings.risky_confirm && is_risky(&line) {
            self.dialog = Some(Dialog::Risky {
                line,
                job,
                n: targets.len(),
            });
            return;
        }
        if job {
            self.run_job(line, &targets);
            return;
        }
        let jobs: Vec<(u64, String)> = self
            .terms_ref()
            .filter(|t| targets.contains(&t.id))
            .map(|t| (t.id, substitute(&line, &self.vars(t))))
            .collect();
        for (id, text) in jobs {
            if let Some(t) = self.term(id) {
                t.send_direct(format!("{text}\r"));
            }
        }
    }

    /// Run as job (SPEC §8.5): a separate exec channel per SSH host.
    fn run_job(&mut self, line: String, targets: &[u64]) {
        let parallel = self.settings.job_parallel.max(1) as usize;
        let delay = Duration::from_millis(self.settings.job_delay_ms.into());
        let not_ssh = self.i18n.t("me-not-ssh");
        let not_connected = self.i18n.t("me-not-connected");
        let mut work = vec![];
        let mut rows = vec![];
        let tabs: Vec<(String, Option<moliterm_core::ssh::SshHandle>, bool, String)> = self
            .terms_ref()
            .filter(|t| targets.contains(&t.id))
            .map(|t| match &t.launch {
                Launch::Ssh { live, .. } => (
                    t.name.clone(),
                    live.lock().ok().and_then(|l| l.clone()),
                    true,
                    substitute(&line, &self.vars(t)),
                ),
                _ => (t.name.clone(), None, false, String::new()),
            })
            .collect();
        for (name, handle, is_ssh, cmd) in tabs {
            let state = match (&handle, is_ssh) {
                (_, false) => JobState::Skipped(not_ssh.clone()),
                (None, true) => JobState::Skipped(not_connected.clone()),
                (Some(h), true) => {
                    work.push((rows.len(), h.clone(), cmd));
                    JobState::Waiting
                }
            };
            rows.push(JobRow { host: name, state });
        }
        let rows = Arc::new(Mutex::new(rows));
        let shared = rows.clone();
        let sem = Arc::new(tokio::sync::Semaphore::new(parallel));
        let wake = self.ctx_wake.clone();
        self.rt.spawn(async move {
            for (i, (row, h, cmd)) in work.into_iter().enumerate() {
                if i > 0 && !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let Ok(permit) = sem.clone().acquire_owned().await else {
                    break;
                };
                let (shared, wake) = (shared.clone(), wake.clone());
                tokio::spawn(async move {
                    let start = Instant::now();
                    if let Ok(mut r) = shared.lock() {
                        r[row].state = JobState::Running(start);
                    }
                    wake();
                    let state = match remote::exec_full(&h, &cmd).await {
                        Ok(result) => JobState::Done {
                            result,
                            took: start.elapsed(),
                        },
                        Err(e) => JobState::Failed(e),
                    };
                    if let Ok(mut r) = shared.lock() {
                        r[row].state = state;
                    }
                    drop(permit);
                    wake();
                });
            }
        });
        self.jobs = Some(Jobs {
            cmd: line,
            rows,
            selected: None,
            failed_only: false,
            open: true,
        });
    }

    /// Takes input captured by tiles and fans it out (live mode) or keeps it
    /// local (excluded tiles, command-line mode).
    fn distribute_input(&mut self) {
        let Some(m) = &self.multi else { return };
        let (live, tabs, excluded) = (m.mode == Mode::Live, m.tabs.clone(), m.excluded.clone());
        let mut fanout: Vec<Vec<u8>> = vec![];
        for t in self.terms() {
            let Some(cap) = t.capture.as_mut() else { continue };
            let data = std::mem::take(cap);
            if data.is_empty() {
                continue;
            }
            if live && !excluded.contains(&t.id) {
                fanout.extend(data);
            } else {
                for d in data {
                    t.send_direct(d);
                }
            }
        }
        for t in self.terms() {
            if tabs.contains(&t.id) && !excluded.contains(&t.id) {
                for d in &fanout {
                    t.send_direct(d.clone());
                }
            }
        }
    }

    /// The tiled multi-exec view replacing the dock while active.
    pub(crate) fn multi_exec_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        cx_settings: &moliterm_core::settings::Settings,
    ) -> Vec<(u64, Req)> {
        let mut reqs = vec![];
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let Some(m) = &mut self.multi else { return reqs };
        m.tabs.retain(|id| {
            self.dock
                .iter_all_tabs()
                .any(|(_, x)| matches!(x, Tab::Term(tt) if tt.id == *id))
        });
        let total = m.tabs.len();
        let included = m.tabs.iter().filter(|id| !m.excluded.contains(id)).count();
        let warn = c(theme.ui.warning);
        let mut exit = false;

        // Banner.
        egui::Frame::new()
            .fill(warn.gamma_multiply(0.12))
            .stroke(Stroke::new(1.0, warn.gamma_multiply(0.5)))
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), egui::Sense::hover());
                    icons::paint(ui.painter(), r, Icon::Broadcast, warn);
                    let text = i18n.t_args("me-banner", &[("n", &included.to_string()), ("m", &total.to_string())]);
                    ui.label(RichText::new(text).strong().color(warn));
                    ui.add_space(12.0);
                    for (mode, key) in [(Mode::Live, "me-live"), (Mode::Line, "me-line")] {
                        ui.selectable_value(&mut m.mode, mode, t(key));
                    }
                    ui.add_space(12.0);
                    for (g, label) in [
                        (Grid::Auto, t("me-grid-auto")),
                        (Grid::Two, "2×2".into()),
                        (Grid::Three, "3×3".into()),
                        (Grid::Columns, t("me-grid-columns")),
                    ] {
                        ui.selectable_value(&mut m.grid, g, label);
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let hk = crate::hotkeys::binding("multi-exec", &cx_settings.hotkeys).to_owned();
                        if ui
                            .add(
                                egui::Button::new(RichText::new(i18n.t_args("me-exit", &[("key", &hk)])).color(warn))
                                    .stroke(Stroke::new(1.0, warn)),
                            )
                            .clicked()
                        {
                            exit = true;
                        }
                    });
                });
            });

        // Command line (bottom).
        let mut send: Option<(String, bool)> = None;
        if m.mode == Mode::Line {
            egui::Panel::bottom("me-line")
                .frame(egui::Frame::new().fill(c(theme.ui.panel)).inner_margin(10))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let edit = ui.add(
                            crate::style::field(&mut m.line)
                                .hint_text(t("me-line-hint"))
                                .font(egui::TextStyle::Monospace)
                                .desired_width(ui.available_width() - 330.0),
                        );
                        if edit.has_focus() {
                            let (up, down) =
                                ui.input(|i| (i.key_pressed(egui::Key::ArrowUp), i.key_pressed(egui::Key::ArrowDown)));
                            if (up || down) && !m.history.is_empty() {
                                let last = m.history.len() - 1;
                                let pos = match (m.hist_pos, up) {
                                    (None, true) => Some(last),
                                    (Some(p), true) => Some(p.saturating_sub(1)),
                                    (Some(p), false) if p < last => Some(p + 1),
                                    _ => None,
                                };
                                m.hist_pos = pos;
                                m.line = pos.map(|p| m.history[p].clone()).unwrap_or_default();
                            }
                        }
                        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                        if primary(ui, theme, i18n.t_args("me-send", &[("n", &included.to_string())])).clicked()
                            || enter
                        {
                            send = Some((m.line.clone(), false));
                            edit.request_focus();
                        }
                        if ui.button(t("me-run-job")).clicked() {
                            send = Some((m.line.clone(), true));
                        }
                    });
                    ui.checkbox(&mut self.risky_confirm_ui, t("me-risky"));
                });
        }
        if let Some((line, _)) = &send
            && !line.trim().is_empty()
        {
            m.history.push(line.clone());
            m.hist_pos = None;
            m.line.clear();
        }

        // Tiles.
        let n = m.tabs.len().max(1);
        let cols = match m.grid {
            Grid::Auto => (n as f32).sqrt().ceil() as usize,
            Grid::Two => 2,
            Grid::Three => 3,
            Grid::Columns => n,
        }
        .clamp(1, n);
        let rows = n.div_ceil(cols);
        let area = ui.available_rect_before_wrap();
        let gap = 8.0;
        let (w, h) = (
            (area.width() - gap * (cols as f32 + 1.0)) / cols as f32,
            (area.height() - gap * (rows as f32 + 1.0)) / rows as f32,
        );
        let tabs = m.tabs.clone();
        let excluded = m.excluded.clone();
        let mut toggle: Option<u64> = None;
        let term_theme = self.term_theme().clone();
        let settings = cx_settings.clone();
        let styled = self.fonts.as_ref().is_some_and(|f| f.styled);
        for (i, id) in tabs.iter().enumerate() {
            let (cx_i, cy_i) = (i % cols, i / cols);
            let rect = Rect::from_min_size(
                pos2(
                    area.left() + gap + cx_i as f32 * (w + gap),
                    area.top() + gap + cy_i as f32 * (h + gap),
                ),
                vec2(w, h),
            );
            let is_excluded = excluded.contains(id);
            let rules = &self.rules.1;
            let Some(tab) = self.dock.iter_all_tabs_mut().find_map(|(_, x)| match x {
                Tab::Term(tt) if tt.id == *id => Some(tt),
                _ => None,
            }) else {
                continue;
            };
            let border = if is_excluded {
                c(theme.ui.border)
            } else {
                warn.gamma_multiply(0.6)
            };
            ui.painter().rect(
                rect,
                6.0,
                c(term_theme.term.bg),
                Stroke::new(1.0, border),
                egui::StrokeKind::Inside,
            );
            ui.scope_builder(UiBuilder::new().max_rect(rect.shrink(6.0)), |ui| {
                ui.horizontal(|ui| {
                    let dot = tab.color.unwrap_or_else(|| tab.state_color(theme));
                    ui.label(RichText::new("●").color(dot));
                    ui.label(RichText::new(&tab.name).strong());
                    let (status, color) = if is_excluded {
                        (t("me-excluded"), c(theme.ui.muted))
                    } else {
                        (t("me-receiving"), warn)
                    };
                    ui.label(RichText::new(status).small().color(color));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut ex = is_excluded;
                        if ui.checkbox(&mut ex, t("me-exclude")).changed() {
                            toggle = Some(*id);
                        }
                    });
                });
                ui.separator();
                let cx = Cx {
                    settings: &settings,
                    palette: &term_theme.term,
                    ui_theme: theme,
                    rules,
                    i18n: &i18n,
                    styled_fonts: styled,
                };
                if is_excluded {
                    ui.set_opacity(0.6);
                }
                let r = tab.ui(ui, &cx);
                reqs.extend(r.into_iter().map(|r| (*id, r)));
            });
        }
        if let Some(id) = toggle
            && let Some(m) = &mut self.multi
            && !m.excluded.remove(&id)
        {
            m.excluded.insert(id);
        }
        if self.settings.risky_confirm != self.risky_confirm_ui {
            let mut s = self.settings.clone();
            s.risky_confirm = self.risky_confirm_ui;
            self.save_settings(s);
        }
        self.distribute_input();
        if let Some((line, job)) = send
            && !line.trim().is_empty()
        {
            self.send_line(line, job, false);
        }
        if exit {
            self.exit_multi_exec();
        }
        reqs
    }

    /// Target popover before entering multi-exec.
    pub(crate) fn multi_exec_picker(&mut self, ctx: &egui::Context, theme: &Theme) {
        let Some(mut p) = self.me_picker.take() else { return };
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let all = self.window_term_ids();
        let selected: Vec<u64> = all
            .iter()
            .copied()
            .filter(|id| self.selected_tabs.contains(id))
            .collect();
        let mut tags: Vec<String> = self.sessions.iter().flat_map(|s| s.tags.clone()).collect();
        tags.sort();
        tags.dedup();
        let mut go = None;
        let mut close = false;
        let frame = egui::Frame::popup(&ctx.global_style())
            .fill(c(theme.ui.surface))
            .inner_margin(18);
        egui::Modal::new("me-picker".into()).frame(frame).show(ctx, |ui| {
            crate::style::fit_width(ui, 420.0);
            ui.label(RichText::new(t("me-title")).heading());
            ui.add_space(6.0);
            ui.radio_value(
                &mut p.choice,
                0,
                i18n.t_args("me-target-all", &[("n", &all.len().to_string())]),
            );
            ui.radio_value(
                &mut p.choice,
                1,
                i18n.t_args("me-target-selected", &[("n", &selected.len().to_string())]),
            );
            ui.radio_value(&mut p.choice, 2, t("me-target-folder"));
            if p.choice == 2 {
                let current = p
                    .folder
                    .as_ref()
                    .map(|f| sessions::path(&self.folders, Some(f)))
                    .unwrap_or_default();
                egui::ComboBox::from_id_salt("me-folder")
                    .selected_text(current)
                    .width(300.0)
                    .show_ui(ui, |ui| {
                        for f in &self.folders {
                            ui.selectable_value(
                                &mut p.folder,
                                Some(f.id.clone()),
                                sessions::path(&self.folders, Some(&f.id)),
                            );
                        }
                    });
            }
            ui.radio_value(&mut p.choice, 3, t("me-target-tag"));
            if p.choice == 3 {
                egui::ComboBox::from_id_salt("me-tag")
                    .selected_text(p.tag.clone().unwrap_or_default())
                    .width(300.0)
                    .show_ui(ui, |ui| {
                        for tag in &tags {
                            ui.selectable_value(&mut p.tag, Some(tag.clone()), tag);
                        }
                    });
            }
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button(t("settings-cancel")).clicked() {
                    close = true;
                }
                if primary(ui, theme, t("me-start")).clicked() {
                    go = Some(p.choice);
                }
            });
        });
        match go {
            Some(0) => self.enter_multi_exec(all),
            Some(1) => self.enter_multi_exec(selected),
            Some(2) => {
                if let Some(f) = p.folder.clone() {
                    self.multi_exec_folder(ctx, &f);
                }
            }
            Some(_) => {
                if let Some(tag) = p.tag.clone() {
                    self.multi_exec_folder(ctx, &tag);
                }
            }
            None if !close => self.me_picker = Some(p),
            None => {}
        }
    }

    /// Results window of a job run.
    pub(crate) fn jobs_window(&mut self, ctx: &egui::Context, theme: &Theme) {
        let Some(jobs) = &mut self.jobs else { return };
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        let mut open = jobs.open;
        egui::Window::new(i18n.t_args("me-results", &[("cmd", &jobs.cmd)]))
            .open(&mut open)
            .default_size([760.0, 420.0])
            .show(ctx, |ui| {
                let Ok(rows) = jobs.rows.lock() else { return };
                let failed = |r: &JobRow| match &r.state {
                    JobState::Done { result, .. } => result.status != Some(0),
                    JobState::Failed(_) => true,
                    _ => false,
                };
                ui.horizontal(|ui| {
                    ui.checkbox(&mut jobs.failed_only, t("me-failed-only"));
                    if ui.button(t("me-copy-all")).clicked() {
                        let mut out = String::new();
                        for r in rows.iter() {
                            out.push_str(&format!("== {} ==\n", r.host));
                            if let JobState::Done { result, .. } = &r.state {
                                out.push_str(&result.stdout);
                                out.push_str(&result.stderr);
                            }
                            out.push('\n');
                        }
                        ui.ctx().copy_text(out);
                    }
                });
                ui.separator();
                egui::Grid::new("jobs")
                    .striped(true)
                    .num_columns(4)
                    .spacing([16.0, 4.0])
                    .show(ui, |ui| {
                        for k in ["me-col-host", "me-col-exit", "me-col-time", "me-col-output"] {
                            ui.label(RichText::new(t(k)).small().strong());
                        }
                        ui.end_row();
                        for (i, r) in rows.iter().enumerate() {
                            if jobs.failed_only && !failed(r) {
                                continue;
                            }
                            if ui.selectable_label(jobs.selected == Some(i), &r.host).clicked() {
                                jobs.selected = Some(i);
                            }
                            match &r.state {
                                JobState::Waiting => {
                                    ui.label("…");
                                    ui.label("");
                                    ui.label("");
                                }
                                JobState::Running(start) => {
                                    ui.spinner();
                                    ui.label(format!("{:.1} s", start.elapsed().as_secs_f32()));
                                    ui.label("");
                                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                                }
                                JobState::Done { result, took } => {
                                    let code = result.status.map_or("?".into(), |s| s.to_string());
                                    let color = if result.status == Some(0) {
                                        c(theme.ui.success)
                                    } else {
                                        c(theme.ui.danger)
                                    };
                                    ui.label(RichText::new(code).color(color).monospace());
                                    ui.label(format!("{:.2} s", took.as_secs_f32()));
                                    let first = result
                                        .stdout
                                        .lines()
                                        .chain(result.stderr.lines())
                                        .next()
                                        .unwrap_or("")
                                        .to_owned();
                                    ui.label(RichText::new(first).monospace());
                                }
                                JobState::Skipped(why) => {
                                    ui.label("—");
                                    ui.label("");
                                    ui.label(RichText::new(why).weak());
                                }
                                JobState::Failed(e) => {
                                    ui.label(RichText::new("!").color(c(theme.ui.danger)));
                                    ui.label("");
                                    ui.label(RichText::new(e).color(c(theme.ui.danger)));
                                }
                            }
                            ui.end_row();
                        }
                    });
                if let Some(JobRow {
                    state: JobState::Done { result, .. },
                    host,
                }) = jobs.selected.and_then(|i| rows.get(i))
                {
                    ui.separator();
                    ui.label(RichText::new(host).strong());
                    egui::ScrollArea::vertical().max_height(220.0).show(ui, |ui| {
                        ui.label(RichText::new(&result.stdout).monospace());
                        if !result.stderr.is_empty() {
                            ui.label(RichText::new(&result.stderr).monospace().color(c(theme.ui.danger)));
                        }
                    });
                }
            });
        jobs.open = open;
        if !open {
            self.jobs = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risky_and_vars() {
        assert!(is_risky("sudo rm -rf /var/tmp/x"));
        assert!(is_risky("dd if=/dev/zero of=/dev/sda"));
        assert!(is_risky(":(){ :|:& };:"));
        assert!(!is_risky("uptime; df -h"));
        let v = vec![("host", "web-01".to_owned()), ("port", "22".to_owned())];
        assert_eq!(substitute("ssh {host}:{port} {x}", &v), "ssh web-01:22 {x}");
    }
}
