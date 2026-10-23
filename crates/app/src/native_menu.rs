//! macOS app menu bar (SPEC §4.1). Items post `Action`s to the UI thread.

use muda::{AboutMetadata, Menu, MenuItem, PredefinedMenuItem, Submenu, accelerator::Accelerator};
use std::sync::mpsc::Sender;

use crate::{
    app::{Action, Side},
    i18n::I18n,
};

pub struct NativeMenu {
    menu: Menu,
}

fn action(id: &str) -> Option<Action> {
    Some(match id {
        "settings" => Action::OpenSettings,
        "toggle-side" => Action::ToggleSidePanel,
        "fullscreen" => Action::Fullscreen,
        "tunnels" => Action::ShowSide(Side::Tunnels),
        "macros" => Action::ShowSide(Side::Macros),
        "vault" => Action::OpenVault,
        "quit" => Action::Quit,
        "local-shell" => Action::NewLocalShell(None),
        "new-session" => Action::NewSession,
        "lock-vault" => Action::LockVault,
        "split-right" => Action::SplitRight,
        "split-down" => Action::SplitDown,
        "multi-exec" => Action::MultiExec,
        "close-tab" => Action::CloseTab,
        "find" => Action::Find,
        "font-bigger" => Action::FontBigger,
        "font-smaller" => Action::FontSmaller,
        "font-reset" => Action::FontReset,
        _ => return None,
    })
}

impl NativeMenu {
    pub fn new(i18n: &I18n, ctx: egui::Context, tx: Sender<Action>) -> Self {
        muda::MenuEvent::set_event_handler(Some(move |e: muda::MenuEvent| {
            if let Some(a) = action(&e.id.0) {
                let _ = tx.send(a);
                ctx.request_repaint();
            }
        }));
        let menu = build(i18n);
        menu.init_for_nsapp();
        Self { menu }
    }

    pub fn rebuild(&mut self, i18n: &I18n) {
        self.menu.remove_for_nsapp();
        self.menu = build(i18n);
        self.menu.init_for_nsapp();
    }
}

fn build(i18n: &I18n) -> Menu {
    let t = |id: &str| i18n.t(id);
    let item = |id: &str, key: &str, enabled: bool| MenuItem::with_id(id, t(key), enabled, None);
    let about = || {
        PredefinedMenuItem::about(
            Some(&t("menu-about")),
            Some(AboutMetadata {
                name: Some(t("app-name")),
                version: Some(env!("CARGO_PKG_VERSION").into()),
                ..Default::default()
            }),
        )
    };
    // Quit goes through the app (not NSApp terminate:) so window state is saved.
    let quit = MenuItem::with_id("quit", t("menu-quit"), true, "cmd+q".parse::<Accelerator>().ok());
    let sep = PredefinedMenuItem::separator;

    let app = Submenu::new(t("app-name"), true);
    let terminal = Submenu::new(t("menu-terminal"), true);
    let sessions = Submenu::new(t("menu-sessions"), true);
    let view = Submenu::new(t("menu-view"), true);
    let tools = Submenu::new(t("menu-tools"), true);
    let settings = Submenu::new(t("menu-settings"), true);
    let help = Submenu::new(t("menu-help"), true);
    let results = [
        app.append_items(&[
            &about(),
            &sep(),
            &item("settings", "menu-open-settings", true),
            &sep(),
            &PredefinedMenuItem::hide(None),
            &PredefinedMenuItem::hide_others(None),
            &PredefinedMenuItem::show_all(None),
            &sep(),
            &quit,
        ]),
        terminal.append_items(&[
            &item("local-shell", "menu-new-local-shell", true),
            &item("find", "menu-find", true),
            &item("close-tab", "menu-close-tab", true),
        ]),
        sessions.append_items(&[
            &item("new-session", "menu-new-session", true),
            &item("lock-vault", "menu-lock-vault", true),
        ]),
        view.append_items(&[
            &item("toggle-side", "menu-toggle-side-panel", true),
            &item("split-right", "hk-split-right", true),
            &item("split-down", "hk-split-down", true),
            &sep(),
            &item("font-bigger", "menu-font-bigger", true),
            &item("font-smaller", "menu-font-smaller", true),
            &item("font-reset", "menu-font-reset", true),
            &sep(),
            &item("fullscreen", "menu-fullscreen", true),
        ]),
        tools.append_items(&[
            &item("multi-exec", "menu-multi-exec", true),
            &item("tunnels", "menu-tunnels", true),
            &item("macros", "menu-macros", true),
            &item("vault", "rail-vault", true),
        ]),
        settings.append(&item("settings", "menu-open-settings", true)),
        help.append(&about()),
    ];
    let menu = Menu::new();
    let appended = menu.append_items(&[&app, &terminal, &sessions, &view, &tools, &settings, &help]);
    for r in results.into_iter().chain([appended]) {
        if let Err(e) = r {
            tracing::error!("native menu: {e}");
        }
    }
    menu
}
