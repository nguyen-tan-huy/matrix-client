//! ChoSua on the Linux desktop: GTK4 + libadwaita over `chosua-core`.
//!
//! Same shape as the Tauri app: the Matrix worker runs on its own tokio
//! runtime; the UI sends it `Command`s and renders the `Event`s it sends
//! back — here straight from the channel on the GTK main loop, no webview
//! or polling in between.

#![recursion_limit = "256"]

mod composer;
mod config;
mod favtabs;
mod images;
mod login;
mod markdown;
mod rooms;
mod roomtools;
mod settings;
mod timeline;
mod tray;
mod viewer;
mod voice;
mod window;

use adw::prelude::*;
use chosua_core::{Command, Event};
use tokio::sync::mpsc;

/// The Wayland app_id: swayctl-center's glass rule (`GLASS_APPS`) and the
/// compositor's backdrop probe both match windows by it.
pub const APP_ID: &str = "chosua";

fn main() -> gtk::glib::ExitCode {
    // `chosua --toggle` (Super+C): show / hide the panel of the running app;
    // with none running it falls through and starts one
    if std::env::args().any(|a| a == "--toggle") && chosua_core::sway_glass::toggle_panel(APP_ID) {
        return gtk::glib::ExitCode::SUCCESS;
    }
    // see tauri-app's `run()`: two rustls backends are reachable
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,matrix_sdk=warn,matrix_sdk_ui=warn")),
        )
        .init();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

    {
        let event_tx = event_tx.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
            rt.block_on(chosua_core::matrix::worker::run(cmd_rx, event_tx));
        });
    }
    // Liquid glass behind the window on swayctl-fx (a fallback rule when
    // swayctl-center isn't installed), and the settings/backdrop watch.
    // the window is a quick panel in sway's scratchpad, not a fixed screen
    std::thread::spawn(|| chosua_core::sway_glass::dock_when_mapped(APP_ID, std::process::id()));
    // the glass settings; the text over the glass is the compositor's job
    // (`glass text light`), so nothing here asks what's behind the window
    std::thread::spawn(move || chosua_core::sway_glass::watch_settings(event_tx));

    // No GApplication id: on Wayland GTK then takes the app_id from the
    // program name, which keeps it "chosua" (an application id needs a dot).
    gtk::glib::set_prgname(Some(APP_ID));
    gtk::glib::set_application_name("ChoSua");
    let app = adw::Application::builder()
        .flags(gtk::gio::ApplicationFlags::NON_UNIQUE)
        .build();

    let event_rx = std::cell::RefCell::new(Some(event_rx));
    app.connect_activate(move |app| {
        let Some(rx) = event_rx.borrow_mut().take() else { return };
        window::build(app, cmd_tx.clone(), rx);
    });
    app.run_with_args::<&str>(&[])
}
