//! Terminal tab: paints an alacritty grid with egui and turns egui input into
//! bytes for the session.

use alacritty_terminal::{
    grid::{Dimensions, Scroll},
    index::{Column, Direction, Line, Point, Side},
    selection::{Selection, SelectionRange, SelectionType},
    term::{
        TermMode,
        cell::Flags,
        color::Colors,
        search::{Match, RegexIter, RegexSearch},
    },
    vte::ansi::{Color as VColor, NamedColor},
};
use egui::{Color32, Event, FontFamily, FontId, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};
use moliterm_core::{
    highlight::Compiled,
    local::Shell,
    settings::{CursorShape, RightClick, Settings},
    terminal::{Cmd, Handle, SessionEvent, Size, Wire, all_text, indexed_color},
    theme::{Rgb, TermColors, Theme},
};

use crate::{i18n::I18n, keys, style::c};

/// How the tab was started; used to reconnect.
pub enum Launch {
    Local(Shell),
    Ssh {
        options: Box<moliterm_core::ssh::Options>,
        live: moliterm_core::ssh::Live,
    },
    Telnet(Box<moliterm_core::telnet::Options>),
    Serial {
        path: String,
        extra: moliterm_core::serial::Extra,
    },
}

pub enum State {
    Connecting,
    Connected,
    Disconnected(String),
}

/// What a tab asks the app to do (dialogs, clipboard, files, tabs).
pub enum Req {
    Paste(String),
    PasteClipboard,
    AskRightClick,
    Osc52(String),
    OpenUrl(String),
    SaveScrollback(String),
    Reconnect,
    /// Open the session editor for this tab's saved session.
    EditSession,
    /// Toggle full screen of this tab's window.
    Fullscreen,
    /// Open the editor prefilled from this quick-connect tab.
    SaveAsSession,
    /// Pick a vault password and type it into this tab (no Enter).
    TypePassword,
    MoveToWindow,
    Close,
}

pub struct Find {
    pub query: String,
    pub regex: bool,
    pub case: bool,
    built: Option<(String, bool, bool)>,
    search: Option<RegexSearch>,
    current: Option<Match>,
    pub error: bool,
    focus: bool,
}

impl Find {
    fn new() -> Self {
        Self {
            query: String::new(),
            regex: false,
            case: false,
            built: None,
            search: None,
            current: None,
            error: false,
            focus: true,
        }
    }

    fn rebuild(&mut self) {
        let key = (self.query.clone(), self.regex, self.case);
        if self.built.as_ref() == Some(&key) {
            return;
        }
        self.built = Some(key);
        self.current = None;
        self.search = None;
        self.error = false;
        if self.query.is_empty() {
            return;
        }
        let pattern = if self.regex {
            self.query.clone()
        } else {
            regex::escape(&self.query)
        };
        let flags = if self.case { "(?-i)" } else { "(?i)" };
        match RegexSearch::new(&format!("{flags}{pattern}")) {
            Ok(s) => self.search = Some(s),
            Err(_) => self.error = true,
        }
    }
}

pub struct TermTab {
    pub id: u64,
    pub name: String,
    pub proto: &'static str,
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub state: State,
    pub handle: Handle,
    pub wire: Wire,
    pub launch: Launch,
    /// Folder/session environment color.
    pub color: Option<Color32>,
    /// Per-session keyword highlighting switch.
    pub highlight: bool,
    /// Saved session this tab was opened from (for split / restore).
    pub session_id: Option<String>,
    /// Multi-exec: input is collected here instead of being sent.
    pub capture: Option<Vec<Vec<u8>>>,
    /// Status bar: target and protocol detail.
    pub info: Option<(String, String)>,
    /// Macro recording: what the user types.
    pub recording: Option<Vec<u8>>,
    /// Remote system monitor (SSH tabs).
    pub monitor: Option<std::sync::Arc<moliterm_core::monitor::Shared>>,
    pub size: Size,
    pub find: Option<Find>,
    /// "Don't ask again for this session" in the paste dialog.
    pub paste_no_ask: bool,
    /// OSC 52 answer for this session.
    pub osc52: Option<bool>,
    menu_at: Option<Pos2>,
    scroll_acc: f32,
    last_mouse_cell: Option<Point>,
    focus_next: bool,
}

/// Everything the tab needs from the app to draw one frame.
pub struct Cx<'a> {
    pub settings: &'a Settings,
    pub palette: &'a TermColors,
    pub ui_theme: &'a Theme,
    pub rules: &'a Compiled,
    pub i18n: &'a I18n,
    pub styled_fonts: bool,
}

pub const INITIAL_SIZE: Size = Size {
    cols: 100,
    rows: 30,
    cell_w: 8,
    cell_h: 16,
};

