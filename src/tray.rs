//! System tray icon via the StatusNotifierItem D-Bus protocol (what
//! waybar/swaybar's "tray" module on sway actually implements). Using
//! `ksni` instead of GTK-based tray crates keeps this dependency-light and
//! Wayland-native — no libappindicator/GTK needed.
//!
//! NOTE: ksni's public API (`Tray` trait methods, `TrayMethods::spawn`) has
//! shifted a bit across 0.1/0.2 releases. This matches the 0.3 shape at
//! time of writing; if `cargo build` complains, check
//! `cargo doc -p ksni --open` for the exact trait signature.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui;
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, Status, Tray, TrayMethods};

/// Draws a plain filled circle at `size`x`size` as an ARGB32 (network byte
/// order) pixmap `ksni::Icon` — no bundled PNG asset, no dependency on the
/// system icon theme resolving a name (`icon_name` alone previously showed
/// up as a broken/fallback icon on this system when the active theme had
/// no `mail-message-new`).
fn draw_dot_icon(size: i32, rgb: [u8; 3]) -> Icon {
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let center = (size - 1) as f32 / 2.0;
    let radius = center * 0.82;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let dist = (dx * dx + dy * dy).sqrt();
            // A soft 1px edge so it doesn't look jagged at tray sizes.
            let alpha = ((radius + 1.0 - dist).clamp(0.0, 1.0) * 255.0) as u8;
            data.extend_from_slice(&[alpha, rgb[0], rgb[1], rgb[2]]);
        }
    }
    Icon { width: size, height: size, data }
}

struct MatrixTray {
    ctx: egui::Context,
    visible: Arc<AtomicBool>,
    has_error: Arc<AtomicBool>,
    icon_normal: Icon,
    icon_error: Icon,
}

impl MatrixTray {
    fn toggle(&mut self) {
        let now_visible = !self.visible.load(Ordering::SeqCst);
        self.visible.store(now_visible, Ordering::SeqCst);
        set_window_visible(&self.ctx, now_visible);
    }
}

impl Tray for MatrixTray {
    fn id(&self) -> String {
        "matrix-egui-client".to_string()
    }

    fn title(&self) -> String {
        if self.has_error.load(Ordering::SeqCst) {
            "Matrix (connection error)".to_string()
        } else {
            "Matrix".to_string()
        }
    }

    /// Solid ARGB pixmap, not a freedesktop icon-theme lookup — see
    /// `draw_dot_icon`'s doc comment for why.
    fn icon_pixmap(&self) -> Vec<Icon> {
        if self.has_error.load(Ordering::SeqCst) {
            vec![self.icon_error.clone()]
        } else {
            vec![self.icon_normal.clone()]
        }
    }

    /// Most hosts prefer `icon_pixmap` when present, but keep a themed
    /// name around as a fallback for the ones that don't read it.
    fn icon_name(&self) -> String {
        if self.has_error.load(Ordering::SeqCst) {
            "dialog-error".to_string()
        } else {
            "mail-message-new".to_string()
        }
    }

    /// Lets hosts that render status distinctly (e.g. blinking/highlighted)
    /// flag the error state beyond just the icon's color.
    fn status(&self) -> Status {
        if self.has_error.load(Ordering::SeqCst) {
            Status::NeedsAttention
        } else {
            Status::Active
        }
    }

    /// Left-click on the tray icon.
    fn activate(&mut self, _x: i32, _y: i32) {
        tracing::info!("tray: activate (left click) received");
        self.toggle();
    }

