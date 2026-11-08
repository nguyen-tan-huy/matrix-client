//! Tauri-side platform setup. The worker (in `chosua-core`) resolves
//! data/download dirs and opens files/URLs itself on desktop; on mobile
//! those need an `AppHandle` (sandboxed app-data paths, the opener and
//! notification plugins), so `init` hands `chosua_core::platform` hooks
//! backed by one stashed here. Runs before the worker thread starts.

use tauri::AppHandle;

#[cfg(target_os = "android")]
pub use chosua_core::platform::data_dir;

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn init(_app: &AppHandle) {}

#[cfg(any(target_os = "android", target_os = "ios"))]
static APP_HANDLE: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn init(app: &AppHandle) {
    use std::path::PathBuf;
    use tauri::Manager;

    let _ = APP_HANDLE.set(app.clone());
    chosua_core::platform::set_hooks(chosua_core::platform::Hooks {
        data_dir: app
            .path()
            .app_data_dir()
            .unwrap_or_else(|_| PathBuf::from("./matrix-tauri-client-data")),
        download_dir: app.path().download_dir().unwrap_or_else(|_| std::env::temp_dir()),
        open_path,
        open_url,
        show_notification: Some(show_notification),
    });

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

#[cfg(any(target_os = "android", target_os = "ios"))]
fn open_path(path: &std::path::Path) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let app = APP_HANDLE.get().ok_or("app not initialized yet")?;
    app.opener()
        .open_path(path.to_string_lossy(), None::<&str>)
        .map_err(|e| e.to_string())
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn open_url(url: &str) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let app = APP_HANDLE.get().ok_or("app not initialized yet")?;
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn show_notification(room_id: &str, thread_id: Option<&str>, title: &str, body: &str) -> Result<(), String> {
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
