//! Settings › Import sessions (SPEC §9.5) and Settings › Advanced.

use std::sync::mpsc;

use egui::RichText;
use moliterm_core::{
    import::{self, Imported},
    sessions,
    settings::Settings,
    theme::Theme,
};

use crate::{app::App, dialogs::primary, style::c};

#[derive(Clone, Copy, PartialEq)]
pub enum Source {
    MobaXterm,
    PuttyReg,
    #[cfg(windows)]
    PuttyRegistry,
    #[cfg(target_os = "linux")]
    PuttyDir,
}

pub struct Preview {
    source: Source,
    file: String,
    data: Imported,
    skip_duplicates: bool,
}

#[derive(Default)]
pub struct ImportUi {
    pick: Option<(Source, mpsc::Receiver<std::path::PathBuf>)>,
    preview: Option<Preview>,
    done: Option<String>,
}

fn same(a: &sessions::Session, b: &sessions::Session) -> bool {
    a.protocol == b.protocol
        && a.host.eq_ignore_ascii_case(&b.host)
        && a.port == b.port
        && a.common.username == b.common.username
}

impl App {
    fn parse_import(&self, source: Source, path: &std::path::Path) -> Imported {
        // MobaXterm and regedit write the Windows ANSI code page.
        let legacy = if self.i18n.lang == "ru" {
            "windows-1251"
        } else {
            "windows-1252"
        };
        let text = || {
            std::fs::read(path)
                .map(|b| import::decode(&b, legacy))
                .unwrap_or_default()
        };
        match source {
            Source::MobaXterm => import::mobaxterm(&text()),
            Source::PuttyReg => import::putty_reg(&text()),
            #[cfg(windows)]
            Source::PuttyRegistry => import::putty_registry(),
            #[cfg(target_os = "linux")]
            Source::PuttyDir => import::putty_dir(path),
        }
    }

