//! Where tabs go (SPEC §4.4): splits, native windows, and restoring the last
//! layout at start (§4.5).

use std::collections::HashSet;

use egui_dock::{DockState, Node, Split, Surface};
use serde::{Deserialize, Serialize};

use crate::{
    app::{App, Side},
    dialogs::Dialog,
    dock::Tab,
    term_view::Launch,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    Focused,
    Right,
    Below,
    Window,
}

/// A detached native window with its own dock and side panel.
pub struct Win {
    pub id: egui::ViewportId,
    pub dock: DockState<Tab>,
    pub side: Option<Side>,
    pub title: String,
    pub multi: Option<crate::multiexec::MultiExec>,
    pub selected: std::collections::HashSet<u64>,
    pub focus: Option<std::time::Instant>,
}

/// What a tab was, for reopening it next time (no scrollback).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TabRef {
    pub name: String,
    pub proto: String,
    pub session: Option<String>,
    /// Local shell program.
    pub shell: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Saved {
    pub main: DockState<TabRef>,
    pub windows: Vec<DockState<TabRef>>,
}

impl Saved {
    pub fn tabs(&self) -> Vec<TabRef> {
        std::iter::once(&self.main)
            .chain(&self.windows)
            .flat_map(|d| d.iter_all_tabs().map(|(_, t)| t.clone()))
            .collect()
    }
}

fn tab_ref(t: &Tab) -> Option<TabRef> {
    if let Tab::Desk(d) = t {
        return Some(TabRef {
            name: d.name.clone(),
            proto: "RDP".into(),
            session: d.session_id.clone(),
            shell: None,
        });
    }
    let Tab::Term(t) = t else {
        return None;
    };
    Some(TabRef {
        name: t.name.clone(),
        proto: t.proto.to_owned(),
        session: t.session_id.clone(),
        shell: match &t.launch {
            Launch::Local(s) => Some(s.program.clone()),
            _ => None,
        },
    })
}

impl App {
    pub(crate) fn place(&mut self, tab: Tab, place: Place) {
        self.dock.retain_tabs(|t| !matches!(t, Tab::Welcome));
        let leaf = self
            .dock
            .focused_leaf()
            .or_else(|| self.dock.iter_leaves().next().map(|(p, _)| p));
        let has_tabs = leaf
            .and_then(|p| self.dock.leaf(p).ok())
            .is_some_and(|l| !l.tabs.is_empty());
        match (place, leaf) {
            (Place::Window, _) => self.new_window(vec![tab]),
            (Place::Right, Some(p)) if has_tabs => {
                self.dock.split(p, Split::Right, 0.5, Node::leaf(tab));
            }
            (Place::Below, Some(p)) if has_tabs => {
                self.dock.split(p, Split::Below, 0.5, Node::leaf(tab));
            }
            _ => self.dock.push_to_focused_leaf(tab),
        }
    }

    pub(crate) fn new_window(&mut self, tabs: Vec<Tab>) {
        let n = self.window_seq;
        self.window_seq += 1;
        let title = match tabs.first().and_then(Tab::name) {
            Some(name) => format!("{} — {}", self.i18n.t("app-name"), name),
            None => self.i18n.t("app-name"),
        };
        self.windows.push(Win {
            id: egui::ViewportId::from_hash_of(("moliterm-window", n)),
            dock: DockState::new(tabs),
            side: None,
            title,
            multi: None,
            selected: Default::default(),
            focus: None,
        });
    }

    /// Split: open another copy of the active tab's session next to it.
    pub(crate) fn split_active(&mut self, ctx: &egui::Context, place: Place) {
        let (session, shell) = match self.active_term() {
            Some(t) => (
                t.session_id.clone(),
                match &t.launch {
                    Launch::Local(s) => Some(s.clone()),
                    _ => None,
                },
            ),
            None => (None, None),
        };
        match (session, shell) {
            (Some(id), _) => {
                self.open_session_at(ctx, &id, place);
            }
            (None, Some(shell)) => {
                let idx = self.shells.iter().position(|s| *s == shell);
                self.open_local_at(ctx, idx, place);
            }
            _ => {
                self.open_local_at(ctx, None, place);
            }
        }
    }

    /// Moves a tab of the current dock into a new native window.
    pub(crate) fn move_to_window(&mut self, id: u64) {
        if let Some(path) = self.dock.find_tab_from(|t| t.id() == Some(id))
            && let Some(tab) = self.dock.remove_tab(path)
        {
            if self.dock.iter_all_tabs().next().is_none() && !self.in_window {
                self.dock.push_to_first_leaf(Tab::Welcome);
            }
            self.new_window(vec![tab]);
        }
    }

    /// Tabs dragged out of the dock land in egui_dock's floating windows; turn
    /// those into native windows (SPEC §4.4 "drag a tab out").
    pub(crate) fn detach_floating(&mut self) {
        let floating: Vec<_> = self
            .dock
            .iter_surfaces_indexed()
            .filter(|(_, s)| matches!(s, Surface::Window(..)))
            .map(|(i, _)| i)
            .collect();
        for idx in floating {
            if let Some(Surface::Window(mut tree, _)) = self.dock.remove_surface(idx) {
                let tabs: Vec<Tab> = tree
                    .iter_mut()
                    .filter_map(|n| n.get_leaf_mut())
                    .flat_map(|leaf| std::mem::take(&mut leaf.tabs))
                    .collect();
                if !tabs.is_empty() {
                    self.new_window(tabs);
                }
            }
        }
    }

