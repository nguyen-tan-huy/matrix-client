use std::collections::HashMap;
use std::sync::Arc;

use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::{OwnedEventId, RoomId};
use matrix_sdk::Client;
use matrix_sdk_ui::timeline::{RoomExt, Timeline};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Mutex;

use crate::command::Command;
use crate::event::Event;
use crate::matrix::convert::convert_items;
use crate::models::RoomSummary;

fn data_dir() -> std::path::PathBuf {
    directories::ProjectDirs::from("com", "example", "matrix-egui-client")
        .map(|d| d.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("./matrix-egui-client-data"))
}

fn session_file() -> std::path::PathBuf {
    data_dir().join("session.json")
}

async fn build_client(homeserver: &str) -> anyhow::Result<Client> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let client = Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(dir.join("store"), None)
        .build()
        .await?;
    Ok(client)
}

/// `MatrixSession` (unlike older matrix-sdk versions) no longer carries the
/// homeserver URL, so it's stashed alongside the session tokens here.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredSession {
    homeserver: String,
    session: matrix_sdk::matrix_auth::MatrixSession,
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
    let state = Arc::new(Mutex::new(WorkerState {
        client: None,
        room_timelines: HashMap::new(),
        thread_timelines: HashMap::new(),
        thread_reply_cursors: HashMap::new(),
    }));

    while let Some(cmd) = rx.recv().await {
        let state = state.clone();
        let tx = tx.clone();

        match &cmd {
            Command::FetchImage { .. } | Command::PlayVideo { .. } => {
                let cmd_for_task = cmd;
                image_runtime().spawn(async move {
                    if let Err(err) = handle(cmd_for_task, state, tx.clone()).await {
                        let _ = tx.send(Event::Error(err.to_string()));
                    }
                });
            }
            Command::LoadTimeline { .. }
            | Command::PaginateBack { .. }
            | Command::LoadThread { .. }
            | Command::LoadMoreThreadReplies { .. }
            | Command::ListThreads { .. } => {
                let cmd_for_task = cmd;
                timeline_runtime().spawn(async move {
                    if let Err(err) = handle(cmd_for_task, state, tx.clone()).await {
                        let _ = tx.send(Event::Error(err.to_string()));
                    }
                });
            }
            _ => {
                tokio::spawn(async move {
                    if let Err(err) = handle(cmd, state, tx.clone()).await {
                        let _ = tx.send(Event::Error(err.to_string()));
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
            if !path.exists() {
                tx.send(Event::SessionChecked(false)).ok();
                return Ok(());
            }
            let raw = std::fs::read_to_string(&path)?;
            let stored: StoredSession = serde_json::from_str(&raw)?;
            let client = build_client(&stored.homeserver).await?;
            client.restore_session(stored.session).await?;
            state.lock().await.client = Some(client);
            tx.send(Event::SessionChecked(true)).ok();
        }

        Command::LoginPassword {
            homeserver,
            username,
            password,
        } => match build_client(&homeserver).await {
            Ok(client) => {
                match client
                    .matrix_auth()
                    .login_username(&username, &password)
                    .initial_device_display_name("Matrix egui Client")
                    .send()
                    .await
                {
                    Ok(_) => {
                        let _ = persist_session(&client, &homeserver);
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
        },

        Command::LoginOAuth { homeserver } => {
            match build_client(&homeserver).await {
                Ok(client) => {
                    use matrix_sdk::ruma::api::client::session::get_login_types::v3::LoginType;

                    match client.matrix_auth().get_login_types().await {
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

                            match client
                                .matrix_auth()
                                .get_sso_login_url(redirect_url.as_str(), None)
                                .await
                            {
                                Ok(sso_url) => {
                                    if let Err(e) = open::that(&sso_url) {
                                        tx.send(Event::LoginError(format!(
                                            "failed to open browser: {e}"
                                        )))
                                        .ok();
                                        return Ok(());
                                    }

                                    match crate::matrix::oidc_callback::wait_for_token().await {
                                        Ok(login_token) => {
                                            match client
                                                .matrix_auth()
                                                .login_token(&login_token)
                                                .initial_device_display_name(
                                                    "Matrix egui Client",
                                                )
                                                .send()
                                                .await
                                            {
                                                Ok(_) => {
                                                    let _ = persist_session(&client, &homeserver);
                                                    state.lock().await.client = Some(client);
                                                    tx.send(Event::LoggedIn).ok();
                                                }
                                                Err(e) => {
                                                    tx.send(Event::LoginError(e.to_string()))
                                                        .ok();
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
                Err(e) => {
                    tx.send(Event::LoginError(e.to_string())).ok();
                }
            }
        }

        Command::StartSync => {
            let client = get_client(&state).await?;
            register_new_message_handler(&client, tx.clone());
            register_redaction_handler(&client, tx.clone());
            crate::matrix::verification::register_verification_handler(&client, tx.clone());

            tracing::info!("starting sync loop");

            let tx2 = tx.clone();
            let tx3 = tx.clone();
            let state2 = state.clone();
            tokio::spawn(async move {
                let tick = std::sync::atomic::AtomicU64::new(0);
                let result = client
                    .sync_with_callback(SyncSettings::default(), move |resp| {
                        let tx2 = tx2.clone();
                        let state2 = state2.clone();
                        let tick = tick.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        tracing::debug!(
                            tick,
                            joined = resp.rooms.join.len(),
                            left = resp.rooms.leave.len(),
                            invited = resp.rooms.invite.len(),
                            to_device = resp.to_device.len(),
                            "sync response received"
                        );
                        async move {
                            if let Err(e) = refresh_rooms(&state2, &tx2).await {
                                tracing::warn!(error = %e, "refresh_rooms failed after sync tick");
                            }
                            matrix_sdk::LoopCtrl::Continue
                        }
                    })
                    .await;
                if let Err(e) = result {
                    tracing::error!(error = %e, "sync loop ended");
                    let _ = tx3.send(Event::Error(format!("sync ended: {e}")));
                }
            });

            // Kick an initial room list refresh immediately after starting.
            refresh_rooms(&state, &tx).await?;
        }

        Command::LoadTimeline { room_id } => {
            let client = get_client(&state).await?;
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
                    .await,
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
            if let Err(e) = timeline
                .paginate_backwards(matrix_sdk_ui::timeline::PaginationOptions::until_num_items(
                    20, 20,
                ))
                .await
            {
                tracing::warn!(error = %e, room_id, "initial backward pagination failed");
            }

            let items = timeline.items().await;
            let events = convert_items(&client, &items).await;
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
            // `until_num_items`, not `simple_request` — since the main
            // timeline's event_filter (see `Command::LoadTimeline`) drops
            // thread replies, a single `simple_request` batch of raw
            // server events can filter down to barely any actual timeline
            // items in a room with heavy thread traffic (one new message
            // shown per "load more" instead of ~20). `until_num_items`
            // keeps fetching further batches until 20 items actually make
            // it through the filter, or the room's history is exhausted.
            if let Err(e) = timeline
                .paginate_backwards(matrix_sdk_ui::timeline::PaginationOptions::until_num_items(
                    20, 20,
                ))
                .await
            {
                tracing::warn!(error = %e, room_id, "PaginateBack failed");
                tx.send(Event::TimelinePrepend {
                    room_id,
                    events: Vec::new(),
                    reached_start: false,
                })
                .ok();
                return Ok(());
            }

            let reached_start = timeline.back_pagination_status().get()
                == matrix_sdk_ui::timeline::BackPaginationStatus::TimelineStartReached;
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
            let mut events = Vec::new();
            if let Some(root) =
                fetch_and_parse_event(&client, &room_id, &root_event_id).await
            {
                events.push(root);
            }
            let (replies, next_batch) =
                fetch_thread_replies_page(&client, &room, &room_id, &root_event_id, None).await;
            events.extend(replies);

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
                None => Arc::new(room.timeline().await),
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
            match list_all_threads(&client, &room_id).await {
                Ok(threads) => {
                    tx.send(Event::ThreadsList { room_id, threads }).ok();
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
                .map(|m| (m.user_id().to_string(), m.name().to_string()))
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
            use matrix_sdk::ruma::events::relation::{InReplyTo, Thread};
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
                (None, Some(reply_id)) => Some(Relation::Reply {
                    in_reply_to: InReplyTo::new(reply_id),
                }),
                (None, None) => None,
            };

            // `room.send()` directly rather than going through a cached
            // `Timeline`'s `.send()` — that returned `()`, giving the UI no
            // way to know whether the send actually succeeded or to clear
            // a "sending…" indicator; this gives back the real event ID (or
            // an error) as soon as the HTTP round trip completes, without
            // waiting on a `/sync` echo.
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

            let upload = match client.media().upload(&content_type, bytes).await {
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
                content.relates_to = Some(matrix_sdk::ruma::events::room::message::Relation::Thread(
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

        Command::Summarize { room_id } => {
            let client = get_client(&state).await?;
            let timeline = state
                .lock()
                .await
                .room_timelines
                .get(&room_id)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("timeline not loaded"))?;

            let items = timeline.items().await;
            let events = convert_items(&client, &items).await;
            let transcript = events
                .iter()
                .map(|e| format!("{}: {}", e.sender_name, e.body))
                .collect::<Vec<_>>()
                .join("\n");

            let api_key = std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY not set"))?;

            let http = reqwest::Client::new();
            let response = http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&serde_json::json!({
                    "model": "claude-sonnet-4-6",
                    "max_tokens": 400,
                    "messages": [{
                        "role": "user",
                        "content": format!(
                            "Summarize the key points and action items of this chat transcript, in a few sentences:\n\n{transcript}"
                        )
                    }]
                }))
                .send()
                .await?;

            let body: serde_json::Value = response.json().await?;
            let text = body["content"]
                .as_array()
                .and_then(|blocks| blocks.iter().find_map(|b| b["text"].as_str()))
                .unwrap_or("(no summary returned)")
                .to_string();

            tx.send(Event::Summary { room_id, text }).ok();
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

            let new_content = match html_body {
                Some(html) => RoomMessageEventContent::text_html(body.clone(), html),
                None => RoomMessageEventContent::text_plain(body.clone()),
            };
            let mut edit = new_content.make_replacement(
                ReplacementMetadata::new(target, None),
                None,
            );
            if !mentions.is_empty() {
                let user_ids: std::collections::BTreeSet<_> = mentions
                    .iter()
                    .filter_map(|m| matrix_sdk::ruma::OwnedUserId::try_from(m.as_str()).ok())
                    .collect();
                if !user_ids.is_empty() {
                    edit = edit
                        .add_mentions(matrix_sdk::ruma::events::Mentions::with_user_ids(user_ids));
                }
            }

            room.send(edit).await?;

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
            refresh_rooms(&state, &tx).await?;
        }

        Command::DeclineInvite { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            room.leave().await?;
            refresh_rooms(&state, &tx).await?;
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
            refresh_rooms(&state, &tx).await?;
        }

        Command::MarkRoomRead { room_id } => {
            let client = get_client(&state).await?;
            let room = client
                .get_room(RoomId::parse(&room_id)?.as_ref())
                .ok_or_else(|| anyhow::anyhow!("room not found"))?;
            if let Some(latest) = room.latest_event() {
                if let Some(event_id) = latest.event_id() {
                    use matrix_sdk::ruma::api::client::receipt::create_receipt::v3::ReceiptType;
                    use matrix_sdk::ruma::events::receipt::ReceiptThread;
                    room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id)
                        .await?;
                }
            }
            refresh_rooms(&state, &tx).await?;
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
                request.creation_content = Some(matrix_sdk::ruma::serde::Raw::new(
                    &serde_json::json!({ "type": "m.space" }),
                )?
                .cast());
            }

            client.create_room(request).await?;
            refresh_rooms(&state, &tx).await?;
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
                                room.is_encrypted().await.unwrap_or(false),
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

        Command::CloseRoomTimeline { room_id } => {
            let mut guard = state.lock().await;
            guard.room_timelines.remove(&room_id);
            // Also drop any open thread timelines that belonged to this room.
            guard
                .thread_timelines
                .retain(|(rid, _), _| rid != &room_id);
        }

        Command::PlayVideo { mxc_uri, filename } => {
            let client = get_client(&state).await?;
            match download_media_bytes(&client, &mxc_uri).await {
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
                    } else if let Err(e) = open::that(&path) {
                        tx.send(Event::Error(format!(
                            "failed to open video player: {e}"
                        )))
                        .ok();
                    }
                }
                Err(e) => {
                    tx.send(Event::Error(format!("video download failed: {e}")))
                        .ok();
                }
            }
        }

        Command::FetchImage { key, mxc_uri } => {
            let client = get_client(&state).await?;
            match download_media_bytes(&client, &mxc_uri).await {
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
                    tx.send(Event::Error(format!("image fetch failed: {e}")))
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
    }

    Ok(())
}

async fn get_client(state: &Arc<Mutex<WorkerState>>) -> anyhow::Result<Client> {
    state
        .lock()
        .await
        .client
        .clone()
        .ok_or_else(|| anyhow::anyhow!("not logged in"))
}

async fn refresh_rooms(
    state: &Arc<Mutex<WorkerState>>,
    tx: &UnboundedSender<Event>,
) -> anyhow::Result<()> {
    let client = get_client(state).await?;
    let mut summaries = Vec::new();

    for room in client.rooms() {
        let is_invite = match room.state() {
            matrix_sdk::RoomState::Joined => false,
            matrix_sdk::RoomState::Invited => true,
            _ => continue, // left/banned rooms: skip
        };

        let name = room
            .display_name()
            .await
            .map(|n| n.to_string())
            .unwrap_or_else(|_| room.room_id().to_string());

        let latest = room.latest_event();
        let (last_message, last_message_ts) = match &latest {
            Some(event) => {
                let value: serde_json::Value =
                    event.event().event.deserialize_as().unwrap_or_default();
                let body = value
                    .pointer("/content/body")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let ts = value
                    .get("origin_server_ts")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                (body, ts)
            }
            None => (None, 0),
        };

        summaries.push(RoomSummary {
            room_id: room.room_id().to_string(),
            name,
            last_message,
            last_message_ts,
            unread_count: room.num_unread_messages(),
            is_encrypted: room.is_encrypted().await.unwrap_or(false),
            is_invite,
            is_space: room.is_space(),
        });
    }

    // Invites first (need action), then by recent activity.
    summaries.sort_by(|a, b| {
        b.is_invite
            .cmp(&a.is_invite)
            .then(b.last_message_ts.cmp(&a.last_message_ts))
    });
    tx.send(Event::Rooms(summaries)).ok();
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

fn notification_mode_to_str(mode: matrix_sdk::notification_settings::RoomNotificationMode) -> &'static str {
    use matrix_sdk::notification_settings::RoomNotificationMode;
    match mode {
        RoomNotificationMode::AllMessages => "all",
        RoomNotificationMode::MentionsAndKeywordsOnly => "mentions",
        RoomNotificationMode::Mute => "mute",
    }
}

fn notification_mode_from_str(mode: &str) -> matrix_sdk::notification_settings::RoomNotificationMode {
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
async fn list_all_threads(
    client: &Client,
    room_id: &str,
) -> anyhow::Result<Vec<crate::models::TimelineEvent>> {
    use matrix_sdk::ruma::api::client::threads::get_threads;
    use matrix_sdk::ruma::UInt;

    let parsed_room_id = RoomId::parse(room_id)?;
    let mut all = Vec::new();
    let mut from: Option<String> = None;

    loop {
        let mut request = get_threads::v1::Request::new(parsed_room_id.clone());
        request.from = from.clone();
        request.limit = Some(UInt::from(50u32));
        request.include = get_threads::v1::IncludeThreads::All;

        let response = client.send(request, None).await?;

        for raw in &response.chunk {
            if let Some(event) = parse_thread_root(client, room_id, raw).await {
                all.push(event);
            }
        }

        match response.next_batch {
            Some(next) if !next.is_empty() => from = Some(next),
            _ => break,
        }
    }

    async fn parse_thread_root(
        client: &Client,
        room_id: &str,
        raw: &matrix_sdk::ruma::serde::Raw<matrix_sdk::ruma::events::AnyTimelineEvent>,
    ) -> Option<crate::models::TimelineEvent> {
        let value: serde_json::Value = raw.deserialize_as().ok()?;
        let mut event = parse_raw_message_event(client, room_id, &value).await?;
        // Thread roots should always show a reply count button, even at 0.
        event.thread_count = event.thread_count.or(Some(0));
        Some(event)
    }

    all.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    Ok(all)
}

/// Fetches a single event by ID and parses it the same way as any other
/// raw timeline event — used to get a thread's root message when it's not
/// necessarily present in whatever timeline the room list already loaded.
async fn fetch_and_parse_event(
    client: &Client,
    room_id: &str,
    event_id: &matrix_sdk::ruma::EventId,
) -> Option<crate::models::TimelineEvent> {
    let room = client.get_room(RoomId::parse(room_id).ok()?.as_ref())?;
    let event = room.event(event_id).await.ok()?;
    let value: serde_json::Value = event.event.deserialize_as().ok()?;
    parse_raw_message_event(client, room_id, &value).await
}

/// Fetches every reply in one thread via `/relations` (see `LoadThread`'s
/// comment for why this beats filtering+paginating a live `Timeline`).
/// Returned in chronological order. Does not include the thread root
/// itself — callers that want it should also call `fetch_and_parse_event`.
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
    use matrix_sdk::ruma::api::client::relations::get_relating_events_with_rel_type;
    use matrix_sdk::ruma::events::relation::RelationType;

    let mut request = get_relating_events_with_rel_type::v1::Request::new(
        room.room_id().to_owned(),
        root_event_id.to_owned(),
        RelationType::Thread,
    );
    request.from = from;
    request.limit = Some(matrix_sdk::ruma::UInt::from(20u32));

    let response = match client.send(request, None).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, room_id, %root_event_id, "fetching thread relations failed");
            return (Vec::new(), None);
        }
    };

    let mut replies = Vec::new();
    for raw in &response.chunk {
        if let Ok(value) = raw.deserialize_as::<serde_json::Value>() {
            if let Some(event) = parse_raw_message_event(client, room_id, &value).await {
                replies.push(event);
            }
        }
    }
    replies.reverse();

    let next_batch = response.next_batch.filter(|n| !n.is_empty());
    (replies, next_batch)
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
    let msgtype = content.and_then(|c| c.get("msgtype")).and_then(|v| v.as_str());
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

    let msg_type = match (msgtype, &media_url) {
        (Some("m.notice"), _) => "notice".to_string(),
        (_, Some(_)) => "file".to_string(), // covers m.image/m.video/m.file alike; UI renders by extension/mimetype
        (Some(t), None) if t == "m.image" || t == "m.video" || t == "m.file" => {
            tracing::debug!(msgtype = t, body, "thread event: no mxc found in raw content");
            "text".to_string()
        }
        _ => "text".to_string(),
    };

    let thread_count = value
        .pointer("/unsigned/m.relations/m.thread/count")
        .and_then(|v| v.as_u64());

    // Same "not the thread's own bookkeeping fallback" check as
    // `convert.rs`'s `reply_to_event_id_from_raw` — see its comment.
    let reply_to_event_id = content.and_then(|c| c.get("m.relates_to")).and_then(|relates_to| {
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

    let sender_name = match matrix_sdk::ruma::OwnedUserId::try_from(sender_str.as_str()) {
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
                .unwrap_or_else(|| sender_str.clone()),
            None => sender_str.clone(),
        },
        Err(_) => sender_str.clone(),
    };

    Some(crate::models::TimelineEvent {
        event_id,
        sender: sender_str,
        sender_name,
        body,
        msg_type,
        media_url,
        thumbnail_url: None,
        timestamp,
        thread_count,
        is_own,
        reply_to_event_id,
        reply_to_preview: None,
    })
}

/// Shared by `Command::FetchImage` and `Command::PlayVideo`.
///
/// Deliberately bypasses `client.media()`: that goes through the legacy
/// unauthenticated `/_matrix/media/v3/download/...` endpoint, which Synapse
/// 1.146+ (per MSC3916) now answers with a plain 404 M_NOT_FOUND — matrix-sdk
/// 0.7 predates MSC3916 support, so there's no SDK method for the new
/// `/_matrix/client/v1/media/download/...` endpoint yet. Fetch it directly
/// with the session's own access token instead.
async fn download_media_bytes(client: &Client, mxc_uri: &str) -> anyhow::Result<Vec<u8>> {
    let mxc = matrix_sdk::ruma::OwnedMxcUri::from(mxc_uri.to_string());
    let (server_name, media_id) = mxc.parts()?;
    let token = client
        .access_token()
        .ok_or_else(|| anyhow::anyhow!("not logged in"))?;

    let url = format!(
        "{}_matrix/client/v1/media/download/{server_name}/{media_id}",
        client.homeserver()
    );

    let response = reqwest::Client::new()
        .get(&url)
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;

    Ok(response.bytes().await?.to_vec())
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

                // A plain reply (not a thread one — those are excluded
                // above) — `is_falling_back` doesn't apply here, `Reply`
                // only exists when it's a genuine reply.
                let reply_to_event_id = match &ev.content.relates_to {
                    Some(Relation::Reply { in_reply_to }) => Some(in_reply_to.event_id.to_string()),
                    _ => None,
                };

                let sender = ev.sender.to_string();
                let is_own = client
                    .user_id()
                    .map(|id| id == ev.sender)
                    .unwrap_or(false);

                // Without this, every live message showed the sender's raw
                // `@user:server` ID instead of their display name — the
                // other conversion paths (`convert.rs`, `parse_raw_message_event`)
                // already resolve this, this one just never did.
                let sender_name = room
                    .get_member(&ev.sender)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|m| m.display_name().map(|n| n.to_string()))
                    .unwrap_or_else(|| sender.clone());

                let (body, msg_type, media_url, thumbnail_url) =
                    crate::matrix::convert::message_type_fields(&ev.content.msgtype);

                let event = crate::models::TimelineEvent {
                    event_id: ev.event_id.to_string(),
                    sender,
                    sender_name,
                    body,
                    msg_type,
                    media_url,
                    thumbnail_url,
                    timestamp: ev.origin_server_ts.0.into(),
                    reply_to_event_id,
                    reply_to_preview: None,
                    thread_count: None,
                    is_own,
                };

                if let Some(thread_root_id) = thread_root_id {
                    tx.send(Event::ThreadReply {
                        room_id,
                        thread_root_id,
                        event,
                    })
                    .ok();
                } else {
                    tx.send(Event::NewMessage { room_id, event }).ok();
                }
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
