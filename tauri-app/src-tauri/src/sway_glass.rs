//! Liquid glass behind the window on swayctl-fx, following the shell
//! (swayctl-center's bar, Quick Settings and OSD):
//!
//! * The glass rule itself (`glass enable, glass refraction …`) comes from
//!   swayctl-center — `chosua` is one of its `GLASS_APPS`, so every change in
//!   Settings > Liquid glass re-applies here too. `apply()` only sends a rule
//!   of its own when swayctl-center isn't installed.
//! * `watch()` mirrors those settings into the webview (`Event::GlassConfig`:
//!   on/off, the capsules' white body, frost) and, while glass is on, polls
//!   the compositor for what's *behind* the window (`IPC_GET_BACKDROP` with
//!   `app_id`: the glass probe, without the window's own content) and sends
//!   it as a luminance grid (`Event::GlassBackdrop`). `app.js` picks light or
//!   dark ink per pane from it, the way swayctl-bar does.
//!
//! Silently a no-op anywhere else (plain sway/SwayFX, no sway at all, ...).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use crate::event::Event;
use crate::models::{GlassBackdrop, GlassConfig};

const APP_ID: &str = "chosua";
const IPC_GET_VERSION: u32 = 7;
const IPC_GET_BACKDROP: u32 = 102;
/// Grid the compositor pools the backdrop into (cells of ~25 px on a
/// 1200x800 window: plenty for a room row or a message).
const COLS: u32 = 48;
const ROWS: u32 = 32;

fn settings_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("swayctl-center/settings.json"))
}

/// swayctl-center's `effects` values, with its schema defaults for anything
/// not stored (settings.json keeps only what differs from the defaults).
#[derive(Clone, PartialEq)]
struct Effects {
    center: bool,
    glass: bool,
    opacity: f64,
    blur: f64,
    refraction: i64,
}

fn effects() -> Effects {
    let path = settings_path();
    let center = path.as_ref().is_some_and(|p| p.exists());
    let fx = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("values")?.get("effects").cloned());
    let num = |k: &str, d: f64| fx.as_ref().and_then(|f| f.get(k)?.as_f64()).unwrap_or(d);
    Effects {
        center,
        // schema.py defaults: glass off, opacity 10, blur 0, refraction 50
        glass: fx.as_ref().and_then(|f| f.get("glass")?.as_bool()).unwrap_or(false),
        opacity: num("glass_opacity", 10.0),
        blur: num("glass_blur", 0.0),
        refraction: num("glass_refraction", 50.0) as i64,
    }
}

/// One sway IPC request on a fresh connection.
fn ipc(kind: u32, payload: &str) -> Option<serde_json::Value> {
    let sock = std::env::var_os("SWAYSOCK")?;
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
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

/// The compositor has window glass and the backdrop probe (swayctl-fx).
fn has_window_glass() -> bool {
    ipc(IPC_GET_VERSION, "")
        .and_then(|v| v.get("swayctl_features")?.as_array().cloned())
        .is_some_and(|f| {
            let has = |n: &str| f.iter().any(|x| x.as_str() == Some(n));
            has("glass-windows") && has("backdrop-sample")
        })
}

/// Fallback for a system without swayctl-center: the same rule it would send.
pub fn apply() {
    if std::env::var_os("SWAYSOCK").is_none() {
        return;
    }
    let fx = effects();
    if fx.center || !fx.glass {
        return; // swayctl-center owns the rule (GLASS_APPS), or glass is off
    }
    let what = format!(
        "glass enable, glass refraction {}, glass blur {}, glass text none, border none, shadows disable",
        fx.refraction, fx.blur as i64
    );
    let criteria = format!("[app_id=\"{APP_ID}\"]");
    // for_window covers windows mapped later; the plain criteria command
    // the one already open (quoted so sway doesn't split at the commas).
    let cmd = format!("for_window {criteria} \"{what}\"; {criteria} {what}");
    let _ = Command::new("swaymsg").arg(cmd).output();
}

/// The shell's capsule body for effects.glass_opacity with glass on
/// (swayctl_center/modules/components.py `shell_milk`).
fn lens_tint(opacity: f64) -> f64 {
    ((opacity / 100.0).max(0.04) * 0.20).max(0.04)
}

fn srgb_to_linear(c: f64) -> f64 {
    let c = c / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// What's behind the window, as luminance, or None (not drawn since the last
/// ask, no glass on it, ...).
fn backdrop() -> Option<GlassBackdrop> {
    let payload = format!("{{\"app_id\":\"{APP_ID}\",\"cols\":{COLS},\"rows\":{ROWS}}}");
    let v = ipc(IPC_GET_BACKDROP, &payload)?;
    if v.get("success")?.as_bool() != Some(true) {
        return None;
    }
    let cols = v.get("cols")?.as_u64()? as usize;
    let rows = v.get("rows")?.as_u64()? as usize;
    let cells = v.get("cells")?.as_array()?;
    if cells.len() != cols * rows * 3 {
        return None;
    }
    let lum = cells
        .chunks_exact(3)
        .map(|c| {
            let ch = |i: usize| srgb_to_linear(c[i].as_f64().unwrap_or(0.0));
            (0.2126 * ch(0) + 0.7152 * ch(1) + 0.0722 * ch(2)) as f32
        })
        .collect();
    let env = |k: &str| -> Option<Vec<f32>> {
        v.get(k)?.as_array()?.iter().map(|n| n.as_f64().map(|y| (y / 255.0) as f32)).collect()
    };
    Some(GlassBackdrop {
        w: v.get("w")?.as_f64()?,
        h: v.get("h")?.as_f64()?,
        cols: cols as u32,
        rows: rows as u32,
        lum,
        lmin: env("lmin")?,
        lmax: env("lmax")?,
    })
}

/// Mirror the glass settings into the webview and, while glass is on, keep
/// sending what's behind the window (4x a second; the compositor only has
/// something new when the backdrop changed under the window).
pub fn watch(tx: UnboundedSender<Event>) {
    if std::env::var_os("SWAYSOCK").is_none() {
        let _ = tx.send(Event::GlassConfig(GlassConfig { on: false, tint: 0.0, frost: 0.0 }));
        return;
    }
    let capable = has_window_glass();
    let mut last: Option<GlassConfig> = None;
    let mut checked = Instant::now() - Duration::from_secs(60);
    loop {
        // settings: every 2 s (cheap; Settings writes settings.json on change)
        if checked.elapsed() >= Duration::from_secs(2) {
            checked = Instant::now();
            let fx = effects();
            let cfg = GlassConfig {
                on: capable && fx.glass,
                tint: lens_tint(fx.opacity),
                frost: (fx.blur / 100.0).clamp(0.0, 1.0),
            };
            if last.as_ref() != Some(&cfg) {
                if tx.send(Event::GlassConfig(cfg.clone())).is_err() {
                    return;
                }
                last = Some(cfg);
            }
        }
        if last.as_ref().is_some_and(|c| c.on) {
            if let Some(b) = backdrop() {
                if tx.send(Event::GlassBackdrop(b)).is_err() {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
