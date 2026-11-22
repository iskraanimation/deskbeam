//! DeskBeam's desktop app: an egui window over the headless engine (plan §3.1).
//!
//! The window holds no state of its own beyond what is being typed. It draws the engine's
//! latest snapshot and sends commands; their results arrive as a new snapshot. Around it sits
//! the desktop shell (plan §3.1b): one app per data directory, the tray, start at login and
//! notifications.

// Release builds on Windows are window programs, so no console opens beside the window. Debug
// builds keep it for logs; `deskbeam-cli` is the terminal program.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod instance;
mod notify;
mod reveal;
mod sound;
mod tray;
mod view;

use std::sync::Arc;

use deskbeam_engine::{Engine, EngineConfig, default_data_dir};
use deskbeam_store::SqliteStore;
use eframe::egui;

fn main() -> anyhow::Result<()> {
    // Started by LAN mode to make the network adapter with admin rights: only that.
    if let Some(code) = deskbeam_engine::run_lan_helper_if_asked() {
        std::process::exit(code);
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Started at login: stay in the tray until asked for. Without a tray there would be no way
    // to the window, so it opens anyway.
    let hidden = tray::AVAILABLE && std::env::args().any(|a| a == autostart::HIDDEN_ARG);

    let data_dir = default_data_dir();
    let (lock, listener) = match instance::claim(&data_dir)? {
        instance::Claim::First { lock, listener } => (lock, listener),
        instance::Claim::AlreadyRunning => {
            eprintln!("DeskBeam is already running; its window was asked to show.");
            return Ok(());
        }
    };

    // The engine runs on its own Tokio runtime; the window's thread only draws.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("engine")
        .build()?;
    let store = Arc::new(SqliteStore::open_in(&data_dir)?);
    let engine = {
        let _inside = runtime.enter();
        Engine::start(EngineConfig::new(&data_dir), store)
    };

    // Where it was last, else 720 × 600 wherever the system puts it.
    let window = egui::ViewportBuilder::default()
        .with_title("DeskBeam")
        .with_inner_size([720.0, 600.0])
        .with_min_inner_size([480.0, 380.0])
        .with_visible(!hidden);
    let mut window = view::window::placed(window, engine.preferences().window);
    // The app's own icon, for the title bar, the taskbar, Alt-Tab and the Dock, where egui's
    // would be. `cargo xtask icon` makes the .ico and .icns from the same picture.
    if let Ok(icon) = eframe::icon_data::from_png_bytes(include_bytes!("../assets/deskbeam.png")) {
        window = window.with_icon(Arc::new(icon));
    }
    let renderer = chosen_renderer();
    let options = eframe::NativeOptions {
        viewport: window,
        renderer,
        ..Default::default()
    };

    let ui_engine = engine.clone();
    let handle = runtime.handle().clone();
    let ran = eframe::run_native(
        "DeskBeam",
        options,
        Box::new(move |cc| {
            let ctx = cc.egui_ctx.clone();
            instance::listen(listener, move || show_window(&ctx));
            Ok(Box::new(view::DeskBeamApp::new(cc, ui_engine, handle)))
        }),
    );

    runtime.block_on(engine.shutdown());
    drop(lock);
    if let Err(e) = &ran
        && renderer == eframe::Renderer::Wgpu
    {
        // Some GPUs, older Intel Macs' among them, fail under wgpu ("Parent device is lost").
        // OpenGL works there. winit lets a process make one event loop only, so the retry is
        // a fresh process.
        eprintln!("The window could not open with wgpu ({e}); trying again with OpenGL.");
        drop(runtime);
        return restart_with(RENDERER_GLOW);
    }
    ran.map_err(|e| anyhow::anyhow!("the window could not open: {e}"))
}

/// `DESKBEAM_RENDERER=glow` draws with OpenGL instead of wgpu.
const RENDERER_ENV: &str = "DESKBEAM_RENDERER";
const RENDERER_GLOW: &str = "glow";

fn chosen_renderer() -> eframe::Renderer {
    match std::env::var(RENDERER_ENV).as_deref() {
        Ok(RENDERER_GLOW) => eframe::Renderer::Glow,
        _ => eframe::Renderer::Wgpu,
    }
}

/// Runs this app again, with the same arguments, drawing with `renderer`.
fn restart_with(renderer: &str) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .args(std::env::args_os().skip(1))
        .env(RENDERER_ENV, renderer);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Only returns if it failed.
        Err(command.exec().into())
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Brings the window back from the tray, or to the front. Safe from any thread: eframe still
/// runs a hidden window's frames when asked to, which is where these commands are carried out.
pub fn show_window(ctx: &egui::Context) {
    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    ctx.request_repaint();
}
