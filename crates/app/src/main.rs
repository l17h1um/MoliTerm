// Hide the console window on Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod asks;
mod chrome;
mod desk_view;
#[cfg(any(debug_assertions, feature = "devtools"))]
mod devshot;
mod dialogs;
mod dock;
mod editor;
mod fonts;
mod hotkeys;
mod i18n;
mod icons;
mod import_ui;
mod keys;
mod layout;
mod macros_ui;
mod multiexec;
#[cfg(target_os = "macos")]
mod native_menu;
mod security_ui;
mod sessions_ui;
mod settings_ui;
mod sftp_ui;
mod style;
mod term_view;
mod tunnels_ui;
mod vault_ui;

use anyhow::Context as _;
use moliterm_core::{db::Db, paths, settings::Settings};

fn init_logging(data: &std::path::Path, debug: bool) -> anyhow::Result<tracing_appender::non_blocking::WorkerGuard> {
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("app")
        .filename_suffix("log")
        .max_log_files(7)
        .build(data.join("logs"))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_env("MOLITERM_LOG").unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if debug {
            "debug,wgpu=warn,naga=warn,egui=info,eframe=info"
        } else {
            "info,wgpu=warn,naga=warn"
        })
    });
    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(filter)
        .init();
    Ok(guard)
}

/// A terminal needs little GPU: one native API, the integrated GPU, one queued
/// frame. Loading every backend and the discrete GPU's driver costs ~100 MB.
fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    use eframe::{egui_wgpu, wgpu};
    let mut setup = egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = if cfg!(windows) {
        wgpu::Backends::DX12
    } else if cfg!(target_os = "macos") {
        wgpu::Backends::METAL
    } else {
        wgpu::Backends::VULKAN | wgpu::Backends::GL
    };
    setup.power_preference = wgpu::PowerPreference::LowPower;
    egui_wgpu::WgpuConfiguration {
        surface: egui_wgpu::SurfaceConfig::LOW_LATENCY,
        wgpu_setup: egui_wgpu::WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

/// Process start, for the "first frame" log line (cold-start NFR).
pub static STARTED: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

fn main() -> anyhow::Result<()> {
    STARTED.get_or_init(std::time::Instant::now);
    let data = paths::data_dir();
    // Debug builds: a throw-away data dir for screenshots and experiments.
    #[cfg(any(debug_assertions, feature = "devtools"))]
    let data = std::env::var_os("MOLITERM_DATA_DIR").map_or(data, std::path::PathBuf::from);
    std::fs::create_dir_all(data.join("logs")).with_context(|| format!("creating {}", data.display()))?;
    let db = Db::open(&data.join("MoliTerm.db")).context("opening database")?;
    let settings = Settings::load(&db).context("loading settings")?;
    let _log_guard = init_logging(&data, settings.debug_log)?;
    let default_hook = std::panic::take_hook();
    let crash_marker = data.join("logs").join("last-crash.txt");
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        // Shown as a dialog on the next start (SPEC §11).
        let _ = std::fs::write(&crash_marker, info.to_string());
        default_hook(info);
    }));
    tracing::info!(
        "MoliTerm {} starting, data dir {}",
        env!("CARGO_PKG_VERSION"),
        data.display()
    );

    // Leaked on purpose: dropping a runtime waits for blocking PTY reads.
    let rt: &'static tokio::runtime::Runtime = Box::leak(Box::new(
        tokio::runtime::Builder::new_multi_thread().enable_all().build()?,
    ));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MoliTerm")
            .with_app_id("MoliTerm")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 480.0]),
        // `MOLITERM_RENDERER=glow` switches to OpenGL, for memory comparisons on Windows.
        renderer: if std::env::var("MOLITERM_RENDERER").as_deref() == Ok("glow") {
            eframe::Renderer::Glow
        } else {
            eframe::Renderer::Wgpu
        },
        wgpu_options: wgpu_options(),
        persist_window: true,
        persistence_path: Some(data.join("window.ron")),
        ..Default::default()
    };
    eframe::run_native(
        "MoliTerm",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, rt.handle().clone(), db, settings, data)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
