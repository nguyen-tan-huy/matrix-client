//! This app is not liquid glass. Glass is for the shell's popups (Quick Settings, the volume pill, the
//! launcher, notifications), never an app window, and how a window looks (corners, shadow, border, glass)
//! is the compositor's (swayctl-fx, under swayctl-center's settings), not this app's to set.
//!
//! What stays here is what the panel *does* on swayctl-fx: dock into the scratchpad and show at a size
//! (`toggle_panel`, `dock_when_mapped`). `watch` and `watch_settings` only tell the UI there is no glass.
//!
//! Silently a no-op anywhere else (plain sway/SwayFX, no sway at all, ...).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use crate::event::Event;
use crate::models::GlassConfig;





/// The socket of the sway running now. SWAYSOCK first, but it can point at
/// a sway that's gone: an app opened by a service left running from the last
/// login inherits that service's environment. Then: the newest live
/// `sway-ipc.<uid>.<pid>.sock` in XDG_RUNTIME_DIR.
fn sway_socket() -> Option<UnixStream> {
    if let Some(s) = std::env::var_os("SWAYSOCK").and_then(|p| UnixStream::connect(p).ok()) {
        return Some(s);
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let mut socks: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir).ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("sway-ipc."))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    socks.sort_by(|a, b| b.0.cmp(&a.0));
    socks.into_iter().find_map(|(_, p)| UnixStream::connect(p).ok())
}

/// One sway IPC request on a fresh connection.
fn ipc(kind: u32, payload: &str) -> Option<serde_json::Value> {
    let mut s = sway_socket()?;
    s.set_read_timeout(Some(Duration::from_millis(2000))).ok()?;
    let mut msg = b"i3-ipc".to_vec();
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(payload.as_bytes());
    s.write_all(&msg).ok()?;
    let mut head = [0u8; 14];
    s.read_exact(&mut head).ok()?;
    let len = u32::from_ne_bytes(head[6..10].try_into().ok()?) as usize;
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).ok()?;
    serde_json::from_slice(&body).ok()
}



// ---- quick panel ------------------------------------------------------
//
// The app is a panel like Quick Settings rather than a window that stays on
// screen: it lives in sway's scratchpad (floating, centred on the
// screen) and one command shows or hides it (`chosua --toggle`, bound to
// Super+C). All plain sway IPC; a no-op without sway.

const IPC_RUN_COMMAND: u32 = 0;
const IPC_GET_OUTPUTS: u32 = 3;
const IPC_GET_TREE: u32 = 4;

/// Where the app's window is right now.
#[derive(Clone, Copy, PartialEq, Debug)]
enum PanelState {
    /// a normal window, not in the scratchpad yet
    Loose,
    /// in the scratchpad and on screen
    Shown,
    /// in the scratchpad, out of sight
    Hidden,
}

struct Panel {
    id: i64,
    state: PanelState,
}

fn find_panel(tree: &serde_json::Value, app_id: &str, pid: Option<u32>, in_scratch: bool, out: &mut Option<Panel>) {
    let name = tree.get("name").and_then(|n| n.as_str());
    let hidden_ws = in_scratch || (tree.get("type").and_then(|t| t.as_str()) == Some("workspace") && name == Some("__i3_scratch"));
    if tree.get("app_id").and_then(|a| a.as_str()) == Some(app_id)
        && pid.is_none_or(|p| tree.get("pid").and_then(|v| v.as_u64()) == Some(p as u64))
    {
        let scratch = tree.get("scratchpad_state").and_then(|v| v.as_str()).is_some_and(|s| s != "none");
        let state = if hidden_ws {
            PanelState::Hidden
        } else if scratch {
            PanelState::Shown
        } else {
            PanelState::Loose
        };
        if let Some(id) = tree.get("id").and_then(|v| v.as_i64()) {
            *out = Some(Panel { id, state });
        }
        return;
    }
    for key in ["nodes", "floating_nodes"] {
        if let Some(kids) = tree.get(key).and_then(|k| k.as_array()) {
            for k in kids {
                find_panel(k, app_id, pid, hidden_ws, out);
                if out.is_some() {
                    return;
                }
            }
        }
    }
}

