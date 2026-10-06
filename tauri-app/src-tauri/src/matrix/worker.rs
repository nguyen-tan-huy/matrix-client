use std::collections::HashMap;
use std::sync::Arc;

use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::ruma::{OwnedEventId, RoomId};
use matrix_sdk::Client;
use matrix_sdk_ui::sync_service::SyncService;
use matrix_sdk_ui::timeline::{RoomExt, Timeline};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;

use crate::command::Command;
use crate::event::{Event, RoomListKind, RoomListOp};
use crate::matrix::convert::convert_items;
use crate::models::RoomSummary;

/// MSC2545 room-level image pack — a state event, any state key (this app
/// only ever reads/writes the empty key itself, but reads all of them so
/// packs another client set up under a different key still show up).
const IMAGE_PACK_EVENT_TYPE: &str = "im.ponies.room_emotes";
/// MSC2545 personal image pack — global account data, so the same pack
/// follows the user into every room rather than being scoped to one.
const PERSONAL_PACK_EVENT_TYPE: &str = "im.ponies.user_emotes";

/// Pulls `{shortcode: {url}}` pairs out of one pack's `content` (whether
/// from a room state event or the personal-pack account data — same
/// shape either way) into our own model. `default_pack_name` is the
/// fallback when the pack didn't set `pack.display_name`.
fn images_from_pack_content(
    content: &serde_json::Value,
    default_pack_name: &str,
    scope: &str,
) -> Vec<crate::models::EmojiImage> {
    let pack_name = content
        .pointer("/pack/display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(default_pack_name)
        .to_string();
    let Some(image_map) = content.get("images").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    image_map
        .iter()
        .filter_map(|(shortcode, info)| {
            let url = info.get("url").and_then(|v| v.as_str())?;
            Some(crate::models::EmojiImage {
                shortcode: shortcode.clone(),
                url: url.to_string(),
                pack_name: pack_name.clone(),
                scope: scope.to_string(),
            })
        })
        .collect()
}

/// Every custom emoji available to send in a room right now — every
/// `im.ponies.room_emotes` state event in the room (any state key),
/// plus the user's own personal pack. Shared by `Command::ListImagePacks`
/// and `Command::AddImagePackEmoji`'s own refresh after adding one.
async fn list_image_packs(client: &Client, room_id: &str) -> Vec<crate::models::EmojiImage> {
    let mut images = Vec::new();

    if let Some(room) = RoomId::parse(room_id).ok().and_then(|id| client.get_room(&id)) {
        let state_events = room
            .get_state_events(matrix_sdk::ruma::events::StateEventType::from(
                IMAGE_PACK_EVENT_TYPE,
            ))
            .await
            .unwrap_or_default();
        for raw in state_events {
            let value: Option<serde_json::Value> = match &raw {
                matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Sync(r) => {
                    r.deserialize_as().ok()
                }
                matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Stripped(r) => {
                    r.deserialize_as().ok()
                }
            };
            if let Some(value) = value {
                images.extend(images_from_pack_content(
                    value.get("content").unwrap_or(&value),
                    "emoji",
                    "room",
                ));
            }
        }
    }

    if let Ok(Some(raw)) = client
        .account()
        .fetch_account_data(matrix_sdk::ruma::events::GlobalAccountDataEventType::from(
            PERSONAL_PACK_EVENT_TYPE,
        ))
        .await
    {
        if let Ok(value) = raw.deserialize_as::<serde_json::Value>() {
            images.extend(images_from_pack_content(&value, "personal", "personal"));
        }
    }

    images
}

/// Fetches the current content of a pack (room state at the empty key, or
/// the personal account-data pack), applies `mutate` to it, and writes
/// the result back. Shared by `Command::AddImagePackEmoji` and
/// `Command::RemoveImagePackEmoji` — both are "read the whole pack,
/// change one `images` entry, write the whole pack back", since MSC2545
/// packs (like all state/account-data content) only support full-content
/// replacement, not partial patches.
async fn mutate_pack_content(
    client: &Client,
    room_id: &str,
    scope: &str,
    mutate: impl FnOnce(&mut serde_json::Value),
) -> anyhow::Result<()> {
    if scope == "personal" {
        let existing = client
            .account()
            .fetch_account_data(matrix_sdk::ruma::events::GlobalAccountDataEventType::from(
                PERSONAL_PACK_EVENT_TYPE,
            ))
            .await
            .ok()
            .flatten()
            .and_then(|raw| raw.deserialize_as::<serde_json::Value>().ok());
        let mut value = existing.unwrap_or_else(|| serde_json::json!({}));
        mutate(&mut value);
        let raw = matrix_sdk::ruma::serde::Raw::new(&value)?.cast_unchecked();
        client
            .account()
            .set_account_data_raw(
                matrix_sdk::ruma::events::GlobalAccountDataEventType::from(
                    PERSONAL_PACK_EVENT_TYPE,
                ),
                raw,
            )
            .await?;
    } else {
        let room = client
            .get_room(RoomId::parse(room_id)?.as_ref())
            .ok_or_else(|| anyhow::anyhow!("room not found"))?;
        // This app only ever reads/writes the pack at the empty state key
        // — a room can have multiple packs at different state keys
        // (MSC2545 allows that, e.g. several separately-managed packs),
        // but managing more than one from here would need its own picker
        // just to choose which pack to edit.
        let existing = room
            .get_state_event(
                matrix_sdk::ruma::events::StateEventType::from(IMAGE_PACK_EVENT_TYPE),
                "",
            )
            .await
            .ok()
            .flatten()
            .and_then(|raw| match raw {
                matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Sync(r) => {
                    r.deserialize_as::<serde_json::Value>().ok()
                }
                matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState::Stripped(r) => {
                    r.deserialize_as::<serde_json::Value>().ok()
                }
            });
        let mut content = existing
            .and_then(|v| v.get("content").cloned())
            .unwrap_or_else(|| serde_json::json!({}));
        mutate(&mut content);
        room.send_state_event_raw(IMAGE_PACK_EVENT_TYPE, "", content)
            .await?;
    }
    Ok(())
}

fn data_dir() -> std::path::PathBuf {
    // Deliberately a different identifier than the old egui app's
    // ("matrix-egui-client") — sharing one sqlite store between two
    // separate app builds meant a fresh login here (new device_id) could
    // collide with crypto/state data an *old* device already wrote there,
    // which is exactly what caused OAuth login to hang forever with no
    // error (traced back to this, not anything OAuth- or Tauri-specific).
    crate::platform::data_dir()
}

fn session_file() -> std::path::PathBuf {
    data_dir().join("session.json")
}

/// Where confirmed-read room IDs (see `WorkerState::confirmed_read`) are
/// persisted, so a room marked read right before the app quits — before the
/// server's own confirmation of that has had a chance to round-trip through
/// a sync response — doesn't show its old unread badge again on next
/// launch.
fn read_state_file() -> std::path::PathBuf {
    data_dir().join("read_state.json")
}

fn load_read_state() -> std::collections::HashSet<String> {
    std::fs::read_to_string(read_state_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_read_state(state: &std::collections::HashSet<String>) {
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(read_state_file(), json);
    }
}

/// General app config, set from the UI (see `Command::SetLvxApiKey`)
/// rather than only via environment variable — the security panel's
/// "LVX API key" field.
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct AppConfig {
    lvx_api_key: Option<String>,
}

fn config_file() -> std::path::PathBuf {
    data_dir().join("config.json")
}

fn load_config() -> AppConfig {
    std::fs::read_to_string(config_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(config: &AppConfig) {
    if let Ok(json) = serde_json::to_string(config) {
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(config_file(), json);
    }
}

/// The API key `Command::Summarize` actually sends to Longvan's LLM proxy
/// — whichever the user set via the security panel (persisted in
/// `config.json`), falling back to the `LVX_API_KEY` environment variable
/// for anyone who'd rather set it that way instead.
fn lvx_api_key() -> Option<String> {
    load_config()
        .lvx_api_key
        .filter(|k| !k.is_empty())
        .or_else(|| std::env::var("LVX_API_KEY").ok())
}

/// `build_client` wrapped in a hard overall timeout. Homeserver discovery
/// (`.build()`'s well-known/version lookups) happens before matrix-sdk's
/// own per-request retry config ever applies, so a bad or unreachable
/// homeserver URL (typo'd domain, DNS black hole, ...) could hang a login
/// attempt forever with no error ever reaching the UI — exactly what a
/// mistyped homeserver did here. This is the backstop: whatever the
/// specific internal cause, login always fails within 25s instead of
/// hanging silently.
async fn build_client_with_timeout(homeserver: &str) -> anyhow::Result<Client> {
    with_timeout(build_client(homeserver)).await.map_err(|_| {
        anyhow::anyhow!("timed out connecting to \"{homeserver}\" — check the homeserver URL")
    })?
}

/// Same backstop as `build_client_with_timeout`, generalized: several of
/// the OAuth flow's individual steps (`get_login_types`,
/// `get_sso_login_url`, the final `login_token().send()`) are plain HTTP
/// calls to the homeserver that can each hang independently. `login_token`
/// specifically also overrides whatever `RequestConfig` the client was
/// built with (hardcodes `RequestConfig::short_retry()`), so the timeout
/// set in `build_client` doesn't even apply there. This makes every step
/// fail within a bounded time regardless of which one the homeserver
/// actually stalls on.
async fn with_timeout<T>(fut: impl std::future::Future<Output = T>) -> anyhow::Result<T> {
    tokio::time::timeout(std::time::Duration::from_secs(25), fut)
        .await
        .map_err(|_| anyhow::anyhow!("request to the homeserver timed out"))
}

/// Same idea as `with_timeout`, but flattens straight to `Result<T, String>`
/// so call sites that already match on a stringified error (`Err(e) =>
/// tx.send(Event::LoginError(e.to_string()))`) don't need restructuring —
/// a timeout just becomes another string error alongside whatever the
/// wrapped call's own error type would have produced.
async fn with_timeout_str<T, E: std::fmt::Display>(
    fut: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String> {
    match tokio::time::timeout(std::time::Duration::from_secs(25), fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("request to the homeserver timed out".to_string()),
    }
}

/// A fresh login (password or OAuth) always creates a brand new device —
/// reusing the on-disk crypto/state store from a *previous* login attempt
/// then fails with an account/device mismatch (`matrix-sdk-crypto` ties a
/// store to the specific account+device that first initialized it). Only
/// `CheckSession`'s restore path is supposed to reuse the existing store,
/// since that's the one case where the device is meant to match. Wiping it
/// here is safe: a brand new login has no session-dependent state worth
/// keeping yet.
fn clear_stale_store() {
    let dir = data_dir();
    let _ = std::fs::remove_dir_all(dir.join("store"));
    let _ = std::fs::remove_file(session_file());
}

/// Shared error-reporting tail for every spawned command handler in
/// `run()`'s dispatch loop — always forwards `err` to the frontend as
/// `Event::Error` (same as before), but first checks whether it's the
/// server flatly rejecting this device's access token (a 401
/// `M_UNKNOWN_TOKEN`/`M_MISSING_TOKEN` — the session was revoked
/// server-side: logged out from another client, the device removed, or
/// the server invalidated it some other way). `CheckSession`'s
/// `restore_session()` is purely local — it never asks the server whether
/// the stored token still works — so a revoked token wasn't previously
/// caught until *some* real request happened to fail with it, and even
/// then the frontend just showed the raw error and sat there stuck: the
/// chat view stayed up, but every action failed the same way, with no
/// path back to the login screen short of manually clearing app data.
/// Once this fires, that stale session/store can't be reused for
/// anything, so it's wiped here and `Event::SessionExpired` sent
/// alongside so the frontend can drop back to login itself.
async fn report_command_error(err: anyhow::Error, state: &Arc<Mutex<WorkerState>>, tx: &UnboundedSender<Event>) {
    let msg = err.to_string();
    if msg.contains("M_UNKNOWN_TOKEN") || msg.contains("M_MISSING_TOKEN") {
        state.lock().await.client = None;
        clear_stale_store();
        tx.send(Event::SessionExpired).ok();
    }
    tx.send(Event::Error(msg)).ok();
}

async fn build_client(homeserver: &str) -> anyhow::Result<Client> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let client = Client::builder()
        .homeserver_url(homeserver)
        // matrix-sdk's default `RequestConfig` retries indefinitely with
        // no overall cap — a homeserver that's unreachable or just never
        // responds (bad DNS, dropped connection, firewall swallowing the
        // request) hangs the calling command forever with no error ever
        // surfacing to the UI. A login stuck on "signing in..." forever
        // looks indistinguishable from "nothing happened" to the user —
        // exactly what made the OAuth token exchange look like the
        // redirect was never received, when it actually had been; the
        // hang was in the *next* step, in this same builder's absence of
        // a timeout.
        .request_config(
            matrix_sdk::config::RequestConfig::new()
                .timeout(std::time::Duration::from_secs(20))
                .retry_limit(2),
        )
        .sqlite_store(dir.join("store"), None)
        // Without this, an expired access token just fails outright even
        // when the session has a perfectly good refresh token sitting
        // right next to it — the SDK won't use it unless told to. See
        // `spawn_session_change_watcher` for the other half of this (the
        // refreshed tokens still need to be re-persisted to disk, and an
        // outright rejection with no refresh token to fall back on still
        // needs to be reported).
        .handle_refresh_tokens()
        .build()
        .await?;
    Ok(client)
}

/// `MatrixSession` (unlike older matrix-sdk versions) no longer carries the
/// homeserver URL, so it's stashed alongside the session tokens here.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredSession {
    homeserver: String,
    session: matrix_sdk::authentication::matrix::MatrixSession,
}

fn persist_session(client: &Client, homeserver: &str) -> anyhow::Result<()> {
    let session = client
        .matrix_auth()
        .session()
        .ok_or_else(|| anyhow::anyhow!("no session after login"))?;
    let stored = StoredSession {
        homeserver: homeserver.to_string(),
        session,
    };
    let json = serde_json::to_string(&stored)?;
    std::fs::create_dir_all(data_dir())?;
    std::fs::write(session_file(), json)?;
    Ok(())
}

/// Watches `client`'s token lifecycle for as long as this session lasts,
/// so it doesn't rot silently. Two things `handle_refresh_tokens()` alone
/// doesn't cover:
///
/// - `SessionChange::TokensRefreshed`: the SDK swapped in a new access
///   (and, usually, refresh) token on its own — but the on-disk copy
///   `persist_session` wrote at login is now stale. Refresh tokens are
///   typically one-time-use, so restoring from that stale copy after a
///   restart would hand the server an already-spent token and fail
///   outright, throwing away a session that was actually still fine right
///   up until the restart. Re-persisting here keeps the file current.
/// - `SessionChange::UnknownToken`: the server flatly rejected the
///   token — expired with no usable refresh token, revoked, the device
///   removed, etc. Previously the *only* way this surfaced was
///   `report_command_error` noticing "M_UNKNOWN_TOKEN" in some later
///   command's error text — which meant nothing happened until the user's
///   next explicit action, and a request that fails inside a background
///   task (the sliding-sync loop, in particular) never goes through
///   `report_command_error` at all, so the app could sit there fully
///   broken with no explanation until something else finally surfaced it.
///   This reacts to the SDK's own signal immediately instead.
fn spawn_session_change_watcher(
    client: Client,
    homeserver: String,
    state: Arc<Mutex<WorkerState>>,
    tx: UnboundedSender<Event>,
) {
    let mut changes = client.subscribe_to_session_changes();
    tokio::spawn(async move {
        while let Ok(change) = changes.recv().await {
            match change {
                matrix_sdk::SessionChange::TokensRefreshed => {
                    if let Err(e) = persist_session(&client, &homeserver) {
                        tracing::warn!(error = %e, "failed to persist refreshed session tokens");
                    }
                }
                matrix_sdk::SessionChange::UnknownToken(_) => {
                    tracing::warn!("session token rejected by homeserver");
                    state.lock().await.client = None;
                    clear_stale_store();
                    tx.send(Event::SessionExpired).ok();
                    break;
                }
            }
        }
    });
}

/// Everything the worker needs to keep around between commands. Lives only
/// on the tokio side — the UI thread never touches these directly.
struct WorkerState {
    client: Option<Client>,
    room_timelines: HashMap<String, Arc<Timeline>>,
    thread_timelines: HashMap<(String, String), Arc<Timeline>>,
    /// (room_id, thread_root_id) -> `/relations` pagination cursor for
    /// that thread's *next older* page of replies. Absent from the map
    /// entirely before the first page loads; `Some(None)` once the oldest
    /// reply has been reached (nothing left to paginate).
    thread_reply_cursors: HashMap<(String, String), Option<String>>,
    /// room_id -> `/threads` pagination cursor for that room's *next*
    /// (older-activity) page of threads — same idea as
    /// `thread_reply_cursors`, but for the room-level thread list itself
    /// (`Command::ListThreads`/`LoadMoreThreads`) rather than one thread's
    /// replies. Absent from the map entirely before the first page loads;
    /// `Some(None)` once the room's oldest thread has been reached.
    thread_list_cursors: HashMap<String, Option<String>>,
    /// Rooms confirmed read via `Command::MarkRoomRead`, persisted to
    /// `read_state.json` (see that file's doc comment) so it survives a
    /// restart. The read receipt sent to the server is fire-and-forget:
    /// the server only reflects a lower `notification_count` in a *later*
    /// sync response for that room, and if the app quits before that
    /// response lands, the locally cached (stale, pre-receipt) count is
    /// what gets persisted to the sqlite store and reloaded on next
    /// launch — a room correctly marked read still shows its old unread
    /// badge, possibly forever if nothing else happens in it. An entry
    /// here overrides that stale count to 0. It's removed the moment this
    /// room next shows up in a room-list diff (see `entry_to_summary`,
    /// which clears it directly — no debounce needed since each diff is
    /// already scoped to just the room(s) it touched) — at that point the
    /// server has necessarily sent *something* new for the room (our own
    /// receipt being echoed back, if nothing else), so its
    /// `unread_notification_counts()` is fresh and can be trusted directly
    /// again. That's what keeps this from becoming the same permanent,
    /// never-invalidated override that caused the badge to hide genuinely
    /// new messages before.
    confirmed_read: std::collections::HashSet<String>,
    /// Drives the main room list (sliding sync) — `None` until
    /// `Command::StartSync` runs. Kept mainly so `sync_service` isn't
    /// dropped (which would tear down the background sync tasks); nothing
    /// currently reads it back out.
    sync_service: Option<Arc<SyncService>>,
    /// Lets `Command::GrowRoomList` grow the client-side display window of
    /// the main (non-invite) room list — see `Command::StartSync`'s
    /// `filters::new_filter_joined()` listener task, which is what creates
    /// this controller in the first place.
    room_list_controller: Option<matrix_sdk_ui::room_list_service::RoomListDynamicEntriesController>,
    /// Whichever room `Command::WatchTyping` last subscribed to, plus the
    /// drop guard keeping that subscription's internal event handler (and,
    /// transitively, the broadcast channel `Command::WatchTyping`'s spawned
    /// task reads from) alive. Overwriting this on the next `WatchTyping`
    /// drops the previous guard, which both deregisters that old handler
    /// and — since nothing else holds the sending half of its broadcast
    /// channel — closes the channel, which is what lets the old task's
    /// `recv().await` loop end on its own instead of leaking one task per
    /// room ever opened this session.
    typing_guard: Option<(String, matrix_sdk::event_handler::EventHandlerDropGuard)>,
}

/// Dedicated runtime for `FetchImage`/`PlayVideo`. A room with many images
/// can fire off a burst of concurrent downloads (see the lazy-load logic in
/// `ui/timeline.rs` — one per image that scrolls into view); running those
/// on their own worker pool means that burst can never starve timeline or
/// thread loads of scheduling time on the ambient runtime, regardless of
/// how many are in flight at once.
fn image_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("image-fetch")
            .enable_all()
            .build()
            .expect("failed to start image-fetch runtime")
    })
}

/// Dedicated runtime for timeline/thread loading (`LoadTimeline`,
/// `PaginateBack`, `LoadThread`, `ListThreads`) — kept off the ambient
/// runtime too, so a heavy pagination/thread-listing request (each is a
/// handful of sequential network round trips) can't delay unrelated
/// control-plane commands (login, sync, sending a message) queuing up
/// behind it, and vice versa.
fn timeline_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("timeline-load")
            .enable_all()
            .build()
            .expect("failed to start timeline-load runtime")
    })
}

/// Entry point run on the dedicated tokio runtime (see main.rs). Reads
/// `Command`s off `rx` and processes them one at a time, sending results
/// back on `tx`. Long-running things (sync loop, summarize HTTP call) are
/// spawned so they don't block the command loop. Image/video fetches and
/// timeline/thread loads are additionally routed to their own dedicated
/// runtimes (see `image_runtime`/`timeline_runtime`) so heavy traffic on
/// one never delays the others.
pub async fn run(mut rx: UnboundedReceiver<Command>, tx: UnboundedSender<Event>) {
    tracing::info!("worker: run() starting");
    let state = Arc::new(Mutex::new(WorkerState {
        client: None,
        room_timelines: HashMap::new(),
        thread_timelines: HashMap::new(),
        thread_reply_cursors: HashMap::new(),
        thread_list_cursors: HashMap::new(),
        confirmed_read: load_read_state(),
        sync_service: None,
        room_list_controller: None,
        typing_guard: None,
    }));

    // Normally the frontend asks for this itself (`send("CheckSession")`
    // right as `app.js` starts running) — but on Android, the native host
    // process restarting (killed and relaunched, confirmed happening
    // constantly on a real MIUI/HyperOS device) doesn't reliably mean the
    // WebView's page/JS actually reloaded with it; when it doesn't, that
    // boot-time `send` never fires again and the stored session — still
    // perfectly valid on disk — never gets checked, leaving the user
    // stuck on the login screen for no visible reason every time. Running
    // the same check unconditionally here instead, right as the worker
    // itself starts (which — unlike the WebView reload — reliably does
    // happen fresh on every native process start), doesn't depend on the
    // frontend's cooperation at all: whatever it emits reaches `app.js`
    // through the same `poll_events` loop everything else does, whether
    // or not the page's own boot script happened to run this time.
    {
        let state = state.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(Command::CheckSession, state, tx.clone()).await {
                tracing::error!(error = %err, "startup check_session failed");
                let _ = tx.send(Event::Error(err.to_string()));
            }
        });
    }

    while let Some(cmd) = rx.recv().await {
        let state = state.clone();
        let tx = tx.clone();

        match &cmd {
            Command::FetchImage { .. } | Command::PlayVideo { .. } => {
                let cmd_for_task = cmd;
                let state_for_error = state.clone();
                image_runtime().spawn(async move {
                    if let Err(err) = handle(cmd_for_task, state, tx.clone()).await {
                        report_command_error(err, &state_for_error, &tx).await;
                    }
                });
            }
            Command::LoadTimeline { .. }
            | Command::PaginateBack { .. }
            | Command::LoadThread { .. }
            | Command::LoadMoreThreadReplies { .. }
            | Command::ListThreads { .. }
            | Command::SearchUserMessages { .. }
            | Command::SearchMessages { .. }
            | Command::ListAllUsers
            | Command::SearchDirectoryUsers { .. }
            | Command::ListPolls { .. }
            | Command::ResolveSharedEvent { .. } => {
                let cmd_for_task = cmd;
                let state_for_error = state.clone();
                timeline_runtime().spawn(async move {
                    if let Err(err) = handle(cmd_for_task, state, tx.clone()).await {
                        report_command_error(err, &state_for_error, &tx).await;
                    }
                });
            }
            _ => {
                let state_for_error = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle(cmd, state, tx.clone()).await {
                        tracing::error!(error = %err, "command failed");
                        report_command_error(err, &state_for_error, &tx).await;
                    }
                });
            }
        }
    }
}

