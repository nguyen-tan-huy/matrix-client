mod command;
mod event;
mod matrix;
mod models;
mod tray;

use command::Command;
use event::Event;
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

    Ok(Some(png_bytes))
}

fn main() {
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

    // Same worker, same channel-based architecture as the egui version —
    // only the UI side changed. Its own dedicated tokio runtime, same as
    // before, kept independent of Tauri's own async runtime.
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("failed to start tokio runtime");
        rt.block_on(matrix::worker::run(cmd_rx, event_tx));
    });

    tauri::Builder::default()
        // A second launch (e.g. from a launcher keybinding) focuses the
        // existing window instead of opening a duplicate — same reasoning
        // as the egui version's flock-based single-instance guard, just
        // via the officially blessed plugin instead of a hand-rolled lock
        // file, since Tauri already ships one.
        // `tauri-plugin-notification` isn't used — see
        // `Command::ShowNotification`'s doc comment for why its Linux
        // path panics in this app, and why notifications are sent via
        // `notify-rust` directly from the worker instead.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .manage(cmd_tx)
        .manage(Mutex::new(event_rx))
        .invoke_handler(tauri::generate_handler![send_command, poll_events, read_clipboard_image])
        .setup(|app| {
            // Real StatusNotifierItem via `ksni` — see `src/tray.rs` for
            // why this replaced Tauri's own `tray-icon` (its Linux
            // backend, libappindicator, never delivers a click event).
            tauri::async_runtime::spawn(tray::run(app.handle().clone()));

            // Closing the window (the X button) hides it instead of
            // quitting — the sync loop and tray keep running in the
            // background. Quitting for real is only via the tray menu's
            // "Quit", same as the egui version.
            if let Some(window) = app.get_webview_window("main") {
                let window_to_hide = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = window_to_hide.hide();
                    }
                });
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