impl TermTab {
    pub fn new(id: u64, name: String, proto: &'static str, handle: Handle, wire: Wire, launch: Launch) -> Self {
        Self {
            id,
            name,
            proto,
            title: None,
            cwd: None,
            state: State::Connecting,
            handle,
            wire,
            launch,
            color: None,
            highlight: true,
            session_id: None,
            capture: None,
            info: None,
            monitor: None,
            recording: None,
            size: INITIAL_SIZE,
            find: None,
            paste_no_ask: false,
            osc52: None,
            menu_at: None,
            scroll_acc: 0.0,
            last_mouse_cell: None,
            focus_next: true,
        }
    }

    pub fn state_color(&self, theme: &Theme) -> Color32 {
        match self.state {
            State::Connected => c(theme.ui.success),
            State::Connecting => c(theme.ui.warning),
            State::Disconnected(_) => c(theme.ui.danger),
        }
    }

    pub fn is_dead(&self) -> bool {
        matches!(self.state, State::Disconnected(_))
    }

    pub fn send(&mut self, bytes: impl Into<Vec<u8>>) {
        let bytes = bytes.into();
        if let Some(r) = &mut self.recording {
            r.extend_from_slice(&bytes);
        }
        if let Some(c) = &mut self.capture {
            c.push(bytes);
            return;
        }
        self.send_direct(bytes);
    }

    /// Sends to this tab's session, bypassing multi-exec capture.
    pub fn send_direct(&self, bytes: impl Into<Vec<u8>>) {
        if self.is_dead() {
            return;
        }
        let mut term = self.handle.term.lock();
        if term.grid().display_offset() != 0 {
            term.scroll_display(Scroll::Bottom);
        }
        drop(term);
        self.handle.send(bytes);
    }

    pub fn toggle_find(&mut self) {
        self.find = match self.find {
            Some(_) => None,
            None => Some(Find::new()),
        };
        self.focus_next = self.find.is_none();
    }

    pub fn selection_text(&self) -> Option<String> {
        self.handle.term.lock().selection_to_string().filter(|s| !s.is_empty())
    }

    pub fn select_all(&self) {
        let mut term = self.handle.term.lock();
        let grid = term.grid();
        let start = Point::new(Line(-(grid.history_size() as i32)), Column(0));
        let end = Point::new(Line(grid.screen_lines() as i32 - 1), grid.last_column());
        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
        sel.update(end, Side::Right);
        term.selection = Some(sel);
    }

    pub fn clear_scrollback(&self) {
        let mut term = self.handle.term.lock();
        term.grid_mut().clear_history();
        term.selection = None;
    }

    pub fn scrollback_text(&self) -> String {
        all_text(&self.handle.term.lock())
    }