async fn handle(
    cmd: Command,
    state: Arc<Mutex<WorkerState>>,
    tx: UnboundedSender<Event>,
) -> anyhow::Result<()> {
    match cmd {
        Command::CheckSession => {
            let path = session_file();
            tracing::info!(path = %path.display(), exists = path.exists(), "check_session: looking for stored session");
            if !path.exists() {
                tx.send(Event::SessionChecked(false)).ok();
                return Ok(());
            }
            let raw = std::fs::read_to_string(&path)?;
            let stored: StoredSession = serde_json::from_str(&raw)?;

            // This runs automatically the instant the app launches — on
            // Android specifically (confirmed on a Xiaomi/HyperOS device,
            // same root cause as the OAuth token-exchange retry above),
            // that can be before the OS has actually finished bringing the
            // process's network access back up, so the very first request
            // this makes (`build_client`'s homeserver discovery) fails with
            // a DNS/connect error despite a perfectly valid stored session.
            // Previously that error just propagated out of this whole
            // command via `?` — no `SessionChecked` event either way, so
            // the frontend never learned the check even happened and sat
            // on the login screen forever with no explanation, session
            // file intact and unused. A few quick retries bridges the gap;
            // desktop always succeeds on the first attempt regardless.
            let mut attempt = 0;
            let client = loop {
                match build_client_with_timeout(&stored.homeserver).await {
                    Ok(client) => break client,
                    Err(e) if attempt < 3 => {
                        attempt += 1;
                        tracing::warn!(attempt, error = %e, "check_session: build_client failed, retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(750 * attempt as u64)).await;
                    }
                    Err(e) => {
                        tx.send(Event::SessionChecked(false)).ok();
                        return Err(e);
                    }
                }
            };
            client.restore_session(stored.session).await?;
            spawn_session_change_watcher(client.clone(), stored.homeserver.clone(), state.clone(), tx.clone());
            state.lock().await.client = Some(client);
            tracing::info!("check_session: restored successfully");
            tx.send(Event::SessionChecked(true)).ok();
        }

        Command::Logout => {
            let (client, sync_service) = {
                let mut guard = state.lock().await;
                (guard.client.take(), guard.sync_service.take())
            };
            // Stop the background sync loop first — it holds its own clone
            // of the client and would otherwise keep hitting the server
            // with a token that's about to be (or already is) invalid.
            if let Some(sync_service) = sync_service {
                sync_service.stop().await;
            }
            if let Some(client) = client {
                // Best-effort: even if the server-side logout call fails
                // (already-invalid token, network down, ...) the local
                // session is wiped regardless right below — there's
                // nothing else useful to retry it against, and the user
                // asked to sign out either way.
                if let Err(err) = client.logout().await {
                    tracing::warn!(error = %err, "logout: server-side logout call failed, clearing local session anyway");
                }
            }
            clear_stale_store();
            {
                let mut guard = state.lock().await;
                guard.room_timelines.clear();
                guard.thread_timelines.clear();
                guard.thread_reply_cursors.clear();
                guard.thread_list_cursors.clear();
                guard.confirmed_read.clear();
                guard.room_list_controller = None;
                guard.typing_guard = None;
            }
            tx.send(Event::LoggedOut).ok();
        }

        Command::LoginPassword {
            homeserver,
            username,
            password,
        } => {
            clear_stale_store();
            match build_client_with_timeout(&homeserver).await {
                Ok(client) => {
                    match with_timeout_str(
                        client
                            .matrix_auth()
                            .login_username(&username, &password)
                            .initial_device_display_name("Matrix egui Client")
                            .request_refresh_token()
                            .send(),
                    )
                    .await
                    {
                        Ok(_) => {
                            let _ = persist_session(&client, &homeserver);
                            spawn_session_change_watcher(client.clone(), homeserver.clone(), state.clone(), tx.clone());
                            state.lock().await.client = Some(client);
                            tx.send(Event::LoggedIn).ok();
                        }
                        Err(e) => {
                            tx.send(Event::LoginError(e.to_string())).ok();
                        }
                    }
                }
                Err(e) => {
                    tx.send(Event::LoginError(e.to_string())).ok();
                }
            }
        }

        Command::LoginOAuth { homeserver } => {
            clear_stale_store();
            match build_client_with_timeout(&homeserver).await {
                Ok(client) => {
                    use matrix_sdk::ruma::api::client::session::get_login_types::v3::LoginType;

                    match with_timeout_str(client.matrix_auth().get_login_types()).await {
                        Ok(login_types) => {
                            let supports_sso = login_types
                                .flows
                                .iter()
                                .any(|f| matches!(f, LoginType::Sso(_)));
                            if !supports_sso {
                                tx.send(Event::LoginError(
                                    "This homeserver does not advertise SSO/OAuth login"
                                        .to_string(),
                                ))
                                .ok();
                                return Ok(());
                            }

                            let redirect_url =
                                crate::matrix::oidc_callback::listen_for_redirect().await;

                            match with_timeout_str(
                                client
                                    .matrix_auth()
                                    .get_sso_login_url(redirect_url.as_str(), None),
                            )
                            .await
                            {
                                Ok(sso_url) => {
                                    if let Err(e) = crate::platform::open_url(sso_url.as_str()) {
                                        tx.send(Event::LoginError(format!(
                                            "failed to open browser: {e}"
                                        )))
                                        .ok();
                                        return Ok(());
                                    }

                                    tracing::info!("oauth: waiting for browser redirect");
                                    match crate::matrix::oidc_callback::wait_for_token().await {
                                        Ok(login_token) => {
                                            tracing::info!(
                                                "oauth: got login token, exchanging for session"
                                            );
                                            // Android (confirmed on a Xiaomi/HyperOS device):
                                            // the app backgrounds itself to let the external
                                            // browser complete the SSO redirect, and the OS
                                            // hasn't necessarily restored this process's
                                            // network access yet by the time the redirect
                                            // deep-link brings it back to the foreground a
                                            // moment later — the very first request after
                                            // resuming fails with a DNS/connect error even
                                            // though the exact same homeserver was reachable
                                            // seconds earlier (both from this process, for
                                            // `get_sso_login_url`, and from the browser that
                                            // just logged in through it). A few quick retries
                                            // bridges that gap; harmless on desktop, where
                                            // this always just succeeds on the first try.
                                            let mut attempt = 0;
                                            let result = loop {
                                                let outcome = with_timeout_str(
                                                    client
                                                        .matrix_auth()
                                                        .login_token(&login_token)
                                                        .initial_device_display_name(
                                                            "Matrix egui Client",
                                                        )
                                                        .request_refresh_token()
                                                        .send(),
                                                )
                                                .await;
                                                attempt += 1;
                                                if outcome.is_ok() || attempt >= 4 {
                                                    break outcome;
                                                }
                                                tracing::warn!(
                                                    attempt,
                                                    "oauth: token exchange failed, retrying"
                                                );
                                                tokio::time::sleep(std::time::Duration::from_millis(
                                                    750 * attempt as u64,
                                                ))
                                                .await;
                                            };
                                            match result {
                                                Ok(_) => {
                                                    tracing::info!(
                                                        "oauth: token exchange succeeded"
                                                    );
                                                    let _ = persist_session(&client, &homeserver);
                                                    spawn_session_change_watcher(client.clone(), homeserver.clone(), state.clone(), tx.clone());
                                                    state.lock().await.client = Some(client);
                                                    tx.send(Event::LoggedIn).ok();
                                                }
                                                Err(e) => {
                                                    tracing::warn!(error = %e, "oauth: token exchange failed");
                                                    tx.send(Event::LoginError(e.to_string())).ok();
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(error = %e, "oauth: wait_for_token failed");
                                            tx.send(Event::LoginError(e.to_string())).ok();
                                        }
                                    }
                                }
                                Err(e) => {
                                    tx.send(Event::LoginError(e.to_string())).ok();
                                }
                            }
                        }
                        Err(e) => {
                            tx.send(Event::LoginError(e.to_string())).ok();
                        }
                    }
                }
                Err(e) => {
                    tx.send(Event::LoginError(e.to_string())).ok();
                }
            }
        }

        Command::StartSync => {
            // Guards against ever running this handler's body twice in one
            // process lifetime. It's only ever meant to run once (on
            // `SessionChecked`/`LoggedIn`, right after login/restore — see
            // `app.js`), but nothing previously enforced that: every
            // `register_*_handler` call below is a bare `client.add_event_handler`
            // (matrix-sdk stacks handlers, it doesn't replace by type), and
            // the `SyncService` built further down replaces
            // `state.sync_service` outright with no `.stop()` on whatever
            // was there before — `SyncService` has no `Drop` impl, so its
            // background sync task doesn't get cancelled just because the
            // `Arc` referencing it was overwritten; it keeps running,
            // detached, forever. A second `StartSync` firing for any
            // reason (a future bug, a frontend double-send, ...) would
            // therefore have silently doubled every live event and every
            // sync loop running in the process from then on — worse with
            // each further occurrence — which reads exactly like "gets
            // laggier over time, only a full restart fixes it." Cheap
            // enough to guard against outright even without a confirmed
            // trigger for it today.
            if state.lock().await.sync_service.is_some() {
                tracing::warn!("StartSync: already running, ignoring duplicate call");
                return Ok(());
            }
            let client = get_client(&state).await?;
            register_new_message_handler(&client, tx.clone());
            register_redaction_handler(&client, tx.clone());
            register_reaction_handler(&client, tx.clone());
            register_sticker_handler(&client, tx.clone());
            register_receipt_handler(&client, tx.clone());
            register_pinned_events_handler(&client, tx.clone());
            register_poll_handlers(&client, tx.clone());
            register_presence_handler(&client, tx.clone());
            crate::matrix::verification::register_verification_handler(&client, tx.clone());

            // Marks the account online as soon as sync starts. Fire-and-
            // forget on its own task — a slow/failed presence write
            // shouldn't hold up the rest of `StartSync`, and there's
            // nothing useful to answer back to the UI with either way
            // (presence isn't something this app's login flow surfaces at
            // all). `SyncSettings::set_presence` (the "normal" way to set
            // this) only takes effect on the *next* `/sync` request, which
            // under `SyncService`/`RoomListService` this app no longer
            // calls directly — going through the raw `set_presence`
            // endpoint instead makes it happen immediately regardless.
            {
                let client = client.clone();
                tokio::spawn(async move {
                    let Some(user_id) = client.user_id().map(|id| id.to_owned()) else {
                        return;
                    };
                    use matrix_sdk::ruma::api::client::presence::set_presence;
                    use matrix_sdk::ruma::presence::PresenceState;
                    let request = set_presence::v3::Request::new(user_id, PresenceState::Online);
                    if let Err(e) = client.send(request).await {
                        tracing::warn!(error = %e, "failed to set presence online");
                    }
                });
            }

            tracing::info!("starting sync (RoomListService / sliding sync, MSC4186)");

            // `SyncService` is the SDK's own recommended entry point for
            // driving `RoomListService` — its own `sync()` stream is
            // explicitly documented as "should be used only for testing".
            // This replaces the old unbounded `client.sync_with_callback`
            // loop. `RoomListService::all_rooms()` starts small and grows
            // in the background (`SlidingSyncMode::Growing`), which is what
            // keeps an account with thousands of rooms from stalling the
            // room list on startup — contrast the removed
            // `client.rooms()`-based `refresh_rooms`, which pulled every
            // room in the account on every refresh.
            let sync_service = SyncService::builder(client.clone()).build().await?;
            let room_list_service = sync_service.room_list_service();
            sync_service.start().await;
            state.lock().await.sync_service = Some(Arc::new(sync_service));

            // TEMP diagnostics — logs every RoomListService state
            // transition (Init/SettingUp/Running/...) so we can see
            // whether it ever reaches `Running` (which is what flips
            // `all_rooms` from its small initial selective range into
            // `Growing` mode).
            {
                let mut state_stream = room_list_service.state();
                tokio::spawn(async move {
                    while let Some(s) = futures_util::StreamExt::next(&mut state_stream).await {
                        tracing::info!(?s, "RoomListService state changed");
                    }
                });
            }

            let room_list = Arc::new(room_list_service.all_rooms().await?);

            // TEMP diagnostics — `add_one_page()` silently no-ops if the
            // server hasn't reported a `maximum_number_of_rooms` yet (or
            // if the display limit has already caught up to it), which is
            // exactly the kind of thing that would explain "grow never
            // does anything." This logs every change to that value.
            {
                let mut loading_state_stream = room_list.loading_state();
                tokio::spawn(async move {
                    while let Some(s) =
                        futures_util::StreamExt::next(&mut loading_state_stream).await
                    {
                        tracing::info!(?s, "RoomList loading_state changed");
                    }
                });
            }

            // Main (non-invite) room list — client-side-paginated over
            // whatever `all_rooms` has synced so far (starts at `PAGE_SIZE`,
            // grows one page at a time via `Command::GrowRoomList`). See
            // `RoomListOp`'s doc comment for why MSC4186 only supports this
            // "growing prefix" model rather than true server-side viewport
            // ranges.
            {
                const PAGE_SIZE: usize = 50;
                let room_list = room_list.clone();
                let state = state.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (stream, controller) = room_list.entries_with_dynamic_adapters(PAGE_SIZE);
                    // Stream only starts yielding after a filter is set —
                    // `new_filter_joined()` also happens to be exactly the
                    // "not an invite, not left" set this view wants.
                    controller.set_filter(Box::new(
                        matrix_sdk_ui::room_list_service::filters::new_filter_joined(),
                    ));
                    state.lock().await.room_list_controller = Some(controller);
                    run_room_list_listener(stream, RoomListKind::Rooms, false, state, tx).await;
                });
            }

            // Invites — a second, independent filtered/paginated view over
            // the *same* underlying `all_rooms` list (MSC4186 doesn't split
            // invites into their own list the way the older MSC3575 draft
            // did). Invite counts are always small, so one large fixed page
            // covers any realistic account without needing to grow it.
            {
                const INVITES_PAGE_SIZE: usize = 200;
                let room_list = room_list.clone();
                let state = state.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (stream, controller) =
                        room_list.entries_with_dynamic_adapters(INVITES_PAGE_SIZE);
                    controller.set_filter(Box::new(
                        matrix_sdk_ui::room_list_service::filters::new_filter_invite(),
                    ));
                    run_room_list_listener(stream, RoomListKind::Invites, true, state, tx).await;
                });
            }

            // Spaces (`m.room.create`'s `type: m.space`) are hardcoded out
            // of `RoomListService`'s own list server-side, and MSC4186 only
            // supports *excluding* room types from a list filter, not
            // including only specific ones — so there's no way to ask this
            // same list for "just the spaces" either. This second, small,
            // unfiltered sliding-sync session's only job is to get spaces
            // into the local store at all; `refresh_spaces` (called
            // whenever this session reports a change) still just reads
            // `client.rooms()` filtered to `is_space()`, same as the very
            // first version of this code — safe now because this session
            // guarantees the local store actually knows about them.
            {
                let client = client.clone();
                let state = state.clone();
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = run_spaces_catchall_sync(client, state, tx).await {
                        tracing::warn!(error = %e, "spaces catch-all sliding sync failed");
                    }
                });
            }

            // See `run_invites_catchall_sync`'s doc comment — without this,
            // an invite to a room ranked far below the main list's growing
            // frontier (by recency) could take arbitrarily long, or never,
            // to actually surface in the invites tab.
            {
                let client = client.clone();
                tokio::spawn(async move {
                    if let Err(e) = run_invites_catchall_sync(client).await {
                        tracing::warn!(error = %e, "invites catch-all sliding sync failed");
                    }
                });
            }

            // Nothing left needs `dirty_rooms`'s old "clear stale
            // confirmed_read overrides" job to run on a fixed timer anymore
            // — `entry_to_summary` (called straight from each diff as it
            // arrives, no debounce) clears them the moment a room shows up
            // fresh in any diff. See that field's doc comment.
        }

        Command::RefreshRooms => {
            // No "rescan everything" primitive exists under sliding sync
            // (the room list is always live) — this now just forces an
            // immediate Spaces re-scan instead of waiting for the spaces
            // catch-all session's own next change notification.
            refresh_spaces(&state, &tx).await?;
        }

        Command::GrowRoomList => {
            let controller = state.lock().await.room_list_controller.take();
            match &controller {
                Some(_) => tracing::info!("GrowRoomList: calling add_one_page()"),
                None => tracing::warn!("GrowRoomList: no room_list_controller in state"),
            }
            if let Some(controller) = controller {
                controller.add_one_page();
                state.lock().await.room_list_controller = Some(controller);
            }
        }

        Command::LoadTimeline { room_id } => {
            let client = get_client(&state).await?;
            // `room_list_service::Room::subscribe()` — which used to force
            // the server to keep sending this room's events regardless of
            // its position in the synced list — doesn't exist in this SDK
            // version, so this just resolves the room directly. In
            // practice this is less of a gap than it sounds: opening a
            // room is itself activity that bumps its recency, keeping it
            // near the top of `all_rooms`' growing prefix, and the
            // `Timeline` built below has its own live event-cache
            // subscription independent of the room list's own sync range.
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            // `room.timeline()` uses matrix-sdk-ui's `default_event_filter`,
            // which — unlike Element's own UI behavior — does NOT exclude
            // thread replies from the main flat view (it only excludes
            // edits). So thread replies naturally show up interleaved with
            // regular messages here unless filtered out explicitly, which
            // is what the extra check below does.
            let timeline = Arc::new(
                room.timeline_builder()
                    .event_filter(|ev, room_version| {
                        use matrix_sdk::ruma::events::{
                            room::message::Relation, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
                        };

                        if !matrix_sdk_ui::timeline::default_event_filter(ev, room_version) {
                            return false;
                        }
                        if let AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomMessage(msg),
                        ) = ev
                        {
                            if let Some(orig) = msg.as_original() {
                                if matches!(orig.content.relates_to, Some(Relation::Thread(_))) {
                                    return false;
                                }
                            }
                        }
                        true
                    })
                    .build()
                    .await?,
            );
            state
                .lock()
                .await
                .room_timelines
                .insert(room_id.clone(), timeline.clone());

            // A freshly built `Timeline` starts empty — it only grows from
            // live sync updates from here on, it does not backfill already
            // synced history on its own. Paginate backwards once up front so
            // opening a room actually shows recent messages.
            //
            // In this SDK version `paginate_backwards` is "lazy": one call
            // can return quickly having fetched few or even zero *new*
            // events while still reporting `reached_start: false` — its own
            // doc comment says a subsequent call is what actually triggers
            // the event cache's real network pagination. Keep calling until
            // either the room's start is reached or enough items have
            // accumulated, capped so a slow homeserver can't hang this
            // command forever.
            for _ in 0..10 {
                if timeline.items().await.len() >= 20 {
                    break;
                }
                match timeline.paginate_backwards(20).await {
                    Ok(true) => {
                        tracing::info!(room_id, "LoadTimeline: reached room start");
                        break;
                    }
                    Ok(false) => continue,
                    Err(e) => {
                        tracing::warn!(error = %e, room_id, "initial backward pagination failed");
                        break;
                    }
                }
            }

            let items = timeline.items().await;
            tracing::info!(room_id, raw_items = items.len(), "LoadTimeline: raw item count");
            let events = convert_items(&client, &items).await;
            tracing::info!(room_id, converted_events = events.len(), "LoadTimeline: converted event count");
            tx.send(Event::Timeline { room_id, events }).ok();
        }

        Command::PaginateBack { room_id } => {
            let client = get_client(&state).await?;
            let Some(timeline) = state.lock().await.room_timelines.get(&room_id).cloned() else {
                // No open Event::TimelinePrepend to answer with, but the UI
                // only tracks "in flight" per room via that event — send an
                // empty one anyway so it doesn't get stuck thinking a
                // pagination request is still pending for this room.
                tx.send(Event::TimelinePrepend {
                    room_id,
                    events: Vec::new(),
                    reached_start: false,
                })
                .ok();
                return Ok(());
            };

            // Reported via the (empty-events, reached_start: false)
            // fallback below rather than propagated with `?`, so a
            // transient pagination error doesn't leave the UI's
            // "in flight" guard stuck forever for this room.
            //
            // Same "lazy pagination" looping as `Command::LoadTimeline` —
            // see its comment for why one call isn't enough anymore.
            let items_before = timeline.items().await.len();
            let mut reached_start = false;
            for _ in 0..10 {
                match timeline.paginate_backwards(20).await {
                    Ok(true) => {
                        reached_start = true;
                        break;
                    }
                    Ok(false) => {
                        if timeline.items().await.len() > items_before {
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, room_id, "PaginateBack failed");
                        tx.send(Event::TimelinePrepend {
                            room_id,
                            events: Vec::new(),
                            reached_start: false,
                        })
                        .ok();
                        return Ok(());
                    }
                }
            }

            let items = timeline.items().await;
            let events = convert_items(&client, &items).await;
            tx.send(Event::TimelinePrepend {
                room_id,
                events,
                reached_start,
            })
            .ok();
        }

        Command::LoadThread {
            room_id,
            thread_root_id,
        } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            let root_event_id = OwnedEventId::try_from(thread_root_id.as_str())?;

            // The previous approach (filter the room's *live* `Timeline`
            // down to this thread via `event_filter`, then
            // `paginate_backwards` until enough filtered items show up)
            // required walking arbitrarily far back through the room's
            // entire history to find a handful of thread replies, and
            // consistently surfaced 0 replies in practice. Fetch the
            // thread directly from the server instead, via the same
            // `/relations` endpoint every other Matrix client uses for
            // threads — it returns exactly this thread's events regardless
            // of how far back they are.
            //
            // Only the most recent page here — a thread with hundreds of
            // replies used to fetch every single one upfront (looping
            // `/relations` until exhausted), which was slow enough to be
            // hard to use. Older replies load on demand via
            // `Command::LoadMoreThreadReplies` instead, same shape as the
            // main timeline's `PaginateBack`.
            // The frontend already has the root (it's what was clicked to
            // open the thread, passed straight into `openThread`) and
            // renders it separately from this `events` list — an earlier
            // version fetched and prepended a fresh copy of it here too,
            // which just meant the root rendered twice: once as the
            // panel's pinned header message, once again as if it were the
            // thread's own first reply.
            let (events, next_batch) =
                fetch_thread_replies_page(&client, &room, &room_id, &root_event_id, None).await;

            tracing::debug!(
                room_id,
                thread_root_id,
                events = events.len(),
                has_more = next_batch.is_some(),
                "thread events fetched via /relations"
            );

            // Still needed for `Command::SendMessage`'s thread-reply path,
            // which calls `.send()` on whatever `Timeline` is registered
            // for this thread key — any live timeline for the room works,
            // since sending just posts through the room regardless of
            // which `Timeline` object issued it.
            let timeline = match state.lock().await.room_timelines.get(&room_id).cloned() {
                Some(t) => t,
                None => Arc::new(room.timeline().await?),
            };
            {
                let mut state = state.lock().await;
                state
                    .thread_timelines
                    .insert((room_id.clone(), thread_root_id.clone()), timeline);
                state
                    .thread_reply_cursors
                    .insert((room_id.clone(), thread_root_id.clone()), next_batch);
            }

            tx.send(Event::ThreadEvents {
                room_id,
                thread_root_id,
                events,
            })
            .ok();
        }

        Command::LoadMoreThreadReplies {
            room_id,
            thread_root_id,
        } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let root_event_id = OwnedEventId::try_from(thread_root_id.as_str())?;

            let cursor = state
                .lock()
                .await
                .thread_reply_cursors
                .get(&(room_id.clone(), thread_root_id.clone()))
                .cloned()
                .flatten();

            let Some(from) = cursor else {
                // Either never loaded a first page (shouldn't happen — the
                // UI only calls this after `LoadThread`) or already hit the
                // oldest reply. Either way, nothing more to fetch.
                tx.send(Event::ThreadEventsPrepend {
                    room_id,
                    thread_root_id,
                    events: Vec::new(),
                    reached_start: true,
                })
                .ok();
                return Ok(());
            };

            let (events, next_batch) =
                fetch_thread_replies_page(&client, &room, &room_id, &root_event_id, Some(from))
                    .await;
            let reached_start = next_batch.is_none();
            state
                .lock()
                .await
                .thread_reply_cursors
                .insert((room_id.clone(), thread_root_id.clone()), next_batch);

            tx.send(Event::ThreadEventsPrepend {
                room_id,
                thread_root_id,
                events,
                reached_start,
            })
            .ok();
        }

        Command::ListThreads { room_id } => {
            let client = get_client(&state).await?;
            match fetch_threads_page(&client, &room_id, None).await {
                Ok((threads, next_batch)) => {
                    let reached_end = next_batch.is_none();
                    state
                        .lock()
                        .await
                        .thread_list_cursors
                        .insert(room_id.clone(), next_batch);
                    tx.send(Event::ThreadsList {
                        room_id,
                        threads,
                        reached_end,
                    })
                    .ok();
                }
                Err(e) => {
                    tx.send(Event::Error(e.to_string())).ok();
                }
            }
        }

        // Continues a room's thread list from wherever `Command::ListThreads`'s
        // first page (or the previous `LoadMoreThreads` call) left off —
        // mirrors `LoadMoreThreadReplies`/`PaginateBack`.
        Command::LoadMoreThreads { room_id } => {
            let client = get_client(&state).await?;
            let cursor = state
                .lock()
                .await
                .thread_list_cursors
                .get(&room_id)
                .cloned()
                .flatten();

            let Some(from) = cursor else {
                // Either never loaded a first page (shouldn't happen — the
                // UI only calls this after `ListThreads`) or already hit
                // the room's oldest thread. Either way, nothing more to
                // fetch.
                tx.send(Event::ThreadsListAppend {
                    room_id,
                    threads: Vec::new(),
                    reached_end: true,
                })
                .ok();
                return Ok(());
            };

            match fetch_threads_page(&client, &room_id, Some(from)).await {
                Ok((threads, next_batch)) => {
                    let reached_end = next_batch.is_none();
                    state
                        .lock()
                        .await
                        .thread_list_cursors
                        .insert(room_id.clone(), next_batch);
                    tx.send(Event::ThreadsListAppend {
                        room_id,
                        threads,
                        reached_end,
                    })
                    .ok();
                }
                Err(e) => {
                    tx.send(Event::Error(e.to_string())).ok();
                }
            }
        }

        Command::ListMembers { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            let members = room
                .members(matrix_sdk::RoomMemberships::JOIN)
                .await?
                .into_iter()
                .map(|m| (m.user_id().to_string(), m.name().to_string(), m.avatar_url().map(|u| u.to_string())))
                .collect();

            tx.send(Event::Members { room_id, members }).ok();
        }

        Command::SendMessage {
            room_id,
            body,
            thread_id,
            mentions,
            html_body,
            local_id,
            reply_to_event_id,
        } => {
            use matrix_sdk::ruma::events::relation::Thread;
            use matrix_sdk::ruma::events::room::message::{Relation, RoomMessageEventContent};

            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            let mut content = match html_body {
                Some(html) => RoomMessageEventContent::text_html(body, html),
                None => RoomMessageEventContent::text_plain(body),
            };

            if !mentions.is_empty() {
                let user_ids: std::collections::BTreeSet<_> = mentions
                    .iter()
                    .filter_map(|m| matrix_sdk::ruma::OwnedUserId::try_from(m.as_str()).ok())
                    .collect();
                if !user_ids.is_empty() {
                    content = content
                        .add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(user_ids));
                }
            }

            // A message can be a thread reply, a quote-reply, both (a
            // genuine reply to a specific message within a thread — spec'd
            // as `Thread` carrying its own `in_reply_to`, not a separate
            // `Reply` relation), or neither.
            let reply_target = reply_to_event_id
                .map(|id| OwnedEventId::try_from(id.as_str()))
                .transpose()?;
            content.relates_to = match (&thread_id, reply_target) {
                (Some(thread_id), Some(reply_id)) => {
                    let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;
                    Some(Relation::Thread(Thread::reply(root_event_id, reply_id)))
                }
                (Some(thread_id), None) => {
                    let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;
                    Some(Relation::Thread(Thread::without_fallback(root_event_id)))
                }
                (None, Some(reply_id)) => {
                    Some(Relation::Reply(matrix_sdk::ruma::events::relation::Reply::with_event_id(reply_id)))
                }
                (None, None) => None,
            };

            // `room.send()` directly rather than going through a cached
            // `Timeline`'s `.send()` — that returned `()`, giving the UI no
            // way to know whether the send actually succeeded or to clear
            // a "sending…" indicator; this gives back the real event ID (or
            // an error) as soon as the HTTP round trip completes, without
            // waiting on a `/sync` echo.
            //
            // Reusing `local_id` itself as the event's transaction ID (rather
            // than letting `room.send` generate a random one) is what lets
            // `register_new_message_handler`'s live echo read it straight
            // back off `unsigned.transaction_id` once this message comes
            // down `/sync` — see `TimelineEvent::local_id`'s doc comment —
            // so `app.js` can reconcile that echo with the optimistic
            // "sending…" bubble it already rendered instead of showing the
            // message twice.
            let txn_id: matrix_sdk::ruma::OwnedTransactionId = local_id.as_str().into();
            match room.send(content).with_transaction_id(txn_id).await {
                Ok(_) => {
                    tx.send(Event::MessageSent {
                        thread_id,
                        local_id,
                    })
                    .ok();
                }
                Err(e) => {
                    tx.send(Event::MessageSendFailed {
                        thread_id,
                        local_id,
                        error: e.to_string(),
                    })
                    .ok();
                }
            }
        }

        Command::SendImage {
            room_id,
            thread_id,
            filename,
            bytes,
            mime,
            local_id,
        } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let content_type: mime::Mime = mime.parse().unwrap_or(mime::IMAGE_PNG);

            let upload = match client.media().upload(&content_type, bytes, None).await {
                Ok(u) => u,
                Err(e) => {
                    tx.send(Event::MessageSendFailed {
                        thread_id,
                        local_id,
                        error: e.to_string(),
                    })
                    .ok();
                    return Ok(());
                }
            };

            use matrix_sdk::ruma::events::room::message::{
                ImageMessageEventContent, MessageType, RoomMessageEventContent,
            };
            let mut content = RoomMessageEventContent::new(MessageType::Image(
                ImageMessageEventContent::plain(filename, upload.content_uri),
            ));

            if let Some(thread_id) = &thread_id {
                let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;
                content.relates_to =
                    Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                        matrix_sdk::ruma::events::relation::Thread::without_fallback(root_event_id),
                    ));
            }

            match room.send(content).await {
                Ok(_) => {
                    tx.send(Event::MessageSent {
                        thread_id,
                        local_id,
                    })
                    .ok();
                }
                Err(e) => {
                    tx.send(Event::MessageSendFailed {
                        thread_id,
                        local_id,
                        error: e.to_string(),
                    })
                    .ok();
                }
            }
        }

        Command::SendFile {
            room_id,
            thread_id,
            filename,
            bytes,
            mime,
            local_id,
        } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let content_type: mime::Mime = mime.parse().unwrap_or(mime::APPLICATION_OCTET_STREAM);
            let size = bytes.len();

            let upload = match client.media().upload(&content_type, bytes, None).await {
                Ok(u) => u,
                Err(e) => {
                    tx.send(Event::MessageSendFailed {
                        thread_id,
                        local_id,
                        error: e.to_string(),
                    })
                    .ok();
                    return Ok(());
                }
            };

            use matrix_sdk::ruma::events::room::message::{
                FileInfo, FileMessageEventContent, MessageType, RoomMessageEventContent,
            };
            let mut info = FileInfo::default();
            info.mimetype = Some(content_type.to_string());
            info.size = matrix_sdk::ruma::UInt::new(size as u64);
            let mut content = RoomMessageEventContent::new(MessageType::File(
                FileMessageEventContent::plain(filename, upload.content_uri).info(Box::new(info)),
            ));

            if let Some(thread_id) = &thread_id {
                let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;
                content.relates_to =
                    Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                        matrix_sdk::ruma::events::relation::Thread::without_fallback(root_event_id),
                    ));
            }

            match room.send(content).await {
                Ok(_) => {
                    tx.send(Event::MessageSent {
                        thread_id,
                        local_id,
                    })
                    .ok();
                }
                Err(e) => {
                    tx.send(Event::MessageSendFailed {
                        thread_id,
                        local_id,
                        error: e.to_string(),
                    })
                    .ok();
                }
            }
        }

        Command::Summarize { room_id, thread_root_id } => {
            let client = get_client(&state).await?;

            let events = match &thread_root_id {
                Some(thread_id) => {
                    // `thread_timelines` isn't actually thread-scoped — see
                    // the comment in `Command::LoadThread` — it's just
                    // whichever live `Timeline` the room already has, kept
                    // around for `SendMessage`'s reply routing. Summarizing
                    // needs the thread's own replies specifically, so fetch
                    // the whole thread directly via `/relations` instead,
                    // the same way `LoadThread`/`LoadMoreThreadReplies` do —
                    // independent of how much the open thread panel has
                    // paginated in so far.
                    let room = client
                        .get_room(RoomId::parse(&room_id)?.as_ref())
                        .ok_or_else(|| anyhow::anyhow!("room not found"))?;
                    let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;

                    let mut pages: Vec<Vec<crate::models::TimelineEvent>> = Vec::new();
                    let mut from = None;
                    loop {
                        let (page, next_batch) =
                            fetch_thread_replies_page(&client, &room, &room_id, &root_event_id, from)
                                .await;
                        let has_more = next_batch.is_some();
                        pages.push(page);
                        if !has_more {
                            break;
                        }
                        from = next_batch;
                    }

                    let mut events = Vec::new();
                    if let Ok(raw_root) = room.event(&root_event_id, None).await {
                        if let Ok(value) = raw_root.raw().deserialize_as::<serde_json::Value>() {
                            if let Some(root) = parse_raw_message_event(&client, &room_id, &value).await {
                                events.push(root);
                            }
                        }
                    }
                    // `pages` was filled newest-page-first (each page itself
                    // oldest-to-newest — see `fetch_thread_replies_page`), so
                    // reversing the page order puts everything in overall
                    // chronological order after the root.
                    for page in pages.into_iter().rev() {
                        events.extend(page);
                    }
                    events
                }
                None => {
                    let timeline = state
                        .lock()
                        .await
                        .room_timelines
                        .get(&room_id)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("timeline not loaded"))?;
                    let items = timeline.items().await;
                    convert_items(&client, &items).await
                }
            };

            let transcript = events
                .iter()
                .map(|e| format!("{}: {}", e.sender_name, e.body))
                .collect::<Vec<_>>()
                .join("\n");

            // Longvan's internal LLM proxy — an OpenAI-compatible
            // `/v1/chat/completions` endpoint, not Anthropic's Messages
            // API, hence the different auth header/request/response
            // shape from what this used to call directly against
            // api.anthropic.com.
            let api_key = lvx_api_key().ok_or_else(|| {
                anyhow::anyhow!(
                    "no LVX API key configured — set one in [ sec ] → LVX API key, or the LVX_API_KEY env var"
                )
            })?;

            let http = reqwest::Client::new();
            let response = http
                .post("https://llm.ai.longvan.vn/v1/chat/completions")
                .header("Authorization", format!("Bearer {api_key}"))
                .header("Content-Type", "application/json")
                .json(&serde_json::json!({
                    "model": "lvx-1-fast",
                    "max_tokens": 400,
                    "stream": false,
                    "messages": [{
                        "role": "user",
                        "content": format!(
                            "Summarize the key points and action items of this chat transcript. \
                             Respond in Vietnamese, regardless of what language the transcript itself is in. \
                             The transcript may contain markdown links in the form [text](url) — whenever a \
                             point you're summarizing corresponds to one of those, keep the real URL and \
                             reference it the same way, as a markdown link, rather than just naming it in \
                             plain text; don't invent a URL for anything that didn't have one in the \
                             transcript:\n\n{transcript}"
                        )
                    }]
                }))
                .send()
                .await?;

            // Checked explicitly rather than just parsing whatever comes
            // back as JSON and letting a missing `choices[0]` silently
            // fall through to "(no summary returned)" — an error response
            // (bad/expired key, wrong model name, rate limit, ...) is
            // still valid JSON, just shaped like `{"error": {...}}`
            // instead of a completion, which read as "the API said
            // nothing" instead of surfacing what actually went wrong.
            let status = response.status();
            let body_text = response.text().await?;
            if !status.is_success() {
                anyhow::bail!("LVX summarize request failed ({status}): {body_text}");
            }
            let body: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
                anyhow::anyhow!("LVX summarize: response wasn't valid JSON ({e}): {body_text}")
            })?;
            let text = body["choices"][0]["message"]["content"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("(no summary returned — raw response: {body_text})"));

            tx.send(Event::Summary { room_id, thread_root_id, text }).ok();
        }

        Command::DeleteMessage { room_id, event_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let target = OwnedEventId::try_from(event_id.as_str())?;

            room.redact(&target, None, None).await?;

            // Optimistic — don't wait for the sync round trip to show the
            // deletion took effect. `register_redaction_handler` will send
            // the same event again once the server echoes it back;
            // applying it twice in the UI is harmless.
            tx.send(Event::MessageDeleted { room_id, event_id }).ok();
        }

        Command::EditMessage {
            room_id,
            event_id,
            body,
            mentions,
            html_body,
        } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let target = OwnedEventId::try_from(event_id.as_str())?;

            use matrix_sdk::ruma::events::room::message::{
                ReplacementMetadata, RoomMessageEventContent,
            };

            let mut new_content = match html_body {
                Some(html) => RoomMessageEventContent::text_html(body.clone(), html),
                None => RoomMessageEventContent::text_plain(body.clone()),
            };
            if !mentions.is_empty() {
                let user_ids: std::collections::BTreeSet<_> = mentions
                    .iter()
                    .filter_map(|m| matrix_sdk::ruma::OwnedUserId::try_from(m.as_str()).ok())
                    .collect();
                if !user_ids.is_empty() {
                    new_content = new_content
                        .add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(user_ids));
                }
            }
            // `add_mentions` must run *before* `make_replacement` — ruma's
            // own doc comment on it says so explicitly: `make_replacement`
            // copies the mentions present at call time into `m.new_content`
            // and filters `content.mentions` against the *original*
            // message's mentions (via `ReplacementMetadata`) so only
            // genuinely new mentions re-trigger a notification. Called the
            // other way around (as this was until now), the edit's own
            // mentions ended up missing from the sent event — a real bug
            // on its own, independent of whatever else is wrong with
            // edits (see the `if let Err` below, added at the same time
            // to stop a failed edit from silently doing nothing visible).
            let edit = new_content.make_replacement(ReplacementMetadata::new(target, None));

            if let Err(e) = room.send(edit).await {
                tracing::warn!(error = %e, event_id, "EditMessage: room.send failed");
                tx.send(Event::Error(format!("failed to edit message: {e}"))).ok();
                return Ok(());
            }

            // Optimistic, same reasoning as `DeleteMessage`.
            tx.send(Event::MessageEdited {
                room_id,
                event_id,
                new_body: body,
            })
            .ok();
        }

        Command::AcceptInvite { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            room.join().await?;
            // No manual refresh needed: the room moving from the `invites`
            // list to `all_rooms`/`visible_rooms` server-side produces
            // diffs on both, which the listener tasks (see
            // `run_room_list_listener`) forward on their own.
        }

        Command::DeclineInvite { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            room.leave().await?;
            // Same reasoning as `AcceptInvite` — the `invites` list's own
            // diff listener picks up the removal.
        }

        Command::InviteUser { room_id, user_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let user_id = matrix_sdk::ruma::UserId::parse(&user_id)?;
            room.invite_user_by_id(&user_id).await?;
        }

        Command::LeaveRoom { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            room.leave().await?;
            {
                let mut guard = state.lock().await;
                guard.room_timelines.remove(&room_id);
                guard.thread_timelines.retain(|(rid, _), _| rid != &room_id);
            }
            // No manual room-list refresh needed: once the room actually
            // disappears from the server's sliding-sync response, the
            // `all_rooms`/`visible_rooms` diff listener (see
            // `run_room_list_listener`) picks up the resulting `Remove` on
            // its own and forwards it to the frontend. There's a small
            // delay (one sync round trip) before that happens, same as
            // Element X.
        }

        Command::MarkRoomRead { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            // The already-loaded per-room `Timeline` (populated by
            // `Command::LoadTimeline`, and what the UI itself renders from)
            // is guaranteed to be there and up to date for a room the user
            // is actively marking read, so get the latest event from it
            // directly rather than round-tripping through
            // `RoomListService::room(id).latest_event()`.
            let timeline = state.lock().await.room_timelines.get(&room_id).cloned();
            let latest_event_id = match &timeline {
                Some(timeline) => timeline.latest_event_id().await,
                None => None,
            };

            if let Some(event_id) = latest_event_id {
                // `send_single_receipt` alone only advances the public
                // `m.receipt` — other clients (Element on Android/iOS/web
                // included) primarily key their own unread-dot/bold-room
                // state off the *fully-read marker* (`m.fully_read`,
                // room account data) instead, which that call never
                // touches. Marking read here but not there meant this
                // room correctly zeroed its own unread badge in this app,
                // while every other client/session on the same account
                // kept showing it unread until they separately caught up
                // past this point on their own — not actually a Rust bug,
                // just an incomplete "mark read" that only ever touched
                // half of what Matrix clients check. `send_multiple_receipts`
                // sets both in one request.
                use matrix_sdk::room::Receipts;
                let receipts = Receipts::new()
                    .fully_read_marker(event_id.clone())
                    .public_read_receipt(event_id.clone());
                room.send_multiple_receipts(receipts).await?;

                let mut guard = state.lock().await;
                guard.confirmed_read.insert(room_id.clone());
                save_read_state(&guard.confirmed_read);
            }
            // No manual room-list refresh needed here either: the frontend
            // already zeroes this room's unread badge optimistically the
            // moment it sends this command (see `app.js`'s mark-read
            // handler) — it doesn't wait for a round trip. The server will
            // eventually echo the receipt back through the room-list diff
            // stream too, confirming the same value.
        }

        Command::MarkThreadRead { room_id, thread_root_id, event_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let root_event_id = OwnedEventId::try_from(thread_root_id.as_str())?;
            let event_id = OwnedEventId::try_from(event_id.as_str())?;

            use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
            use matrix_sdk::ruma::events::receipt::ReceiptThread;
            room.send_single_receipt(ReceiptType::Read, ReceiptThread::Thread(root_event_id), event_id)
                .await?;
        }

        Command::CreateRoom {
            name,
            is_public,
            is_space,
        } => {
            let client = get_client(&state).await?;
            use matrix_sdk::ruma::api::client::room::create_room;

            let mut request = create_room::v3::Request::new();
            request.name = Some(name);
            request.preset = Some(if is_public {
                create_room::v3::RoomPreset::PublicChat
            } else {
                create_room::v3::RoomPreset::PrivateChat
            });
            if is_space {
                request.creation_content = Some(
                    matrix_sdk::ruma::serde::Raw::new(&serde_json::json!({ "type": "m.space" }))?
                        .cast_unchecked(),
                );
            }

            client.create_room(request).await?;
            if is_space {
                // Spaces don't go through `RoomListService` at all (see
                // `refresh_spaces`) — a new one needs an explicit re-scan
                // of the dedicated spaces sliding-sync list to show up,
                // there's no diff listener that would pick it up on its
                // own the way a regular room's would.
                refresh_spaces(&state, &tx).await?;
            }
            // A regular (non-space) room needs no manual refresh — same
            // reasoning as `AcceptInvite`/`LeaveRoom`: it'll show up via
            // the `all_rooms`/`visible_rooms` diff listener once the
            // server includes it in a sliding-sync response.
        }

        Command::ListSpaceChildren { space_room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&space_room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            let room_ids = fetch_space_children(&room).await;

            tx.send(Event::SpaceChildren {
                space_room_id,
                room_ids,
            })
            .ok();
        }

        Command::AddRoomToSpace {
            space_room_id,
            room_id,
        } => {
            let client = get_client(&state).await?;
            let space = client
                .get_room(RoomId::parse(&space_room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("space not found"))?;
            let child_room_id = RoomId::parse(&room_id)?;

            use matrix_sdk::ruma::events::space::child::SpaceChildEventContent;
            let content = SpaceChildEventContent::new(vec![client
                .homeserver()
                .host_str()
                .and_then(|h| matrix_sdk::ruma::OwnedServerName::try_from(h).ok())
                .ok_or_else(|| anyhow::anyhow!("could not determine own server name"))?]);

            space
                .send_state_event_for_key(&child_room_id, content)
                .await?;

            let room_ids = fetch_space_children(&space).await;
            tx.send(Event::SpaceChildren {
                space_room_id,
                room_ids,
            })
            .ok();
        }

        Command::GetNotificationMode { room_id } => {
            let client = get_client(&state).await?;
            let parsed_room_id = RoomId::parse(&room_id)?;
            let room = client
                .get_room(&parsed_room_id)
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            let settings = client.notification_settings().await;
            let mode = settings
                .get_user_defined_room_notification_mode(&parsed_room_id)
                .await
                .unwrap_or(
                    settings
                        .get_default_room_notification_mode(
                            matrix_sdk::notification_settings::IsEncrypted::from(
                                room.encryption_state().is_encrypted(),
                            ),
                            matrix_sdk::notification_settings::IsOneToOne::from(
                                room.is_direct().await.unwrap_or(false),
                            ),
                        )
                        .await,
                );

            tx.send(Event::NotificationMode {
                room_id,
                mode: notification_mode_to_str(mode).to_string(),
            })
            .ok();
        }

        Command::SetNotificationMode { room_id, mode } => {
            let client = get_client(&state).await?;
            let parsed_room_id = RoomId::parse(&room_id)?;
            let parsed_mode = notification_mode_from_str(&mode);

            client
                .notification_settings()
                .await
                .set_room_notification_mode(&parsed_room_id, parsed_mode)
                .await?;

            tx.send(Event::NotificationMode { room_id, mode }).ok();
        }
        Command::SetRoomFavorite { room_id, favorite } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            room.set_is_favourite(favorite, None).await?;
            tx.send(Event::RoomFavoriteSet { room_id, favorite }).ok();
        }
        Command::ShowNotification { room_id, thread_id, title, body, icon_mxc, icon_png } => {
            // Only the Linux (D-Bus/xdg) backend has an async API
            // (`show_async`/`wait_for_action_async`) — Windows and macOS's
            // backends in this crate are sync-only (`show`/
            // `wait_for_response`), so those run on a blocking-pool thread
            // instead, via `spawn_blocking`, to avoid parking one of the
            // async runtime's own worker threads on `events.recv()`.
            //
            // Linux goes through `desktop_notify` instead: notify-rust opens a
            // new session-bus connection per notification and holds it until
            // clicked/closed, which under swaync's DND is never — leaking
            // connections until dbus-broker ran out of fds and crashed the
            // session. See that module's doc comment.
            #[cfg(not(target_os = "linux"))]
            let _ = (icon_mxc, icon_png);
            #[cfg(target_os = "linux")]
            {
                let image = match (icon_mxc.as_deref(), icon_png.as_deref()) {
                    (Some(mxc), _) => notification_avatar_path(&state, mxc).await,
                    (None, Some(png)) => notification_initial_avatar_path(png),
                    (None, None) => None,
                };
                let on_click = move || {
                    tx.send(Event::NotificationClicked { room_id, thread_id }).ok();
                };
                if let Err(err) = crate::desktop_notify::show(&title, &body, image.as_deref(), on_click).await {
                    tracing::warn!(error = %err, "failed to show desktop notification");
                }
            }
            #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos", target_os = "android", target_os = "ios"))))]
            {
                let handle = notify_rust::Notification::new()
                    .summary(&title)
                    .body(&body)
                    // Without a declared action, clicking the notification
                    // body on at least mako (confirmed live via
                    // `dbus-monitor`: identical `Notify` call with an empty
                    // `actions` array produced only `NotificationClosed`
                    // reason 2 "dismissed by user" on click, never
                    // `ActionInvoked`) — and likely other XDG notification
                    // servers with the same "nothing declared, nothing to
                    // invoke" behavior — has nothing to invoke, so it just
                    // dismisses instead of ever firing the `ActionInvoked`
                    // signal `wait_for_action_async` below is listening
                    // for. The label ("Open") only matters for a server
                    // that renders an actual button for it; mako (and
                    // GNOME/KDE) treat the "default" id specially as "the
                    // action invoked by clicking the body itself" and never
                    // show a button for it at all.
                    .action("default", "Open")
                    .show_async()
                    .await;
                if let Ok(handle) = handle {
                    // Waits until the notification is clicked/dismissed —
                    // this command's own spawned task (see the dispatch
                    // loop) just stays alive for that long, same as any
                    // other in-flight command; nothing else is blocked by
                    // it.
                    handle
                        .wait_for_action_async(|response| {
                            if matches!(response, notify_rust::NotificationResponse::Default) {
                                tx.send(Event::NotificationClicked { room_id, thread_id }).ok();
                            }
                        })
                        .await;
                }
            }
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                let _ = tokio::task::spawn_blocking(move || {
                    // See the unix branch's comment above on why an
                    // explicit "default" action is needed for a body click
                    // to ever produce a `NotificationResponse::Default`.
                    let handle = notify_rust::Notification::new()
                        .summary(&title)
                        .body(&body)
                        .action("default", "Open")
                        .show();
                    if let Ok(handle) = handle {
                        let _ = handle.wait_for_response(|response: &notify_rust::NotificationResponse| {
                            if matches!(response, notify_rust::NotificationResponse::Default) {
                                tx.send(Event::NotificationClicked { room_id, thread_id }).ok();
                            }
                        });
                    }
                })
                .await;
            }
            // Mobile has no in-process click callback the way notify-rust
            // does — tapping the notification instead re-launches the app
            // via a `matrixtauriclient://notification?...` deep link (see
            // `MainActivity.kt`'s `handleNotificationTap` and this file's
            // `HandleNotificationClick` handler below), carrying room_id/
            // thread_id as the notification's own `extra` data so that
            // native-side redirect has something to work with.
            #[cfg(any(target_os = "android", target_os = "ios"))]
            {
                let _ = crate::platform::show_notification(&room_id, thread_id.as_deref(), &title, &body);
            }
        }
        Command::HandleNotificationClick { room_id, thread_id } => {
            tx.send(Event::NotificationClicked { room_id, thread_id }).ok();
        }

        Command::ToggleReaction { room_id, event_id, emoji } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let owned_event_id = matrix_sdk::ruma::OwnedEventId::try_from(event_id.as_str())?;

            let my_id = client.user_id().map(|id| id.to_string());
            let existing = fetch_reaction_events(&client, &room, &owned_event_id).await;
            let mine = my_id.as_deref().and_then(|me| {
                existing
                    .iter()
                    .find(|r| r.sender == me && r.emoji == emoji)
            });

            if let Some(reaction) = mine {
                room.redact(&reaction.reaction_event_id, None, None).await?;
            } else {
                use matrix_sdk::ruma::events::reaction::ReactionEventContent;
                use matrix_sdk::ruma::events::relation::Annotation;
                let content =
                    ReactionEventContent::new(Annotation::new(owned_event_id.clone(), emoji));
                room.send(content).await?;
            }

            // Re-fetch rather than adjust the in-memory list ourselves —
            // this is the source of truth regardless of whether the
            // add/redact above has fully round-tripped through sync yet
            // (the homeserver's own relations listing reflects it
            // immediately either way, unlike the room's live event stream).
            let refreshed = fetch_reaction_events(&client, &room, &owned_event_id).await;
            tx.send(Event::Reactions {
                room_id,
                event_id,
                reactions: summarize_reactions(&refreshed, my_id.as_deref()),
            })
            .ok();
        }

        // See `Command::SetPushEndpoint`'s own doc comment for the whole
        // picture. This runs right at cold start (`lib.rs`'s `.setup()`
        // fires it unconditionally whenever Kotlin has a stored endpoint),
        // often racing `CheckSession`'s own login restore — waits up to
        // ~10s for `state.client` to show up rather than just failing once
        // and never trying again, since there's no other retry for this
        // one launch (there will be a fresh attempt next launch either
        // way, but there's no reason to waste this one on a race that
        // usually resolves within a second or two).
        Command::SetPushEndpoint { endpoint } => {
            let client = {
                let mut client = state.lock().await.client.clone();
                let mut waited = 0;
                while client.is_none() && waited < 20 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    client = state.lock().await.client.clone();
                    waited += 1;
                }
                client
            };
            let Some(client) = client else {
                tracing::warn!("set_push_endpoint: no session after waiting, skipping for this launch");
                return Ok(());
            };

            let endpoint_url = match url::Url::parse(&endpoint) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(error = %e, "set_push_endpoint: invalid endpoint URL");
                    return Ok(());
                }
            };
            let Some(host) = endpoint_url.host_str() else {
                tracing::warn!("set_push_endpoint: endpoint URL has no host");
                return Ok(());
            };
            let port_suffix = endpoint_url.port().map(|p| format!(":{p}")).unwrap_or_default();
            // The distributor's own ntfy-compatible server doubles as the
            // Matrix push gateway at this fixed path on the same host —
            // see `ForegroundSyncService.kt`'s sibling doc comment in
            // `UnifiedPushServiceImpl.kt` for the full flow this is one
            // half of. `pushkey` is the endpoint itself (unique per
            // install, carries the topic the gateway needs to forward
            // to); `data.url` is that gateway's generic notify endpoint,
            // not the topic-specific one.
            let gateway_url =
                format!("{}://{}{}/_matrix/push/v1/notify", endpoint_url.scheme(), host, port_suffix);

            use matrix_sdk::ruma::api::client::push::{set_pusher, Pusher, PusherIds, PusherInit, PusherKind};
            use matrix_sdk::ruma::push::{HttpPusherData, PushFormat};

            let mut http_data = HttpPusherData::new(gateway_url);
            // Keeps message content out of the third-party push
            // gateway/distributor entirely — the homeserver only tells it
            // "something happened in this room", and the app does its own
            // real `/sync` once woken to find out what and show it. Same
            // default Element itself has used for exactly this privacy
            // reason since it added UnifiedPush support.
            http_data.format = Some(PushFormat::EventIdOnly);

            let pusher: Pusher = PusherInit {
                ids: PusherIds::new(endpoint.clone(), "com.example.matrixtauriclient".to_string()),
                kind: PusherKind::Http(http_data),
                app_display_name: "Matrix (UnifiedPush)".to_string(),
                device_display_name: "Android".to_string(),
                profile_tag: None,
                lang: "en".to_string(),
            }
            .into();

            match client.send(set_pusher::v3::Request::post(pusher)).await {
                Ok(_) => tracing::info!("set_push_endpoint: pusher registered"),
                Err(e) => tracing::warn!(error = %e, "set_push_endpoint: pushers/set failed"),
            }
        }

        Command::ListImagePacks { room_id } => {
            let client = get_client(&state).await?;
            let images = list_image_packs(&client, &room_id).await;
            tx.send(Event::ImagePacks { room_id, images }).ok();
        }

        Command::AddImagePackEmoji {
            room_id,
            scope,
            shortcode,
            bytes,
            mime,
        } => {
            let client = get_client(&state).await?;
            let content_type: mime::Mime = mime.parse().unwrap_or(mime::IMAGE_PNG);
            let upload = client.media().upload(&content_type, bytes, None).await?;
            let url = upload.content_uri.to_string();

            mutate_pack_content(&client, &room_id, &scope, |content| {
                content["images"][&shortcode] = serde_json::json!({ "url": url });
                if content.get("pack").is_none() {
                    let default_name = if scope == "personal" { "personal" } else { "room" };
                    content["pack"] = serde_json::json!({ "display_name": default_name });
                }
            })
            .await?;

            // Refresh the picker with the newly-added emoji.
            let images = list_image_packs(&client, &room_id).await;
            tx.send(Event::ImagePacks { room_id, images }).ok();
        }

        Command::RemoveImagePackEmoji { room_id, scope, shortcode } => {
            let client = get_client(&state).await?;
            mutate_pack_content(&client, &room_id, &scope, |content| {
                if let Some(images) = content.get_mut("images").and_then(|v| v.as_object_mut()) {
                    images.remove(&shortcode);
                }
            })
            .await?;

            let images = list_image_packs(&client, &room_id).await;
            tx.send(Event::ImagePacks { room_id, images }).ok();
        }

        Command::SearchUserMessages { user_id, from_ts, to_ts } => {
            let client = get_client(&state).await?;
            let (results, truncated) = search_user_messages(&client, &user_id, from_ts, to_ts).await;
            tx.send(Event::UserMessagesSearchResult {
                user_id,
                results,
                truncated,
            })
            .ok();
        }

        Command::ListAllUsers => {
            let client = get_client(&state).await?;
            let mut by_id: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            for room in client.rooms() {
                if room.state() != matrix_sdk::RoomState::Joined || room.is_space() {
                    continue;
                }
                let Ok(members) = room.members(matrix_sdk::RoomMemberships::JOIN).await else {
                    continue;
                };
                for member in members {
                    by_id
                        .entry(member.user_id().to_string())
                        .or_insert_with(|| member.name().to_string());
                }
            }
            let mut users: Vec<(String, String)> = by_id.into_iter().collect();
            users.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
            tx.send(Event::AllUsers { users }).ok();
        }

        Command::SearchDirectoryUsers { query } => {
            let client = get_client(&state).await?;
            let mut users = Vec::new();
            if !query.trim().is_empty() {
                match client.search_users(query.trim(), 20).await {
                    Ok(response) => {
                        users = response
                            .results
                            .into_iter()
                            .map(|u| crate::models::DirectoryUser {
                                user_id: u.user_id.to_string(),
                                display_name: u.display_name,
                                avatar_url: u.avatar_url.map(|url| url.to_string()),
                            })
                            .collect();
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, query, "SearchDirectoryUsers: search failed");
                    }
                }
            }
            tx.send(Event::DirectoryUsers { query, users }).ok();
        }

        Command::ResolveSharedEvent { room_id, event_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let parsed_event_id = OwnedEventId::try_from(event_id.as_str())?;

            let (found, thread_root_id) = match room.event(&parsed_event_id, None).await {
                Ok(raw_event) => {
                    // Same raw-JSON check `convert_item` uses to keep thread
                    // replies out of the main timeline in the first place —
                    // reused here (rather than ruma's typed `Relation`,
                    // which only exists on `RoomMessageEventContent`) so
                    // this works for a shared link to a threaded sticker or
                    // any other event type too.
                    let root = raw_event
                        .raw()
                        .deserialize_as::<serde_json::Value>()
                        .ok()
                        .filter(|v| {
                            v.pointer("/content/m.relates_to/rel_type").and_then(|v| v.as_str())
                                == Some("m.thread")
                        })
                        .and_then(|v| {
                            v.pointer("/content/m.relates_to/event_id")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        });
                    (true, root)
                }
                Err(_) => (false, None),
            };
            tx.send(Event::SharedEventResolved { room_id, event_id, found, thread_root_id }).ok();
        }

        Command::SendMeme { room_id, thread_id, url, shortcode } => {
            tracing::info!(room_id, ?thread_id, url, shortcode, "SendMeme: command received");
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            if let Some(thread_id) = thread_id {
                // A real `m.sticker` has no typed relation support at all
                // in this SDK version (`StickerEventContent` doesn't
                // declare an `m.relates_to` field, so serde silently
                // drops anything spliced into that raw JSON key on
                // deserialize) — not just in the live-echo handler, but
                // in matrix-sdk-ui's own `Timeline` object too, which
                // means the main timeline's own thread-reply filter (see
                // `Command::LoadTimeline`'s `event_filter`, which only
                // ever recognizes `m.room.message` + `Relation::Thread`)
                // could never have excluded a thread-targeted sticker
                // either — it would keep showing up in the main timeline
                // no matter which of *our* handlers got fixed. Sending a
                // typed `m.room.message` image instead — the exact same
                // shape `Command::SendImage`'s own thread branch already
                // uses — routes correctly everywhere that already
                // understands `Relation::Thread` properly: this filter,
                // `register_new_message_handler`'s live echo, and the
                // `/relations` thread reload path.
                use matrix_sdk::ruma::events::relation::Thread;
                use matrix_sdk::ruma::events::room::message::{
                    ImageMessageEventContent, MessageType, Relation, RoomMessageEventContent,
                };
                let mxc_url = matrix_sdk::ruma::OwnedMxcUri::from(url);
                let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;
                let mut content = RoomMessageEventContent::new(MessageType::Image(
                    ImageMessageEventContent::plain(shortcode, mxc_url),
                ));
                content.relates_to = Some(Relation::Thread(Thread::without_fallback(root_event_id)));
                room.send(content).await?;
            } else {
                let content = serde_json::json!({
                    "body": shortcode.clone(),
                    "url": url.clone(),
                    "info": {},
                });
                let response = room.send_raw("m.sticker", content).await?;
                // `register_sticker_handler` skips our own sends
                // unconditionally (see its doc comment), so push this one
                // ourselves — same reasoning as the thread branch not
                // needing to (there, `register_new_message_handler`
                // already does this for a typed `Relation::Thread` image).
                let sender = client.user_id().map(|id| id.to_string()).unwrap_or_default();
                let own_member = match client.user_id() {
                    Some(id) => room.get_member(id).await.ok().flatten(),
                    None => None,
                };
                let sender_name = own_member
                    .as_ref()
                    .and_then(|m| m.display_name().map(|n| n.to_string()))
                    .unwrap_or_else(|| sender.clone());
                let sender_avatar_url = own_member
                    .as_ref()
                    .and_then(|m| m.avatar_url().map(|u| u.to_string()));
                let event = crate::models::TimelineEvent {
                    event_id: response.response.event_id.to_string(),
                    sender,
                    sender_name,
                    sender_avatar_url,
                    body: shortcode,
                    msg_type: "image".to_string(),
                    media_url: Some(url),
                    media_mime: None,
                    media_encryption: None,
                    thumbnail_url: None,
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                    reply_to_event_id: None,
                    reply_to_preview: None,
                    thread_count: None,
                    is_own: true,
                    mentions_me: false,
                    mentioned_user_ids: Vec::new(),
                    reactions: Vec::new(),
                    read_by: Vec::new(),
                    latest_reply_sender_name: None,
                    latest_reply_body: None,
                    latest_reply_ts: None,
                    latest_reply_event_id: None,
                    latest_reply_mentions_me: false,
                    latest_reply_msg_type: None,
                    latest_reply_media_url: None,
                    latest_reply_media_mime: None,
                    latest_reply_media_encryption: None,
                    is_unread: None,
                    local_id: None,
                };
                tx.send(Event::NewMessage { room_id, event }).ok();
            }
        }

        Command::CloseRoomTimeline { room_id } => {
            let mut guard = state.lock().await;
            guard.room_timelines.remove(&room_id);
            // Also drop any open thread timelines that belonged to this room.
            guard.thread_timelines.retain(|(rid, _), _| rid != &room_id);
        }

        Command::PlayVideo { mxc_uri, filename, media_encryption } => {
            let client = get_client(&state).await?;
            match download_media_bytes(&client, &mxc_uri, media_encryption.as_deref()).await {
                Ok(bytes) => {
                    let safe_name = sanitize_filename(&filename);
                    let path = std::env::temp_dir().join(format!(
                        "matrix-egui-{}-{}",
                        chrono_like_timestamp(),
                        safe_name
                    ));
                    if let Err(e) = std::fs::write(&path, &bytes) {
                        tx.send(Event::Error(format!("failed to save video: {e}")))
                            .ok();
                    } else if let Err(e) = crate::platform::open_path(&path) {
                        tx.send(Event::Error(format!("failed to open video player: {e}")))
                            .ok();
                    }
                }
                Err(e) => {
                    tx.send(Event::Error(format!("video download failed: {e}")))
                        .ok();
                }
            }
        }

        Command::OpenMediaExternally { mxc_uri, filename, media_encryption } => {
            let client = get_client(&state).await?;
            match download_media_bytes(&client, &mxc_uri, media_encryption.as_deref()).await {
                Ok(bytes) => {
                    let safe_name = sanitize_filename(&filename);
                    let path = std::env::temp_dir().join(format!(
                        "matrix-egui-{}-{}",
                        chrono_like_timestamp(),
                        safe_name
                    ));
                    if let Err(e) = std::fs::write(&path, &bytes) {
                        tx.send(Event::Error(format!("failed to save file: {e}")))
                            .ok();
                    } else if let Err(e) = crate::platform::open_path(&path) {
                        tx.send(Event::Error(format!("failed to open file: {e}")))
                            .ok();
                    }
                }
                Err(e) => {
                    tx.send(Event::Error(format!("download failed: {e}"))).ok();
                }
            }
        }

        Command::OpenUrl { url } => {
            if let Err(e) = crate::platform::open_url(&url) {
                tx.send(Event::Error(format!("failed to open link: {e}"))).ok();
            }
        }

        Command::DownloadMedia { mxc_uri, filename, media_encryption } => {
            let client = get_client(&state).await?;
            match download_media_bytes(&client, &mxc_uri, media_encryption.as_deref()).await {
                Ok(bytes) => {
                    let dir = crate::platform::download_dir();
                    if let Err(e) = std::fs::create_dir_all(&dir) {
                        tx.send(Event::Error(format!("failed to prepare downloads folder: {e}")))
                            .ok();
                    } else {
                        let path = unique_download_path(&dir, &sanitize_filename(&filename));
                        match std::fs::write(&path, &bytes) {
                            Ok(()) => {
                                tx.send(Event::MediaDownloaded {
                                    path: path.display().to_string(),
                                })
                                .ok();
                            }
                            Err(e) => {
                                tx.send(Event::Error(format!("failed to save download: {e}")))
                                    .ok();
                            }
                        }
                    }
                }
                Err(e) => {
                    tx.send(Event::Error(format!("download failed: {e}"))).ok();
                }
            }
        }

        Command::FetchImage { key, mxc_uri, media_encryption } => {
            let client = get_client(&state).await?;
            match download_media_bytes(&client, &mxc_uri, media_encryption.as_deref()).await {
                Ok(bytes) => {
                    // TEMP diagnostic: confirm we actually got image bytes
                    // back (vs. e.g. an HTML error page or empty body that
                    // downloaded "successfully" but can't be decoded as an
                    // image, which would render as nothing visible).
                    let magic = bytes.get(..8.min(bytes.len())).unwrap_or(&[]);
                    tracing::debug!(
                        key,
                        mxc_uri,
                        len = bytes.len(),
                        magic = ?magic,
                        "image bytes fetched"
                    );
                    tx.send(Event::ImageBytes { key, bytes }).ok();
                }
                Err(e) => {
                    tracing::warn!(key, mxc_uri, error = %e, "image fetch failed");
                    tx.send(Event::ImageFetchFailed {
                        key,
                        error: e.to_string(),
                    })
                    .ok();
                }
            }
        }

        // Verification / recovery: real implementation in
        // src/matrix/verification.rs (ported from the Tauri version).
        Command::StartSelfVerification => {
            let client = get_client(&state).await?;
            crate::matrix::verification::start_self_verification(&client, tx.clone()).await?;
        }
        Command::ConfirmVerification => {
            let client = get_client(&state).await?;
            crate::matrix::verification::confirm_verification(&client).await?;
        }
        Command::CancelVerification => {
            let client = get_client(&state).await?;
            crate::matrix::verification::cancel_verification(&client).await?;
        }
        Command::RecoverWithKey { recovery_key } => {
            let client = get_client(&state).await?;
            match crate::matrix::verification::recover_with_key(&client, &recovery_key).await {
                Ok(()) => {
                    tx.send(Event::RecoveryStatus(
                        "recovered — history should now decrypt".to_string(),
                    ))
                    .ok();
                }
                Err(e) => {
                    tx.send(Event::RecoveryStatus(format!("recovery failed: {e}")))
                        .ok();
                }
            }
        }

        Command::SetLvxApiKey { api_key } => {
            let mut config = load_config();
            config.lvx_api_key = if api_key.is_empty() { None } else { Some(api_key) };
            save_config(&config);
            tx.send(Event::LvxApiKeyStatus {
                configured: config.lvx_api_key.is_some(),
            })
            .ok();
        }

        Command::GetLvxApiKeyStatus => {
            tx.send(Event::LvxApiKeyStatus {
                configured: lvx_api_key().is_some(),
            })
            .ok();
        }
        Command::ImportRoomKeys { bytes, passphrase } => {
            let client = get_client(&state).await?;
            match crate::matrix::verification::import_room_keys(&client, &bytes, &passphrase)
                .await
            {
                Ok((imported, total)) => {
                    tx.send(Event::RoomKeysImported { imported, total }).ok();
                }
                Err(e) => {
                    tx.send(Event::Error(format!("key import failed: {e}"))).ok();
                }
            }
        }

        Command::GetOwnProfile => {
            let client = get_client(&state).await?;
            send_own_profile(&client, &tx).await;
        }

        Command::SetDisplayName { name } => {
            let client = get_client(&state).await?;
            let name = name.trim();
            let value = if name.is_empty() { None } else { Some(name) };
            match client.account().set_display_name(value).await {
                Ok(()) => send_own_profile(&client, &tx).await,
                Err(e) => {
                    tx.send(Event::Error(format!("failed to set display name: {e}")))
                        .ok();
                }
            }
        }

        Command::SetAvatar { bytes, mime } => {
            let client = get_client(&state).await?;
            let content_type: mime::Mime = mime.parse().unwrap_or(mime::IMAGE_PNG);
            match client.account().upload_avatar(&content_type, bytes).await {
                Ok(mxc_uri) => match client.account().set_avatar_url(Some(&mxc_uri)).await {
                    Ok(()) => send_own_profile(&client, &tx).await,
                    Err(e) => {
                        tx.send(Event::Error(format!("failed to set avatar: {e}")))
                            .ok();
                    }
                },
                Err(e) => {
                    tx.send(Event::Error(format!("avatar upload failed: {e}")))
                        .ok();
                }
            }
        }

        Command::SetTyping { room_id, typing } => {
            let client = get_client(&state).await?;
            if let Some(room) = client.get_room(RoomId::parse(&room_id)?.as_ref()) {
                if let Err(e) = room.typing_notice(typing).await {
                    tracing::warn!(error = %e, room_id, "typing_notice failed");
                }
            }
        }

        Command::WatchTyping { room_id } => {
            let client = get_client(&state).await?;
            let Some(room) = client.get_room(RoomId::parse(&room_id)?.as_ref()) else {
                return Ok(());
            };
            let (guard, mut rx_typing) = room.subscribe_to_typing_notifications();
            state.lock().await.typing_guard = Some((room_id.clone(), guard));
            // Clear out whatever the previously-open room last showed —
            // otherwise switching rooms would leave a stale "X is
            // typing..." banner up until the new room's next actual
            // typing change.
            tx.send(Event::TypingUsers {
                room_id: room_id.clone(),
                user_ids: Vec::new(),
            })
            .ok();
            tokio::spawn(async move {
                while let Ok(user_ids) = rx_typing.recv().await {
                    let user_ids = user_ids.into_iter().map(|id| id.to_string()).collect();
                    if tx
                        .send(Event::TypingUsers {
                            room_id: room_id.clone(),
                            user_ids,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        Command::GetRoomInfo { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            tx.send(Event::RoomInfo(compute_room_info(&client, &room).await))
                .ok();
        }

        Command::SetRoomName { room_id, name } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            match room.set_name(name).await {
                Ok(_) => {
                    tx.send(Event::RoomInfo(compute_room_info(&client, &room).await))
                        .ok();
                }
                Err(e) => {
                    tx.send(Event::Error(format!("failed to set room name: {e}")))
                        .ok();
                }
            }
        }

        Command::SetRoomTopic { room_id, topic } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            match room.set_room_topic(&topic).await {
                Ok(_) => {
                    tx.send(Event::RoomInfo(compute_room_info(&client, &room).await))
                        .ok();
                }
                Err(e) => {
                    tx.send(Event::Error(format!("failed to set room topic: {e}")))
                        .ok();
                }
            }
        }

        Command::SetRoomAvatar { room_id, bytes, mime } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let content_type: mime::Mime = mime.parse().unwrap_or(mime::IMAGE_PNG);
            match room.upload_avatar(&content_type, bytes, None).await {
                Ok(_) => {
                    tx.send(Event::RoomInfo(compute_room_info(&client, &room).await))
                        .ok();
                }
                Err(e) => {
                    tx.send(Event::Error(format!("failed to set room avatar: {e}")))
                        .ok();
                }
            }
        }

        Command::GetPinnedEvents { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let event_ids = room
                .load_pinned_events()
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            tx.send(Event::PinnedEvents { room_id, event_ids }).ok();
        }

        Command::PinMessage { room_id, event_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let parsed_event_id = OwnedEventId::try_from(event_id.as_str())?;
            if let Err(e) = room.pin_event(&parsed_event_id).await {
                tx.send(Event::Error(format!("failed to pin message: {e}"))).ok();
                return Ok(());
            }
            let event_ids = room
                .load_pinned_events()
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            tx.send(Event::PinnedEvents { room_id, event_ids }).ok();
        }

        Command::UnpinMessage { room_id, event_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let parsed_event_id = OwnedEventId::try_from(event_id.as_str())?;
            if let Err(e) = room.unpin_event(&parsed_event_id).await {
                tx.send(Event::Error(format!("failed to unpin message: {e}"))).ok();
                return Ok(());
            }
            let event_ids = room
                .load_pinned_events()
                .await
                .ok()
                .flatten()
                .unwrap_or_default()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            tx.send(Event::PinnedEvents { room_id, event_ids }).ok();
        }

        Command::SearchMessages { query, room_id, from_ts, to_ts } => {
            let client = get_client(&state).await?;
            let (results, truncated) =
                search_messages_by_content(&client, &query, room_id.as_deref(), from_ts, to_ts).await;
            tx.send(Event::MessageSearchResult { query, results, truncated }).ok();
        }

        Command::SendVoiceMessage {
            room_id,
            thread_id,
            bytes,
            mime,
            duration_ms,
            waveform,
            local_id,
        } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let content_type: mime::Mime =
                mime.parse().unwrap_or_else(|_| "audio/ogg".parse().unwrap());
            let bytes_len = bytes.len() as u64;

            let upload = match client.media().upload(&content_type, bytes, None).await {
                Ok(u) => u,
                Err(e) => {
                    tx.send(Event::MessageSendFailed {
                        thread_id,
                        local_id,
                        error: e.to_string(),
                    })
                    .ok();
                    return Ok(());
                }
            };

            use matrix_sdk::ruma::events::room::message::{
                AudioInfo, AudioMessageEventContent, MessageType, RoomMessageEventContent,
                UnstableAmplitude, UnstableAudioDetailsContentBlock, UnstableVoiceContentBlock,
            };

            let mut audio_content =
                AudioMessageEventContent::plain("voice-message.ogg".to_string(), upload.content_uri);
            let mut info = AudioInfo::new();
            info.duration = Some(std::time::Duration::from_millis(duration_ms));
            info.mimetype = Some(mime);
            info.size = matrix_sdk::ruma::UInt::try_from(bytes_len).ok();
            audio_content.info = Some(Box::new(info));
            audio_content.voice = Some(UnstableVoiceContentBlock::new());
            audio_content.audio = Some(UnstableAudioDetailsContentBlock::new(
                std::time::Duration::from_millis(duration_ms),
                waveform
                    .iter()
                    .map(|v| UnstableAmplitude::new((v.clamp(0.0, 1.0) * 1024.0) as u16))
                    .collect(),
            ));

            let mut content = RoomMessageEventContent::new(MessageType::Audio(audio_content));
            if let Some(thread_id) = &thread_id {
                let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;
                content.relates_to =
                    Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
                        matrix_sdk::ruma::events::relation::Thread::without_fallback(root_event_id),
                    ));
            }

            match room.send(content).await {
                Ok(_) => {
                    tx.send(Event::MessageSent { thread_id, local_id }).ok();
                }
                Err(e) => {
                    tx.send(Event::MessageSendFailed {
                        thread_id,
                        local_id,
                        error: e.to_string(),
                    })
                    .ok();
                }
            }
        }

        Command::ListPolls { room_id } => {
            let client = get_client(&state).await?;
            let polls = list_polls(&client, &room_id).await;
            tx.send(Event::PollsList { room_id, polls }).ok();
        }

        Command::StartPoll {
            room_id,
            thread_id,
            question,
            options,
            max_selections,
        } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;

            use matrix_sdk::ruma::events::poll::unstable_start::{
                NewUnstablePollStartEventContent, UnstablePollAnswer, UnstablePollAnswers,
                UnstablePollStartContentBlock, UnstablePollStartEventContent,
            };
            use matrix_sdk::ruma::events::room::message::RelationWithoutReplacement;
            use matrix_sdk::ruma::events::relation::Thread;
            use matrix_sdk::ruma::UInt;

            let answers: Vec<UnstablePollAnswer> = options
                .iter()
                .enumerate()
                .map(|(i, text)| UnstablePollAnswer::new(format!("option-{i}"), text.clone()))
                .collect();
            let answers = match UnstablePollAnswers::try_from(answers) {
                Ok(a) => a,
                Err(e) => {
                    tx.send(Event::Error(format!("invalid poll options: {e}"))).ok();
                    return Ok(());
                }
            };
            let mut block = UnstablePollStartContentBlock::new(question.clone(), answers);
            block.max_selections = UInt::try_from(max_selections.max(1)).unwrap_or(UInt::from(1u32));

            let mut new_content = NewUnstablePollStartEventContent::plain_text(question.clone(), block);
            if let Some(thread_id) = &thread_id {
                let root_event_id = OwnedEventId::try_from(thread_id.as_str())?;
                new_content.relates_to =
                    Some(RelationWithoutReplacement::Thread(Thread::without_fallback(root_event_id)));
            }
            let content = UnstablePollStartEventContent::New(new_content);

            match room.send(content).await {
                Ok(resp) => {
                    let poll_event_id = resp.response.event_id.to_string();
                    tx.send(Event::PollUpdated(crate::models::PollData {
                        room_id,
                        thread_id,
                        poll_event_id,
                        question,
                        options: options
                            .iter()
                            .enumerate()
                            .map(|(i, text)| crate::models::PollOptionResult {
                                id: format!("option-{i}"),
                                text: text.clone(),
                                votes: 0,
                            })
                            .collect(),
                        total_votes: 0,
                        my_vote_ids: Vec::new(),
                        ended: false,
                        max_selections,
                    }))
                    .ok();
                }
                Err(e) => {
                    tx.send(Event::Error(format!("failed to start poll: {e}"))).ok();
                }
            }
        }

        Command::VotePoll { room_id, poll_event_id, answer_ids } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let parsed_poll_id = OwnedEventId::try_from(poll_event_id.as_str())?;

            use matrix_sdk::ruma::events::poll::unstable_response::UnstablePollResponseEventContent;
            let content = UnstablePollResponseEventContent::new(answer_ids, parsed_poll_id.clone());
            if let Err(e) = room.send(content).await {
                tx.send(Event::Error(format!("failed to vote: {e}"))).ok();
                return Ok(());
            }
            if let Some(poll) = fetch_poll_data(&client, &room, &parsed_poll_id, &room_id).await {
                tx.send(Event::PollUpdated(poll)).ok();
            }
        }

        Command::EndPoll { room_id, poll_event_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let parsed_poll_id = OwnedEventId::try_from(poll_event_id.as_str())?;

            use matrix_sdk::ruma::events::poll::unstable_end::UnstablePollEndEventContent;
            let content = UnstablePollEndEventContent::new("Poll ended", parsed_poll_id.clone());
            if let Err(e) = room.send(content).await {
                tx.send(Event::Error(format!("failed to end poll: {e}"))).ok();
                return Ok(());
            }
            if let Some(poll) = fetch_poll_data(&client, &room, &parsed_poll_id, &room_id).await {
                tx.send(Event::PollUpdated(poll)).ok();
            }
        }

        Command::GetPresence { user_ids } => {
            let client = get_client(&state).await?;
            let mut tasks = Vec::new();
            for uid in user_ids {
                let Ok(parsed) = matrix_sdk::ruma::OwnedUserId::try_from(uid.as_str()) else {
                    continue;
                };
                let client = client.clone();
                let tx = tx.clone();
                tasks.push(tokio::spawn(async move {
                    use matrix_sdk::ruma::api::client::presence::get_presence;
                    let request = get_presence::v3::Request::new(parsed.clone());
                    if let Ok(resp) = client.send(request).await {
                        tx.send(Event::PresenceUpdated(crate::models::PresenceInfo {
                            user_id: parsed.to_string(),
                            presence: resp.presence.to_string(),
                            currently_active: resp.currently_active,
                            last_active_ago_ms: resp.last_active_ago.map(|d| d.as_millis() as u64),
                        }))
                        .ok();
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
        }

        Command::GetEventPreview { room_id, event_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            let parsed_event_id = OwnedEventId::try_from(event_id.as_str())?;

            let event = 'fetch: {
                let Ok(raw) = room.event(&parsed_event_id, None).await else {
                    break 'fetch None;
                };
                let Ok(value) = raw.raw().deserialize_as::<serde_json::Value>() else {
                    break 'fetch None;
                };
                let value = decrypt_if_needed(&room, raw.raw().cast_ref_unchecked(), value).await;
                parse_raw_message_event(&client, &room_id, &value).await
            };
            tx.send(Event::EventPreview { room_id, event_id, event }).ok();
        }
    }

    Ok(())
}

/// Reads a room's `m.room.name`/`m.room.topic`/`m.room.avatar` plus whether
/// the logged-in user can actually change each — shared by
/// `Command::GetRoomInfo` and every `SetRoom*` command's success path (so
/// the settings panel always reflects what the server actually has, same
/// "re-read rather than assume the write went through" idea as
/// `OwnProfile`).
async fn compute_room_info(client: &Client, room: &matrix_sdk::Room) -> crate::models::RoomInfo {
    use matrix_sdk::ruma::events::StateEventType;

    let (can_set_name, can_set_topic, can_set_avatar) = match client.user_id() {
        Some(uid) => match room.get_member(uid).await {
            Ok(Some(member)) => (
                member.can_send_state(StateEventType::RoomName),
                member.can_send_state(StateEventType::RoomTopic),
                member.can_send_state(StateEventType::RoomAvatar),
            ),
            _ => (false, false, false),
        },
        None => (false, false, false),
    };

    crate::models::RoomInfo {
        room_id: room.room_id().to_string(),
        name: room.name(),
        topic: room.topic(),
        avatar_url: room.avatar_url().map(|u| u.to_string()),
        can_set_name,
        can_set_topic,
        can_set_avatar,
    }
}

/// Reads the logged-in user's own display name + avatar straight from the
/// homeserver (not just the local cache — `get_display_name`/`get_avatar_url`
/// both call `Account::fetch_user_profile` under the hood) and reports it as
/// `Event::OwnProfile`. Shared by `Command::GetOwnProfile` and by
/// `SetDisplayName`/`SetAvatar` on success, so the profile panel always
/// shows what the server actually has rather than the UI's own guess at
/// what its write did.
async fn send_own_profile(client: &Client, tx: &UnboundedSender<Event>) {
    let user_id = match client.user_id() {
        Some(id) => id.to_string(),
        None => return,
    };
    let display_name = client
        .account()
        .get_display_name()
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| user_id.clone());
    let avatar_url = client
        .account()
        .get_avatar_url()
        .await
        .ok()
        .flatten()
        .map(|u| u.to_string());
    tx.send(Event::OwnProfile(crate::models::OwnProfile {
        user_id,
        display_name,
        avatar_url,
    }))
    .ok();
}

async fn get_client(state: &Arc<Mutex<WorkerState>>) -> anyhow::Result<Client> {
    state
        .lock()
        .await
        .client
        .clone()
        .ok_or_else(|| anyhow::anyhow!("not logged in"))
}

/// The room list's avatar for a room, with the same "no room avatar set"
/// fallback `display_name()` already applies to the *name*: for a 1:1 DM
/// with no explicit `m.room.avatar`, use the other member's profile
/// avatar (the SDK's computed `heroes()`) instead of leaving it blank.
/// Without this, a DM whose name correctly falls back to the other
/// member's display name still shows a blank/initial avatar in the room
/// list while the timeline (which reads the sender's profile directly)
/// shows their real photo — the two disagreeing on the same person.
fn resolve_room_avatar_url(room: &matrix_sdk::Room) -> Option<String> {
    if let Some(url) = room.avatar_url() {
        return Some(url.to_string());
    }
    if room.direct_targets_length() != 1 {
        return None;
    }
    room.heroes().into_iter().find_map(|hero| hero.avatar_url.map(|u| u.to_string()))
}

/// Converts one `room_list_service::RoomListItem` (a real, resolved room —
/// MSC4186/this SDK version has no "not-yet-synced placeholder" concept the
/// way the older MSC3575 draft's `RoomListEntry::Empty` did) into a
/// `RoomSummary`.
///
/// Deliberately does *not* reuse a cross-call cache the way the old
/// `refresh_rooms` (removed by this migration) did for `display_name()`/
/// `is_encrypted()` — this is now only ever called for rooms that actually
/// appear in a diff batch (i.e. currently in the synced growing-list
/// prefix), not for the whole account on every tick, so recomputing those
/// two real `.await`-ed lookups fresh every time is cheap.
async fn entry_to_summary(
    room: &matrix_sdk_ui::room_list_service::RoomListItem,
    is_invite: bool,
    state: &Arc<Mutex<WorkerState>>,
) -> RoomSummary {
    let room_id_str = room.room_id().to_string();

    // This room just showed up in a diff, i.e. sliding sync just sent
    // something fresh for it — drop any stale `confirmed_read` override
    // for it now (see that field's doc comment): its
    // `unread_notification_counts()` below is fresh and can be trusted.
    {
        let mut guard = state.lock().await;
        if guard.confirmed_read.remove(&room_id_str) {
            save_read_state(&guard.confirmed_read);
        }
    }

    let name = room
        .display_name()
        .await
        .map(|n| n.to_string())
        .unwrap_or_else(|_| room_id_str.clone());
    let avatar_url = resolve_room_avatar_url(room);
    let is_encrypted = room.encryption_state().is_encrypted();

    // Prefer an already-open `Timeline` (see `Command::LoadTimeline`) for
    // the preview text — it's what the UI itself renders from when the
    // room is open, so it's never stale; walk its items from the end to
    // find the last actual event (skipping virtual items like date
    // dividers), converting the same way the main timeline view does
    // (`convert_item`) so every `TimelineItemContent` shape is handled
    // consistently in one place. Otherwise fall back to
    // `Room::latest_event()` (`matrix_sdk_base`'s own cache) — reliably
    // populated here since every sliding-sync list sets a
    // `timeline_limit`, so the server always sends at least the most
    // recent event for any room actually synced.
    let room_timelines = state.lock().await.room_timelines.clone();
    let (last_message, last_message_ts) = if let Some(timeline) = room_timelines.get(&room_id_str)
    {
        let client = room.client();
        let items = timeline.items().await;
        let mut result = (None, 0);
        for item in items.iter().rev() {
            if let Some(converted) = crate::matrix::convert::convert_item(&client, item).await {
                result = (Some(converted.body), converted.timestamp);
                break;
            }
        }
        result
    } else {
        // `RoomExt::latest_event()` (same trait `.timeline()` comes from)
        // gives back matrix-sdk-ui's own richer `LatestEventValue`, with a
        // typed `TimelineItemContent` already resolved — same shape
        // `convert_item` reads from a `Timeline`'s items, so extract the
        // body the same way rather than re-deriving it from raw JSON.
        match room.latest_event().await {
            matrix_sdk_ui::timeline::LatestEventValue::Remote { timestamp, content, .. } => {
                let body = match &content {
                    matrix_sdk_ui::timeline::TimelineItemContent::MsgLike(msg_like) => {
                        match &msg_like.kind {
                            matrix_sdk_ui::timeline::MsgLikeKind::Message(m) => {
                                Some(m.msgtype().body().to_string())
                            }
                            matrix_sdk_ui::timeline::MsgLikeKind::Redacted => {
                                Some("[message removed]".to_string())
                            }
                            matrix_sdk_ui::timeline::MsgLikeKind::Sticker(s) => {
                                Some(s.content().body.clone())
                            }
                            _ => None,
                        }
                    }
                    _ => None,
                };
                (body, timestamp.get().into())
            }
            _ => (None, 0),
        }
    };

    // NOT `unread_notification_counts()` (the server-computed one) — its
    // own doc comment warns it "might be incorrect for encrypted rooms,
    // since the server doesn't know which events are relevant standalone
    // messages ... nor can it inspect mentions", and in practice on this
    // (almost entirely E2EE) account it came back 0 for essentially every
    // room regardless of how many messages Element showed as unread.
    // `num_unread_notifications()` is matrix-sdk's own client-side count
    // (derived from read receipts vs. actual decrypted events), which is
    // what Element itself falls back to for the same reason.
    let unread_count = room.num_unread_notifications();
    // Same client-side derivation, scoped to just the messages that
    // actually ping this account — see `RoomSummary::mention_count`'s doc
    // comment.
    let mention_count = room.num_unread_mentions();

    RoomSummary {
        room_id: room_id_str,
        name,
        avatar_url,
        last_message,
        last_message_ts,
        unread_count,
        mention_count,
        is_encrypted,
        is_invite,
        is_favorite: room.is_favourite(),
        is_space: false,
        is_loading: false,
    }
}

/// Consumes one filtered/paginated view of `RoomListService::all_rooms()`
/// (see `Command::StartSync`, which builds one of these per
/// `RoomListKind`) and forwards every diff batch to the frontend as
/// `Event::RoomListUpdate`, mapping `eyeball_im::VectorDiff<Room>` 1:1 onto
/// `RoomListOp` (converting each room via `entry_to_summary` along the
/// way). Runs for as long as the stream keeps producing — i.e. the
/// lifetime of the sync session — or until the frontend event channel
/// closes.
async fn run_room_list_listener(
    stream: impl futures_util::Stream<Item = Vec<VectorDiff<matrix_sdk_ui::room_list_service::RoomListItem>>>,
    kind: RoomListKind,
    is_invite: bool,
    state: Arc<Mutex<WorkerState>>,
    tx: UnboundedSender<Event>,
) {
    let mut stream = std::pin::pin!(stream);
    tracing::info!(?kind, "run_room_list_listener: stream started");

    while let Some(diffs) = stream.next().await {
        let summary: Vec<String> = diffs
            .iter()
            .map(|d| match d {
                VectorDiff::Append { values } => format!("Append({})", values.len()),
                VectorDiff::Clear => "Clear".to_string(),
                VectorDiff::PushFront { .. } => "PushFront".to_string(),
                VectorDiff::PushBack { .. } => "PushBack".to_string(),
                VectorDiff::PopFront => "PopFront".to_string(),
                VectorDiff::PopBack => "PopBack".to_string(),
                VectorDiff::Insert { index, .. } => format!("Insert({index})"),
                VectorDiff::Set { index, .. } => format!("Set({index})"),
                VectorDiff::Remove { index } => format!("Remove({index})"),
                VectorDiff::Truncate { length } => format!("Truncate({length})"),
                VectorDiff::Reset { values } => format!("Reset({})", values.len()),
            })
            .collect();
        tracing::info!(?kind, ?summary, "run_room_list_listener: diff batch received");
        let mut ops = Vec::with_capacity(diffs.len());
        for diff in diffs {
            let op = match diff {
                VectorDiff::Append { values } => {
                    let mut out = Vec::with_capacity(values.len());
                    for room in values.iter() {
                        out.push(entry_to_summary(room, is_invite, &state).await);
                    }
                    RoomListOp::Append { values: out }
                }
                VectorDiff::Clear => RoomListOp::Clear,
                VectorDiff::PushFront { value } => RoomListOp::PushFront {
                    value: entry_to_summary(&value, is_invite, &state).await,
                },
                VectorDiff::PushBack { value } => RoomListOp::PushBack {
                    value: entry_to_summary(&value, is_invite, &state).await,
                },
                VectorDiff::PopFront => RoomListOp::PopFront,
                VectorDiff::PopBack => RoomListOp::PopBack,
                VectorDiff::Insert { index, value } => RoomListOp::Insert {
                    index: index as u32,
                    value: entry_to_summary(&value, is_invite, &state).await,
                },
                VectorDiff::Set { index, value } => {
                    let summary = entry_to_summary(&value, is_invite, &state).await;
                    tracing::info!(
                        ?kind,
                        index,
                        room_id = %summary.room_id,
                        unread_count = summary.unread_count,
                        "run_room_list_listener: Set diff computed"
                    );
                    RoomListOp::Set {
                        index: index as u32,
                        value: summary,
                    }
                }
                VectorDiff::Remove { index } => RoomListOp::Remove { index: index as u32 },
                VectorDiff::Truncate { length } => RoomListOp::Truncate {
                    length: length as u32,
                },
                VectorDiff::Reset { values } => {
                    let mut out = Vec::with_capacity(values.len());
                    for room in values.iter() {
                        out.push(entry_to_summary(room, is_invite, &state).await);
                    }
                    RoomListOp::Reset { values: out }
                }
            };
            ops.push(op);
        }
        if tx.send(Event::RoomListUpdate { list: kind, ops }).is_err() {
            break;
        }
    }
}

/// Drives the small, unfiltered, always-growing sliding-sync session whose
/// only job is to get Spaces into the local store (see `refresh_spaces`'s
/// doc comment for why that needs its own session at all under MSC4186).
/// Calls `refresh_spaces` after the initial sync and again after every
/// subsequent update — cheap, since `refresh_spaces` itself only iterates
/// whatever's already in the local store, filtered to `is_space()`.
/// Invites depend on the invited room's *server-side sort position* (by
/// recency, mixed in with every joined room) ever falling within the main
/// room list's currently-synced "growing" range — `entries_with_dynamic_adapters`
/// (see the `Invites` listener in `Command::StartSync`) only filters/shows
/// whatever `client.rooms_stream()` already knows about locally, and a room
/// only enters that local store once *some* sliding-sync list has actually
/// fetched it from the server. On a large (3000+ room) account, an invite
/// to an old/quiet room can rank far below where the growing frontier has
/// reached, so it silently never shows up in the invites tab at all.
///
/// The obvious fix — a dedicated sliding-sync list using the server-side
/// `is_invite` filter (MSC4186 / ruma's `ListFilters::is_invite`), so
/// ranking wouldn't matter at all — turned out not to work: tested live
/// against a room confirmed still pending in Element, this homeserver's
/// `is_invite` filter came back with zero rooms regardless (same rough
/// shape as its other confirmed MSC4186 bug, the 500 on the rel_type-scoped
/// `/relations` route — see `fetch_thread_replies_page`). So instead this
/// uses `SlidingSyncMode::Growing` with *no* filter and no cap — its
/// request generator (see `sliding_sync/list/request_generator.rs`)
/// auto-widens its requested range by `batch_size` on every subsequent
/// tick of the loop below on its own, with no manual driving needed (unlike
/// `RoomListDynamicEntriesController::add_one_page()`, which only advances
/// a client-side *display* cap over data some list already fetched, not
/// the server-side fetch itself) — so this session eventually fetches
/// every room on the account into the local store, guaranteeing the
/// invite shows up there eventually regardless of its rank. No `Event`
/// needs sending from here: the existing `Invites`
/// `entries_with_dynamic_adapters` listener already watches
/// `client.rooms_stream()` and picks up each newly-known invited room on
/// its own the moment this session fetches it.
async fn run_invites_catchall_sync(client: Client) -> anyhow::Result<()> {
    use matrix_sdk::sliding_sync::{SlidingSyncList, SlidingSyncMode};

    let list = SlidingSyncList::builder("invites")
        .sync_mode(SlidingSyncMode::new_growing(200))
        .timeline_limit(0u32);

    let sliding_sync = client
        .sliding_sync("invites-catchall")?
        .add_list(list)
        .build()
        .await?;

    let stream = sliding_sync.sync();
    let mut stream = std::pin::pin!(stream);
    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            tracing::warn!(error = %e, "invites catch-all sliding sync tick failed");
        }
    }
    tracing::warn!("invites catch-all sliding sync loop ended");
    Ok(())
}

