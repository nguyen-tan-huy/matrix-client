//! System tray, split per platform:
//! - Linux: the StatusNotifierItem D-Bus protocol via `ksni` (what
//!   waybar/swaybar's "tray" module actually implements) — Tauri's own
//!   `tray-icon` crate is backed by `libappindicator` there, whose
//!   AppIndicator protocol has no left-click "Activate" concept at all (a
//!   click always just opens the menu, so `on_tray_icon_event` never
//!   fires), which is what forced this alternative in the first place.
//! - Windows/macOS: Tauri's own `tray-icon` backend directly — the
//!   AppIndicator problem above is Linux-specific and doesn't affect
//!   either of these.
//! - Android/iOS: no tray at all (see `lib.rs`'s `setup`, which simply
//!   doesn't call into this module there).

/// Relaunches the app as a fresh process, then exits this one — the
/// tray's "Restart" menu item. Exists because closing the window only
/// hides it (see `lib.rs`'s `CloseRequested` handler) — the process, and
/// whatever it's accumulated over a long uptime (the matrix-sdk sync
/// loop, WebKitGTK's own renderer/GPU-compositor state), otherwise
/// survives indefinitely, including across the host machine suspending
/// and resuming. A real restart gets a fresh WebKitGTK view/compositor
/// context instead of one that just sat through a suspend — which is the
/// same fix reaching for "quit from the tray, then reopen by hand" already
/// gets, just one click instead of two separate ones (and without having
/// to remember the tray only *hides* on a plain window close, not quits).
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn restart_app() -> ! {
    if let Ok(exe) = std::env::current_exe() {
        if let Err(err) = std::process::Command::new(exe).spawn() {
            tracing::error!(error = %err, "restart: failed to relaunch, quitting anyway");
        }
    } else {
        tracing::error!("restart: current_exe() failed, quitting anyway");
    }
    std::process::exit(0);
}

#[cfg(target_os = "linux")]
mod linux {
    use ksni::menu::StandardItem;
    use ksni::{Icon, MenuItem, Status, Tray, TrayMethods};
    use tauri::{AppHandle, Manager};

    /// The app's real icon, decoded once and reused for every
    /// `icon_pixmap()` call — StatusNotifierItem wants raw ARGB32
    /// (network byte order, i.e. A,R,G,B per pixel) pixel data, not a PNG,
    /// so this decodes `icons/tray-icon.png` and reorders its RGBA bytes
    /// into that layout once at startup rather than on every call.
    /// A dedicated source rather than reusing `icons/128x128.png`: the
    /// other one is padded down to ~65% of its canvas so Android's
    /// adaptive-icon mask doesn't crop the dog's ears/paws off — the
    /// desktop tray has no such mask and just looked small with all that
    /// empty margin around it.
    fn app_icon_pixmap() -> Icon {
        static ICON: std::sync::OnceLock<Icon> = std::sync::OnceLock::new();
        ICON.get_or_init(|| {
            let bytes = include_bytes!("../icons/tray-icon.png");
            let img = image::load_from_memory(bytes)
                .expect("bundled tray icon PNG failed to decode")
                .into_rgba8();
            let (width, height) = img.dimensions();
            let mut data = Vec::with_capacity((width * height * 4) as usize);
            for px in img.pixels() {
                let [r, g, b, a] = px.0;
                data.extend_from_slice(&[a, r, g, b]);
            }
            Icon { width: width as i32, height: height as i32, data }
        })
        .clone()
    }

    struct MatrixTray {
        app: AppHandle,
    }

    impl MatrixTray {
        /// Window show()/hide() are GTK calls — they have to happen on the
        /// main thread, not wherever ksni's dbus event loop happens to be
        /// running, hence `run_on_main_thread`.
        fn toggle(&self) {
            let app = self.app.clone();
            let _ = app.clone().run_on_main_thread(move || {
                if let Some(window) = app.get_webview_window("main") {
                    let visible = window.is_visible().unwrap_or(true);
                    if visible {
                        let _ = window.hide();
                    } else {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
            });
        }
    }

    impl Tray for MatrixTray {
        fn id(&self) -> String {
            "matrix-tauri-client".to_string()
        }

        fn title(&self) -> String {
            "ChoSua".to_string()
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            vec![app_icon_pixmap()]
        }

        fn icon_name(&self) -> String {
            // Empty so hosts that prefer a themed name over the pixmap
            // (when one resolves) fall back to `icon_pixmap()` instead —
            // there's no reason to show a generic mail icon over the
            // app's own one now that it's an actual pixmap, not a
            // hand-drawn dot with no icon of its own to be a fallback for.
            String::new()
        }

        fn status(&self) -> Status {
            Status::Active
        }

        /// Left-click on the tray icon.
        fn activate(&mut self, _x: i32, _y: i32) {
            self.toggle();
        }

        /// Middle-click — some hosts route it here instead of `activate`.
        fn secondary_activate(&mut self, _x: i32, _y: i32) {
            self.toggle();
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem {
                    label: "Show/hide window".into(),
                    activate: Box::new(|this: &mut Self| this.toggle()),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Restart (fix sluggishness after sleep)".into(),
                    activate: Box::new(|_this: &mut Self| super::restart_app()),
                    ..Default::default()
                }
                .into(),
                StandardItem {
                    label: "Quit".into(),
                    activate: Box::new(|_this: &mut Self| std::process::exit(0)),
                    ..Default::default()
                }
                .into(),
            ]
        }
    }

    /// Registers the tray icon with the StatusNotifierWatcher. Call from
    /// Tauri's own async runtime (e.g. `tauri::async_runtime::spawn`) —
    /// ksni needs a tokio context and Tauri already runs one.
    pub async fn run(app: AppHandle) {
        let tray = MatrixTray { app };
        match tray.spawn().await {
            Ok(_handle) => {
                tracing::info!("tray icon registered with the StatusNotifierWatcher");
            }
            Err(err) => {
                tracing::error!(error = %err, "failed to start tray icon");
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::run;

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use tauri::{AppHandle, Manager};

    fn toggle(app: &AppHandle) {
        if let Some(window) = app.get_webview_window("main") {
            let visible = window.is_visible().unwrap_or(true);
            if visible {
                let _ = window.hide();
            } else {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }
    }

    /// Registers the tray icon via Tauri's own (working, on these two
    /// platforms) `tray-icon` backend. Called synchronously from
    /// `lib.rs`'s `.setup()` — no dedicated async runtime needed here,
    /// unlike the Linux/ksni path.
    pub fn setup(app: &AppHandle) -> tauri::Result<()> {
        let show_hide = MenuItemBuilder::with_id("show_hide", "Show/hide window").build(app)?;
        let restart =
            MenuItemBuilder::with_id("restart", "Restart (fix sluggishness after sleep)").build(app)?;
        let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
        let menu = MenuBuilder::new(app)
            .item(&show_hide)
            .item(&PredefinedMenuItem::separator(app)?)
            .item(&restart)
            .item(&quit)
            .build()?;

        let icon = app
            .default_window_icon()
            .cloned()
            .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

        TrayIconBuilder::new()
            .icon(icon)
            .menu(&menu)
            .tooltip("ChoSua")
            .on_menu_event(|app, event| match event.id.as_ref() {
                "show_hide" => toggle(app),
                "restart" => super::restart_app(),
                "quit" => std::process::exit(0),
                _ => {}
            })
            .on_tray_icon_event(|tray, event| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                    toggle(tray.app_handle());
                }
            })
            .build(app)?;

        Ok(())
    }
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use desktop::setup;
