//! Linux desktop notifications over ONE shared session-bus connection.
//!
//! This replaces `notify-rust`'s `show_async()` + `wait_for_action_async()`,
//! which opens a brand-new session-bus connection for every notification
//! and keeps it open until that notification is clicked or closed. With
//! swaync in Do Not Disturb (notifications only land on waybar's counter,
//! never as a popup) most notifications are never clicked or closed, so
//! every incoming message leaked one connection for the life of the
//! process — ~360/hour in practice, until the user's `dbus-broker` hit its
//! 1024-fd limit ("Too many open files"), died, and took the whole session
//! (swaync, portals, wireplumber, ...) down with it.
//!
//! Here, one connection is opened lazily, and one listener task maps
//! `ActionInvoked`/`NotificationClosed` signals back to the click callback
//! registered for that notification id. Callbacks for notifications that
//! are never closed are capped at `MAX_PENDING`, oldest dropped first.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use zbus::{proxy, zvariant::Value, Connection};

const MAX_PENDING: usize = 256;

#[proxy(
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications",
    interface = "org.freedesktop.Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

const APP_ICON: &str = "chosua";

type OnClick = Box<dyn FnOnce() + Send>;
// BTreeMap so the oldest (lowest, server ids only increase) is cheap to evict.
type Pending = Arc<Mutex<BTreeMap<u32, OnClick>>>;

struct Notifier {
    proxy: NotificationsProxy<'static>,
    pending: Pending,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Notifier {
    // The listener's signal streams hold their own clone of the connection;
    // aborting it is what actually lets the socket close.
    fn drop(&mut self) {
        self.listener.abort();
    }
}

impl Notifier {
    async fn connect() -> zbus::Result<Self> {
        let connection = Connection::session().await?;
        let proxy = NotificationsProxy::new(&connection).await?;
        let mut invoked = proxy.receive_action_invoked().await?;
        let mut closed = proxy.receive_notification_closed().await?;
        let pending: Pending = Arc::default();

        let listener_pending = pending.clone();
        let listener = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(signal) = invoked.next() => {
                        let Ok(args) = signal.args() else { continue };
                        if args.action_key() != "default" {
                            continue;
                        }
                        let on_click = listener_pending.lock().unwrap().remove(args.id());
                        if let Some(on_click) = on_click {
                            on_click();
                        }
                    }
                    Some(signal) = closed.next() => {
                        if let Ok(args) = signal.args() {
                            listener_pending.lock().unwrap().remove(args.id());
                        }
                    }
                    else => break,
                }
            }
        });

        Ok(Self { proxy, pending, listener })
    }
}

static NOTIFIER: tokio::sync::Mutex<Option<Notifier>> = tokio::sync::Mutex::const_new(None);

/// Shows a notification with a "default" action (clicking its body) that
/// runs `on_click`. Returns as soon as the server has accepted it — nothing
/// waits on the user.
pub async fn show(
    summary: &str,
    body: &str,
    image: Option<&std::path::Path>,
    on_click: impl FnOnce() + Send + 'static,
) -> zbus::Result<()> {
    let mut guard = NOTIFIER.lock().await;
    if guard.is_none() {
        *guard = Some(Notifier::connect().await?);
    }
    let notifier = guard.as_ref().unwrap();

    // Without a declared "default" action, clicking the body on mako/swaync
    // just dismisses instead of firing `ActionInvoked` (confirmed via
    // `dbus-monitor`). The label only shows on servers that render a button.
    // Icon name / desktop entry both match what the Arch package installs
    // (`/usr/share/icons/hicolor/*/apps/chosua.png`, `chosua.desktop`) —
    // left empty, notification daemons just show no icon at all.
    let mut hints = HashMap::new();
    hints.insert("desktop-entry", Value::from(APP_ICON));
    // The sender's avatar — shown as the notification's main image, with
    // the app icon as a small badge on daemons that render both (swaync).
    let image_path = image.map(|p| p.to_string_lossy().into_owned());
    if let Some(image_path) = &image_path {
        hints.insert("image-path", Value::from(image_path.as_str()));
    }
    let result = notifier
        .proxy
        .notify("ChoSua", 0, APP_ICON, summary, body, &["default", "Open"], hints, -1)
        .await;

    match result {
        Ok(id) => {
            let mut pending = notifier.pending.lock().unwrap();
            pending.insert(id, Box::new(on_click));
            while pending.len() > MAX_PENDING {
                pending.pop_first();
            }
            Ok(())
        }
        Err(err) => {
            // Drop (and so close) the connection: if the bus restarted, the
            // next call reconnects instead of failing forever on a dead one.
            *guard = None;
            Err(err)
        }
    }
}