async fn run_spaces_catchall_sync(
    client: Client,
    state: Arc<Mutex<WorkerState>>,
    tx: UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let sliding_sync = client.sliding_sync("spaces-catchall")?.build().await?;

    let stream = sliding_sync.sync();
    let mut stream = std::pin::pin!(stream);
    while let Some(result) = stream.next().await {
        if let Err(e) = result {
            tracing::warn!(error = %e, "spaces catch-all sliding sync tick failed");
            continue;
        }
        if let Err(e) = refresh_spaces(&state, &tx).await {
            tracing::warn!(error = %e, "refresh_spaces failed");
        }
    }
    tracing::warn!("spaces catch-all sliding sync loop ended");
    Ok(())
}

/// Rebuilds the Space list from `client.rooms()`, filtered to `is_space()`
/// — the same approach the very first version of this code used for
/// *every* room. Spaces are excluded from `RoomListService`'s own list
/// server-side (MSC4186 only supports *excluding* room types from a list
/// filter, not including only specific ones, so there's no way to ask for
/// "just the spaces" instead), which is why `run_spaces_catchall_sync`'s
/// separate, unfiltered sliding-sync session exists — its only job is to
/// make sure spaces actually end up in the local store this reads from.
/// Sends a full replacement snapshot, not a diff — spaces are always few
/// on any account, so re-reading the whole small set fresh (including
/// `compute_display_name()`/`is_encrypted()` for each) on every call is
/// not a performance concern the way it was for the 3000+-room main list
/// this migration is about.
async fn refresh_spaces(state: &Arc<Mutex<WorkerState>>, tx: &UnboundedSender<Event>) -> anyhow::Result<()> {
    let client = get_client(state).await?;
    let mut spaces = Vec::new();

    for room in client.rooms() {
        if !room.is_space() {
            continue;
        }
        let is_invite = match room.state() {
            matrix_sdk::RoomState::Joined => false,
            matrix_sdk::RoomState::Invited => true,
            _ => continue, // left/banned: skip
        };
        let name = room
            .display_name()
            .await
            .map(|n| n.to_string())
            .unwrap_or_else(|_| room.room_id().to_string());
        let avatar_url = room.avatar_url().map(|u| u.to_string());
        let is_encrypted = room.encryption_state().is_encrypted();

        spaces.push(RoomSummary {
            room_id: room.room_id().to_string(),
            name,
            avatar_url,
            last_message: None,
            last_message_ts: 0,
            unread_count: 0,
            mention_count: 0,
            is_encrypted,
            is_invite,
            is_favorite: room.is_favourite(),
            is_space: true,
            is_loading: false,
        });
    }

    spaces.sort_by(|a, b| a.name.cmp(&b.name));
    tx.send(Event::Spaces(spaces)).ok();
    Ok(())
}

