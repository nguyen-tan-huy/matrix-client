//! The desktop app's own settings (look, behaviour, pane sizes, shortcuts),
//! kept in `$XDG_CONFIG_HOME/chosua/gtk.json`. Account things live on the
//! server; this is only how this app behaves on this machine.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The font picker's choices (app.js's FONT_FAMILY_OPTIONS): names likely
/// installed, rather than free text that silently falls back.
pub const FONT_FAMILIES: &[(&str, &str)] = &[
    ("Theo hệ thống", ""),
    ("Inter", "Inter"),
    ("Noto Sans", "Noto Sans"),
    ("DejaVu Sans", "DejaVu Sans"),
    ("Liberation Sans", "Liberation Sans"),
    ("Roboto", "Roboto"),
    ("Cantarell", "Cantarell"),
    ("JetBrains Mono (monospace)", "JetBrains Mono"),
    ("DejaVu Sans Mono (monospace)", "DejaVu Sans Mono"),
    ("Noto Sans Mono (monospace)", "Noto Sans Mono"),
    ("Liberation Mono (monospace)", "Liberation Mono"),
    ("Ubuntu Mono (monospace)", "Ubuntu Mono"),
    ("Cascadia Code (monospace)", "Cascadia Code"),
    ("Fira Code (monospace)", "Fira Code"),
    ("Roboto Mono (monospace)", "Roboto Mono"),
    ("IBM Plex Mono (monospace)", "IBM Plex Mono"),
    ("Source Code Pro (monospace)", "Source Code Pro"),
    ("Inconsolata (monospace)", "Inconsolata"),
];

/// Every rebindable action: (id, what it does, default accelerator).
pub const ACTIONS: &[(&str, &str, &str)] = &[
    ("search-rooms", "Tìm phòng", "<Control>k"),
    ("prev-room", "Phòng phía trên", "<Alt>Up"),
    ("next-room", "Phòng phía dưới", "<Alt>Down"),
    ("search-messages", "Tìm tin nhắn", "<Control>f"),
    ("threads", "Mở/đóng danh sách thread", "<Control>t"),
    ("mark-read", "Đánh dấu phòng đã đọc", "<Control><Shift>r"),
    ("settings", "Cài đặt", "<Control>comma"),
    ("shortcuts", "Bảng phím tắt", "<Control>question"),
    ("toggle-sidebar", "Ẩn/hiện danh sách phòng", "<Control>b"),
    ("quit", "Thoát", "<Control>q"),
];

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// "system" | "light" | "dark"
    pub theme: String,
    /// "" = the system's; else a family from FONT_FAMILIES
    pub font_family: String,
    /// px, 12..22; 0 = the system's
    pub font_size: u32,
    /// font weight of message text, 300 (light) .. 800 (extra bold)
    pub message_weight: u32,
    /// Enter sends (Shift+Enter: new line); off: Ctrl+Enter sends
    pub enter_sends: bool,
    pub notifications: bool,
    pub typing_notices: bool,
    /// opening a room also opens its threads panel
    pub auto_threads: bool,
    /// closing the window hides it to the tray instead of quitting
    pub close_to_tray: bool,
    pub start_hidden: bool,
    pub restart_on_resume: bool,
    /// widths (px) of the room list and the thread panel
    pub rooms_width: i32,
    pub side_width: i32,
    pub window_width: i32,
    pub window_height: i32,
    /// action id -> accelerator (only the ones changed from the default)
    pub shortcuts: BTreeMap<String, String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "system".into(),
            font_family: String::new(),
            font_size: 0,
            message_weight: 500,
            enter_sends: true,
            notifications: true,
            typing_notices: true,
            auto_threads: true,
            close_to_tray: true,
            start_hidden: false,
            restart_on_resume: true,
            rooms_width: 340,
            side_width: 380,
            window_width: 1200,
            window_height: 800,
            shortcuts: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn accel(&self, action: &str) -> String {
        self.shortcuts.get(action).cloned().unwrap_or_else(|| {
            ACTIONS.iter().find(|(id, ..)| *id == action).map(|(.., d)| d.to_string()).unwrap_or_default()
        })
    }
}

fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("chosua").join("gtk.json")
}

thread_local! {
    static CONFIG: RefCell<Config> = RefCell::new(load());
}

fn load() -> Config {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn get() -> Config {
    CONFIG.with(|c| c.borrow().clone())
}

/// Change and save.
pub fn update(f: impl FnOnce(&mut Config)) {
    CONFIG.with(|c| {
        f(&mut c.borrow_mut());
        let p = path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(&*c.borrow()) {
            Ok(json) => {
                let tmp = p.with_extension("json.tmp");
                if std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
                    tracing::warn!(path = %p.display(), "could not save settings");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not serialize settings"),
        }
    });
}
