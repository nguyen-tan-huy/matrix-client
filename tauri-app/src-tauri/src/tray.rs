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

#[cfg(target_os = "linux")]
mod linux {
    use ksni::menu::StandardItem;
    use ksni::{Icon, MenuItem, Status, Tray, TrayMethods};
    use tauri::{AppHandle, Manager};

    /// Draws a plain filled circle at `size`x`size` as an ARGB32 (network
    /// byte order) pixmap `ksni::Icon` — no bundled PNG asset, no
    /// dependency on the system icon theme resolving a name.
    fn draw_dot_icon(size: i32, rgb: [u8; 3]) -> Icon {
        let mut data = Vec::with_capacity((size * size * 4) as usize);
        let center = (size - 1) as f32 / 2.0;
        let radius = center * 0.82;
        for y in 0..size {
            for x in 0..size {
                let dx = x as f32 - center;
                let dy = y as f32 - center;
                let dist = (dx * dx + dy * dy).sqrt();
                let alpha = ((radius + 1.0 - dist).clamp(0.0, 1.0) * 255.0) as u8;
                data.extend_from_slice(&[alpha, rgb[0], rgb[1], rgb[2]]);
            }
        }
        Icon { width: size, height: size, data }
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
            "Matrix".to_string()
        }

        fn icon_pixmap(&self) -> Vec<Icon> {
            vec![draw_dot_icon(22, [90, 150, 230])]
        }

        fn icon_name(&self) -> String {
            "mail-message-new".to_string()
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
        let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;
        let menu = MenuBuilder::new(app)
            .item(&show_hide)
            .item(&PredefinedMenuItem::separator(app)?)
            .item(&quit)
            .build()?;

        let icon = app
            .default_window_icon()
            .cloned()
            .ok_or_else(|| tauri::Error::AssetNotFound("default window icon".into()))?;

        TrayIconBuilder::new()
            .icon(icon)
            .menu(&menu)
            .tooltip("Matrix")
            .on_menu_event(|app, event| match event.id.as_ref() {
                "show_hide" => toggle(app),
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