    /// Pastes as is: bracketed if the remote enabled it, newlines as CR.
    pub fn paste_now(&self, text: &str) {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let bracketed = self.handle.term.lock().mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            let body = text.replace('\x1b', "");
            self.send_direct(format!("\x1b[200~{body}\x1b[201~"));
        } else {
            self.send_direct(text);
        }
    }

    /// Sends each line followed by CR, `delay_ms` apart.
    pub fn paste_lines(&self, rt: &tokio::runtime::Handle, text: &str, delay_ms: u32) {
        let enc = self.handle.encoding;
        let lines: Vec<Vec<u8>> = text
            .replace("\r\n", "\n")
            .lines()
            .map(|l| moliterm_core::terminal::encode(enc, format!("{l}\r").into_bytes()))
            .collect();
        let cmd = self.handle.cmd.clone();
        rt.spawn(async move {
            for (i, line) in lines.into_iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms.into())).await;
                }
                if cmd.send(Cmd::Input(line)).is_err() {
                    break;
                }
            }
        });
    }

    pub fn poll_events(&mut self, reqs: &mut Vec<Req>) {
        for ev in self.handle.events.try_iter() {
            match ev {
                SessionEvent::Title(t) => self.title = t,
                SessionEvent::Clipboard(text) => reqs.push(Req::Osc52(text)),
                SessionEvent::Bell => {}
                SessionEvent::Cwd(dir) => self.cwd = Some(dir),
                SessionEvent::Connected => self.state = State::Connected,
                SessionEvent::Info { target, detail } => self.info = Some((target, detail)),
                SessionEvent::Disconnected(reason) => self.state = State::Disconnected(reason),
                SessionEvent::WeakSecurity => {}
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &Cx) -> Vec<Req> {
        let mut reqs = vec![];
        self.poll_events(&mut reqs);
        if let Some(m) = &self.monitor {
            m.touch();
        }
        if self.find.is_some() {
            self.find_bar(ui, cx);
        }

        let font = FontId::monospace(cx.settings.term_font_size);
        let (cw, ch) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let rect = ui.available_rect_before_wrap();
        let id = ui.id().with(("term", self.id));
        ui.allocate_rect(rect, Sense::hover());
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        let inner = rect.shrink(6.0);
        let cols = ((inner.width() / cw).floor() as u16).max(2);
        let rows = ((inner.height() / ch).floor() as u16).max(1);
        let size = Size {
            cols,
            rows,
            cell_w: cw.round() as u16,
            cell_h: ch.round() as u16,
        };
        if size != self.size {
            self.size = size;
            self.handle.term.lock().resize(size);
            let _ = self.handle.cmd.send(Cmd::Resize(size));
        }

        if self.focus_next || resp.clicked() || resp.drag_started() || resp.secondary_clicked() {
            resp.request_focus();
            self.focus_next = false;
        }
        let focused = resp.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
            self.keyboard(ui, &mut reqs);
        }
        let grid = Grid {
            origin: inner.min,
            cw,
            ch,
            cols,
            rows,
        };
        self.mouse(ui, &resp, &grid, cx, &mut reqs);
        self.paint(ui, &grid, rect, &font, cx, focused);
        if cx.settings.color_border
            && let Some(color) = self.color
        {
            ui.painter()
                .rect_stroke(rect.shrink(1.0), 0.0, Stroke::new(2.0, color), egui::StrokeKind::Inside);
        }
        self.context_menu(ui, cx, &mut reqs);
        if let State::Disconnected(reason) = &self.state {
            overlay(ui, rect, cx, reason, self.session_id.is_some(), &mut reqs);
        }
        reqs
    }

    fn keyboard(&mut self, ui: &egui::Ui, reqs: &mut Vec<Req>) {
        let mac = cfg!(target_os = "macos");
        let (events, mods) = ui.input(|i| (i.events.clone(), i.modifiers));
        let app_cursor = self.handle.term.lock().mode().contains(TermMode::APP_CURSOR);
        for e in events {
            match e {
                Event::Text(t) => self.send(keys::text(&t, mods)),
                Event::Ime(egui::ImeEvent::Commit(t)) => self.send(t),
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    if self.is_dead() && key == egui::Key::R && modifiers.is_none() {
                        reqs.push(Req::Reconnect);
                    } else if let Some(bytes) = keys::encode(key, modifiers, app_cursor) {
                        self.send(bytes);
                    }
                }
                // egui-winit turns Ctrl+C/X/V into Copy/Cut/Paste on Windows/Linux;
                // without Shift those belong to the terminal.
                Event::Copy => {
                    if mac || mods.shift {
                        if let Some(text) = self.selection_text() {
                            ui.ctx().copy_text(text);
                        }
                    } else {
                        self.send([3u8]);
                    }
                }
                Event::Cut if !mac => self.send([0x18u8]),
                Event::Paste(text) => {
                    if mac || mods.shift {
                        reqs.push(Req::Paste(text));
                    } else {
                        self.send([0x16u8]);
                    }
                }
                _ => {}
            }
        }
    }

    fn mouse(&mut self, ui: &egui::Ui, resp: &egui::Response, g: &Grid, cx: &Cx, reqs: &mut Vec<Req>) {
        let (mods, events, scroll) = ui.input(|i| (i.modifiers, i.events.clone(), i.smooth_scroll_delta.y));
        let mut term = self.handle.term.lock();
        let mode = *term.mode();
        let offset = term.grid().display_offset();
        let mouse_mode = mode.intersects(TermMode::MOUSE_MODE) && !mods.shift;

        if resp.hovered() && scroll != 0.0 {
            ui.ctx().input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
            self.scroll_acc += scroll / g.ch;
            let lines = self.scroll_acc.trunc() as i32;
            self.scroll_acc -= lines as f32;
            if lines != 0 {
                if mouse_mode {
                    if let Some(p) = resp.hover_pos() {
                        let (pt, _) = g.cell(p, 0);
                        let button = if lines > 0 { 64 } else { 65 };
                        for _ in 0..lines.abs() {
                            self.handle.send(mouse_report(mode, button, pt, true, mods));
                        }
                    }
                } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL) {
                    let key = if lines > 0 { "A" } else { "B" };
                    let prefix = if mode.contains(TermMode::APP_CURSOR) {
                        "\x1bO"
                    } else {
                        "\x1b["
                    };
                    self.handle
                        .send(format!("{prefix}{key}").repeat(lines.unsigned_abs() as usize));
                } else {
                    term.scroll_display(Scroll::Delta(lines));
                }
            }
        }

        if mouse_mode {
            for e in &events {
                if let Event::PointerButton {
                    pos,
                    button,
                    pressed,
                    modifiers,
                } = e
                    && g.rect().contains(*pos)
                {
                    let b = match button {
                        egui::PointerButton::Primary => 0,
                        egui::PointerButton::Middle => 1,
                        egui::PointerButton::Secondary => 2,
                        _ => continue,
                    };
                    let (pt, _) = g.cell(*pos, 0);
                    self.handle.send(mouse_report(mode, b, pt, *pressed, *modifiers));
                }
            }
            let motion = mode.contains(TermMode::MOUSE_MOTION)
                || (mode.contains(TermMode::MOUSE_DRAG) && ui.input(|i| i.pointer.any_down()));
            if motion && let Some(p) = resp.hover_pos() {
                let (pt, _) = g.cell(p, 0);
                if self.last_mouse_cell != Some(pt) {
                    self.last_mouse_cell = Some(pt);
                    let held = ui.input(|i| i.pointer.primary_down());
                    let b = if held { 32 } else { 35 };
                    self.handle.send(mouse_report(mode, b, pt, true, mods));
                }
            }
            return;
        }

        let pos = resp.interact_pointer_pos().or(resp.hover_pos());
        if let Some(p) = pos {
            let (pt, side) = g.cell(p, offset);
            if resp.triple_clicked() {
                term.selection = Some(Selection::new(SelectionType::Lines, pt, side));
                drop(term);
                self.copy_on_select(ui, cx);
                return;
            } else if resp.double_clicked() {
                term.selection = Some(Selection::new(SelectionType::Semantic, pt, side));
                drop(term);
                self.copy_on_select(ui, cx);
                return;
            } else if resp.drag_started_by(egui::PointerButton::Primary) {
                let origin = ui.input(|i| i.pointer.press_origin()).unwrap_or(p);
                let (start, start_side) = g.cell(origin, offset);
                let ty = if mods.alt {
                    SelectionType::Block
                } else {
                    SelectionType::Simple
                };
                let mut sel = Selection::new(ty, start, start_side);
                sel.update(pt, side);
                term.selection = Some(sel);
            } else if resp.dragged_by(egui::PointerButton::Primary) {
                if let Some(sel) = &mut term.selection {
                    sel.update(pt, side);
                }
            } else if resp.drag_stopped() {
                drop(term);
                self.copy_on_select(ui, cx);
                return;
            } else if resp.clicked() {
                let link = term.grid()[pt].hyperlink();
                term.selection = None;
                if (mods.command || mods.ctrl)
                    && let Some(link) = link
                {
                    reqs.push(Req::OpenUrl(link.uri().to_owned()));
                }
            }
            if mods.command && resp.hovered() && pt.line >= -(offset as i32) && term.grid()[pt].hyperlink().is_some() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
        }
        drop(term);

        if resp.secondary_clicked() {
            let at = resp.interact_pointer_pos().unwrap_or(g.origin);
            if mods.shift {
                self.menu_at = Some(at);
            } else {
                match cx.settings.right_click {
                    RightClick::Ask => reqs.push(Req::AskRightClick),
                    RightClick::Paste => reqs.push(Req::PasteClipboard),
                    RightClick::Menu => self.menu_at = Some(at),
                }
            }
        }
    }

    pub fn open_menu(&mut self, at: Pos2) {
        self.menu_at = Some(at);
    }

    fn copy_on_select(&self, ui: &egui::Ui, cx: &Cx) {
        if cx.settings.copy_on_select
            && let Some(text) = self.selection_text()
        {
            ui.ctx().copy_text(text);
        }
    }

    fn context_menu(&mut self, ui: &egui::Ui, cx: &Cx, reqs: &mut Vec<Req>) {
        let Some(at) = self.menu_at else {
            return;
        };
        let t = |k: &str| cx.i18n.t(k);
        let mut close = false;
        let area = egui::Area::new(ui.id().with(("term-menu", self.id)))
            .fixed_pos(at)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(200.0);
                    let has_sel = self.selection_text().is_some();
                    if ui.add_enabled(has_sel, egui::Button::new(t("term-copy"))).clicked() {
                        if let Some(text) = self.selection_text() {
                            ui.ctx().copy_text(text);
                        }
                        close = true;
                    }
                    if ui.button(t("term-paste")).clicked() {
                        reqs.push(Req::PasteClipboard);
                        close = true;
                    }
                    if ui.button(t("term-select-all")).clicked() {
                        self.select_all();
                        close = true;
                    }
                    ui.separator();
                    if ui.button(t("term-find")).clicked() {
                        self.toggle_find();
                        close = true;
                    }
                    if ui.button(t("term-clear-scrollback")).clicked() {
                        self.clear_scrollback();
                        close = true;
                    }
                    if ui.button(t("term-save-scrollback")).clicked() {
                        reqs.push(Req::SaveScrollback(self.scrollback_text()));
                        close = true;
                    }
                    if matches!(self.launch, Launch::Serial { .. })
                        && ui
                            .add_enabled(!self.is_dead(), egui::Button::new(t("term-send-break")))
                            .clicked()
                    {
                        let _ = self.handle.cmd.send(Cmd::Break);
                        close = true;
                    }
                    if !matches!(self.launch, Launch::Local(_)) && ui.button(t("term-type-password")).clicked() {
                        reqs.push(Req::TypePassword);
                        close = true;
                    }
                    ui.separator();
                    if ui.button(t("tab-move-window")).clicked() {
                        reqs.push(Req::MoveToWindow);
                        close = true;
                    }
                    if ui.button(t("menu-close-tab")).clicked() {
                        reqs.push(Req::Close);
                        close = true;
                    }
                });
            });
        let clicked_outside = ui.input(|i| i.pointer.any_click())
            && !area.response.contains_pointer()
            && !ui.input(|i| i.pointer.secondary_clicked());
        if close || clicked_outside || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.menu_at = None;
        }
    }

    fn find_bar(&mut self, ui: &mut egui::Ui, cx: &Cx) {
        let t = |k: &str| cx.i18n.t(k);
        let Some(find) = &mut self.find else {
            return;
        };
        let mut step = None;
        let mut close = false;
        egui::Frame::new()
            .fill(c(cx.ui_theme.ui.panel))
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let edit = ui.add(
                        crate::style::field(&mut find.query)
                            .hint_text(t("find-hint"))
                            .desired_width(240.0)
                            .text_color_opt(find.error.then_some(c(cx.ui_theme.ui.danger))),
                    );
                    if find.focus {
                        edit.request_focus();
                        find.focus = false;
                    }
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let back = ui.input(|i| i.modifiers.shift);
                        step = Some(if back { Direction::Left } else { Direction::Right });
                        edit.request_focus();
                    }
                    if edit.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        close = true;
                    }
                    ui.toggle_value(&mut find.regex, ".*").on_hover_text(t("find-regex"));
                    ui.toggle_value(&mut find.case, "Aa").on_hover_text(t("find-case"));
                    if ui.button("↑").on_hover_text(t("find-prev")).clicked() {
                        step = Some(Direction::Left);
                    }
                    if ui.button("↓").on_hover_text(t("find-next")).clicked() {
                        step = Some(Direction::Right);
                    }
                    if ui.button(t("find-close")).clicked() {
                        close = true;
                    }
                });
            });
        find.rebuild();
        if let (Some(dir), Some(search)) = (step, &mut find.search) {
            let mut term = self.handle.term.lock();
            let origin = match (&find.current, dir) {
                (Some(m), Direction::Right) => *m.end(),
                (Some(m), Direction::Left) => *m.start(),
                (None, _) => term.grid().cursor.point,
            };
            let side = if dir == Direction::Right {
                Side::Right
            } else {
                Side::Left
            };
            if let Some(m) = term.search_next(search, origin, dir, side, None) {
                term.scroll_to_point(*m.start());
                find.current = Some(m);
            }
        }
        if close {
            self.find = None;
            self.focus_next = true;
        }
    }

    fn paint(&mut self, ui: &egui::Ui, g: &Grid, rect: Rect, font: &FontId, cx: &Cx, focused: bool) {
        let painter = ui.painter_at(rect);
        let pal = Palette::new(cx.palette);
        painter.rect_filled(rect, 0.0, pal.bg);

        let term = self.handle.term.lock();
        let offset = term.grid().display_offset() as i32;
        let colors = term.colors();
        let sel = term.selection.as_ref().and_then(|s| s.to_range(&*term));
        let mode = *term.mode();
        let matches = visible_matches(&mut self.find, &term, g, offset);
        let current = self.find.as_ref().and_then(|f| f.current.clone());
        let hl_on = cx.settings.highlight && self.highlight && !cx.rules.is_empty();
        let sel_bg = c(cx.palette.selection);
        let match_bg = c(cx.ui_theme.ui.warning).gamma_multiply(0.45);
        let current_bg = c(cx.ui_theme.ui.accent);

        for r in 0..g.rows as i32 {
            let line = Line(r - offset);
            let row = &term.grid()[line];
            let y = g.origin.y + r as f32 * g.ch;
            let cols = g.cols as usize;

            // Highlight rules → per-column overrides.
            let mut over: Vec<(Option<Rgb>, Option<Rgb>)> = vec![];
            if hl_on {
                let mut text = String::with_capacity(cols);
                let mut col_of = Vec::with_capacity(cols);
                for x in 0..cols {
                    let ch = row[Column(x)].c;
                    for _ in 0..ch.len_utf8() {
                        col_of.push(x);
                    }
                    text.push(ch);
                }
                for (range, fg, bg) in cx.rules.matches(&text) {
                    if over.is_empty() {
                        over = vec![(None, None); cols];
                    }
                    let (a, b) = (col_of[range.start], col_of[range.end - 1]);
                    for o in &mut over[a..=b] {
                        *o = (fg.or(o.0), bg.or(o.1));
                    }
                }
            }

            let mut bg_run: Option<(usize, Color32)> = None;
            let mut text_run: Option<(usize, String, Color32, FontFamily)> = None;
            let flush_bg = |run: &mut Option<(usize, Color32)>, end: usize| {
                if let Some((start, color)) = run.take() {
                    let r = Rect::from_min_max(pos2(g.x(start), y), pos2(g.x(end), y + g.ch));
                    painter.rect_filled(r, 0.0, color);
                }
            };
            let mut texts: Vec<(f32, String, Color32, FontFamily)> = vec![];
            let mut lines: Vec<(f32, f32, f32, Color32)> = vec![];

            for x in 0..cols {
                let cell = &row[Column(x)];
                let point = Point::new(line, Column(x));
                let mut fg = pal.resolve(cell.fg, colors, true);
                let mut bg = pal.resolve(cell.bg, colors, false);
                if cell.flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if cell.flags.contains(Flags::DIM) {
                    fg = fg.lerp_to_gamma(bg, 0.4);
                }
                if let Some((hf, hb)) = over.get(x) {
                    fg = hf.map_or(fg, c);
                    bg = hb.map_or(bg, c);
                }
                if cell.flags.contains(Flags::HIDDEN) {
                    fg = bg;
                }
                if sel.as_ref().is_some_and(|s: &SelectionRange| s.contains(point)) {
                    bg = sel_bg;
                } else if current.as_ref().is_some_and(|m| m.contains(&point)) {
                    bg = current_bg;
                    fg = pal.bg;
                } else if matches.iter().any(|m| m.contains(&point)) {
                    bg = match_bg;
                }

                let want_bg = (bg != pal.bg).then_some(bg);
                match (&mut bg_run, want_bg) {
                    (Some((_, cur)), Some(b)) if *cur == b => {}
                    (_, want) => {
                        flush_bg(&mut bg_run, x);
                        bg_run = want.map(|b| (x, b));
                    }
                }

                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let family = if cx.styled_fonts {
                    match (cell.flags.contains(Flags::BOLD), cell.flags.contains(Flags::ITALIC)) {
                        (true, true) => FontFamily::Name("mono-bi".into()),
                        (true, false) => FontFamily::Name("mono-b".into()),
                        (false, true) => FontFamily::Name("mono-i".into()),
                        (false, false) => FontFamily::Monospace,
                    }
                } else {
                    FontFamily::Monospace
                };
                if cell.flags.intersects(Flags::ALL_UNDERLINES) || cell.hyperlink().is_some() {
                    let w = if cell.flags.contains(Flags::WIDE_CHAR) {
                        2.0
                    } else {
                        1.0
                    };
                    lines.push((g.x(x), g.x(x) + g.cw * w, y + g.ch - 1.5, fg));
                }
                if cell.flags.contains(Flags::STRIKEOUT) {
                    lines.push((g.x(x), g.x(x) + g.cw, y + g.ch * 0.55, fg));
                }
                let ch = cell.c;
                let narrow = ch.is_ascii_graphic() || ch == ' ';
                match &mut text_run {
                    Some((_, s, f, fam)) if narrow && *f == fg && *fam == family => s.push(ch),
                    run => {
                        if let Some((start, s, f, fam)) = run.take() {
                            texts.push((g.x(start), s, f, fam));
                        }
                        if narrow {
                            *run = Some((x, ch.to_string(), fg, family));
                        } else if ch != '\0' {
                            texts.push((g.x(x), ch.to_string(), fg, family));
                        }
                    }
                }
            }
            flush_bg(&mut bg_run, cols);
            if let Some((start, s, f, fam)) = text_run.take() {
                texts.push((g.x(start), s, f, fam));
            }
            for (x, s, color, family) in texts {
                if s.trim().is_empty() {
                    continue;
                }
                let f = FontId::new(font.size, family);
                painter.text(pos2(x, y), egui::Align2::LEFT_TOP, s, f, color);
            }
            for (x0, x1, ly, color) in lines {
                painter.line_segment([pos2(x0, ly), pos2(x1, ly)], Stroke::new(1.0, color));
            }
        }

        // Cursor.
        let cur = term.grid().cursor.point;
        let vr = cur.line.0 + offset;
        if mode.contains(TermMode::SHOW_CURSOR) && (0..g.rows as i32).contains(&vr) && !self.is_dead() {
            let wide = term.grid()[cur].flags.contains(Flags::WIDE_CHAR);
            let x = g.x(cur.column.0);
            let y = g.origin.y + vr as f32 * g.ch;
            let w = if wide { g.cw * 2.0 } else { g.cw };
            let r = Rect::from_min_size(pos2(x, y), vec2(w, g.ch));
            let color = c(cx.palette.cursor);
            let blink_on = !cx.settings.cursor_blink || !focused || {
                let t = ui.input(|i| i.time);
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(530 - (t * 1000.0) as u64 % 530));
                (t / 0.53) as i64 % 2 == 0
            };
            if !focused {
                painter.rect_stroke(r.shrink(0.5), 0.0, Stroke::new(1.0, color), egui::StrokeKind::Inside);
            } else if blink_on {
                match cx.settings.cursor {
                    CursorShape::Block => {
                        painter.rect_filled(r, 0.0, color);
                        let ch = term.grid()[cur].c;
                        if ch != ' ' && ch != '\0' {
                            painter.text(r.min, egui::Align2::LEFT_TOP, ch, font.clone(), pal.bg);
                        }
                    }
                    CursorShape::Beam => {
                        painter.rect_filled(r.with_max_x(x + 2.0), 0.0, color);
                    }
                    CursorShape::Underline => {
                        painter.rect_filled(r.with_min_y(y + g.ch - 2.0), 0.0, color);
                    }
                }
            }
        }
    }
}