/// Reads a room's `m.space.child` state events — the room IDs it groups as
/// a Space. An empty `via` list means the link was removed (that's how
/// removal works per spec — the state event is emptied, not deleted), so
/// those are skipped.
async fn fetch_space_children(room: &matrix_sdk::Room) -> Vec<String> {
    use matrix_sdk::ruma::events::StateEventType;

    let raw_events = room
        .get_state_events(StateEventType::SpaceChild)
        .await
        .unwrap_or_default();

    let mut room_ids = Vec::new();
    for raw in &raw_events {
        let Some(value) = raw_state_event_json(raw) else {
            continue;
        };
        let Some(state_key) = value.get("state_key").and_then(|v| v.as_str()) else {
            continue;
        };
        let has_via = value
            .pointer("/content/via")
            .and_then(|v| v.as_array())
            .is_some_and(|via| !via.is_empty());
        if has_via {
            room_ids.push(state_key.to_string());
        }
    }
    room_ids
}

fn raw_state_event_json(
    raw: &matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState,
) -> Option<serde_json::Value> {
    use matrix_sdk::deserialized_responses::RawAnySyncOrStrippedState as R;
    match raw {
        R::Sync(r) => r.deserialize_as().ok(),
        R::Stripped(r) => r.deserialize_as().ok(),
    }
}

