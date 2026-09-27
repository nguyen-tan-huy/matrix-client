mod command;
#[cfg(target_os = "linux")]
mod desktop_notify;
mod event;
#[cfg(target_os = "linux")]
mod gtk_theme;
mod matrix;
mod models;
mod platform;
#[cfg(target_os = "linux")]
mod suspend_watch;
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
/// every ~150ms instead of using Tauri's push-based `event.listen`/`emit`.
/// Tried switching to that push-based version once already (confirmed via
/// this comment's own git history) — it stayed broken for two *different*
/// reasons stacked on top of each other, worth recording so a third
/// attempt doesn't have to rediscover both from scratch:
///   1. `capabilities/default.json` granted zero permissions at all, not
///      even `core:event:default` — silently blocked `listen()` on its
///      own. Fixed (that permission is still granted in both capability
///      files) and confirmed *not* sufficient by itself.
///   2. Even with that fixed, a real device test showed the app hanging
///      completely at boot (no `Event` ever visibly reaching the
///      frontend — confirmed via logcat: `worker: run()` and session
///      restore logged fine, but `StartSync` — which only ever fires from
///      `dist/app.js`'s own `SessionChecked` handler — never did). Root
///      cause: a genuine race, not a permissions problem. The worker
///      thread's unprompted `Command::CheckSession` (see `matrix/worker.rs`'s
///      `run()`) fires within milliseconds of the process starting: often
///      *before* the WebView has finished loading `app.js` far enough to
///      call `listen()`, whose own registration is itself an async
///      round-trip to the Rust side. `emit()` doesn't buffer/replay for a
///      listener that subscribes late — it's fire-to-whoever's-currently-
///      subscribed, full stop — so that first, one-time, unprompted
///      `SessionChecked`/`LoggedIn` pair (nothing else ever re-sends it)
///      was lost every time, and nothing after it could ever run.
/// Polling sidesteps this architecturally, not by accident: each call
/// atomically drains whatever's queued *regardless of when this page's own
/// boot code happened to run* — which is exactly the property this app
/// needs anyway for the *other* known Android quirk already documented at
/// the bottom of this file's frontend counterpart (the native host process
/// restarting doesn't reliably mean the WebView's page reloaded with it,
/// so nothing can assume a fresh `listen()` call ever happens at a
/// predictable time relative to the backend's own boot sequence either).
/// A genuinely race-proof push-based version is possible (e.g. buffering
/// early events until a frontend-ready signal, or having the frontend
/// pull once right after `listen()` resolves to catch up on anything
/// already missed) but is real design work, not a one-line fix, so
/// polling stays for now — at the cost of up to ~150ms latency on
/// backend-originated updates, unnoticeable for a chat UI. See `app.js`'s
/// own boot-time comment for how far that interval can safely be
/// stretched (the JS side goes further: slowing it down while the page
/// isn't visible, since a mobile device backgrounded around the clock for
/// notifications is where 150ms-forever actually costs real battery).
#[tauri::command]
async fn poll_events(state: tauri::State<'_, Mutex<mpsc::UnboundedReceiver<Event>>>) -> Result<Vec<Event>, String> {
    let mut rx = state.lock().await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    Ok(events)
}
// produced; they now reach `dist/app.js` via `app.emit("backend-event",
// ...)` in the drain task spawned there, with `window.__TAURI__.event.listen`
// on the JS side (see the "Boot" section of app.js) replacing `poll_events`.

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

                // UnifiedPush endpoint handoff (Android only — see
                // `UnifiedPushServiceImpl.kt`'s doc comment for the whole
                // flow this is one half of). Deliberately *not* a deep
                // link the way OAuth/notification-tap routing above is:
                // `UnifiedPushServiceImpl` is a plain background `Service`
                // that can run with no `MainActivity`/WebView alive at
                // all, so it has no safe way to fire a *second*
                // `startActivity()` here — the comment on
                // `rewriteNotificationLaunchIntent` in `MainActivity.kt`
                // already found that races the deep-link plugin's own
                // async setup on a cold start and silently drops. Instead
                // Kotlin just writes the endpoint to a plain file in the
                // app's data dir, and this reads it back unconditionally
                // on every launch — cold or warm, no timing to get wrong.
                // Re-registering the same pushkey every launch (rather
                // than only when it changed) is what makes this
                // self-healing if a previous attempt failed with no
                // session/network yet, with no success/failure bookkeeping
                // needed on either side.
                #[cfg(target_os = "android")]
                {
                    let pending_path = crate::platform::data_dir().join("unifiedpush_endpoint.txt");
                    if let Ok(endpoint) = std::fs::read_to_string(&pending_path) {
                        let endpoint = endpoint.trim().to_string();
                        if !endpoint.is_empty() {
                            let _ = cmd_tx_for_links.send(Command::SetPushEndpoint { endpoint });
                        }
                    }
                }

                // Shared by both paths below: `on_open_url` only fires for
                // links received *after* it's registered — it does NOT
                // replay the URL that cold-started the app in the first
                // place (confirmed against a real device: tapping a
                // notification while the app was fully killed launched it,
                // but never routed to the room — silently landing on
                // whatever screen was last open instead). The plugin's own
                // docs call this out: "Use `get_current` on app load to
                // check whether your app was started via a deep link."
                fn handle_urls(urls: Vec<url::Url>, cmd_tx: &mpsc::UnboundedSender<Command>) {
                    for url in urls {
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
                                    let _ =
                                        cmd_tx.send(Command::HandleNotificationClick { room_id, thread_id });
                                }
                            }
                            _ => matrix::oidc_callback::handle_redirect_url(url.as_str()),
                        }
                    }
                }

                match app.try_state::<tauri_plugin_deep_link::DeepLink<tauri::Wry>>() {
                    Some(deep_link) => {
                        // Cold-start case: whatever URL launched this process,
                        // if any (a warm-start relaunch, e.g. tapping the
                        // notification while the app was merely backgrounded,
                        // goes through `on_open_url` below instead — this call
                        // returns `None` for that case since nothing "new" was
                        // ever queued for it).
                        match deep_link.get_current() {
                            Ok(Some(urls)) => handle_urls(urls, &cmd_tx_for_links),
                            Ok(None) => {}
                            Err(e) => tracing::warn!("deep-link get_current failed: {e}"),
                        }

                        let cmd_tx_for_open_url = cmd_tx_for_links.clone();
                        deep_link.on_open_url(move |event| {
                            handle_urls(event.urls(), &cmd_tx_for_open_url);
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

            // Auto-restarts the process on resume from suspend, before the
            // known post-sleep freeze (sync loop stuck on a dead pooled
            // connection, or WebKitGTK's compositor left broken by the GPU
            // driver reset) ever gets a chance to show up — see
            // `suspend_watch.rs`.
            #[cfg(target_os = "linux")]
            tauri::async_runtime::spawn(suspend_watch::watch());

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