fn visible_matches<T>(
    find: &mut Option<Find>,
    term: &alacritty_terminal::Term<T>,
    g: &Grid,
    offset: i32,
) -> Vec<Match> {
    let Some(search) = find.as_mut().and_then(|f| f.search.as_mut()) else {
        return vec![];
    };
    let start = Point::new(Line(-offset), Column(0));
    let end = Point::new(Line(g.rows as i32 - 1 - offset), Column(g.cols as usize - 1));
    RegexIter::new(start, end, Direction::Right, term, search)
        .take(500)
        .collect()
}

struct Grid {
    origin: Pos2,
    cw: f32,
    ch: f32,
    cols: u16,
    rows: u16,
}

impl Grid {
    fn x(&self, col: usize) -> f32 {
        self.origin.x + col as f32 * self.cw
    }

    fn rect(&self) -> Rect {
        Rect::from_min_size(
            self.origin,
            vec2(self.cw * self.cols as f32, self.ch * self.rows as f32),
        )
    }

    /// Grid point (with scrollback offset applied) and cell side under `p`.
    fn cell(&self, p: Pos2, offset: usize) -> (Point, Side) {
        let fx = ((p.x - self.origin.x) / self.cw).max(0.0);
        let col = (fx as usize).min(self.cols as usize - 1);
        let side = if fx.fract() < 0.5 { Side::Left } else { Side::Right };
        let row = (((p.y - self.origin.y) / self.ch).max(0.0) as usize).min(self.rows as usize - 1);
        (Point::new(Line(row as i32 - offset as i32), Column(col)), side)
    }
}