fn notification_mode_to_str(
    mode: matrix_sdk::notification_settings::RoomNotificationMode,
) -> &'static str {
    use matrix_sdk::notification_settings::RoomNotificationMode;
    match mode {
        RoomNotificationMode::AllMessages => "all",
        RoomNotificationMode::MentionsAndKeywordsOnly => "mentions",
        RoomNotificationMode::Mute => "mute",
    }
}

fn notification_mode_from_str(
    mode: &str,
) -> matrix_sdk::notification_settings::RoomNotificationMode {
    use matrix_sdk::notification_settings::RoomNotificationMode;
    match mode {
        "mentions" => RoomNotificationMode::MentionsAndKeywordsOnly,
        "mute" => RoomNotificationMode::Mute,
        _ => RoomNotificationMode::AllMessages,
    }
}

/// Fetches every thread in the room directly from the homeserver via
/// `GET /_matrix/client/v1/rooms/{roomId}/threads`, paginating until
/// `next_batch` is empty — ported from the Tauri version's
/// commands/threads.rs. Finds threads regardless of how far back the user
/// has scrolled locally.
/// Fetches one page of a room's thread list via `/threads` (most recent
/// activity first, per the server's own ordering), returning that page
/// plus a cursor for the next (older) one. Used to be a loop that fetched
/// *every* page up front — fine for a room with a handful of threads, but
/// a real account can have hundreds, and paying for the full history just
/// to open the thread list once made it feel like the room had frozen.
/// Now it's the same one-page-at-a-time shape as
/// `fetch_thread_replies_page` below — the UI asks for more explicitly
/// via `Command::LoadMoreThreads`.
async fn fetch_threads_page(
    client: &Client,
    room_id: &str,
    from: Option<String>,
) -> anyhow::Result<(Vec<crate::models::TimelineEvent>, Option<String>)> {
    use matrix_sdk::ruma::api::client::threads::get_threads;
    use matrix_sdk::ruma::UInt;

    let parsed_room_id = RoomId::parse(room_id)?;
    let mut request = get_threads::v1::Request::new(parsed_room_id.clone());
    request.from = from;
    request.limit = Some(UInt::from(50u32));
    request.include = get_threads::v1::IncludeThreads::All;

    let response = client.send(request).await?;

    let room = client.get_room(&parsed_room_id);
    let mut page = Vec::new();
    for raw in &response.chunk {
        if let Some(event) = parse_thread_root(client, room.as_ref(), room_id, raw).await {
            page.push(event);
        }
    }
    // Only sorts within this one page — the server already hands back
    // pages in recency order, so this just normalizes ties, not
    // re-establishing order across pages the way the old full-history sort
    // effectively did.
    page.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let next_batch = response.next_batch.filter(|n| !n.is_empty());
    Ok((page, next_batch))
}