fn panel(app_id: &str, pid: Option<u32>) -> Option<Panel> {
    let tree = ipc(IPC_GET_TREE, "")?;
    let mut out = None;
    find_panel(&tree, app_id, pid, false, &mut out);
    out
}

/// x, y, width, height of the output the panel opens on (the focused one).
fn output_rect() -> Option<(i64, i64, i64, i64)> {
    let outs = ipc(IPC_GET_OUTPUTS, "")?;
    let outs = outs.as_array()?;
    let o = outs
        .iter()
        .find(|o| o.get("focused").and_then(|f| f.as_bool()) == Some(true))
        .or_else(|| outs.iter().find(|o| o.get("active").and_then(|f| f.as_bool()) == Some(true)))?;
    let r = o.get("rect")?;
    Some((r.get("x")?.as_i64()?, r.get("y")?.as_i64()?, r.get("width")?.as_i64()?, r.get("height")?.as_i64()?))
}

/// "resize …, move …" for the panel: a window floating in the middle of the
/// screen, over whatever is tiled behind it.
fn panel_geometry() -> String {
    let (x, y, w, h) = output_rect().unwrap_or((0, 0, 1536, 960));
    let pw = (w * 95 / 100).max(990).min(w);
    // shorter than the screen: the bars at the top and bottom stay clear
    let ph = (h * 87 / 100).max(480).min(h);
    format!("resize set {pw} {ph}, move absolute position {} {}", x + (w - pw) / 2, y + (h - ph) / 2)
}

fn show_command(id: i64) -> String {
    format!("[con_id={id}] scratchpad show, {}", panel_geometry())
}

/// Put the window into the scratchpad and show it as the panel.
fn dock(id: i64) {
    // How it looks (border, corners, shadow) is the compositor's: those are its settings for every window,
    // not this app's to set. Here only what the panel *does*: float, and live in the scratchpad.
    let _ = ipc(IPC_RUN_COMMAND, &format!("[con_id={id}] floating enable, move scratchpad"));
    let _ = ipc(IPC_RUN_COMMAND, &show_command(id));
}

/// `chosua --toggle`: show the panel when it's hidden, hide it when it's
/// up. False when no window of the app is open (the caller starts the app).
pub fn toggle_panel(app_id: &str) -> bool {
    let Some(p) = panel(app_id, None) else { return false };
    match p.state {
        PanelState::Hidden => {
            let _ = ipc(IPC_RUN_COMMAND, &show_command(p.id));
        }
        PanelState::Shown => {
            let _ = ipc(IPC_RUN_COMMAND, &format!("[con_id={}] scratchpad show", p.id));
        }
        PanelState::Loose => dock(p.id),
    }
    true
}

/// At start-up: wait for this process's window, then make it the panel.
pub fn dock_when_mapped(app_id: &str, pid: u32) {
    for _ in 0..100 {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(p) = panel(app_id, Some(pid)) {
            if p.state == PanelState::Loose {
                dock(p.id);
            }
            // the toolkit settles on its own size a moment after the window
            // maps: say the panel's size again, while it's still up
            for _ in 0..4 {
                std::thread::sleep(Duration::from_millis(700));
                match panel(app_id, Some(pid)) {
                    Some(p) if p.state == PanelState::Shown => {
                        let _ = ipc(IPC_RUN_COMMAND, &format!("[con_id={}] {}", p.id, panel_geometry()));
                    }
                    _ => return,
                }
            }
            return;
        }
    }
}




/// This app is not liquid glass: that is for the shell's popups (Quick Settings, the volume pill, the launcher,
/// notifications), never an app window. It says so once and asks the compositor nothing.
pub fn watch(_app_id: &str, tx: UnboundedSender<Event>) {
    let _ = tx.send(Event::GlassConfig(GlassConfig { on: false, ink: false, tint: 0.0, frost: 0.0 }));
}

/// The same for an app on the GTK front end.
pub fn watch_settings(tx: UnboundedSender<Event>) {
    let _ = tx.send(Event::GlassConfig(GlassConfig { on: false, ink: false, tint: 0.0, frost: 0.0 }));
}
