mod app;
mod command;
mod event;
mod matrix;
mod models;
mod notify;
mod single_instance;
mod tray;
mod ui;

use eframe::egui;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use app::{App, Screen};

fn main() -> eframe::Result<()> {
    // `matrix-egui-client --quit` stops an already-running instance (found
    // via the single-instance lock file's PID) instead of starting a new
    // one — handy after installing a rebuilt binary without having to hunt
    // down the PID by hand.
    if std::env::args().any(|a| a == "--quit") {
        single_instance::try_quit_running_instance();
        return Ok(());
    }

    // RUST_LOG controls verbosity, e.g. `RUST_LOG=matrix_egui_client=debug`.
    // Defaults to info-level app logs with SDK internals kept quiet.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| tracing_subscriber::EnvFilter::new("info,matrix_sdk=warn,matrix_sdk_ui=warn"),
        ))
        .init();

    // Must be held for the whole process lifetime — see single_instance.rs.
    let _lock = single_instance::acquire_or_exit();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<command::Command>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<event::Event>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        rt.block_on(matrix::worker::run(cmd_rx, event_tx));
    });

    // Shared with the tray icon so both it and the window's own close
    // button agree on whether the window is currently visible.
    let window_visible = Arc::new(AtomicBool::new(true));

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "matrix",
        native_options,
        Box::new(move |cc| {
            apply_terminal_style(&cc.egui_ctx);
            egui_extras::install_image_loaders(&cc.egui_ctx);

            let app = App::new(cmd_tx, event_rx);

            let tray_ctx = cc.egui_ctx.clone();
            let tray_visible = window_visible.clone();
            let tray_error = app.tray_error_handle();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("failed to start tray runtime");
                rt.block_on(tray::run(tray_ctx, tray_visible, tray_error));
            });

            Ok(Box::new(AppShell { app, window_visible }))
        }),
    )
}

/// Flat, low-contrast, monospace look — layout clarity over visual polish,
/// as requested: no rounded corners, no shadows, no accent colors beyond
/// what's needed to tell selected/unselected apart.
fn apply_terminal_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    // egui's bundled default font (Hack) has no Vietnamese glyph coverage,
    // so diacritics rendered as tofu boxes. Noto Sans Mono covers Vietnamese
    // and keeps the monospace terminal look; it's inserted ahead of Hack so
    // it's tried first, with Hack staying as a fallback for anything it's
    // missing (icons, etc.).
    fonts.font_data.insert(
        "noto_sans_mono".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/fonts/NotoSansMono-Regular.ttf")),
    );
    fonts
        .families
        .entry(egui::FontFamily::Monospace)
        .or_default()
        .insert(0, "noto_sans_mono".to_owned());

    // Use egui's built-in monospace font family for *everything*, not just
    // egui::Monospace-tagged text, for a consistent terminal feel.
    if let Some(mono) = fonts.families.get(&egui::FontFamily::Monospace).cloned() {
        fonts.families.insert(egui::FontFamily::Proportional, mono);
    }
    ctx.set_fonts(fonts);

    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.window_rounding = egui::Rounding::ZERO;
    style.visuals.widgets.noninteractive.rounding = egui::Rounding::ZERO;
    style.visuals.widgets.inactive.rounding = egui::Rounding::ZERO;
    style.visuals.widgets.hovered.rounding = egui::Rounding::ZERO;
    style.visuals.widgets.active.rounding = egui::Rounding::ZERO;
    style.visuals.menu_rounding = egui::Rounding::ZERO;
    style.visuals.window_shadow = egui::Shadow::NONE;
    style.visuals.popup_shadow = egui::Shadow::NONE;
    style.spacing.item_spacing = egui::vec2(6.0, 6.0);
    ctx.set_style(style);
}

struct AppShell {
    app: App,
    window_visible: Arc<AtomicBool>,
}

impl eframe::App for AppShell {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Clicking the OS window-close button hides the window instead of
        // exiting the process — the tray/sync loop keep running in the
        // background. Quitting for real is only via the tray menu's "Quit".
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.window_visible.store(false, Ordering::SeqCst);
            tray::set_window_visible(ctx, false);
        }

        self.app.poll_events(ctx);

        match self.app.screen {
            Screen::CheckingSession => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.centered_and_justified(|ui| ui.label("checking session..."));
                });
            }
            Screen::Login => {
                ui::login::show(&mut self.app, ctx);
            }
            Screen::Chat => {
                ui::room_list::show(&mut self.app, ctx);
                ui::thread_panel::show(&mut self.app, ctx);
                ui::timeline::show(&mut self.app, ctx);
                ui::security::show(&mut self.app, ctx);

                if let Some(status) = self.app.status_line.clone() {
                    egui::TopBottomPanel::bottom("status_line").show(ctx, |ui| {
                        ui.label(egui::RichText::new(status).weak().small());
                    });
                }
            }
        }

        // Keep polling for worker events even with no user input, since
        // messages can arrive from the sync loop at any time.
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }
}
