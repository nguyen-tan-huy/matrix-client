//! Per-platform bits that the worker thread needs but can't get on its own:
//! it runs on a dedicated tokio runtime spawned before `tauri::Builder`
//! finishes, so it never gets an `AppHandle` naturally. `init` stashes one
//! (mobile only, see below) plus the resolved data/download directories the
//! first time it's called from `lib.rs`'s `.setup()`; everything else here
//! just reads that back.
//!
//! Desktop keeps resolving paths via the `directories` crate exactly as
//! before this refactor (same paths as every existing Linux install, so no
//! session/store migration) — only mobile needs `AppHandle`-based
//! resolution, since sandboxed app-data paths there aren't something
//! `directories` can find on its own.

use std::path::PathBuf;
use std::sync::OnceLock;

use tauri::AppHandle;

static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();
static DOWNLOAD_DIR: OnceLock<PathBuf> = OnceLock::new();
#[cfg(any(target_os = "android", target_os = "ios"))]
static APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn init(_app: &AppHandle) {
    // Unchanged from the pre-refactor behavior: a fixed identifier
    // independent of the OS-provided app-data resolution, so existing
    // installs keep using the same on-disk store.
    let dir = directories::ProjectDirs::from("com", "example", "matrix-tauri-client")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./matrix-tauri-client-data"));
    let _ = DATA_DIR.set(dir);

    let download_dir = directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(|d| d.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    let _ = DOWNLOAD_DIR.set(download_dir);
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn init(app: &AppHandle) {
    use tauri::Manager;

    let dir = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| PathBuf::from("./matrix-tauri-client-data"));
    let _ = DATA_DIR.set(dir);

    let download_dir = app
        .path()
        .download_dir()
        .unwrap_or_else(|_| std::env::temp_dir());
    let _ = DOWNLOAD_DIR.set(download_dir);

    let _ = APP_HANDLE.set(app.clone());

    // Android 13+ (API 33+) treats notifications as a runtime permission
    // (`POST_NOTIFICATIONS`, already declared in AndroidManifest.xml) —
    // the plugin never prompts for it on its own, so without this call
    // `show_notification` below would just silently do nothing forever,
    // with no dialog ever having given the user a chance to allow it.
    //
    // On its own, dedicated thread — confirmed the hard way against a real
    // device that `request_permission()` is *not* safe to call inline
    // here: it only returns promptly the one time it actually needs to
    // show the OS dialog (a fresh install). Once the user has answered
    // that dialog, every later launch has nothing left to prompt for, and
    // the call just never returns at all instead of resolving immediately
    // — which, called synchronously from `.setup()` like it was, silently
    // blocked *everything* after it forever, worker thread included: the
    // WebView still rendered (that doesn't depend on `.setup()` finishing)
    // so the app looked alive, but nothing backend-related — session
    // restore, OAuth, sending messages, all of it — ever ran again after
    // the very first launch. Fire-and-forget on its own thread means a
    // hang here only ever costs that one thread, never the app.
    std::thread::spawn({
        let app = app.clone();
        move || {
            use tauri_plugin_notification::NotificationExt;
            let _ = app.notification().request_permission();
        }
    });
}

pub fn data_dir() -> PathBuf {
    DATA_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("./matrix-tauri-client-data"))
}

pub fn download_dir() -> PathBuf {
    DOWNLOAD_DIR.get().cloned().unwrap_or_else(std::env::temp_dir)
}

/// Opens a local file/path in its default application (video player,
/// image viewer, ...). Desktop: the `open` crate, unchanged. Mobile: the
/// official `tauri-plugin-opener`, via the `AppHandle` stashed in `init`.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn open_path(path: impl AsRef<std::path::Path>) -> Result<(), String> {
    open::that(path.as_ref()).map_err(|e| e.to_string())
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn open_path(path: impl AsRef<std::path::Path>) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let app = APP_HANDLE.get().ok_or("app not initialized yet")?;
    app.opener()
        .open_path(path.as_ref().to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

/// Opens a URL in the system browser — used for the OAuth/SSO login step.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn open_url(url: &str) -> Result<(), String> {
    open::that(url).map_err(|e| e.to_string())
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn open_url(url: &str) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let app = APP_HANDLE.get().ok_or("app not initialized yet")?;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

/// Shows an OS notification via `tauri-plugin-notification` — mobile only,
/// see `Command::ShowNotification` in `matrix/worker.rs` for the desktop
/// path (`notify-rust`). `room_id`/`thread_id` ride along as the
/// notification's own `extra` payload purely so `MainActivity.kt`'s
/// `handleNotificationTap` has something to read back out of the tap
/// intent — this plugin has no click-callback hook the way notify-rust
/// does, so routing the tap anywhere useful has to happen on the native
/// side instead, not here.
#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn show_notification(room_id: &str, thread_id: Option<&str>, title: &str, body: &str) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt;
    let app = APP_HANDLE.get().ok_or("app not initialized yet")?;
    // `icon` (a plain white silhouette, `res/drawable/ic_notification.png`)
    // is what shows in the status bar itself — Android tints that one
    // down to just its alpha channel regardless of what's supplied, so a
    // full-color icon there comes out as an illegible blob. `large_icon`
    // is the bigger one shown inside the expanded notification, where
    // color is fine — a separate `ic_notification_large.png` asset for
    // that. iOS has no such split; `icon`/`large_icon` are simply ignored
    // there.
    let mut builder = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .icon("ic_notification")
        .large_icon("ic_notification_large")
        .extra("room_id", room_id);
    if let Some(thread_id) = thread_id {
        builder = builder.extra("thread_id", thread_id);
    }
    builder.show().map_err(|e| e.to_string())
}