struct Palette {
    fg: Color32,
    bg: Color32,
    cursor: Color32,
    ansi: [Color32; 16],
    ansi_rgb: [Rgb; 16],
}

impl Palette {
    fn new(t: &TermColors) -> Self {
        Self {
            fg: c(t.fg),
            bg: c(t.bg),
            cursor: c(t.cursor),
            ansi: t.ansi.map(c),
            ansi_rgb: t.ansi,
        }
    }

    fn resolve(&self, color: VColor, overrides: &Colors, fg: bool) -> Color32 {
        let rgb = |r: alacritty_terminal::vte::ansi::Rgb| Color32::from_rgb(r.r, r.g, r.b);
        match color {
            VColor::Spec(r) => rgb(r),
            VColor::Indexed(i) => overrides[i as usize].map_or_else(|| c(indexed_color(i, &self.ansi_rgb)), rgb),
            VColor::Named(n) => {
                let i = n as usize;
                if let Some(o) = overrides[i] {
                    return rgb(o);
                }
                match n {
                    NamedColor::Foreground | NamedColor::BrightForeground => self.fg,
                    NamedColor::DimForeground => self.fg.lerp_to_gamma(self.bg, 0.4),
                    NamedColor::Background => self.bg,
                    NamedColor::Cursor => self.cursor,
                    _ if i < 16 => self.ansi[i],
                    _ => {
                        let base = i - NamedColor::DimBlack as usize;
                        let c = self
                            .ansi
                            .get(base)
                            .copied()
                            .unwrap_or(if fg { self.fg } else { self.bg });
                        c.lerp_to_gamma(self.bg, 0.4)
                    }
                }
            }
        }
    }
}