    /// Middle-click — some hosts (including some waybar builds) route
    /// middle-click here instead of ever calling `activate`, so wire it to
    /// the same toggle rather than leaving it a no-op.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        tracing::info!("tray: secondary_activate (middle click) received");
        self.toggle();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        tracing::info!("tray: menu (right click) requested");
        let visible = self.visible.load(Ordering::SeqCst);
        vec![
            StandardItem {
                label: if visible { "Hide window".into() } else { "Show window".into() },
                activate: Box::new(|this: &mut Self| {
                    tracing::info!("tray: hide/show window clicked");
                    this.toggle();
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|_this: &mut Self| {
                    tracing::info!("tray: quit clicked");
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// Shows or hides the window. Tried first via `swaymsg` moving the window
/// in/out of the scratchpad (matched by our own PID, so it works
/// regardless of window title) — sway is a tiling WM with no real
/// minimize/restore concept, and winit's Wayland backend has had bugs
/// where a surface hidden via `ViewportCommand::Visible(false)` doesn't
/// reliably remap when set visible again, which is exactly what made the
/// tray icon look unresponsive: `activate` was firing every click (see the
/// tracing logs), the window just never came back. `scratchpad show` is a
/// compositor-driven action, so it also sidesteps Wayland's focus-stealing
/// prevention that can silently swallow a client's own focus request.
/// Falls back to the plain egui viewport commands when `swaymsg` isn't
/// available (not running under sway) or fails.
pub(crate) fn set_window_visible(ctx: &egui::Context, want_visible: bool) {
    if !sway_set_visible(want_visible) {
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(want_visible));
        if want_visible {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }
}

/// Returns `true` if the sway IPC command was issued successfully (meaning
/// the caller should skip the egui-level fallback), `false` otherwise.
fn sway_set_visible(want_visible: bool) -> bool {
    if std::env::var_os("SWAYSOCK").is_none() {
        return false;
    }
    let pid = std::process::id();
    let criteria = format!("[pid={pid}]");
    // `scratchpad show` always brings the window back floating — that's
    // inherent to sway's scratchpad, not something the show itself can be
    // told not to do. `scratchpad show` also focuses the window it just
    // showed, so the trailing `floating disable` (no criteria needed) then
    // re-tiles that same now-focused window, restoring the normal tiled
    // layout it had before being hidden.
    let action = if want_visible {
        "scratchpad show; floating disable"
    } else {
        "move to scratchpad"
    };
    match std::process::Command::new("swaymsg")
        .arg(format!("{criteria} {action}"))
        .status()
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            tracing::warn!(?status, "swaymsg exited non-zero, falling back to viewport commands");
            false
        }
        Err(err) => {
            tracing::warn!(error = %err, "swaymsg unavailable, falling back to viewport commands");
            false
        }
    }
}

/// Starts the tray icon and blocks forever (call this from a dedicated
/// thread with its own tokio runtime — see main.rs). `visible` is shared
/// with the GUI thread's window-close handler so toggling stays correct
/// even when the window was hidden via the OS close button rather than the
/// tray icon itself. `has_error` is shared with `App` — set on any
/// `Event::Error`, cleared on the next healthy sync tick — so the icon
/// visibly flags a broken connection instead of looking identical to the
/// working state.
pub async fn run(ctx: egui::Context, visible: Arc<AtomicBool>, has_error: Arc<AtomicBool>) {
    let tray = MatrixTray {
        ctx,
        visible,
        has_error: has_error.clone(),
        icon_normal: draw_dot_icon(22, [90, 150, 230]),
        icon_error: draw_dot_icon(22, [220, 60, 60]),
    };
    let handle = match tray.spawn().await {
        Ok(handle) => {
            tracing::info!("tray icon registered with the StatusNotifierWatcher");
            handle
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to start tray icon");
            return;
        }
    };

    // `icon_pixmap`/`status`/`title` above read `has_error` fresh on every
    // D-Bus property fetch, but hosts only re-fetch when told a property
    // changed — nothing pushes that notification on its own since
    // `has_error` flips from a plain atomic write on another thread. Poll
    // it and nudge the tray service to emit the change signal whenever it
    // flips.
    let mut last = has_error.load(Ordering::SeqCst);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let now = has_error.load(Ordering::SeqCst);
        if now != last {
            last = now;
            if handle.update(|_tray| {}).await.is_none() {
                // Tray service shut down.
                return;
            }
        }
    }
}