/// Scans every joined (non-space) room's history via `/messages` for
/// events sent by `user_id`, most recent first — backs
/// `Command::SearchUserMessages`. Uses `room.messages()` rather than a
/// `matrix-sdk-ui` `Timeline` (like the main view does) since that decrypts
/// automatically and needs no live `Timeline` object per room, which would
/// be wasteful to spin up just for a search across possibly hundreds of
/// rooms. The server-side `senders` filter narrows the request itself
/// where the homeserver honors it; the sender is also checked again
/// client-side since that's not guaranteed by the spec.
///
/// Deliberately not lazy/paginated from the UI's side — an earlier version
/// only scanned a handful of rooms per call with a "load more" button, but
/// that made it easy to miss a sender's older messages in a room you never
/// got around to clicking "load more" into. This scans every room's full
/// history in one call instead (bounded only by `from_ts`/`to_ts`, if
/// given, or by `MAX_PAGES_PER_ROOM` as a last-resort safety net against
/// one very active room turning an unbounded search into an effectively
/// endless scan) — slower up front, but nothing is silently left out.
/// `from_ts`/`to_ts` are `origin_server_ts` millisecond bounds (inclusive);
/// either or both may be absent for an open-ended search.
async fn search_user_messages(
    client: &Client,
    user_id: &str,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
) -> (Vec<crate::models::UserSearchHit>, bool) {
    use matrix_sdk::room::MessagesOptions;
    use matrix_sdk::ruma::api::client::filter::RoomEventFilter;
    use matrix_sdk::ruma::UInt;

    const PAGE_SIZE: u32 = 100;
    const MAX_PAGES_PER_ROOM: usize = 40;

    let Ok(target) = matrix_sdk::ruma::OwnedUserId::try_from(user_id) else {
        return (Vec::new(), false);
    };

    let mut results = Vec::new();
    let mut truncated = false;

    for room in client.rooms() {
        if room.state() != matrix_sdk::RoomState::Joined || room.is_space() {
            continue;
        }
        let room_id = room.room_id().to_string();
        let mut room_name: Option<String> = None;
        let mut from = None;

        'paging: for page_idx in 0..MAX_PAGES_PER_ROOM {
            let mut filter = RoomEventFilter::default();
            filter.senders = Some(vec![target.clone()]);
            filter.types = Some(vec!["m.room.message".to_string(), "m.sticker".to_string()]);

            let mut options = MessagesOptions::backward();
            options.limit = UInt::from(PAGE_SIZE);
            options.filter = filter;
            options.from = from.clone();

            let response = match room.messages(options).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, room_id, "SearchUserMessages: /messages failed");
                    break;
                }
            };

            for raw in &response.chunk {
                let Ok(value) = raw.raw().deserialize_as::<serde_json::Value>() else {
                    continue;
                };
                // Checked before the sender match below (and not skipped
                // along with a non-matching sender) since `/messages`
                // returns events in strict time order regardless of
                // sender — once *any* event in the page is older than
                // `from_ts`, everything after it (this page and every
                // later one) is guaranteed older too, so this is safe to
                // treat as "this room is done" rather than just "this
                // event doesn't count".
                let ts = value.get("origin_server_ts").and_then(|v| v.as_i64());
                if let (Some(from_ts), Some(ts)) = (from_ts, ts) {
                    if ts < from_ts {
                        break 'paging;
                    }
                }
                if value.get("sender").and_then(|v| v.as_str()) != Some(target.as_str()) {
                    continue;
                }
                if let (Some(to_ts), Some(ts)) = (to_ts, ts) {
                    if ts > to_ts {
                        continue;
                    }
                }
                if let Some(event) = parse_raw_message_event(client, &room_id, &value).await {
                    if room_name.is_none() {
                        room_name = Some(
                            room.display_name()
                                .await
                                .map(|n| n.to_string())
                                .unwrap_or_else(|_| room_id.clone()),
                        );
                    }
                    results.push(crate::models::UserSearchHit {
                        room_id: room_id.clone(),
                        room_name: room_name.clone().unwrap(),
                        event,
                    });
                }
            }

            from = response.end;
            if from.is_none() {
                break;
            }
            if page_idx + 1 == MAX_PAGES_PER_ROOM {
                truncated = true;
            }
        }
    }

    results.sort_by(|a, b| b.event.timestamp.cmp(&a.event.timestamp));
    (results, truncated)
}

/// `Command::SearchMessages`. Same "scan every room's full history, one
/// call, bounded only by `from_ts`/`to_ts`/`MAX_PAGES_PER_ROOM`" approach as
/// `search_user_messages` above (see its doc comment) — just matched by a
/// plain case-insensitive substring of the message body instead of a
/// sender, and (unlike that function) actually decrypts each page first,
/// since without that an encrypted room's messages never have a plaintext
/// `content.body` to match against at all.
async fn search_messages_by_content(
    client: &Client,
    query: &str,
    room_id_filter: Option<&str>,
    from_ts: Option<i64>,
    to_ts: Option<i64>,
) -> (Vec<crate::models::SearchHit>, bool) {
    use matrix_sdk::room::MessagesOptions;
    use matrix_sdk::ruma::UInt;

    const PAGE_SIZE: u32 = 100;
    const MAX_PAGES_PER_ROOM: usize = 40;

    let query_lower = query.trim().to_lowercase();
    if query_lower.is_empty() {
        return (Vec::new(), false);
    }

    let rooms: Vec<matrix_sdk::Room> = match room_id_filter {
        Some(id) => RoomId::parse(id)
            .ok()
            .and_then(|id| client.get_room(&id))
            .into_iter()
            .collect(),
        None => client.rooms(),
    };

    let mut results = Vec::new();
    let mut truncated = false;

    for room in rooms {
        if room.state() != matrix_sdk::RoomState::Joined || room.is_space() {
            continue;
        }
        let room_id = room.room_id().to_string();
        let mut room_name: Option<String> = None;
        let mut from = None;

        'paging: for page_idx in 0..MAX_PAGES_PER_ROOM {
            let mut options = MessagesOptions::backward();
            options.limit = UInt::from(PAGE_SIZE);
            options.from = from.clone();

            let response = match room.messages(options).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(error = %e, room_id, "SearchMessages: /messages failed");
                    break;
                }
            };

            for raw in &response.chunk {
                let Ok(value) = raw.raw().deserialize_as::<serde_json::Value>() else {
                    continue;
                };
                let value = decrypt_if_needed(&room, raw.raw().cast_ref_unchecked(), value).await;

                let ts = value.get("origin_server_ts").and_then(|v| v.as_i64());
                if let (Some(from_ts), Some(ts)) = (from_ts, ts) {
                    if ts < from_ts {
                        break 'paging;
                    }
                }
                if value.get("type").and_then(|v| v.as_str()) != Some("m.room.message") {
                    continue;
                }
                if let (Some(to_ts), Some(ts)) = (to_ts, ts) {
                    if ts > to_ts {
                        continue;
                    }
                }
                let body_matches = value
                    .pointer("/content/body")
                    .and_then(|v| v.as_str())
                    .is_some_and(|b| b.to_lowercase().contains(&query_lower));
                if !body_matches {
                    continue;
                }
                if let Some(event) = parse_raw_message_event(client, &room_id, &value).await {
                    if room_name.is_none() {
                        room_name = Some(
                            room.display_name()
                                .await
                                .map(|n| n.to_string())
                                .unwrap_or_else(|_| room_id.clone()),
                        );
                    }
                    results.push(crate::models::SearchHit {
                        room_id: room_id.clone(),
                        room_name: room_name.clone().unwrap(),
                        event,
                    });
                }
            }

            from = response.end;
            if from.is_none() {
                break;
            }
            if page_idx + 1 == MAX_PAGES_PER_ROOM {
                truncated = true;
            }
        }
    }

    results.sort_by(|a, b| b.event.timestamp.cmp(&a.event.timestamp));
    (results, truncated)
}

/// Scans a room's most recent history (bounded — see `Command::ListPolls`'s
/// doc comment) for `m.poll.start` events and compiles each one's current
/// tally via `fetch_poll_data`.
async fn list_polls(client: &Client, room_id: &str) -> Vec<crate::models::PollData> {
    use matrix_sdk::room::MessagesOptions;
    use matrix_sdk::ruma::UInt;

    const MAX_PAGES: usize = 10;
    const PAGE_SIZE: u32 = 50;

    let Some(room) = RoomId::parse(room_id).ok().and_then(|id| client.get_room(&id)) else {
        return Vec::new();
    };

    let mut poll_event_ids = Vec::new();
    let mut from = None;
    for _ in 0..MAX_PAGES {
        let mut options = MessagesOptions::backward();
        options.limit = UInt::from(PAGE_SIZE);
        options.from = from.clone();
        let response = match room.messages(options).await {
            Ok(r) => r,
            Err(_) => break,
        };
        for raw in &response.chunk {
            let Ok(value) = raw.raw().deserialize_as::<serde_json::Value>() else {
                continue;
            };
            if value.get("type").and_then(|v| v.as_str()) == Some("org.matrix.msc3381.poll.start") {
                if let Some(event_id) = value.get("event_id").and_then(|v| v.as_str()) {
                    if let Ok(id) = OwnedEventId::try_from(event_id) {
                        poll_event_ids.push(id);
                    }
                }
            }
        }
        from = response.end;
        if from.is_none() {
            break;
        }
    }

    let mut polls = Vec::new();
    for id in poll_event_ids {
        if let Some(poll) = fetch_poll_data(client, &room, &id, room_id).await {
            polls.push(poll);
        }
    }
    polls
}