/// xterm mouse report (SGR when enabled, else the legacy X10 form).
fn mouse_report(mode: TermMode, button: u8, p: Point, pressed: bool, m: egui::Modifiers) -> Vec<u8> {
    let b = button + 4 * u8::from(m.shift) + 8 * u8::from(m.alt) + 16 * u8::from(m.ctrl);
    let (x, y) = (p.column.0 + 1, p.line.0.max(0) as usize + 1);
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        format!("\x1b[<{b};{x};{y}{end}").into_bytes()
    } else {
        let b = if pressed { b } else { 3 + (b & !3) };
        let enc = |v: usize| (32 + v).min(255) as u8;
        vec![0x1b, b'[', b'M', 32 + b, enc(x), enc(y)]
    }
}

/// Technical reason → i18n key of a plain-words explanation (SPEC §11).
pub fn friendly(reason: &str) -> &'static str {
    let r = reason.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| r.contains(w));
    if r.starts_with("exit status") || r.starts_with("signal") || r == "channel closed" {
        "err-exited"
    } else if has(&["cancelled"]) {
        "err-cancelled"
    } else if has(&["host key", "certificate was not accepted"]) {
        "err-hostkey"
    } else if has(&["refused"]) {
        "err-refused"
    } else if has(&["timed out", "timeout"]) {
        "err-timeout"
    } else if has(&["lookup", "nodename", "not known", "no such host", "dns"]) {
        "err-dns"
    } else if has(&["unreachable", "no route", "host is down"]) {
        "err-unreachable"
    } else if has(&["authentication", "passphrase", "credssp", "logon", "permission denied"]) {
        "err-auth"
    } else if has(&["lost", "reset", "broken pipe", "closed by peer", "eof"]) {
        "err-lost"
    } else {
        "err-generic"
    }
}

