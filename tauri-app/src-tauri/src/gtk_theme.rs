//! Mirrors the running GTK theme's resolved colors into the webview as
//! `Event::SystemTheme` — once at startup, and again on every live theme
//! switch — so the UI matches the actual system theme (Sway/GTK, whatever
//! it's set to, auto-dark-mode included) instead of the static built-in
//! palette in `style.css`. Linux desktop only: this app's window is a GTK
//! window under the hood there (WebKitGTK), so the theme it's already
//! rendering itself in — toolbars, dialogs, buttons — is right there for
//! the taking via `StyleContext`, no separate theme-detection story
//! needed. Windows/macOS have no equivalent here; `app.js` just keeps the
//! built-in palette if this event never arrives.
//!
//! Every GTK/GDK call below has to run on the GTK main thread — the same
//! thread `app.run()` is already pumping as its event loop — so it all
//! goes through `AppHandle::run_on_main_thread` to get there, whether
//! called from `.setup()` (itself already on that thread, but
//! `run_on_main_thread` is a no-op-safe way to say so either way) or from
//! this module's own signal-handler callbacks later.

use tauri::{AppHandle, Manager};
use tokio::sync::mpsc::UnboundedSender;

use crate::event::Event;
use crate::models::SystemTheme;

/// Reads the current theme once and starts watching for live changes.
/// Fire-and-forget — nothing here is load-bearing for the rest of the app,
/// so any failure (no "main" window yet, GTK settings unavailable, ...)
/// just means the frontend never gets a `SystemTheme` event and quietly
/// keeps its built-in palette instead.
pub fn watch(app: AppHandle, tx: UnboundedSender<Event>) {
    let Some(window) = app.get_webview_window("main") else {
        tracing::warn!("gtk_theme: no \"main\" window yet, skipping system-theme sync");
        return;
    };

    let result = app.run_on_main_thread(move || {
        let Ok(gtk_window) = window.gtk_window() else {
            tracing::warn!("gtk_theme: window has no GTK handle, skipping system-theme sync");
            return;
        };

        read_and_send(&gtk_window, &tx);

        let Some(settings) = gtk::Settings::default() else {
            return;
        };
        use gtk::prelude::GtkSettingsExt;

        // Two different ways a theme switch reaches this app: the theme
        // name itself changing (picking a different GTK theme), or just
        // the "prefer dark variant" toggle flipping for the *same* theme
        // (most GTK themes ship a `-dark` CSS variant selected this way,
        // which is exactly the "auto dark mode" case) — either one means
        // every color this app cares about needs re-reading.
        {
            let gtk_window = gtk_window.clone();
            let tx = tx.clone();
            settings.connect_gtk_theme_name_notify(move |_| {
                read_and_send(&gtk_window, &tx);
            });
        }
        {
            let gtk_window = gtk_window.clone();
            let tx = tx.clone();
            settings.connect_gtk_application_prefer_dark_theme_notify(move |_| {
                read_and_send(&gtk_window, &tx);
            });
        }
    });
    if let Err(e) = result {
        tracing::warn!(error = %e, "gtk_theme: run_on_main_thread failed, skipping system-theme sync");
    }
}

fn read_and_send(gtk_window: &gtk::ApplicationWindow, tx: &UnboundedSender<Event>) {
    if let Some(theme) = read_theme(gtk_window) {
        let _ = tx.send(Event::SystemTheme(theme));
    }
}

/// Standard GTK3 theme-provided symbolic colors (`@define-color` names
/// every mainstream theme — Adwaita, Arc, Yaru, Breeze-GTK, ... —
/// defines), mapped onto this app's own `--bg`/`--text`/etc. custom
/// properties. `theme_selected_bg_color` is the closest GTK equivalent to
/// "the system accent color": whatever a theme uses to highlight a
/// selected row/active control is exactly the color a Sway/GTK user would
/// recognize as *their* accent. Colors this app needs but no GTK theme
/// reliably names (a dimmed "weak" text tone, a brighter accent variant)
/// are derived from the ones that do exist instead of falling back to a
/// hardcoded hex disconnected from the actual running theme.
fn read_theme(gtk_window: &gtk::ApplicationWindow) -> Option<SystemTheme> {
    use gtk::prelude::{StyleContextExt, WidgetExt};

    let ctx = gtk_window.style_context();
    let lookup = |name: &str| ctx.lookup_color(name).map(|c| rgba_to_hex(&c));

    let bg = lookup("theme_bg_color")?;
    let text = lookup("theme_text_color").or_else(|| lookup("theme_fg_color"))?;
    let bg_alt = lookup("theme_base_color").unwrap_or_else(|| bg.clone());
    let accent = lookup("theme_selected_bg_color").unwrap_or_else(|| text.clone());
    let border = lookup("borders").unwrap_or_else(|| mix_hex(&bg, &text, 0.15));
    let text_weak = mix_hex(&text, &bg, 0.45);
    let accent_strong = mix_hex(&accent, "#ffffff", 0.2);

    Some(SystemTheme {
        bg,
        bg_alt,
        border,
        text,
        text_weak,
        accent,
        accent_strong,
    })
}

fn rgba_to_hex(c: &gdk::RGBA) -> String {
    let to_u8 = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", to_u8(c.red()), to_u8(c.green()), to_u8(c.blue()))
}

fn hex_to_rgb(hex: &str) -> (u8, u8, u8) {
    let hex = hex.trim_start_matches('#');
    let byte = |i: usize| u8::from_str_radix(hex.get(i..i + 2).unwrap_or("00"), 16).unwrap_or(0);
    (byte(0), byte(2), byte(4))
}

/// Blends `a` toward `b` by `t` (`0.0` = pure `a`, `1.0` = pure `b`).
fn mix_hex(a: &str, b: &str, t: f64) -> String {
    let (ar, ag, ab) = hex_to_rgb(a);
    let (br, bg, bb) = hex_to_rgb(b);
    let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    format!("#{:02x}{:02x}{:02x}", mix(ar, br), mix(ag, bg), mix(ab, bb))
}