/// Fetches a poll's start event plus every `m.reference` relation on it
/// (responses and, if present, the end event) and compiles the current
/// tally via ruma's own `compile_unstable_poll_results` — same "always
/// re-derive from every event on the poll, never incrementally patch"
/// approach `fetch_reaction_events`/`Event::Reactions` uses, so a missed or
/// duplicate live vote can never leave the tally wrong. Returns `None` if
/// the start event itself can't be found/parsed (deleted, or not actually
/// a poll).
async fn fetch_poll_data(
    client: &Client,
    room: &matrix_sdk::Room,
    poll_event_id: &matrix_sdk::ruma::EventId,
    room_id: &str,
) -> Option<crate::models::PollData> {
    use matrix_sdk::ruma::api::client::relations::get_relating_events_with_rel_type;
    use matrix_sdk::ruma::events::poll::{
        compile_unstable_poll_results, unstable_start::UnstablePollStartEventContent,
        PollResponseData,
    };
    use matrix_sdk::ruma::events::relation::RelationType;
    use matrix_sdk::ruma::{MilliSecondsSinceUnixEpoch, OwnedUserId, UInt};

    // Every early return below used to be a silent `?` — harmless when a
    // poll really did just fail to exist (deleted, no permission), but
    // indistinguishable from a genuine parsing bug quietly making a poll
    // that *does* exist vanish from `Command::ListPolls`'s results (and,
    // for a live vote/end update, from `register_poll_handlers`'s
    // re-fetch) with zero trace of why. Logged instead so "created a poll
    // but it never shows up" has an actual error to look at.
    let raw_root = match room.event(poll_event_id, None).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, %poll_event_id, "fetch_poll_data: room.event failed");
            return None;
        }
    };
    let start_value: serde_json::Value = match raw_root.raw().deserialize_as() {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, %poll_event_id, "fetch_poll_data: failed to deserialize raw event as JSON");
            return None;
        }
    };
    let start_value =
        decrypt_if_needed(room, raw_root.raw().cast_ref_unchecked(), start_value).await;

    let Some(content_value) = start_value.get("content") else {
        tracing::warn!(%poll_event_id, ?start_value, "fetch_poll_data: event has no \"content\" field");
        return None;
    };
    let start_content: UnstablePollStartEventContent =
        match serde_json::from_value(content_value.clone()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, %poll_event_id, content = %content_value, "fetch_poll_data: failed to parse poll start content");
                return None;
            }
        };
    let poll_block = start_content.poll_start().clone();

    // A poll started inside a thread carries the same `m.relates_to:
    // {rel_type: "m.thread", event_id: ...}` shape `SendMessage`/
    // `Command::StartPoll` produce for any other threaded message.
    let thread_id = start_value
        .pointer("/content/m.relates_to")
        .filter(|r| r.get("rel_type").and_then(|v| v.as_str()) == Some("m.thread"))
        .and_then(|r| r.get("event_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let mut request = get_relating_events_with_rel_type::v1::Request::new(
        room.room_id().to_owned(),
        poll_event_id.to_owned(),
        RelationType::Reference,
    );
    request.limit = Some(UInt::from(500u32));
    let response = match client.send(request).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, %poll_event_id, "fetch_poll_data: fetching poll responses/end via /relations failed");
            return None;
        }
    };

    let mut owned_responses: Vec<(OwnedUserId, MilliSecondsSinceUnixEpoch, Vec<String>)> = Vec::new();
    let mut end_ts: Option<MilliSecondsSinceUnixEpoch> = None;

    for raw in &response.chunk {
        let Ok(value) = raw.deserialize_as::<serde_json::Value>() else {
            continue;
        };
        let value = decrypt_if_needed(room, raw, value).await;
        let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or_default();
        let Some(sender) = value
            .get("sender")
            .and_then(|v| v.as_str())
            .and_then(|s| OwnedUserId::try_from(s).ok())
        else {
            continue;
        };
        let ts = value.get("origin_server_ts").and_then(|v| v.as_i64()).unwrap_or(0).max(0);
        let ts = MilliSecondsSinceUnixEpoch(UInt::try_from(ts).unwrap_or_default());

        match event_type {
            "org.matrix.msc3381.poll.response" => {
                let answers = value
                    .pointer("/content/org.matrix.msc3381.poll.response/answers")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                owned_responses.push((sender, ts, answers));
            }
            "org.matrix.msc3381.poll.end" => {
                end_ts = Some(match end_ts {
                    Some(existing) if existing >= ts => existing,
                    _ => ts,
                });
            }
            _ => {}
        }
    }

    let ended = end_ts.is_some();
    let response_data: Vec<PollResponseData> = owned_responses
        .iter()
        .map(|(uid, ts, answers)| PollResponseData {
            sender: uid,
            origin_server_ts: *ts,
            selections: answers.as_slice(),
        })
        .collect();
    let results = compile_unstable_poll_results(&poll_block, response_data, end_ts);

    let my_user_id = client.user_id();
    let mut total_voters: std::collections::BTreeSet<&matrix_sdk::ruma::UserId> =
        std::collections::BTreeSet::new();
    let mut options = Vec::new();
    let mut my_vote_ids = Vec::new();
    for answer in poll_block.answers.iter() {
        let voters = results.get(answer.id.as_str());
        let count = voters.map(|s| s.len()).unwrap_or(0) as u64;
        if let Some(voters) = voters {
            total_voters.extend(voters.iter());
            if let Some(uid) = my_user_id {
                if voters.contains(uid) {
                    my_vote_ids.push(answer.id.clone());
                }
            }
        }
        options.push(crate::models::PollOptionResult {
            id: answer.id.clone(),
            text: answer.text.clone(),
            votes: count,
        });
    }

    Some(crate::models::PollData {
        room_id: room_id.to_string(),
        thread_id,
        poll_event_id: poll_event_id.to_string(),
        question: poll_block.question.text.clone(),
        options,
        total_votes: total_voters.len() as u64,
        my_vote_ids,
        ended,
        max_selections: u64::from(poll_block.max_selections),
    })
}

async fn parse_thread_root(
    client: &Client,
    room: Option<&matrix_sdk::Room>,
    room_id: &str,
    raw: &matrix_sdk::ruma::serde::Raw<matrix_sdk::ruma::events::AnyTimelineEvent>,
) -> Option<crate::models::TimelineEvent> {
    let value: serde_json::Value = raw.deserialize_as().ok()?;
    // Same raw-`/threads`-bypasses-decryption issue as `/relations` —
    // see `decrypt_if_needed`'s doc comment.
    let value = match room {
        Some(room) => decrypt_if_needed(room, raw.cast_ref_unchecked(), value).await,
        None => value,
    };
    let mut event = parse_raw_message_event(client, room_id, &value).await?;
    // Thread roots should always show a reply count button, even at 0.
    event.thread_count = event.thread_count.or(Some(0));
    Some(event)
}

/// Fetches every reply in one thread via `/relations` (see `LoadThread`'s
/// comment for why this beats filtering+paginating a live `Timeline`).
/// Returned in chronological order. Does not include the thread root
/// itself — the frontend already has that (see `LoadThread`'s comment).
/// Fetches one page of a thread's replies via `/relations`, most recent
/// first from the server (each call continues further into older history
/// than the last, via `from`). Returns the page in chronological order
/// (oldest of the page first — ready to prepend above whatever's already
/// shown) plus a `next_batch` token to pass as `from` for the next older
/// page, or `None` once there's nothing older left.
///
/// Deliberately page-by-page rather than looping until exhausted — a
/// thread with hundreds of replies made `LoadThread` fetch every single
/// one upfront, which was slow to the point of being hard to use for busy
/// threads. `Command::LoadThread` only calls this once (most recent page);
/// `Command::LoadMoreThreadReplies` calls it again with the stored
/// `next_batch` when the user scrolls up for more, same shape as the main
/// timeline's `PaginateBack`.
async fn fetch_thread_replies_page(
    client: &Client,
    room: &matrix_sdk::Room,
    room_id: &str,
    root_event_id: &matrix_sdk::ruma::EventId,
    from: Option<String>,
) -> (Vec<crate::models::TimelineEvent>, Option<String>) {
    use matrix_sdk::ruma::api::client::relations::{
        get_relating_events, get_relating_events_with_rel_type,
    };
    use matrix_sdk::ruma::events::relation::RelationType;

    let mut request = get_relating_events_with_rel_type::v1::Request::new(
        room.room_id().to_owned(),
        root_event_id.to_owned(),
        RelationType::Thread,
    );
    request.from = from.clone();
    request.limit = Some(matrix_sdk::ruma::UInt::from(20u32));

    // This homeserver (confirmed via testing) 500s on the `rel_type`-scoped
    // route specifically (`/relations/{eventId}/m.thread`) for every
    // thread tried, while the un-scoped route (`/relations/{eventId}`,
    // just "every relation of any kind", filtered down to `m.thread`
    // below) works fine — a server-side routing bug on their end, not
    // something wrong with this request. Falling back rather than just
    // giving up keeps threads usable on servers with that bug instead of
    // silently showing 0 replies.
    let (chunk, next_batch) = match client.send(request).await {
        Ok(r) => (r.chunk, r.next_batch),
        Err(e) => {
            tracing::warn!(
                error = %e,
                room_id,
                %root_event_id,
                "fetching rel_type-scoped thread relations failed, falling back to unscoped /relations"
            );
            let mut fallback_request = get_relating_events::v1::Request::new(
                room.room_id().to_owned(),
                root_event_id.to_owned(),
            );
            fallback_request.from = from;
            fallback_request.limit = Some(matrix_sdk::ruma::UInt::from(20u32));
            match client.send(fallback_request).await {
                Ok(r) => {
                    let chunk = r
                        .chunk
                        .into_iter()
                        .filter(|raw| {
                            raw.get_field::<serde_json::Value>("content")
                                .ok()
                                .flatten()
                                .and_then(|content| content.get("m.relates_to").cloned())
                                .and_then(|rel| rel.get("rel_type").cloned())
                                .and_then(|v| v.as_str().map(|s| s == "m.thread"))
                                .unwrap_or(false)
                        })
                        .collect();
                    (chunk, r.next_batch)
                }
                Err(e) => {
                    tracing::warn!(error = %e, room_id, %root_event_id, "fallback thread relations fetch also failed");
                    return (Vec::new(), None);
                }
            }
        }
    };

    let mut replies = Vec::new();
    for raw in &chunk {
        if let Ok(value) = raw.deserialize_as::<serde_json::Value>() {
            let value = decrypt_if_needed(room, raw, value).await;
            if let Some(event) = parse_raw_message_event(client, room_id, &value).await {
                replies.push(event);
            }
        }
    }
    replies.reverse();

    let next_batch = next_batch.filter(|n| !n.is_empty());
    (replies, next_batch)
}

/// `/relations` (used for thread replies — see `fetch_thread_replies_page`)
/// is a raw REST call that bypasses matrix-sdk's usual decryption
/// pipeline entirely — every event in an encrypted room is transmitted as
/// an opaque `m.room.encrypted` envelope, and only the sync/Timeline
/// machinery normally decrypts those automatically. Without this, thread
/// replies in any encrypted room showed up with garbage/empty bodies
/// (and, for image replies, media that decrypted to nothing meaningful)
/// since `parse_raw_message_event` was being handed ciphertext shaped
/// nothing like the `content.body`/`content.msgtype` it expects.
async fn decrypt_if_needed(
    room: &matrix_sdk::Room,
    raw: &matrix_sdk::ruma::serde::Raw<matrix_sdk::ruma::events::AnyMessageLikeEvent>,
    value: serde_json::Value,
) -> serde_json::Value {
    if value.get("type").and_then(|v| v.as_str()) != Some("m.room.encrypted") {
        return value;
    }
    match room.decrypt_event(raw.cast_ref_unchecked(), None).await {
        Ok(decrypted) => decrypted
            .raw()
            .deserialize_as::<serde_json::Value>()
            .unwrap_or(value),
        Err(e) => {
            tracing::debug!(error = %e, "failed to decrypt thread reply");
            let mut fallback = value;
            fallback["content"] =
                serde_json::json!({ "msgtype": "m.notice", "body": "[unable to decrypt]" });
            fallback
        }
    }
}

struct RawReaction {
    sender: String,
    emoji: String,
    reaction_event_id: matrix_sdk::ruma::OwnedEventId,
}

/// Fetches every `m.reaction` event that annotates `event_id`, via
/// `/relations/{eventId}/m.annotation` — same endpoint shape (and same
/// decrypt-the-raw-response need, see `decrypt_if_needed`) as
/// `fetch_thread_replies_page` uses for `m.thread`. Deliberately not
/// filtered by `event_type=m.reaction` server-side: that filter matches
/// against the *outer* event type, which in an encrypted room is always
/// `m.room.encrypted` regardless of what's inside — filtering there would
/// silently return nothing for any encrypted room.
async fn fetch_reaction_events(
    client: &Client,
    room: &matrix_sdk::Room,
    event_id: &matrix_sdk::ruma::EventId,
) -> Vec<RawReaction> {
    use matrix_sdk::ruma::api::client::relations::get_relating_events_with_rel_type;
    use matrix_sdk::ruma::events::relation::RelationType;

    let mut request = get_relating_events_with_rel_type::v1::Request::new(
        room.room_id().to_owned(),
        event_id.to_owned(),
        RelationType::Annotation,
    );
    request.limit = Some(matrix_sdk::ruma::UInt::from(100u32));

    let response = match client.send(request).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, %event_id, "fetching reactions failed");
            return Vec::new();
        }
    };

    let mut reactions = Vec::new();
    for raw in &response.chunk {
        let Ok(value) = raw.deserialize_as::<serde_json::Value>() else {
            continue;
        };
        let value = decrypt_if_needed(room, raw, value).await;
        if value.get("type").and_then(|v| v.as_str()) != Some("m.reaction") {
            continue;
        }
        let sender = value.get("sender").and_then(|v| v.as_str());
        let emoji = value.pointer("/content/m.relates_to/key").and_then(|v| v.as_str());
        let reaction_event_id = value
            .get("event_id")
            .and_then(|v| v.as_str())
            .and_then(|s| matrix_sdk::ruma::OwnedEventId::try_from(s).ok());
        if let (Some(sender), Some(emoji), Some(reaction_event_id)) =
            (sender, emoji, reaction_event_id)
        {
            reactions.push(RawReaction {
                sender: sender.to_string(),
                emoji: emoji.to_string(),
                reaction_event_id,
            });
        }
    }
    reactions
}

/// Groups raw reaction events by emoji into what the frontend actually
/// renders — a count, the list of reactors, and whether `my_id` is among
/// them.
fn summarize_reactions(reactions: &[RawReaction], my_id: Option<&str>) -> Vec<crate::models::ReactionSummary> {
    let mut by_emoji: Vec<(String, Vec<String>, bool)> = Vec::new();
    for r in reactions {
        if let Some(entry) = by_emoji.iter_mut().find(|(e, ..)| e == &r.emoji) {
            entry.1.push(r.sender.clone());
            entry.2 = entry.2 || my_id == Some(r.sender.as_str());
        } else {
            by_emoji.push((r.emoji.clone(), vec![r.sender.clone()], my_id == Some(r.sender.as_str())));
        }
    }
    by_emoji
        .into_iter()
        .map(|(emoji, senders, by_me)| crate::models::ReactionSummary {
            emoji,
            count: senders.len() as u64,
            by_me,
            senders,
        })
        .collect()
}

