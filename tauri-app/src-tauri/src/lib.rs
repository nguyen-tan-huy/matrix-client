mod command;
mod event;
#[cfg(target_os = "linux")]
mod gtk_theme;
mod matrix;
mod models;
mod platform;
mod tray;

use command::Command;
use event::Event;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
use tauri::Manager;
use tokio::sync::{mpsc, Mutex};

/// The one bridge between JS and the worker's `Command` enum — JS sends
/// `{ type: "SendMessage", data: {...} }` (matches `Command`'s serde
/// tagging) instead of one hand-written wrapper per variant.
#[tauri::command]
fn send_command(
    state: tauri::State<mpsc::UnboundedSender<Command>>,
    cmd: Command,
) -> Result<(), String> {
    state.send(cmd).map_err(|e| e.to_string())
}

/// Pull side of the worker -> frontend bridge. `dist/app.js` polls this
/// every ~150ms instead of using Tauri's push-based `event.listen` — that
/// API turned out not to be wired up correctly in this setup (its global
/// init script came out empty at build time, and no `Event` this app ever
/// sent — not `LoggedIn`, not `LoginError`, nothing — reached the
/// frontend, while `invoke("send_command", ...)` worked reliably the
/// whole time). Draining a channel through a plain command sidesteps
/// whatever was wrong there entirely, at the cost of up to ~150ms latency
/// on backend-originated updates, which is unnoticeable for a chat UI.
#[tauri::command]
async fn poll_events(state: tauri::State<'_, Mutex<mpsc::UnboundedReceiver<Event>>>) -> Result<Vec<Event>, String> {
    let mut rx = state.lock().await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    Ok(events)
}