/// Dead-tab card: plain reason, technical detail, Reconnect / Edit session.
pub fn dead_card(
    ui: &egui::Ui,
    rect: Rect,
    i18n: &I18n,
    theme: &Theme,
    reason: &str,
    editable: bool,
    reqs: &mut Vec<Req>,
) {
    let t = |k: &str| i18n.t(k);
    egui::Area::new(ui.id().with("dead-overlay"))
        .fixed_pos(rect.center() - vec2(190.0, 70.0))
        .order(egui::Order::Middle)
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style())
                .stroke(Stroke::new(1.0, c(theme.ui.danger)))
                .show(ui, |ui| {
                    crate::style::fit_width(ui, 380.0);
                    ui.label(
                        egui::RichText::new(t(friendly(reason)))
                            .strong()
                            .color(c(theme.ui.danger)),
                    );
                    egui::CollapsingHeader::new(egui::RichText::new(t("err-details")).small())
                        .id_salt("dead-details")
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new(reason).small().monospace());
                        });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        if ui.button(t("term-reconnect")).clicked() {
                            reqs.push(Req::Reconnect);
                        }
                        if editable && ui.button(t("err-edit-session")).clicked() {
                            reqs.push(Req::EditSession);
                        }
                        ui.label(egui::RichText::new(t("term-press-r")).small());
                    });
                });
        });
}

fn overlay(ui: &egui::Ui, rect: Rect, cx: &Cx, reason: &str, editable: bool, reqs: &mut Vec<Req>) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, c(cx.palette.bg).gamma_multiply(0.75));
    dead_card(ui, rect, cx.i18n, cx.ui_theme, reason, editable, reqs);
}

#[cfg(test)]
mod tests {
    use super::friendly;

    #[test]
    fn plain_reasons() {
        assert_eq!(friendly("10.0.0.1:22: Connection refused (os error 61)"), "err-refused");
        assert_eq!(friendly("connection to h:22 timed out"), "err-timeout");
        assert_eq!(
            friendly("h:22: failed to lookup address information: nodename nor servname provided"),
            "err-dns"
        );
        assert_eq!(friendly("authentication failed for root@h"), "err-auth");
        assert_eq!(friendly("host key of h was not accepted"), "err-hostkey");
        assert_eq!(friendly("exit status 0"), "err-exited");
        assert_eq!(friendly("connection lost"), "err-lost");
        assert_eq!(friendly("something odd"), "err-generic");
    }
}