/// Parses a raw `m.room.message`-shaped JSON event (as returned by
/// `/messages`, `/relations`, `/threads`, `room.event()`, etc.) into our
/// own `TimelineEvent` model, independent of ruma's typed `MessageType`
/// enum — deliberately more permissive than the typed path in
/// `convert.rs`, since some clients/bridges send message content ruma's
/// strict variants don't recognize.
async fn parse_raw_message_event(
    client: &Client,
    room_id: &str,
    value: &serde_json::Value,
) -> Option<crate::models::TimelineEvent> {
    let event_id = value.get("event_id")?.as_str()?.to_string();
    let sender_str = value.get("sender")?.as_str()?.to_string();
    let timestamp = value.get("origin_server_ts")?.as_i64()?;

    let content = value.get("content");
    let msgtype = content
        .and_then(|c| c.get("msgtype"))
        .and_then(|v| v.as_str());
    let body = content
        .and_then(|c| c.get("body"))
        .and_then(|v| v.as_str())
        .unwrap_or("(unsupported message type)")
        .to_string();
    let media_url = content
        .and_then(|c| {
            c.get("url")
                .and_then(|v| v.as_str())
                .or_else(|| c.pointer("/file/url").and_then(|v| v.as_str()))
        })
        .map(|s| s.to_string());
    let media_mime = content
        .and_then(|c| c.pointer("/info/mimetype"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // Encrypted attachments carry their AES key/iv/hashes under
    // `content.file` instead of a plain `content.url` — pass that whole
    // object through as-is, `download_media_bytes` deserializes it back
    // into an `EncryptedFile` to decrypt with.
    let media_encryption = content
        .and_then(|c| c.get("file"))
        .map(|f| f.to_string());

    // This is the raw-JSON path used for thread replies (fetched via
    // `/relations`, not matrix-sdk-ui's typed `Timeline`), so unlike
    // `convert.rs` there's no ruma `MessageType` enum to match on — just
    // whatever string the server sent. Used to lump every attachment type
    // into `"file"` regardless of the actual `msgtype`, which meant a
    // `m.image` in a thread never got the `msg_type: "image"` the
    // frontend specifically checks for to render an `<img>` — it just
    // showed the filename as plain text instead, indistinguishable from
    // the image never having loaded at all.
    // A sticker's content has no `msgtype` field at all (just `body`/
    // `url`/`info` directly) — without checking the event's own top-level
    // `type` too, one always fell into the generic `(_, Some(_)) =>
    // "file"` case below, which the frontend renders as a filename link
    // rather than the actual picture (its own "does this filename look
    // like an image" fallback never matches, since a sticker's `body` is
    // a shortcode like "cat-wave", not "cat-wave.png").
    let is_sticker = value.get("type").and_then(|v| v.as_str()) == Some("m.sticker");
    let msg_type = match (msgtype, &media_url) {
        (Some("m.notice"), _) => "notice".to_string(),
        (Some("m.image"), Some(_)) => "image".to_string(),
        (Some("m.video"), Some(_)) => "video".to_string(),
        (Some("m.audio"), Some(_)) => "audio".to_string(),
        (None, Some(_)) if is_sticker => "image".to_string(),
        (_, Some(_)) => "file".to_string(),
        (Some(t), None) if t == "m.image" || t == "m.video" || t == "m.file" => {
            tracing::debug!(
                msgtype = t,
                body,
                "thread event: no mxc found in raw content"
            );
            "text".to_string()
        }
        _ => "text".to_string(),
    };

    let thread_count = value
        .pointer("/unsigned/m.relations/m.thread/count")
        .and_then(|v| v.as_u64());

    // Same "not the thread's own bookkeeping fallback" check as
    // `convert.rs`'s `reply_to_event_id_from_raw` — see its comment.
    let reply_to_event_id = content
        .and_then(|c| c.get("m.relates_to"))
        .and_then(|relates_to| {
            if relates_to.get("rel_type").and_then(|v| v.as_str()) == Some("m.thread")
                && relates_to.get("is_falling_back").and_then(|v| v.as_bool()) == Some(true)
            {
                return None;
            }
            relates_to
                .pointer("/m.in_reply_to/event_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

    let is_own = client
        .user_id()
        .map(|id| id.as_str() == sender_str)
        .unwrap_or(false);

    let mentions_me = mentions_user(content, client.user_id());
    let mentioned_user_ids = crate::matrix::convert::mentioned_user_ids_from_content(content);

    let sender_name = resolve_sender_name(client, room_id, &sender_str).await;
    let sender_avatar_url = resolve_sender_avatar_url(client, room_id, &sender_str).await;

    // A thread root's bundled aggregation (same `unsigned.m.relations.m.thread`
    // block `thread_count` above comes from) also carries the thread's most
    // recent reply inline — no extra `/relations` round trip needed just to
    // show a "first message / last message" preview for a thread that isn't
    // open. Only ever present alongside `thread_count`, so these three stay
    // `None` for every other event this function parses (a normal timeline
    // message, an actual thread reply, ...) — harmless, just unused there.
    let latest_event = value.pointer("/unsigned/m.relations/m.thread/latest_event");
    let latest_reply_sender_name = match latest_event.and_then(|e| e.get("sender")).and_then(|v| v.as_str()) {
        Some(latest_sender) => Some(resolve_sender_name(client, room_id, latest_sender).await),
        None => None,
    };
    let latest_reply_body = latest_event
        .and_then(|e| e.pointer("/content/body"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let latest_reply_ts = latest_event
        .and_then(|e| e.get("origin_server_ts"))
        .and_then(|v| v.as_i64());
    let latest_reply_event_id = latest_event
        .and_then(|e| e.get("event_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let latest_reply_mentions_me =
        mentions_user(latest_event.and_then(|e| e.get("content")), client.user_id());
    // Same field-by-field extraction `parse_raw_message_event`'s own
    // `msg_type`/`media_url`/`media_mime`/`media_encryption` above use on
    // this function's own `content` — mirrored here against the latest
    // reply's bundled `content` so an image/video/file reply previews as
    // more than just its filename text.
    let latest_reply_content = latest_event.and_then(|e| e.get("content"));
    let latest_reply_msgtype = latest_reply_content
        .and_then(|c| c.get("msgtype"))
        .and_then(|v| v.as_str());
    let latest_reply_media_url = latest_reply_content
        .and_then(|c| {
            c.get("url")
                .and_then(|v| v.as_str())
                .or_else(|| c.pointer("/file/url").and_then(|v| v.as_str()))
        })
        .map(|s| s.to_string());
    let latest_reply_media_mime = latest_reply_content
        .and_then(|c| c.pointer("/info/mimetype"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let latest_reply_media_encryption = latest_reply_content
        .and_then(|c| c.get("file"))
        .map(|f| f.to_string());
    let latest_reply_msg_type = match (latest_reply_msgtype, &latest_reply_media_url) {
        (Some("m.notice"), _) => Some("notice".to_string()),
        (Some("m.image"), Some(_)) => Some("image".to_string()),
        (Some("m.video"), Some(_)) => Some("video".to_string()),
        (Some("m.audio"), Some(_)) => Some("audio".to_string()),
        (_, Some(_)) => Some("file".to_string()),
        _ => None,
    };
    // The server's own answer to "is this thread unread", derived from
    // this account's actual *threaded* read receipt (`m.receipt` with
    // `thread_id` — MSC3771) for it, if one exists — not a session-local
    // guess. Deliberately not gated on this app being the one that sent
    // that receipt: receipts are account-wide state, so a thread read via
    // Element (or any other client) on the same account correctly shows
    // as read here too. A thread with no replies yet has nothing to be
    // behind on, so it's simply never unread.
    let is_unread = match (thread_count, latest_event.and_then(|e| e.get("event_id")).and_then(|v| v.as_str())) {
        (Some(count), Some(latest_event_id)) if count > 0 => {
            thread_is_unread(client, room_id, &event_id, latest_event_id).await
        }
        (Some(0), _) => Some(false),
        _ => None,
    };

    Some(crate::models::TimelineEvent {
        event_id,
        sender: sender_str,
        sender_name,
        sender_avatar_url,
        body,
        msg_type,
        media_url,
        media_mime,
        media_encryption,
        thumbnail_url: None,
        timestamp,
        thread_count,
        is_own,
        reply_to_event_id,
        reply_to_preview: None,
        mentions_me,
        mentioned_user_ids,
        reactions: Vec::new(),
        read_by: Vec::new(),
        latest_reply_sender_name,
        latest_reply_body,
        latest_reply_ts,
        latest_reply_event_id,
        latest_reply_mentions_me,
        latest_reply_msg_type,
        latest_reply_media_url,
        latest_reply_media_mime,
        latest_reply_media_encryption,
        is_unread,
        local_id: None,
    })
}

/// Whether `root_event_id` (a thread root in `room_id`) is unread, per this
/// account's own threaded read receipt for it (`m.receipt`/`thread_id`,
/// MSC3771) — `None` if that can't be determined (room not found, no
/// logged-in user, invalid event id), `Some(true)` if there's no such
/// receipt yet or it points at an older event than `latest_event_id`.
/// Reads straight from the local sync store (`Room::load_user_receipt`) —
/// no network round trip, since receipts already flow in continuously as
/// `m.receipt` ephemeral events over `/sync`.
async fn thread_is_unread(
    client: &Client,
    room_id: &str,
    root_event_id: &str,
    latest_event_id: &str,
) -> Option<bool> {
    use matrix_sdk::ruma::events::receipt::{ReceiptThread, ReceiptType};

    let room = RoomId::parse(room_id).ok().and_then(|id| client.get_room(&id))?;
    let user_id = client.user_id()?;
    let root_event_id = OwnedEventId::try_from(root_event_id).ok()?;

    let read = room
        .load_user_receipt(ReceiptType::Read, ReceiptThread::Thread(root_event_id), user_id)
        .await
        .ok()
        .flatten();

    Some(match read {
        Some((read_event_id, _)) => read_event_id.as_str() != latest_event_id,
        None => true,
    })
}

/// Resolves a sender's room-specific display name (falling back to their
/// bare user id if they have none set, or the room/id can't be resolved at
/// all) — factored out since both a message's own sender and, for a
/// thread root, its bundled latest-reply sender need the exact same
/// lookup.
async fn resolve_sender_name(client: &Client, room_id: &str, sender_str: &str) -> String {
    match matrix_sdk::ruma::OwnedUserId::try_from(sender_str) {
        Ok(user_id) => match matrix_sdk::ruma::RoomId::parse(room_id)
            .ok()
            .and_then(|rid| client.get_room(&rid))
        {
            Some(room) => room
                .get_member(&user_id)
                .await
                .ok()
                .flatten()
                .and_then(|m| m.display_name().map(|n| n.to_string()))
                .unwrap_or_else(|| sender_str.to_string()),
            None => sender_str.to_string(),
        },
        Err(_) => sender_str.to_string(),
    }
}

/// Same lookup as `resolve_sender_name`, for the sender's avatar instead of
/// their display name.
async fn resolve_sender_avatar_url(client: &Client, room_id: &str, sender_str: &str) -> Option<String> {
    let user_id = matrix_sdk::ruma::OwnedUserId::try_from(sender_str).ok()?;
    let room = matrix_sdk::ruma::RoomId::parse(room_id)
        .ok()
        .and_then(|rid| client.get_room(&rid))?;
    room.get_member(&user_id)
        .await
        .ok()
        .flatten()
        .and_then(|m| m.avatar_url().map(|u| u.to_string()))
}

/// Whether `content."m.mentions".user_ids` names `user_id` — used for
/// `TimelineEvent::mentions_me` on the two paths that build events from
/// raw JSON directly (`convert.rs`'s `convert_item` gets this for free
/// from matrix-sdk's own push-rule evaluation via `is_highlighted()`
/// instead, which these raw-JSON paths don't have access to).
fn mentions_user(content: Option<&serde_json::Value>, user_id: Option<&matrix_sdk::ruma::UserId>) -> bool {
    let (Some(content), Some(user_id)) = (content, user_id) else {
        return false;
    };
    content
        .pointer("/m.mentions/user_ids")
        .and_then(|v| v.as_array())
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(user_id.as_str())))
}

/// Shared by `Command::FetchImage` and `Command::PlayVideo`.
///
/// Deliberately bypasses `client.media()`: that goes through the legacy
/// unauthenticated `/_matrix/media/v3/download/...` endpoint, which Synapse
/// 1.146+ (per MSC3916) now answers with a plain 404 M_NOT_FOUND — matrix-sdk
/// 0.7 predates MSC3916 support, so there's no SDK method for the new
/// `/_matrix/client/v1/media/download/...` endpoint yet. Fetch it directly
/// with the session's own access token instead.
/// A fresh `reqwest::Client` per download (the old behavior) means a fresh
/// TLS handshake and no connection reuse every single time — scrolling
/// through a timeline can fire off a dozen concurrent image fetches, and
/// with no timeout configured either, one slow/stuck connection among them
/// just hangs forever instead of failing and letting the frontend's retry
/// logic recover. One shared, connection-pooling, timeout-bound client
/// fixes both.
fn media_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .expect("failed to build media http client")
    })
}

/// `media_encryption` is `TimelineEvent::media_encryption` — JSON of an
/// `EncryptedFile` (AES key/iv/hashes), present when this media belongs
/// to an encrypted room. The homeserver only ever stores/serves the
/// ciphertext for those (E2EE means the server never has the key), so
/// downloading without decrypting here would hand back garbage bytes —
/// exactly what made every image in an encrypted room look like a broken
/// image regardless of its real format.
async fn download_media_bytes(
    client: &Client,
    mxc_uri: &str,
    media_encryption: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    let mxc = matrix_sdk::ruma::OwnedMxcUri::from(mxc_uri.to_string());
    let (server_name, media_id) = mxc.parts()?;
    let token = client
        .access_token()
        .ok_or_else(|| anyhow::anyhow!("not logged in"))?;

    let url = format!(
        "{}_matrix/client/v1/media/download/{server_name}/{media_id}",
        client.homeserver()
    );

    let response = media_http_client()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;

    let ciphertext = response.bytes().await?.to_vec();

    let Some(encryption_json) = media_encryption else {
        return Ok(ciphertext);
    };

    let file: matrix_sdk::ruma::events::room::EncryptedFile = serde_json::from_str(encryption_json)?;
    let mut cursor = std::io::Cursor::new(ciphertext);
    let mut decryptor = matrix_sdk_crypto::AttachmentDecryptor::new(&mut cursor, file.into())?;
    let mut decrypted = Vec::new();
    std::io::Read::read_to_end(&mut decryptor, &mut decrypted)?;
    Ok(decrypted)
}

/// Local file holding a small thumbnail of an avatar, for use as a desktop
/// notification's `image-path` — notification daemons need a file path
/// (or raw pixels), not an `mxc://` URI. Cached on disk per `mxc` (avatar
/// URIs are immutable: a changed avatar gets a new one), so a busy room
/// doesn't refetch it for every message. `None` on any failure, which
/// just leaves the notification showing the app icon instead.
#[cfg(target_os = "linux")]
async fn notification_avatar_path(state: &Arc<Mutex<WorkerState>>, mxc_uri: &str) -> Option<std::path::PathBuf> {
    let path = data_dir()
        .join("notification-avatars")
        .join(sanitize_filename(mxc_uri.trim_start_matches("mxc://")));
    if path.exists() {
        return Some(path);
    }
    let fetch = async {
        let client = get_client(state).await?;
        let mxc = matrix_sdk::ruma::OwnedMxcUri::from(mxc_uri.to_string());
        let (server_name, media_id) = mxc.parts()?;
        let token = client
            .access_token()
            .ok_or_else(|| anyhow::anyhow!("not logged in"))?;
        let url = format!(
            "{}_matrix/client/v1/media/thumbnail/{server_name}/{media_id}?width=96&height=96&method=crop",
            client.homeserver()
        );
        let bytes = media_http_client()
            .get(&url)
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        // Written under a temp name first so a half-written file never
        // gets picked up by the `exists()` check above.
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        anyhow::Ok(())
    };
    match fetch.await {
        Ok(()) => Some(path),
        Err(err) => {
            tracing::warn!(mxc_uri, error = %err, "failed to fetch notification avatar");
            None
        }
    }
}

/// Counterpart to `notification_avatar_path` for a sender with no avatar
/// image: writes the frontend-drawn colored-initial PNG to a file named by
/// its content hash, so each distinct one is only written once.
#[cfg(target_os = "linux")]
fn notification_initial_avatar_path(png_base64: &str) -> Option<std::path::PathBuf> {
    use base64::Engine as _;
    use std::hash::{Hash, Hasher};
    let write = || {
        let bytes = base64::engine::general_purpose::STANDARD.decode(png_base64)?;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        let path = data_dir()
            .join("notification-avatars")
            .join(format!("initial-{:016x}.png", hasher.finish()));
        if !path.exists() {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let tmp = path.with_extension("part");
            std::fs::write(&tmp, &bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        anyhow::Ok(path)
    };
    match write() {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!(error = %err, "failed to write notification initial avatar");
            None
        }
    }
}

/// Strips anything that isn't a reasonably safe filename character, so a
/// message body used as a video's display name can't do anything weird
/// when turned into a temp file path.
fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "video.mp4".to_string()
    } else {
        cleaned
    }
}

/// Picks `dir/name`, or `dir/name (1)`, `dir/name (2)`, ... the first one
/// that doesn't already exist — so downloading the same attachment twice
/// (or one that happens to share a filename with something already in
/// Downloads) doesn't silently clobber the earlier file.
fn unique_download_path(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    if !path.exists() {
        return path;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (name.to_string(), String::new()),
    };
    for n in 1..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(name)
}

fn chrono_like_timestamp() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Pushes every new `m.room.message` to the UI as it arrives during sync,
/// so the room list preview / unread state can update even for rooms whose
/// timeline isn't currently open.
fn register_new_message_handler(client: &Client, tx: UnboundedSender<Event>) {
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::room::message::OriginalSyncRoomMessageEvent,
              room: matrix_sdk::room::Room,
              client: Client| {
            let tx = tx.clone();
            async move {
                let room_id = room.room_id().to_string();

                // An edit arrives as a brand new `m.room.message` event
                // (with `m.relates_to: {rel_type: "m.replace", ...}`), not
                // as some distinct "edit" event type. Without this check
                // it used to show up as a duplicate new message instead of
                // updating the original in place.
                use matrix_sdk::ruma::events::room::message::Relation;

                // Thread replies used to get folded into the main room
                // timeline here too, same bug the main timeline's
                // `LoadTimeline` had before its `event_filter` excluded
                // them — a live reply arriving via `/sync` while the room
                // was open bypassed that filter entirely (it only applies
                // to matrix-sdk-ui's `Timeline` object, not this raw event
                // handler). They belong in the thread panel instead —
                // tracked here so the panel/badge can pick them up below.
                let thread_root_id = match &ev.content.relates_to {
                    Some(Relation::Replacement(repl)) => {
                        tx.send(Event::MessageEdited {
                            room_id,
                            event_id: repl.event_id.to_string(),
                            new_body: repl.new_content.msgtype.body().to_string(),
                        })
                        .ok();
                        return;
                    }
                    Some(Relation::Thread(thread)) => Some(thread.event_id.to_string()),
                    _ => None,
                };

                // A plain (non-thread) reply, or a genuine reply to a
                // specific message *within* a thread — the latter is also
                // spec'd as a `Thread` relation, carrying its own
                // `in_reply_to` alongside the thread root, and only
                // counts as a real reply when `is_falling_back` is
                // `false` (a `true` one is just the thread's own
                // fallback-for-non-threaded-clients bookkeeping, not
                // something the sender chose to reply to). Missing the
                // `Thread` arm here meant a live thread reply — including
                // your own message, arriving back right after you send
                // it — never showed its reply-preview quote at all, no
                // matter how you sent it, until the thread was reloaded
                // through the `/relations` path (`parse_raw_message_event`)
                // that already had this check.
                let reply_to_event_id = match &ev.content.relates_to {
                    Some(Relation::Reply(reply)) => Some(reply.in_reply_to.event_id.to_string()),
                    Some(Relation::Thread(thread)) if !thread.is_falling_back => {
                        thread.in_reply_to.as_ref().map(|r| r.event_id.to_string())
                    }
                    _ => None,
                };

                let sender = ev.sender.to_string();
                let is_own = client.user_id().map(|id| id == ev.sender).unwrap_or(false);

                // Without this, every live message showed the sender's raw
                // `@user:server` ID instead of their display name — the
                // other conversion paths (`convert.rs`, `parse_raw_message_event`)
                // already resolve this, this one just never did.
                let member = room.get_member(&ev.sender).await.ok().flatten();
                let sender_name = member
                    .as_ref()
                    .and_then(|m| m.display_name().map(|n| n.to_string()))
                    .unwrap_or_else(|| sender.clone());
                let sender_avatar_url = member
                    .as_ref()
                    .and_then(|m| m.avatar_url().map(|u| u.to_string()));

                let (body, msg_type, media_url, thumbnail_url, media_mime, media_encryption) =
                    crate::matrix::convert::message_type_fields(&ev.content.msgtype);

                let mentions_me = client
                    .user_id()
                    .zip(ev.content.mentions.as_ref())
                    .is_some_and(|(uid, mentions)| mentions.user_ids.contains(uid));
                // `m.mentions` (the modern, spec'd source) plus, since
                // not every client sets that field for a mention it still
                // renders as a pill, anything a matrix.to user link in
                // the formatted body implies too — same union
                // `parse_raw_message_event`'s raw-JSON path gets via
                // `mentioned_user_ids_from_content`.
                let mut mentioned_user_ids: Vec<String> = ev
                    .content
                    .mentions
                    .as_ref()
                    .map(|m| m.user_ids.iter().map(|id| id.to_string()).collect())
                    .unwrap_or_default();
                if let Some(html) = crate::matrix::convert::formatted_html_of(&ev.content.msgtype) {
                    for id in crate::matrix::convert::user_ids_from_formatted_body(html) {
                        if !mentioned_user_ids.contains(&id) {
                            mentioned_user_ids.push(id);
                        }
                    }
                }

                let event = crate::models::TimelineEvent {
                    event_id: ev.event_id.to_string(),
                    sender,
                    sender_name,
                    sender_avatar_url,
                    body,
                    msg_type,
                    media_url,
                    media_mime,
                    media_encryption,
                    thumbnail_url,
                    timestamp: ev.origin_server_ts.0.into(),
                    reply_to_event_id,
                    reply_to_preview: None,
                    thread_count: None,
                    is_own,
                    mentions_me,
                    mentioned_user_ids,
                    reactions: Vec::new(),
                    read_by: Vec::new(),
                    latest_reply_sender_name: None,
                    latest_reply_body: None,
                    latest_reply_ts: None,
                    latest_reply_event_id: None,
                    latest_reply_mentions_me: false,
                    latest_reply_msg_type: None,
                    latest_reply_media_url: None,
                    latest_reply_media_mime: None,
                    latest_reply_media_encryption: None,
                    is_unread: None,
                    // Only ever `Some` down the sync stream reaching the
                    // exact device that sent this — see `local_id`'s doc
                    // comment on `TimelineEvent`. `Command::SendMessage`
                    // sets this event's transaction ID to its own
                    // `local_id` (see `with_transaction_id` below) so this
                    // round-trips straight back here.
                    local_id: ev.unsigned.transaction_id.as_ref().map(|id| id.to_string()),
                };

                if let Some(thread_root_id) = thread_root_id {
                    tx.send(Event::ThreadReply {
                        room_id,
                        thread_root_id,
                        event,
                    })
                    .ok();
                } else {
                    tracing::info!(room_id = %room_id, "register_new_message_handler: NewMessage received (live event handler fired)");
                    tx.send(Event::NewMessage { room_id, event }).ok();
                }
            }
        },
    );
}

/// Live `m.sticker` messages from *other* people (`Command::SendMeme`
/// handles ours directly — see its comment for why: this handler's typed
/// event can't see the `m.relates_to` a thread-targeted sticker carries
/// at all, `StickerEventContent` has no field for it, so it can never
/// tell a thread reply from a plain one; `is_own` stickers are skipped
/// here unconditionally rather than trying to guess). Without this
/// handler at all, someone else's sticker never showed up in an
/// already-open room — `register_new_message_handler` only listens for
/// `m.room.message`, and stickers are a distinct event type, so sending
/// one looked like it silently did nothing until the room was reloaded
/// (the point it *would* show, via `convert.rs`'s existing
/// `TimelineItemContent::Sticker` handling for the initial-load path).
/// A sticker someone else sends *into a thread* still can't be routed
/// there live for the same reason `Command::SendMeme` has to handle its
/// own — it shows in the main timeline instead until that thread is next
/// opened/reloaded, which reads raw JSON and gets it right.
/// Pushes `Event::UnreadCountChanged` whenever an `m.receipt` ephemeral
/// event arrives for a room — including one that's just this account's own
/// receipt echoing back down `/sync` after being sent from a *different*
/// device/session (see `Command::MarkRoomRead`'s `send_multiple_receipts`
/// call). Without this, marking a room read on desktop only ever cleared
/// its unread badge *there* — every other session on the same account kept
/// showing it unread (the server-side state was correct the whole time;
/// nothing here ever re-read it) until that session happened to restart
/// and recompute its whole room list from scratch, which is what made it
/// look like the fix only "really" took effect after a reload. Recomputes
/// fresh from `room.num_unread_notifications()`/`num_unread_mentions()` —
/// same client-side derivation `entry_to_summary` uses for the initial
/// `RoomSummary`, now just re-run on demand instead of only once at load.
fn register_receipt_handler(client: &Client, tx: UnboundedSender<Event>) {
    client.add_event_handler(
        move |_ev: matrix_sdk::ruma::events::SyncEphemeralRoomEvent<
                  matrix_sdk::ruma::events::receipt::ReceiptEventContent,
              >,
              room: matrix_sdk::room::Room| {
            let tx = tx.clone();
            async move {
                tx.send(Event::UnreadCountChanged {
                    room_id: room.room_id().to_string(),
                    unread_count: room.num_unread_notifications(),
                    mention_count: room.num_unread_mentions(),
                })
                .ok();
            }
        },
    );
}

fn register_sticker_handler(client: &Client, tx: UnboundedSender<Event>) {
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::sticker::OriginalSyncStickerEvent,
              room: matrix_sdk::room::Room,
              client: Client| {
            let tx = tx.clone();
            async move {
                if client.user_id().is_some_and(|id| id == ev.sender) {
                    return;
                }

                let sender = ev.sender.to_string();
                let member = room.get_member(&ev.sender).await.ok().flatten();
                let sender_name = member
                    .as_ref()
                    .and_then(|m| m.display_name().map(|n| n.to_string()))
                    .unwrap_or_else(|| sender.clone());
                let sender_avatar_url = member
                    .as_ref()
                    .and_then(|m| m.avatar_url().map(|u| u.to_string()));

                let event = crate::models::TimelineEvent {
                    event_id: ev.event_id.to_string(),
                    sender,
                    sender_name,
                    sender_avatar_url,
                    body: ev.content.body,
                    msg_type: "image".to_string(),
                    media_url: crate::matrix::convert::sticker_mxc_of(&ev.content.source),
                    media_mime: None,
                    media_encryption: None,
                    thumbnail_url: None,
                    timestamp: ev.origin_server_ts.0.into(),
                    reply_to_event_id: None,
                    reply_to_preview: None,
                    thread_count: None,
                    is_own: false,
                    mentions_me: false,
                    mentioned_user_ids: Vec::new(),
                    reactions: Vec::new(),
                    read_by: Vec::new(),
                    latest_reply_sender_name: None,
                    latest_reply_body: None,
                    latest_reply_ts: None,
                    latest_reply_event_id: None,
                    latest_reply_mentions_me: false,
                    latest_reply_msg_type: None,
                    latest_reply_media_url: None,
                    latest_reply_media_mime: None,
                    latest_reply_media_encryption: None,
                    is_unread: None,
                    local_id: None,
                };
                tx.send(Event::NewMessage { room_id: room.room_id().to_string(), event })
                    .ok();
            }
        },
    );
}

/// Live redactions (deletes) — either our own `Command::DeleteMessage`
/// echoed back, or someone else's, arriving via `/sync`.
fn register_redaction_handler(client: &Client, tx: UnboundedSender<Event>) {
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::room::redaction::OriginalSyncRoomRedactionEvent,
              room: matrix_sdk::room::Room| {
            let tx = tx.clone();
            async move {
                let Some(redacts) = ev.redacts.or(ev.content.redacts) else {
                    return;
                };
                tx.send(Event::MessageDeleted {
                    room_id: room.room_id().to_string(),
                    event_id: redacts.to_string(),
                })
                .ok();
            }
        },
    );
}

/// Pushes an updated `Event::Reactions` whenever anyone's `m.reaction`
/// lands live via sync — matrix-sdk decrypts before dispatching to event
/// handlers, so this fires correctly for encrypted rooms too, unlike the
/// raw `/relations` fetches used elsewhere in this file. Re-fetches the
/// full reaction list rather than incrementing a local count so the
/// result is exactly what a fresh `Command::ToggleReaction` would compute
/// (correct `by_me`, no drift from a missed/duplicate event).
fn register_reaction_handler(client: &Client, tx: UnboundedSender<Event>) {
    let my_id = client.user_id().map(|id| id.to_string());
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::reaction::OriginalSyncReactionEvent,
              room: matrix_sdk::room::Room| {
            let tx = tx.clone();
            let my_id = my_id.clone();
            async move {
                let target = ev.content.relates_to.event_id;
                let client = room.client();
                let reactions = fetch_reaction_events(&client, &room, &target).await;
                tx.send(Event::Reactions {
                    room_id: room.room_id().to_string(),
                    event_id: target.to_string(),
                    reactions: summarize_reactions(&reactions, my_id.as_deref()),
                })
                .ok();
            }
        },
    );
}

/// Pushes an updated `Event::PinnedEvents` whenever anyone (including this
/// account, from another session) changes a room's `m.room.pinned_events`
/// live via sync — same "just re-read the full current state" idea as
/// every other live handler in this file, and it's already a full replace
/// (not a diff) straight from the spec, so there's nothing to reconcile.
fn register_pinned_events_handler(client: &Client, tx: UnboundedSender<Event>) {
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::room::pinned_events::OriginalSyncRoomPinnedEventsEvent,
              room: matrix_sdk::room::Room| {
            let tx = tx.clone();
            async move {
                let event_ids = ev.content.pinned.into_iter().map(|id| id.to_string()).collect();
                tx.send(Event::PinnedEvents {
                    room_id: room.room_id().to_string(),
                    event_ids,
                })
                .ok();
            }
        },
    );
}

/// Pushes a freshly-recompiled `Event::PollUpdated` whenever a vote
/// (`org.matrix.msc3381.poll.response`) or an end
/// (`org.matrix.msc3381.poll.end`) for a poll arrives live via sync — a
/// poll's *start* needs no live handler of its own, since starting one
/// always originates from this app's own `Command::StartPoll`, which
/// already answers with the initial `Event::PollUpdated` itself; a poll
/// someone else starts only becomes visible once the polls panel is opened
/// (`Command::ListPolls`), same "not part of the live timeline stream" way
/// this app treats polls generally (see `Command::ListPolls`'s doc
/// comment).
fn register_poll_handlers(client: &Client, tx: UnboundedSender<Event>) {
    {
        let tx = tx.clone();
        client.add_event_handler(
            move |ev: matrix_sdk::ruma::events::poll::unstable_response::OriginalSyncUnstablePollResponseEvent,
                  room: matrix_sdk::room::Room| {
                let tx = tx.clone();
                async move {
                    let client = room.client();
                    let poll_event_id = ev.content.relates_to.event_id.clone();
                    let room_id = room.room_id().to_string();
                    if let Some(poll) = fetch_poll_data(&client, &room, &poll_event_id, &room_id).await {
                        tx.send(Event::PollUpdated(poll)).ok();
                    }
                }
            },
        );
    }
    {
        let tx = tx.clone();
        client.add_event_handler(
            move |ev: matrix_sdk::ruma::events::poll::unstable_end::OriginalSyncUnstablePollEndEvent,
                  room: matrix_sdk::room::Room| {
                let tx = tx.clone();
                async move {
                    let client = room.client();
                    let poll_event_id = ev.content.relates_to.event_id.clone();
                    let room_id = room.room_id().to_string();
                    if let Some(poll) = fetch_poll_data(&client, &room, &poll_event_id, &room_id).await {
                        tx.send(Event::PollUpdated(poll)).ok();
                    }
                }
            },
        );
    }
}

/// Pushes `Event::PresenceUpdated` for every `m.presence` event this
/// account's sync stream sees — global (not scoped to any one room, since
/// presence itself isn't), so this only ever needs registering once (see
/// `Command::StartSync`), not per-room the way `Command::WatchTyping` needs
/// to be.
fn register_presence_handler(client: &Client, tx: UnboundedSender<Event>) {
    client.add_event_handler(
        move |ev: matrix_sdk::ruma::events::presence::PresenceEvent| {
            let tx = tx.clone();
            async move {
                tx.send(Event::PresenceUpdated(crate::models::PresenceInfo {
                    user_id: ev.sender.to_string(),
                    presence: ev.content.presence.to_string(),
                    currently_active: ev.content.currently_active,
                    last_active_ago_ms: ev.content.last_active_ago.map(u64::from),
                }))
                .ok();
            }
        },
    );
}