    /// Draws every detached window. Each one temporarily becomes "the" dock
    /// so the same chrome/dock code serves all windows.
    pub(crate) fn windows_ui(&mut self, ctx: &egui::Context) {
        let mut wins = std::mem::take(&mut self.windows);
        let mut closed = vec![];
        for (i, w) in wins.iter_mut().enumerate() {
            let builder = egui::ViewportBuilder::default()
                .with_title(w.title.clone())
                .with_inner_size([1100.0, 720.0])
                .with_min_inner_size([520.0, 360.0]);
            ctx.show_viewport_immediate(w.id, builder, |ui, _| {
                let swap = |app: &mut App, w: &mut Win| {
                    std::mem::swap(&mut app.dock, &mut w.dock);
                    std::mem::swap(&mut app.side, &mut w.side);
                    std::mem::swap(&mut app.multi, &mut w.multi);
                    std::mem::swap(&mut app.selected_tabs, &mut w.selected);
                    std::mem::swap(&mut app.focus, &mut w.focus);
                };
                swap(self, w);
                self.in_window = true;
                self.window_frame(ui);
                self.in_window = false;
                swap(self, w);
                if ui.input(|i| i.viewport().close_requested()) || w.dock.iter_all_tabs().next().is_none() {
                    closed.push(i);
                }
            });
        }
        for i in closed.into_iter().rev() {
            let w = wins.remove(i);
            for (_, t) in w.dock.iter_all_tabs() {
                t.close();
                if let Some(id) = t.id() {
                    self.drop_sftp(id);
                }
            }
        }
        // Windows created while drawing (e.g. a tab dragged out of a window).
        wins.append(&mut self.windows);
        self.windows = wins;
    }

    // ---- restore --------------------------------------------------------

    pub(crate) fn save_layout(&self) {
        let saved = Saved {
            main: self.dock.filter_map_tabs(tab_ref),
            windows: self.windows.iter().map(|w| w.dock.filter_map_tabs(tab_ref)).collect(),
        };
        match serde_json::to_string(&saved) {
            Ok(json) => {
                if let Err(e) = self.db.meta_set("restore", &json) {
                    tracing::warn!("saving layout: {e}");
                }
            }
            Err(e) => tracing::warn!("saving layout: {e}"),
        }
    }

    /// At start: offer to reopen last run's tabs (SPEC §4.5).
    pub(crate) fn offer_restore(&mut self, ctx: &egui::Context) {
        use moliterm_core::settings::RestoreMode;
        let Some(saved) = self
            .db
            .meta_get("restore")
            .and_then(|j| serde_json::from_str::<Saved>(&j).ok())
        else {
            return;
        };
        let tabs = saved.tabs();
        if tabs.is_empty() {
            return;
        }
        match self.settings.restore {
            RestoreMode::Never => {}
            RestoreMode::Always => {
                let all = (0..tabs.len()).collect();
                self.reopen(ctx, saved, &all);
            }
            RestoreMode::Ask => {
                self.dialog = Some(Dialog::Restore {
                    checked: vec![true; tabs.len()],
                    tabs,
                    saved: Box::new(saved),
                    remember: false,
                });
            }
        }
    }

    /// Starts a tab for a saved reference (session or local shell) without placing it.
    fn make_tab(&mut self, ctx: &egui::Context, r: &TabRef) -> Option<Tab> {
        if let Some(id) = &r.session {
            let s = self.find_session(id)?;
            return self.make_session_tab(ctx, s);
        }
        let shell = self
            .shells
            .iter()
            .find(|s| Some(&s.program) == r.shell.as_ref())
            .cloned()
            .or_else(|| {
                moliterm_core::local::detect()
                    .into_iter()
                    .find(|s| Some(&s.program) == r.shell.as_ref())
            })
            .or_else(|| moliterm_core::local::detect().into_iter().next())?;
        Some(Tab::Term(Box::new(self.make_local_tab(ctx, shell))))
    }

    /// Reopens the tabs whose index (in [`Saved::tabs`] order) is in `keep`, in their saved layout.
    pub(crate) fn reopen(&mut self, ctx: &egui::Context, saved: Saved, keep: &HashSet<usize>) {
        let mut n = 0usize;
        let mut open = |r: &TabRef, app: &mut App| -> Option<Tab> {
            let i = n;
            n += 1;
            if !keep.contains(&i) {
                return None;
            }
            app.make_tab(ctx, r)
        };
        let main = saved.main.filter_map_tabs(|r| open(r, self));
        let windows: Vec<DockState<Tab>> = saved
            .windows
            .iter()
            .map(|d| d.filter_map_tabs(|r| open(r, self)))
            .collect();
        if main.iter_all_tabs().next().is_some() {
            self.dock = main;
        }
        for d in windows {
            if d.iter_all_tabs().next().is_some() {
                let n = self.window_seq;
                self.window_seq += 1;
                self.windows.push(Win {
                    id: egui::ViewportId::from_hash_of(("moliterm-window", n)),
                    dock: d,
                    side: None,
                    title: self.i18n.t("app-name"),
                    multi: None,
                    selected: Default::default(),
                    focus: None,
                });
            }
        }
    }
}