    pub(crate) fn import_section(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        ui.heading(t("settings-import"));
        ui.label(RichText::new(t("import-about")).weak());
        ui.add_space(10.0);
        if let Some((source, rx)) = &self.import.pick
            && let Ok(path) = rx.try_recv()
        {
            let source = *source;
            self.import.pick = None;
            let data = self.parse_import(source, &path);
            self.import.preview = Some(Preview {
                source,
                file: path.display().to_string(),
                data,
                skip_duplicates: true,
            });
        }
        let mut start: Option<Source> = None;
        ui.horizontal_wrapped(|ui| {
            if ui.button(t("import-mobaxterm")).clicked() {
                start = Some(Source::MobaXterm);
            }
            if ui.button(t("import-putty-reg")).clicked() {
                start = Some(Source::PuttyReg);
            }
            #[cfg(windows)]
            if ui.button(t("import-putty-registry")).clicked() {
                let data = import::putty_registry();
                self.import.preview = Some(Preview {
                    source: Source::PuttyRegistry,
                    file: "HKCU\\Software\\SimonTatham\\PuTTY\\Sessions".into(),
                    data,
                    skip_duplicates: true,
                });
            }
            #[cfg(target_os = "linux")]
            if ui.button(t("import-putty-dir")).clicked()
                && let Some(home) = std::env::var_os("HOME")
            {
                let dir = std::path::Path::new(&home).join(".putty").join("sessions");
                let data = import::putty_dir(&dir);
                self.import.preview = Some(Preview {
                    source: Source::PuttyDir,
                    file: dir.display().to_string(),
                    data,
                    skip_duplicates: true,
                });
            }
        });
        if let Some(source) = start {
            let (tx, rx) = mpsc::channel();
            let mut dialog = rfd::AsyncFileDialog::new();
            dialog = match source {
                Source::MobaXterm => dialog.add_filter("MobaXterm", &["ini", "mxtsessions", "moba"]),
                _ => dialog.add_filter("PuTTY", &["reg"]),
            };
            let file = dialog.pick_file();
            let wake = ui.ctx().clone();
            self.rt.spawn(async move {
                if let Some(f) = file.await {
                    let _ = tx.send(f.path().to_path_buf());
                    wake.request_repaint();
                }
            });
            self.import.pick = Some((source, rx));
            self.import.done = None;
        }
        if let Some(msg) = &self.import.done {
            ui.add_space(8.0);
            ui.label(RichText::new(msg).color(c(theme.ui.success)));
        }
        let Some(p) = &mut self.import.preview else {
            return;
        };
        ui.add_space(12.0);
        ui.label(RichText::new(&p.file).monospace().small());
        let counts = p.data.counts();
        if counts.is_empty() {
            ui.label(RichText::new(t("import-nothing")).color(c(theme.ui.warning)));
        }
        for (tag, n) in &counts {
            ui.label(format!("{tag}: {n}"));
        }
        let dups = p
            .data
            .sessions
            .iter()
            .filter(|(_, s)| self.sessions.iter().any(|x| same(x, s)))
            .count();
        if dups > 0 {
            ui.checkbox(
                &mut p.skip_duplicates,
                i18n.t_args("import-skip-dups", &[("n", &dups.to_string())]),
            );
        }
        if !p.data.skipped.is_empty() {
            ui.add_space(6.0);
            ui.label(i18n.t_args("import-skipped", &[("n", &p.data.skipped.len().to_string())]));
            for (name, kind) in p.data.skipped.iter().take(20) {
                ui.label(RichText::new(format!("  {name} ({kind})")).small().weak());
            }
        }
        let n = p.data.sessions.len() - if p.skip_duplicates { dups } else { 0 };
        ui.add_space(10.0);
        let mut go = false;
        ui.horizontal(|ui| {
            if ui
                .add_enabled_ui(n > 0, |ui| {
                    primary(ui, theme, i18n.t_args("import-go", &[("n", &n.to_string())]))
                })
                .inner
                .clicked()
            {
                go = true;
            }
            if ui.button(t("settings-cancel")).clicked() {
                self.import.preview = None;
            }
        });
        if go && let Some(mut p) = self.import.preview.take() {
            if p.skip_duplicates {
                let existing = self.sessions.clone();
                p.data.sessions.retain(|(_, s)| !existing.iter().any(|x| same(x, s)));
            }
            let root = t(match p.source {
                Source::MobaXterm => "import-root-moba",
                _ => "import-root-putty",
            });
            let (folders, list) = p.data.into_tree(&root);
            let count = list.len();
            for f in &folders {
                if let Err(e) = sessions::save_folder(&self.db, f) {
                    tracing::error!("import folder: {e}");
                }
            }
            for s in &list {
                if let Err(e) = sessions::save_session(&self.db, s) {
                    tracing::error!("import session: {e}");
                }
            }
            self.reload_sessions();
            self.import.done = Some(i18n.t_args("import-done", &[("n", &count.to_string()), ("folder", &root)]));
        }
    }

    pub(crate) fn advanced_section(&mut self, ui: &mut egui::Ui, draft: &mut Settings) {
        let i18n = self.i18n.clone();
        let t = |k: &str| i18n.t(k);
        ui.heading(t("settings-advanced"));
        ui.add_space(10.0);
        ui.checkbox(&mut draft.debug_log, t("advanced-debug-log"));
        ui.label(RichText::new(t("advanced-debug-hint")).small().weak());
        ui.add_space(10.0);
        ui.label(t("advanced-data-dir"));
        ui.label(RichText::new(self.data_dir.display().to_string()).monospace());
        ui.horizontal(|ui| {
            if ui.button(t("advanced-open-logs")).clicked() {
                open_folder(&self.data_dir.join("logs"));
            }
            if ui.button(t("advanced-open-data")).clicked() {
                open_folder(&self.data_dir);
            }
        });
    }
}

pub fn open_folder(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let r = std::process::Command::new("explorer").arg(path).spawn();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let r = std::process::Command::new("xdg-open").arg(path).spawn();
    if let Err(e) = r {
        tracing::warn!("open folder: {e}");
    }
}