/// Reads an image off the OS clipboard and returns it PNG-encoded.
/// WebKitGTK's own `navigator.clipboard.read()` refuses with a permission
/// error in this webview with no way to grant it (confirmed — every
/// attempt fails the same way, no prompt ever appears to accept). The
/// obvious next step, `arboard` (proven working in the egui version of
/// this app), turned out to fail too — but specifically its Wayland
/// backend (`wl_clipboard_rs`, `Seat::Unspecified`) returns
/// "content not available" on this compositor even for a clipboard just
/// set with `wl-copy --type image/png`, confirmed by testing it in
/// isolation. The `wl-paste` *binary* reads that exact same clipboard
/// correctly, so this shells out to it instead of the Rust crate — same
/// underlying protocol, working implementation. `Ok(None)` means the
/// clipboard just doesn't currently hold an image (e.g. plain text), not
/// an error.
#[tauri::command]
fn read_clipboard_image() -> Result<Option<Vec<u8>>, String> {
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match std::process::Command::new("wl-paste")
            .args(["--no-newline", "-t", "image/png"])
            .output()
        {
            Ok(output) if output.status.success() && !output.stdout.is_empty() => {
                return Ok(Some(output.stdout));
            }
            // Non-zero exit / empty output just means "no image on the
            // clipboard" (or, if `wl-paste` isn't installed, `Err` here) —
            // either way, fall through to the arboard/X11 path below.
            _ => {}
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        let image = match clipboard.get_image() {
            Ok(img) => img,
            Err(arboard::Error::ContentNotAvailable) => return Ok(None),
            Err(e) => return Err(e.to_string()),
        };

        let rgba = image::RgbaImage::from_raw(image.width as u32, image.height as u32, image.bytes.into_owned())
            .ok_or("clipboard image had an unexpected size")?;

        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;

        return Ok(Some(png_bytes));
    }

    // Android/iOS: no clipboard-image API wired up yet (arboard doesn't
    // support either), so a clipboard paste in the composer just never
    // finds an image there — not an error, the same as "clipboard has
    // text, not an image" on desktop.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    Ok(None)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // rustls can't auto-select a default `CryptoProvider` when more than
    // one backend is reachable in the dependency graph — and here both
    // `aws-lc-rs` (reqwest's own default) and `ring` (pulled in
    // transitively via `rustls-platform-verifier` -> `rustls-webpki`) are,
    // confirmed via `cargo tree -i ring`/`-i aws-lc-rs`. Without this call,
    // the very first TLS connection (e.g. the OAuth/SSO login flow's
    // `get_login_types()` request) panics the whole tokio worker thread
    // instead of returning an error — observed on Android via `adb
    // logcat` as a silent "nothing happens" from the UI's perspective
    // (the panicked task just never sends its response back), which is
    // what made this look like "the browser doesn't open" rather than
    // "the app already crashed before it got that far". Desktop builds
    // happened to not hit this in testing, but the ambiguity is real
    // there too — this call is unconditional for all targets.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Runs under native Wayland — unlike the old egui/winit version, GTK's
    // own Wayland input-method integration (`gtk-im-context-wayland`)
    // registers correctly with fcitx5 here (`frontend:wayland_v2`,
    // confirmed via `fcitx5-diagnose`), so there's no need for winit's
    // approach of forcing XWayland+XIM. That detour was tried first and
    // worked for IME too (`frontend:dbus`), but XWayland runs unscaled by
    // default and gets upscaled by sway's compositor-side bilinear
    // stretch on this HiDPI output — blurry. Native Wayland renders at the
    // correct resolution directly, no workaround needed.

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,matrix_sdk=warn,matrix_sdk_ui=warn")
            }),
        )
        .init();

    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
    // Cloned before `event_tx` itself is moved into the worker thread
    // below — `gtk_theme::watch` (Linux only) pushes `Event::SystemTheme`
    // through this same channel, reusing the exact plumbing `poll_events`
    // already delivers every other `Event` to `app.js` through, rather
    // than needing a second one just for this.
    #[cfg(target_os = "linux")]
    let theme_event_tx = event_tx.clone();

    let mut builder = tauri::Builder::default();

    // Second-launch focuses the existing window instead of opening a
    // duplicate. Desktop only — the plugin doesn't support mobile, and
    // mobile OSes already enforce a single app instance themselves.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }));
    }

    #[cfg(any(target_os = "android", target_os = "ios"))]
    {
        builder = builder
            .plugin(tauri_plugin_opener::init())
            .plugin(tauri_plugin_notification::init())
            .plugin(tauri_plugin_deep_link::init());
    }

    builder
        .manage(cmd_tx)
        .manage(Mutex::new(event_rx))
        .invoke_handler(tauri::generate_handler![send_command, poll_events, read_clipboard_image])
        .setup(|app| {
            // Resolves data/download dirs (and, on mobile, stashes the
            // `AppHandle`) — must run before the worker thread below
            // touches any path, hence spawning it only after this call
            // instead of before `tauri::Builder` like the pre-refactor
            // version did.
            platform::init(&app.handle().clone());

            // Spawned first and unconditionally, before any other setup
            // step that could plausibly fail (deep-link registration,
            // tray setup, ...) — none of those should ever be able to
            // take the whole app down with them by preventing this from
            // running. (They used to run first: a panic in one of them
            // silently meant this line was never reached at all, and
            // every UI action just hung forever with zero indication why
            // — confirmed the hard way against a real device. That
            // specific case turned out to be `platform::init`'s own
            // notification-permission request hanging on every launch
            // after the first, see its doc comment — but keeping this
            // spawn first and unconditional stays warranted regardless,
            // as insurance against the next thing that can fail here.)
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
                rt.block_on(matrix::worker::run(cmd_rx, event_tx));
            });

            // Routes every `matrixtauriclient://...` deep link this app
            // handles:
            //   - `oauth-callback?loginToken=...` — the SSO/OAuth redirect,
            //     forwarded to `oidc_callback::handle_redirect_url` (see
            //     that module for why mobile catches it this way instead
            //     of desktop's loopback HTTP server).
            //   - `notification?room_id=...&thread_id=...` — fired by
            //     `MainActivity.kt`'s `handleNotificationTap` when the user
            //     taps a notification (this plugin has no click-callback
            //     hook the way desktop's notify-rust does, so the native
            //     side re-dispatches through this instead); forwarded to
            //     the worker as `Command::HandleNotificationClick`, which
            //     just re-emits it as the same `Event::NotificationClicked`
            //     the desktop click path already sends, so `app.js` only
            //     needs the one handler for both platforms.
            // `try_state` instead of the panicking `DeepLinkExt::deep_link()`
            // convenience method — if the plugin's own setup didn't
            // complete for some reason, losing just these two redirect
            // paths is far better than a panic here taking the rest of
            // `.setup()` down with it (the worker spawn above is safe
            // either way now, but nothing after this point should be able
            // to make that true again).
            #[cfg(any(target_os = "android", target_os = "ios"))]
            {
                use tauri::Manager;
                let cmd_tx_for_links = app.state::<mpsc::UnboundedSender<Command>>().inner().clone();
                match app.try_state::<tauri_plugin_deep_link::DeepLink<tauri::Wry>>() {
                    Some(deep_link) => {
                        deep_link.on_open_url(move |event| {
                            for url in event.urls() {
                                match url.host_str() {
                                    Some("notification") => {
                                        let mut pairs = url.query_pairs();
                                        let room_id =
                                            pairs.find(|(k, _)| k == "room_id").map(|(_, v)| v.into_owned());
                                        let thread_id = url
                                            .query_pairs()
                                            .find(|(k, _)| k == "thread_id")
                                            .map(|(_, v)| v.into_owned());
                                        if let Some(room_id) = room_id {
                                            let _ = cmd_tx_for_links
                                                .send(Command::HandleNotificationClick { room_id, thread_id });
                                        }
                                    }
                                    _ => matrix::oidc_callback::handle_redirect_url(url.as_str()),
                                }
                            }
                        });
                    }
                    None => {
                        tracing::error!(
                            "deep-link plugin state missing — OAuth/SSO login won't be able to \
                             catch the browser redirect"
                        );
                    }
                }
            }

            // Real StatusNotifierItem via `ksni` on Linux (see
            // `src/tray.rs` for why: Tauri's own tray-icon backend there,
            // libappindicator, never delivers a click event). Windows/
            // macOS use Tauri's own tray-icon backend directly, which
            // works fine on those two. No tray at all on mobile — there's
            // no persistent background-icon concept to hang a "show/hide
            // window" toggle off of there.
            #[cfg(target_os = "linux")]
            tauri::async_runtime::spawn(tray::run(app.handle().clone()));
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            tray::setup(app.handle())?;

            // Syncs the UI's colors to the running GTK/Sway theme instead
            // of the static built-in palette — see `gtk_theme.rs`.
            #[cfg(target_os = "linux")]
            gtk_theme::watch(app.handle().clone(), theme_event_tx);

            // Closing the window (the X button) hides it instead of
            // quitting on desktop — the sync loop and tray keep running in
            // the background. Quitting for real is only via the tray
            // menu's "Quit". Mobile has no tray to bring the window back
            // from, so it keeps the OS's normal close/backgrounding
            // behavior instead.
            #[cfg(not(any(target_os = "android", target_os = "ios")))]
            if let Some(window) = app.get_webview_window("main") {
                let window_to_hide = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = window_to_hide.hide();
                    }
                });
            }
            // `decorations: false` in tauri.conf.json makes the window
            // fully frameless — deliberate on Linux (this app has no
            // custom drag-region/titlebar built in the frontend to stand
            // in for one, but that's fine under a tiling WM, which doesn't
            // need one anyway) but on Windows it meant no titlebar at all:
            // the window's title ("Matrix") was never shown, and there was
            // no OS-provided way to move/minimize/maximize/close it.
            // `tauri.windows.conf.json` overrides `decorations: true` back
            // on for Windows builds specifically — merged into the base
            // config by `tauri-build`'s build.rs based on the actual
            // compile target (`CARGO_CFG_TARGET_OS`), so this applies even
            // to a plain `cargo build --target x86_64-pc-windows-gnu`, not
            // just `cargo tauri build`. A previous attempt did this at
            // runtime instead, via `window.set_decorations(true)` right
            // here — moved to config so the window is created with its
            // real titlebar from the start rather than toggled on after
            // the frameless one already rendered.

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
