//! Auto-restarts the process the instant the host resumes from system
//! suspend — automating what the tray's manual "Restart (fix sluggishness
//! after sleep)" item (see `tray::restart_app`'s doc comment) already
//! exists to fix by hand. A process that's sat through a suspend can come
//! back with its matrix-sdk sync loop stuck on a now-dead pooled HTTP
//! connection, or with WebKitGTK's own GPU-compositor context broken by
//! the GPU driver resetting underneath it — neither has an in-place fix,
//! only a fresh process does. Without this, the symptom is exactly what
//! sent someone hunting for the tray's restart item in the first place:
//! the whole window turns unresponsive after the laptop wakes up, and
//! nothing short of a manual restart clears it.
//!
//! Systemd-logind's `PrepareForSleep` signal on the system bus is the
//! standard, race-free way any Linux desktop app observes this — logind
//! fires it with `true` right before the machine actually suspends and
//! `false` the moment it's back, which is unambiguous and instant, unlike
//! trying to infer a suspend from a gap between timer ticks.

use zbus::{proxy, Connection};

#[proxy(
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    interface = "org.freedesktop.login1.Manager"
)]
trait Login1Manager {
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// Runs forever. Fire-and-forget from `lib.rs`'s `.setup()` — nothing else
/// depends on this task, so a system-bus connection failure (logind not
/// present, e.g. some minimal/non-systemd Linux setups) just means this
/// app falls back to needing the tray's manual restart item again, same
/// as before this existed.
pub async fn watch() {
    loop {
        if let Err(err) = watch_once().await {
            tracing::warn!(
                error = %err,
                "suspend_watch: lost connection to the system bus, retrying in 30s"
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

async fn watch_once() -> zbus::Result<()> {
    let connection = Connection::system().await?;
    let manager = Login1ManagerProxy::new(&connection).await?;
    let mut sleep_signals = manager.receive_prepare_for_sleep().await?;

    tracing::info!("suspend_watch: watching logind for suspend/resume");

    while let Some(signal) = futures_util::StreamExt::next(&mut sleep_signals).await {
        let args = signal.args()?;
        // `true` fires right before suspending — nothing to do yet, the
        // restart only makes sense once the app is actually back and a
        // user could hit the frozen state. `false` is the resume.
        if !args.start {
            tracing::info!(
                "suspend_watch: host resumed from suspend, restarting to avoid the known post-sleep freeze"
            );
            crate::tray::restart_app();
        }
    }

    Ok(())
}
