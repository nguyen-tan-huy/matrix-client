//! Liquid glass behind the window on swayctl-fx — the same window rule
//! swayctl-center sends for its own settings window
//! (`modules/effects.py::app_glass`): the window is clear and the
//! compositor shapes the glass by what the page draws. Refraction/blur
//! follow swayctl-center's `effects` settings. Silently a no-op anywhere
//! else (plain sway/SwayFX rejects the command, no sway at all, ...).

use std::process::Command;

const APP_ID: &str = "chosua";

fn effects() -> (bool, i64, i64) {
    let path = std::env::var_os("HOME")
        .map(|h| std::path::PathBuf::from(h).join(".config/swayctl-center/settings.json"));
    let fx = path
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("values")?.get("effects").cloned());
    let get = |k: &str, d: i64| fx.as_ref().and_then(|f| f.get(k)?.as_i64()).unwrap_or(d);
    let glass = fx.as_ref().and_then(|f| f.get("glass")?.as_bool()).unwrap_or(true);
    (glass, get("glass_refraction", 30), get("glass_blur", 0))
}

pub fn apply() {
    if std::env::var_os("SWAYSOCK").is_none() {
        return;
    }
    let (glass, refraction, blur) = effects();
    if !glass {
        return;
    }
    let what = format!(
        "glass enable, glass refraction {refraction}, glass blur {blur}, border none, shadows disable"
    );
    let criteria = format!("[app_id=\"{APP_ID}\"]");
    // for_window covers windows mapped later; the plain criteria command
    // the one already open (quoted so sway doesn't split at the commas).
    let cmd = format!("for_window {criteria} \"{what}\"; {criteria} {what}");
    let _ = Command::new("swaymsg").arg(cmd).output();
}
