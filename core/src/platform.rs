//! What the worker needs from the host but can't get on its own: where to
//! keep its store, where downloads go, how to open a file/URL and (mobile)
//! how to post a notification.
//!
//! Desktop resolves all of it here with `directories`/`open` — the same
//! paths every existing install already uses, so no store migration. A
//! host that can't (Tauri on Android/iOS: sandboxed paths, opener and
//! notification plugins behind an `AppHandle`) calls `set_hooks` before
//! starting the worker.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub struct Hooks {
    pub data_dir: PathBuf,
    pub download_dir: PathBuf,
    pub open_path: fn(&Path) -> Result<(), String>,
    pub open_url: fn(&str) -> Result<(), String>,
    /// `(room_id, thread_id, title, body)`; `None` = the host has no
    /// notification path of its own (desktop: `worker` posts them itself).
    pub show_notification: Option<fn(&str, Option<&str>, &str, &str) -> Result<(), String>>,
}

static HOOKS: OnceLock<Hooks> = OnceLock::new();

/// Install the host's hooks. Only the first call counts; call it before
/// `matrix::worker::run`.
pub fn set_hooks(hooks: Hooks) {
    let _ = HOOKS.set(hooks);
}

fn hooks() -> &'static Hooks {
    HOOKS.get_or_init(default_hooks)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn default_hooks() -> Hooks {
    // A fixed identifier independent of the front end, so the Tauri and
    // GTK desktop apps share one store (one login, one set of device keys).
    let data_dir = directories::ProjectDirs::from("com", "example", "matrix-tauri-client")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("./matrix-tauri-client-data"));
    let download_dir = directories::UserDirs::new()
        .and_then(|u| u.download_dir().map(|d| d.to_path_buf()))
        .unwrap_or_else(std::env::temp_dir);
    Hooks {
        data_dir,
        download_dir,
        open_path: |p| open::that(p).map_err(|e| e.to_string()),
        open_url: |u| open::that(u).map_err(|e| e.to_string()),
        show_notification: None,
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
fn default_hooks() -> Hooks {
    Hooks {
        data_dir: PathBuf::from("./matrix-tauri-client-data"),
        download_dir: std::env::temp_dir(),
        open_path: |_| Err("platform hooks not set".into()),
        open_url: |_| Err("platform hooks not set".into()),
        show_notification: None,
    }
}

pub fn data_dir() -> PathBuf {
    hooks().data_dir.clone()
}

pub fn download_dir() -> PathBuf {
    hooks().download_dir.clone()
}

pub fn open_path(path: impl AsRef<Path>) -> Result<(), String> {
    (hooks().open_path)(path.as_ref())
}

pub fn open_url(url: &str) -> Result<(), String> {
    (hooks().open_url)(url)
}

pub fn show_notification(room_id: &str, thread_id: Option<&str>, title: &str, body: &str) -> Result<(), String> {
    match hooks().show_notification {
        Some(show) => show(room_id, thread_id, title, body),
        None => Ok(()),
    }
}
