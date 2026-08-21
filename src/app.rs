use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use eframe::egui;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::command::Command;
use crate::event::Event;
use crate::models::{RoomSummary, TimelineEvent};

#[derive(Clone, Copy, PartialEq)]
pub enum Screen {
    CheckingSession,
    Login,
    Chat,
}

/// Caps for the UI-side caches — see `image_lru` / `room_lru` fields below.
const MAX_CACHED_ROOMS: usize = 15;
const MAX_CACHED_IMAGES: usize = 80;

/// Whatever's shown in the right-hand panel: nothing, the list of threads
/// in the room, or one specific thread's timeline.
pub enum RightPanel {
    None,
    /// `Some(room_id)` for the per-room threads button — scoped to just
    /// that room. `None` for the global threads button — one list across
    /// every joined room.
    ThreadsList(Option<String>),
    Thread {
        root: TimelineEvent,
        events: Vec<TimelineEvent>,
    },
}

/// An image picked from disk or pasted from the clipboard, staged for the
/// user to confirm or cancel before it's actually uploaded and sent — see
/// `App::pick_image` / `paste_image_from_clipboard` /
/// `confirm_pending_image` / `cancel_pending_image`.
pub struct PendingImage {
    pub texture: egui::TextureHandle,
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime: String,
    pub room_id: String,
    pub thread_id: Option<String>,
}

/// A message currently loaded into a compose box for editing — see
/// `App::start_edit` / `cancel_edit` / `is_editing`.
struct PendingEdit {
    room_id: String,
    event_id: String,
    thread_id: Option<String>,
}

/// A message queued up to quote-reply to — see `App::start_reply` /
/// `cancel_reply` / `replying_to_preview`. Unlike `PendingEdit` this
/// doesn't touch the compose text at all; it's a plain "m.in_reply_to"
/// quote-reply (or, inside a thread, `Thread::reply` — a genuine in-thread
/// reply, not the automatic fallback every thread reply also carries).
struct PendingReply {
    room_id: String,
    event_id: String,
    thread_id: Option<String>,
    /// "Sender: body snippet", shown above the compose box.
    preview: String,
}

pub struct App {
    pub tx: UnboundedSender<Command>,
    pub rx: UnboundedReceiver<Event>,

    pub screen: Screen,

    // Login form fields
    pub homeserver: String,
    pub username: String,
    pub password: String,
    pub login_error: Option<String>,
    pub logging_in: bool,

    // Chat state
    pub rooms: Vec<RoomSummary>,
    pub room_filter: String,
    pub selected_room: Option<String>,
    pub timelines: HashMap<String, Vec<TimelineEvent>>,
    pub compose_text: String,
    /// room_id -> when its last live `Event::NewMessage` arrived, purely
    /// for the brief "●" activity flash in the room list — see
    /// `room_activity_flash`. Not an unread/notification count (that's
    /// `RoomSummary::unread_count`, sourced from `refresh_rooms`).
    recent_activity: HashMap<String, std::time::Instant>,
    /// Rooms with a `Command::PaginateBack` currently in flight — guards
    /// against firing another one every frame while the scroll position
    /// stays near the top waiting on the first to come back.
    pagination_in_flight: HashSet<String>,
    /// Rooms whose full history has already been loaded — once set, the
    /// UI stops asking for more when scrolled near the top.
    pagination_reached_start: HashSet<String>,
    /// Rooms that just loaded an older batch and haven't been scrolled
    /// back away from the top since. Prepending older messages doesn't
    /// adjust the scroll offset to compensate for the new content's
    /// height, so without this the view stays sitting at "near the top"
    /// right after a load completes and would otherwise immediately fire
    /// another `PaginateBack`, cascading into loading batch after batch
    /// with no further scrolling from the user. Cleared once the scroll
    /// offset moves back out past `PAGINATE_REARM_PX` — see
    /// `maybe_paginate_back`.
    pagination_cooldown: HashSet<String>,

    /// Same three guards as `pagination_in_flight`/`_reached_start`/
    /// `_cooldown`, but for `Command::LoadMoreThreadReplies` in the thread
    /// panel — keyed by `(room_id, thread_root_id)`.
    thread_pagination_in_flight: HashSet<(String, String)>,
    thread_pagination_reached_start: HashSet<(String, String)>,
    thread_pagination_cooldown: HashSet<(String, String)>,

    /// room_id -> (user_id, display_name) pairs, fetched once per room for
    /// the @mention autocomplete in the compose box.
    pub room_members: HashMap<String, Vec<(String, String)>>,

    /// Image picked/pasted but not yet confirmed for sending — see
    /// `PendingImage`.
    pub pending_image: Option<PendingImage>,

    /// Shared cache `egui_commonmark` uses across frames (parsed
    /// image/link state) — one per app, not per message, otherwise every
    /// message would reparse and re-fetch on every frame.
    pub markdown_cache: egui_commonmark::CommonMarkCache,

    /// Currently selected Space (`room_id`) to filter the room list by —
    /// `None` means "all rooms", no filtering.
    pub selected_space: Option<String>,
    /// space_room_id -> its child room IDs (`m.space.child`), fetched on
    /// demand when a space is first selected.
    pub space_children: HashMap<String, Vec<String>>,
    /// room_id -> `"all"` | `"mentions"` | `"mute"`, fetched on demand.
    pub room_notification_modes: HashMap<String, String>,
    /// Guards `notification_mode` against re-sending `GetNotificationMode`
    /// every frame while the first fetch for a room is still in flight.
    notification_mode_requested: HashSet<String>,

    /// "Create room" dialog state.
    pub show_create_room_dialog: bool,
    pub create_room_name: String,
    pub create_room_is_public: bool,
    pub create_room_is_space: bool,

    /// Message currently loaded into a compose box for editing, if any.
    editing_message: Option<PendingEdit>,
    /// Message currently queued for quote-reply, if any.
    replying_to: Option<PendingReply>,

    /// Counter for generating `Command::SendMessage`/`SendImage`'s
    /// `local_id` — just needs to be unique per in-flight send, nothing
    /// fancier.
    next_local_id: u64,
    /// Number of sends currently in flight for the main compose box / the
    /// open thread's compose box, respectively — drives the "sending…"
    /// indicator. A count rather than a bool so two rapid sends in the same
    /// box don't have the second one's completion clear the indicator for
    /// the first one still in flight.
    main_send_in_flight: u32,
    thread_send_in_flight: u32,

    pub right_panel: RightPanel,
    /// room_id -> that room's threads, populated by `open_global_threads`
    /// issuing one `Command::ListThreads` per joined room. The threads
    /// popup is global — one view across every room, Element-style — not
    /// scoped to whichever room happens to be selected.
    pub threads_by_room: HashMap<String, Vec<TimelineEvent>>,
    pub thread_compose_text: String,
    /// (room_id, thread_root_event_id) pairs with a reply that arrived via
    /// live `/sync` while that thread wasn't the one open in
    /// `right_panel` — drives the global unread-thread dot on the
    /// "[ threads ]" button and in the threads list. Cleared per-thread
    /// when the user actually opens it (`open_thread`).
    pub unread_threads: HashSet<(String, String)>,

    pub summaries: HashMap<String, String>,
    pub summarizing: bool,

    pub status_line: Option<String>,

    // Security / E2E
    pub show_security_panel: bool,
    pub verification_emojis: Option<Vec<(String, String)>>,
    pub recovery_key_input: String,
    pub security_status: Option<String>,

    /// event_id -> decoded GPU texture, fetched with auth via
    /// `Command::FetchImage`. Decoded once (off the `bytes://` URI loader
    /// chain entirely — see `apply_event`'s `Event::ImageBytes` arm for
    /// why) into a texture the UI can render directly.
    pub image_cache: HashMap<String, egui::TextureHandle>,
    pub image_requested: HashSet<String>,
    /// Most-recently-used order for `image_cache`, back = most recent.
    /// Evicted (LRU) once len() exceeds MAX_CACHED_IMAGES.
    image_lru: VecDeque<String>,

    /// Most-recently-used order for `timelines`, back = most recent.
    /// Evicted (LRU) once len() exceeds MAX_CACHED_ROOMS — the evicted
    /// room's messages are dropped from RAM (worker's Timeline object is
    /// closed too via Command::CloseRoomTimeline), but nothing is lost:
    /// re-opening the room just reloads from the on-disk sqlite store.
    room_lru: VecDeque<String>,

    /// Mirrors "there's an unhandled `Event::Error`" for the tray icon to
    /// read from its own thread — see `tray_error_handle` and `tray.rs`.
    /// Set on any `Event::Error`, cleared on the next sign of a healthy
    /// sync loop (`Event::Rooms`/`Event::LoggedIn`).
    tray_error: Arc<AtomicBool>,
}

impl App {
    pub fn new(tx: UnboundedSender<Command>, rx: UnboundedReceiver<Event>) -> Self {
        tx.send(Command::CheckSession).ok();
        Self {
            tx,
            rx,
            screen: Screen::CheckingSession,
            homeserver: "https://matrix.org".to_string(),
            username: String::new(),
            password: String::new(),
            login_error: None,
            logging_in: false,
            rooms: Vec::new(),
            room_filter: String::new(),
            selected_room: None,
            timelines: HashMap::new(),
            compose_text: String::new(),
            recent_activity: HashMap::new(),
            pagination_in_flight: HashSet::new(),
            pagination_reached_start: HashSet::new(),
            pagination_cooldown: HashSet::new(),
            thread_pagination_in_flight: HashSet::new(),
            thread_pagination_reached_start: HashSet::new(),
            thread_pagination_cooldown: HashSet::new(),
            room_members: HashMap::new(),
            pending_image: None,
            markdown_cache: egui_commonmark::CommonMarkCache::default(),
            selected_space: None,
            space_children: HashMap::new(),
            room_notification_modes: HashMap::new(),
            notification_mode_requested: HashSet::new(),
            show_create_room_dialog: false,
            create_room_name: String::new(),
            create_room_is_public: false,
            create_room_is_space: false,
            editing_message: None,
            replying_to: None,
            next_local_id: 0,
            main_send_in_flight: 0,
            thread_send_in_flight: 0,
            right_panel: RightPanel::None,
            threads_by_room: HashMap::new(),
            unread_threads: HashSet::new(),
            thread_compose_text: String::new(),
            summaries: HashMap::new(),
            summarizing: false,
            status_line: None,
            show_security_panel: false,
            verification_emojis: None,
            recovery_key_input: String::new(),
            security_status: None,
            image_cache: HashMap::new(),
            image_requested: HashSet::new(),
            image_lru: VecDeque::new(),
            room_lru: VecDeque::new(),
            tray_error: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Handle to the error flag the tray icon reads from its own thread —
    /// grab this once right after `App::new` and hand it to `tray::run`.
    pub fn tray_error_handle(&self) -> Arc<AtomicBool> {
        self.tray_error.clone()
    }

    /// Drains every pending event from the worker without blocking. Called
    /// once at the top of every `update()` frame.
    pub fn poll_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.rx.try_recv() {
            self.apply_event(ctx, event);
        }
    }

    /// Applies an edit/delete to a message wherever it's currently
    /// displayed — the main timeline, the open thread panel (both its root
    /// and its replies), and the threads list. A message can legitimately
    /// appear in more than one of these at once (a thread root shows in
    /// both the main timeline and the thread panel), so this doesn't stop
    /// at the first match.
    fn update_event_in_place(
        &mut self,
        room_id: &str,
        event_id: &str,
        f: impl Fn(&mut TimelineEvent),
    ) {
        if let Some(events) = self.timelines.get_mut(room_id) {
            if let Some(e) = events.iter_mut().find(|e| e.event_id == event_id) {
                f(e);
            }
        }
        for thread in self.threads_by_room.values_mut().flatten() {
            if thread.event_id == event_id {
                f(thread);
            }
        }
        if let RightPanel::Thread { root, events } = &mut self.right_panel {
            if root.event_id == event_id {
                f(root);
            }
            if let Some(e) = events.iter_mut().find(|e| e.event_id == event_id) {
                f(e);
            }
        }
    }

    /// Looks for an already-loaded message by event ID — same search scope
    /// as `update_event_in_place`, read-only. Used to build a reply-quote
    /// preview locally for messages where the worker didn't already
    /// resolve one (e.g. a live `/sync` message — see `TimelineEvent`'s
    /// `reply_to_preview` doc), instead of leaving it as a bare "replying
    /// to a message" when the target is right there on screen.
    pub fn find_event(&self, room_id: &str, event_id: &str) -> Option<&TimelineEvent> {
        if let Some(events) = self.timelines.get(room_id) {
            if let Some(e) = events.iter().find(|e| e.event_id == event_id) {
                return Some(e);
            }
        }
        if let RightPanel::Thread { root, events } = &self.right_panel {
            if root.event_id == event_id {
                return Some(root);
            }
            if let Some(e) = events.iter().find(|e| e.event_id == event_id) {
                return Some(e);
            }
        }
        self.threads_by_room
            .values()
            .flatten()
            .find(|t| t.event_id == event_id)
    }

    /// Fires a desktop notification for an incoming message, unless it's
    /// our own message or the room is the one currently open on screen
    /// (avoids notifying about something already visible).
    fn maybe_notify(&self, room_id: &str, event: &crate::models::TimelineEvent) {
        if event.is_own {
            return;
        }
        if self.selected_room.as_deref() == Some(room_id) {
            return;
        }

        let room_name = self
            .rooms
            .iter()
            .find(|r| r.room_id == room_id)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| room_id.to_string());

        let summary = format!("{} — {}", event.sender_name, room_name);
        let body = if event.msg_type == "image" {
            format!("📷 {}", event.body)
        } else {
            event.body.clone()
        };

        crate::notify::notify(&summary, &body);
    }

    fn apply_event(&mut self, ctx: &egui::Context, event: Event) {
        match event {
            Event::SessionChecked(true) => {
                self.screen = Screen::Chat;
                self.tx.send(Command::StartSync).ok();
            }
            Event::SessionChecked(false) => {
                self.screen = Screen::Login;
            }
            Event::LoggedIn => {
                self.logging_in = false;
                self.login_error = None;
                self.screen = Screen::Chat;
                self.tx.send(Command::StartSync).ok();
                self.tray_error.store(false, Ordering::SeqCst);
            }
            Event::LoginError(msg) => {
                self.logging_in = false;
                self.login_error = Some(msg);
            }
            Event::Rooms(rooms) => {
                self.rooms = rooms;
                // A room-list refresh only ever arrives after a sync tick
                // completed successfully — the clearest "we're actually
                // connected" signal available, so use it to clear the tray
                // icon's error state.
                self.tray_error.store(false, Ordering::SeqCst);
            }
            Event::Timeline { room_id, events } => {
                self.timelines.insert(room_id, events);
            }
            Event::TimelinePrepend {
                room_id,
                events,
                reached_start,
            } => {
                self.pagination_in_flight.remove(&room_id);
                if reached_start {
                    self.pagination_reached_start.insert(room_id.clone());
                }
                // `events` here is the *complete* current set from the
                // worker's `Timeline` (old items plus newly paginated-in
                // older ones), not just the new slice — replace outright.
                // (Treating it as "only the new part" and appending the
                // previous list after it used to duplicate every
                // already-shown message on each pagination.)
                self.timelines.insert(room_id, events);
            }
            Event::MessageDeleted { room_id, event_id } => {
                self.update_event_in_place(&room_id, &event_id, |e| {
                    e.body = "[message removed]".to_string();
                    e.msg_type = "notice".to_string();
                    e.media_url = None;
                    e.thumbnail_url = None;
                });
            }
            Event::MessageEdited {
                room_id,
                event_id,
                new_body,
            } => {
                self.update_event_in_place(&room_id, &event_id, |e| {
                    e.body = new_body.clone();
                });
            }
            Event::MessageSent { thread_id, .. } => {
                if thread_id.is_some() {
                    self.thread_send_in_flight = self.thread_send_in_flight.saturating_sub(1);
                } else {
                    self.main_send_in_flight = self.main_send_in_flight.saturating_sub(1);
                }
            }
            Event::MessageSendFailed {
                thread_id, error, ..
            } => {
                if thread_id.is_some() {
                    self.thread_send_in_flight = self.thread_send_in_flight.saturating_sub(1);
                } else {
                    self.main_send_in_flight = self.main_send_in_flight.saturating_sub(1);
                }
                self.status_line = Some(format!("send failed: {error}"));
            }
            Event::SpaceChildren {
                space_room_id,
                room_ids,
            } => {
                self.space_children.insert(space_room_id, room_ids);
            }
            Event::NotificationMode { room_id, mode } => {
                self.room_notification_modes.insert(room_id, mode);
            }
            Event::NewMessage { room_id, event } => {
                self.maybe_notify(&room_id, &event);
                // Keep the room marked read while it's the one open and on
                // screen — otherwise a live message landing in the open
                // room would still bump its unread badge right back up.
                if self.selected_room.as_deref() == Some(room_id.as_str()) {
                    self.tx
                        .send(Command::MarkRoomRead {
                            room_id: room_id.clone(),
                        })
                        .ok();
                }
                // Brief per-room flash so incoming /sync data is visible in
                // the room list even for rooms not currently open — see
                // `room_activity_flash` and its use in `ui/room_list.rs`.
                self.recent_activity.insert(room_id.clone(), std::time::Instant::now());
                self.timelines.entry(room_id).or_default().push(event);
            }
            Event::ThreadReply {
                room_id,
                thread_root_id,
                event,
            } => {
                let open_here = matches!(
                    &self.right_panel,
                    RightPanel::Thread { root, .. } if root.event_id == thread_root_id
                );
                if open_here {
                    if let RightPanel::Thread { root, events } = &mut self.right_panel {
                        root.thread_count = Some(root.thread_count.unwrap_or(0) + 1);
                        events.push(event);
                    }
                } else {
                    self.unread_threads
                        .insert((room_id.clone(), thread_root_id.clone()));
                }
                for t in self
                    .threads_by_room
                    .get_mut(&room_id)
                    .into_iter()
                    .flatten()
                    .chain(self.timelines.get_mut(&room_id).into_iter().flatten())
                {
                    if t.event_id == thread_root_id {
                        t.thread_count = Some(t.thread_count.unwrap_or(0) + 1);
                    }
                }
            }
            Event::ThreadEvents {
                room_id,
                thread_root_id,
                events,
            } => {
                // Fresh load (or reload) of this thread — whatever
                // pagination state was left over from a previous visit no
                // longer applies.
                let key = (room_id, thread_root_id.clone());
                self.thread_pagination_in_flight.remove(&key);
                self.thread_pagination_reached_start.remove(&key);
                self.thread_pagination_cooldown.remove(&key);

                if let RightPanel::Thread { root, .. } = &self.right_panel {
                    if root.event_id == thread_root_id {
                        self.right_panel = RightPanel::Thread {
                            root: root.clone(),
                            events,
                        };
                    }
                }
            }
            Event::ThreadEventsPrepend {
                room_id,
                thread_root_id,
                events,
                reached_start,
            } => {
                let key = (room_id, thread_root_id.clone());
                self.thread_pagination_in_flight.remove(&key);
                if reached_start {
                    self.thread_pagination_reached_start.insert(key);
                }
                if let RightPanel::Thread { root, events: existing } = &self.right_panel {
                    if root.event_id == thread_root_id {
                        let mut merged = events;
                        merged.extend(existing.iter().cloned());
                        self.right_panel = RightPanel::Thread {
                            root: root.clone(),
                            events: merged,
                        };
                    }
                }
            }
            Event::ThreadsList { room_id, threads } => {
                if threads.is_empty() {
                    self.threads_by_room.remove(&room_id);
                } else {
                    self.threads_by_room.insert(room_id, threads);
                }
            }
            Event::Members { room_id, members } => {
                self.room_members.insert(room_id, members);
            }
            Event::Summary { room_id, text } => {
                self.summarizing = false;
                self.summaries.insert(room_id, text);
            }
            Event::VerificationEmojis(emojis) => {
                self.verification_emojis = Some(emojis);
            }
            Event::VerificationDone => {
                self.verification_emojis = None;
                self.security_status = Some("device verified".to_string());
            }
            Event::VerificationCancelled(reason) => {
                self.verification_emojis = None;
                self.security_status = Some(format!("verification cancelled: {reason}"));
            }
            Event::RecoveryStatus(msg) => {
                self.security_status = Some(msg);
            }
            Event::ImageBytes { key, bytes } => {
                // Decode and upload to a GPU texture directly here, rather
                // than handing raw bytes to egui's `bytes://` URI loader
                // chain (`ctx.include_bytes` + `egui::Image::new(uri)`) —
                // that chain consistently produced a blank render for some
                // attachments with no visible error, for reasons that
                // didn't reproduce outside egui (bytes decoded fine via
                // this same `image` crate on their own). Decoding and
                // uploading ourselves sidesteps that chain entirely and is
                // the standard pattern for dynamically-loaded images in
                // egui.
                match image::load_from_memory(&bytes) {
                    Ok(img) => {
                        let img = img.to_rgba8();
                        let size = [img.width() as usize, img.height() as usize];
                        let color_image =
                            egui::ColorImage::from_rgba_unmultiplied(size, img.as_flat_samples().as_slice());
                        let texture = ctx.load_texture(&key, color_image, egui::TextureOptions::default());
                        self.image_cache.insert(key, texture);
                    }
                    Err(e) => {
                        tracing::warn!(key, error = %e, "failed to decode fetched image");
                        self.status_line = Some(format!("failed to decode image: {e}"));
                    }
                }
            }
            Event::Error(msg) => {
                if !msg.is_empty() {
                    self.status_line = Some(msg);
                    self.tray_error.store(true, Ordering::SeqCst);
                }
            }
        }
    }

    pub fn select_room(&mut self, room_id: &str) {
        self.selected_room = Some(room_id.to_string());
        self.right_panel = RightPanel::None;
        if !self.timelines.contains_key(room_id) {
            self.tx
                .send(Command::LoadTimeline {
                    room_id: room_id.to_string(),
                })
                .ok();
        }
        if !self.room_members.contains_key(room_id) {
            self.tx
                .send(Command::ListMembers {
                    room_id: room_id.to_string(),
                })
                .ok();
        }
        // Without this, `RoomSummary::unread_count` (client-computed from
        // read receipts) never drops back to zero just from looking at a
        // room — it only reflects receipts actually sent, which nothing
        // did before this.
        self.tx
            .send(Command::MarkRoomRead {
                room_id: room_id.to_string(),
            })
            .ok();
        self.touch_room(room_id);
    }

    /// Called by the timeline UI when the scroll position is near the top.
    /// Requests older messages once per room at a time — a no-op if a
    /// request is already in flight, this room's full history has already
    /// been loaded, or a batch just loaded and the view hasn't been
    /// scrolled away from the top since (see `pagination_cooldown`).
    pub fn maybe_paginate_back(&mut self, room_id: &str) {
        if self.pagination_reached_start.contains(room_id)
            || self.pagination_in_flight.contains(room_id)
            || self.pagination_cooldown.contains(room_id)
        {
            return;
        }
        self.pagination_in_flight.insert(room_id.to_string());
        self.pagination_cooldown.insert(room_id.to_string());
        self.tx
            .send(Command::PaginateBack {
                room_id: room_id.to_string(),
            })
            .ok();
    }

    /// Called by the timeline UI once the scroll offset moves back out past
    /// the "near top" zone — re-arms `maybe_paginate_back` for this room so
    /// the next time the user scrolls back up near the top, it'll load
    /// another batch. Without this re-arm step, the view sitting at "near
    /// top" right after a batch loads (since prepending older messages
    /// doesn't compensate the scroll offset) would otherwise trigger load
    /// after load with no further scrolling from the user.
    pub fn rearm_paginate_back(&mut self, room_id: &str) {
        self.pagination_cooldown.remove(room_id);
    }

    /// True while a `Command::PaginateBack` for this room is in flight —
    /// lets the UI show a "loading older messages..." indicator instead of
    /// silently doing nothing (or, worse, looking exactly like "there's no
    /// more history") while it waits on the response.
    pub fn is_paginating(&self, room_id: &str) -> bool {
        self.pagination_in_flight.contains(room_id)
    }

    /// True once this room's full history has been loaded — lets the UI
    /// show "— beginning of conversation —" instead of a loading spinner
    /// that will never resolve to anything once the user scrolls to the
    /// very top.
    pub fn reached_start(&self, room_id: &str) -> bool {
        self.pagination_reached_start.contains(room_id)
    }

    /// Thread-panel equivalent of `maybe_paginate_back` — requests the
    /// next older page of a thread's replies, subject to the same
    /// in-flight/reached-start/cooldown guards.
    pub fn maybe_load_more_thread_replies(&mut self, room_id: &str, thread_root_id: &str) {
        let key = (room_id.to_string(), thread_root_id.to_string());
        if self.thread_pagination_reached_start.contains(&key)
            || self.thread_pagination_in_flight.contains(&key)
            || self.thread_pagination_cooldown.contains(&key)
        {
            return;
        }
        self.thread_pagination_in_flight.insert(key.clone());
        self.thread_pagination_cooldown.insert(key);
        self.tx
            .send(Command::LoadMoreThreadReplies {
                room_id: room_id.to_string(),
                thread_root_id: thread_root_id.to_string(),
            })
            .ok();
    }

    /// Thread-panel equivalent of `rearm_paginate_back`.
    pub fn rearm_thread_pagination(&mut self, room_id: &str, thread_root_id: &str) {
        self.thread_pagination_cooldown
            .remove(&(room_id.to_string(), thread_root_id.to_string()));
    }

    /// Thread-panel equivalent of `is_paginating`.
    pub fn is_thread_paginating(&self, room_id: &str, thread_root_id: &str) -> bool {
        self.thread_pagination_in_flight
            .contains(&(room_id.to_string(), thread_root_id.to_string()))
    }

    /// Thread-panel equivalent of `reached_start`.
    pub fn thread_reached_start(&self, room_id: &str, thread_root_id: &str) -> bool {
        self.thread_pagination_reached_start
            .contains(&(room_id.to_string(), thread_root_id.to_string()))
    }

    /// True for a brief window after a room last received a live message
    /// via `/sync` — drives the "●" activity flash next to its name in the
    /// room list, so incoming data is visible even for a room that isn't
    /// currently open.
    pub fn room_activity_flash(&self, room_id: &str) -> bool {
        const FLASH_DURATION: std::time::Duration = std::time::Duration::from_millis(1200);
        self.recent_activity
            .get(room_id)
            .is_some_and(|t| t.elapsed() < FLASH_DURATION)
    }

    /// Marks a room as recently used and evicts the least-recently-used
    /// room's cached timeline if we're now over `MAX_CACHED_ROOMS`.
    fn touch_room(&mut self, room_id: &str) {
        self.room_lru.retain(|id| id != room_id);
        self.room_lru.push_back(room_id.to_string());

        while self.room_lru.len() > MAX_CACHED_ROOMS {
            let Some(evicted) = self.room_lru.pop_front() else {
                break;
            };
            // Never evict the room currently open on screen.
            if self.selected_room.as_deref() == Some(evicted.as_str()) {
                self.room_lru.push_back(evicted);
                break;
            }
            self.timelines.remove(&evicted);
            self.summaries.remove(&evicted);
            self.tx
                .send(Command::CloseRoomTimeline { room_id: evicted })
                .ok();
        }
    }

    /// Public wrapper around the LRU touch, for callers that already have
    /// the bytes (cache hit) and just need to mark the entry as recently
    /// used without re-requesting it.
    pub fn touch_image_public(&mut self, event_id: &str, ctx: &egui::Context) {
        self.touch_image(event_id, ctx);
    }

    /// Marks an image as recently used and evicts the least-recently-used
    /// image's texture if we're now over `MAX_CACHED_IMAGES`. Dropping the
    /// `TextureHandle` here releases the GPU texture on its own — no
    /// separate `ctx.forget_image` call needed now that we upload textures
    /// directly instead of going through egui's URI-based image loaders.
    fn touch_image(&mut self, event_id: &str, _ctx: &egui::Context) {
        self.image_lru.retain(|id| id != event_id);
        self.image_lru.push_back(event_id.to_string());

        while self.image_lru.len() > MAX_CACHED_IMAGES {
            let Some(evicted) = self.image_lru.pop_front() else {
                break;
            };
            self.image_cache.remove(&evicted);
            self.image_requested.remove(&evicted);
        }
    }

    pub fn send_message(&mut self, room_id: &str, thread_id: Option<String>) {
        let body = if thread_id.is_some() {
            std::mem::take(&mut self.thread_compose_text)
        } else {
            std::mem::take(&mut self.compose_text)
        };
        let body = body.trim().to_string();
        if body.is_empty() {
            return;
        }

        let (mentions, html_body) = self.resolve_mentions(room_id, &body);

        if let Some(edit) = self.editing_message.take() {
            if edit.room_id == room_id && edit.thread_id == thread_id {
                self.tx
                    .send(Command::EditMessage {
                        room_id: room_id.to_string(),
                        event_id: edit.event_id,
                        body,
                        mentions,
                        html_body,
                    })
                    .ok();
                return;
            }
            // Context mismatch (shouldn't normally happen — the compose
            // box this came from should always match what `start_edit` set
            // up) — put it back rather than silently dropping the pending
            // edit and sending a brand new message instead.
            self.editing_message = Some(edit);
        }

        let reply_to_event_id = match &self.replying_to {
            Some(reply) if reply.room_id == room_id && reply.thread_id == thread_id => {
                self.replying_to.take().map(|r| r.event_id)
            }
            _ => None,
        };

        let local_id = self.new_local_id();
        if thread_id.is_some() {
            self.thread_send_in_flight += 1;
        } else {
            self.main_send_in_flight += 1;
        }

        self.tx
            .send(Command::SendMessage {
                room_id: room_id.to_string(),
                body,
                thread_id,
                mentions,
                html_body,
                local_id,
                reply_to_event_id,
            })
            .ok();
    }

    /// Queues a quote-reply to `event` — the next `send_message` for the
    /// matching compose box (`thread_id`) attaches it as `m.in_reply_to`
    /// (or, inside a thread, `Thread::reply` — a genuine in-thread reply,
    /// distinct from the automatic fallback every thread reply carries).
    /// Unlike `start_edit`, doesn't touch the compose text — the user still
    /// writes a fresh message.
    pub fn start_reply(&mut self, room_id: &str, event: &TimelineEvent, thread_id: Option<String>) {
        self.replying_to = Some(PendingReply {
            room_id: room_id.to_string(),
            event_id: event.event_id.clone(),
            thread_id,
            preview: format!("{}: {}", event.sender_name, event.body),
        });
    }

    /// Cancels a queued quote-reply for the compose box identified by
    /// `thread_id`. A no-op if the queued reply belongs to a different box.
    pub fn cancel_reply(&mut self, thread_id: Option<&str>) {
        if self
            .replying_to
            .as_ref()
            .is_some_and(|r| r.thread_id.as_deref() == thread_id)
        {
            self.replying_to = None;
        }
    }

    /// The queued reply's preview text, if the compose box identified by
    /// `thread_id` has one pending.
    pub fn replying_to_preview(&self, thread_id: Option<&str>) -> Option<&str> {
        self.replying_to
            .as_ref()
            .filter(|r| r.thread_id.as_deref() == thread_id)
            .map(|r| r.preview.as_str())
    }

    fn new_local_id(&mut self) -> String {
        self.next_local_id += 1;
        format!("local-{}", self.next_local_id)
    }

    /// Whether the compose box identified by `thread_id` (`None` for the
    /// main timeline's) has a send in flight — drives a "sending…"
    /// indicator next to it.
    pub fn is_sending(&self, thread_id: Option<&str>) -> bool {
        if thread_id.is_some() {
            self.thread_send_in_flight > 0
        } else {
            self.main_send_in_flight > 0
        }
    }

    /// Loads a message's body into the relevant compose box and marks it
    /// as being edited — the next `send_message` for that same box sends
    /// `Command::EditMessage` instead of a new message.
    pub fn start_edit(&mut self, room_id: &str, event: &TimelineEvent, thread_id: Option<String>) {
        if thread_id.is_some() {
            self.thread_compose_text = event.body.clone();
        } else {
            self.compose_text = event.body.clone();
        }
        self.editing_message = Some(PendingEdit {
            room_id: room_id.to_string(),
            event_id: event.event_id.clone(),
            thread_id,
        });
    }

    /// Cancels an in-progress edit and clears the compose box it was in.
    pub fn cancel_edit(&mut self, thread_id: Option<&str>) {
        if thread_id.is_some() {
            self.thread_compose_text.clear();
        } else {
            self.compose_text.clear();
        }
        self.editing_message = None;
    }

    /// Whether the compose box identified by `thread_id` (`None` for the
    /// main timeline's) currently has an edit in progress.
    pub fn is_editing(&self, thread_id: Option<&str>) -> bool {
        self.editing_message
            .as_ref()
            .is_some_and(|e| e.thread_id.as_deref() == thread_id)
    }

    /// Redacts (deletes) a message. Matrix has no client-side confirmation
    /// step beyond whatever the UI puts in front of this — the server will
    /// reject it (surfaced as `Event::Error`) if you don't have permission.
    pub fn delete_message(&mut self, room_id: &str, event_id: &str) {
        self.tx
            .send(Command::DeleteMessage {
                room_id: room_id.to_string(),
                event_id: event_id.to_string(),
            })
            .ok();
    }

    /// Stages raw image bytes (from a file pick or a clipboard paste) for
    /// review — decodes a preview texture and puts it in `pending_image`
    /// rather than sending immediately. `App::confirm_pending_image` /
    /// `cancel_pending_image` act on it once the user decides.
    fn stage_image_bytes(
        &mut self,
        ctx: &egui::Context,
        room_id: &str,
        thread_id: Option<String>,
        filename: String,
        bytes: Vec<u8>,
        mime: String,
    ) {
        match image::load_from_memory(&bytes) {
            Ok(img) => {
                let img = img.to_rgba8();
                let size = [img.width() as usize, img.height() as usize];
                let color_image =
                    egui::ColorImage::from_rgba_unmultiplied(size, img.as_flat_samples().as_slice());
                let texture = ctx.load_texture("pending-image-preview", color_image, egui::TextureOptions::default());
                self.pending_image = Some(PendingImage {
                    texture,
                    bytes,
                    filename,
                    mime,
                    room_id: room_id.to_string(),
                    thread_id,
                });
            }
            Err(e) => {
                self.status_line = Some(format!("failed to decode image: {e}"));
            }
        }
    }

    /// Actually uploads and sends whatever's staged in `pending_image`, if
    /// it's still there. Clears it either way.
    pub fn confirm_pending_image(&mut self) {
        let Some(pending) = self.pending_image.take() else {
            return;
        };
        let local_id = self.new_local_id();
        if pending.thread_id.is_some() {
            self.thread_send_in_flight += 1;
        } else {
            self.main_send_in_flight += 1;
        }
        self.tx
            .send(Command::SendImage {
                room_id: pending.room_id,
                thread_id: pending.thread_id,
                filename: pending.filename,
                bytes: pending.bytes,
                mime: pending.mime,
                local_id,
            })
            .ok();
    }

    /// Discards whatever's staged in `pending_image` without sending it.
    pub fn cancel_pending_image(&mut self) {
        self.pending_image = None;
    }

    /// Opens a native file picker (filtered to common image formats) and
    /// stages whatever the user chose for review (see `stage_image_bytes`).
    /// Blocks the UI thread until the (modal, OS-native) dialog closes —
    /// same as every other egui app using `rfd` synchronously.
    pub fn pick_image(&mut self, ctx: &egui::Context, room_id: &str, thread_id: Option<String>) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("images", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
            .pick_file()
        else {
            return;
        };

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                self.status_line = Some(format!("failed to read {}: {e}", path.display()));
                return;
            }
        };

        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "image".to_string());
        let mime = mime_from_extension(&filename);

        self.stage_image_bytes(ctx, room_id, thread_id, filename, bytes, mime);
    }

    /// Reads an image off the OS clipboard (if there is one) and stages it
    /// for review (see `stage_image_bytes`). Called on Ctrl+V while a
    /// compose box has focus. Reports *why* via `status_line` on every
    /// failure path — silently doing nothing when a paste doesn't work is
    /// indistinguishable from the feature being broken.
    pub fn paste_image_from_clipboard(&mut self, ctx: &egui::Context, room_id: &str, thread_id: Option<String>) {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(c) => c,
            Err(e) => {
                self.status_line = Some(format!("clipboard unavailable: {e}"));
                return;
            }
        };
        let image = match clipboard.get_image() {
            Ok(img) => img,
            Err(e) => {
                // Not necessarily an error worth surfacing — e.g. the
                // clipboard holds plain text, which is the common case for
                // Ctrl+V and should fall through to normal text pasting.
                // Only arboard::Error::ContentNotAvailable means "no image
                // there"; anything else (permissions, no clipboard backend
                // on this display server, ...) is worth telling the user
                // about since it'll look identical to "paste does nothing".
                if !matches!(e, arboard::Error::ContentNotAvailable) {
                    self.status_line = Some(format!("clipboard image read failed: {e}"));
                }
                return;
            }
        };

        let Some(rgba) = image::RgbaImage::from_raw(
            image.width as u32,
            image.height as u32,
            image.bytes.into_owned(),
        ) else {
            self.status_line = Some("clipboard image had an unexpected size".to_string());
            return;
        };

        let mut png_bytes = Vec::new();
        if let Err(e) = image::DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
        {
            self.status_line = Some(format!("failed to encode pasted image: {e}"));
            return;
        }

        self.stage_image_bytes(
            ctx,
            room_id,
            thread_id,
            "pasted-image.png".to_string(),
            png_bytes,
            "image/png".to_string(),
        );
    }

    /// Scans `body` for "@DisplayName" occurrences matching a known member
    /// of `room_id` (checked longest-name-first, so "@Anh" can't shadow a
    /// match for "@Anh Tuấn") and turns them into a proper `m.mentions`
    /// user-id list plus HTML with matrix.to pill links. Returns
    /// `(Vec::new(), None)` if nothing in `body` matches a member — the
    /// message is then just sent as plain text, same as before this
    /// feature existed.
    fn resolve_mentions(&self, room_id: &str, body: &str) -> (Vec<String>, Option<String>) {
        let Some(members) = self.room_members.get(room_id) else {
            return (Vec::new(), None);
        };

        let mut candidates: Vec<&(String, String)> =
            members.iter().filter(|(_, name)| !name.is_empty()).collect();
        candidates.sort_by_key(|(_, name)| std::cmp::Reverse(name.chars().count()));

        let mut mentions = Vec::new();
        let mut html = String::new();
        let mut rest = body;
        'outer: while !rest.is_empty() {
            for (user_id, name) in &candidates {
                let needle = format!("@{name}");
                if let Some(after) = rest.strip_prefix(needle.as_str()) {
                    mentions.push(user_id.clone());
                    html.push_str(&format!(
                        r#"<a href="https://matrix.to/#/{}">{}</a>"#,
                        html_escape(user_id),
                        html_escape(&needle),
                    ));
                    rest = after;
                    continue 'outer;
                }
            }
            let mut chars = rest.chars();
            let c = chars.next().expect("rest is non-empty");
            html.push_str(&html_escape(&c.to_string()));
            rest = chars.as_str();
        }

        if mentions.is_empty() {
            (Vec::new(), None)
        } else {
            mentions.sort();
            mentions.dedup();
            (mentions, Some(html))
        }
    }

    /// Opens a specific thread's reply panel. `room_id` is explicit (not
    /// inferred from `self.selected_room`) because this can be reached
    /// from the global threads popup for a room other than the one
    /// currently open — in that case it also switches the selected room,
    /// since the thread panel's compose/pagination logic reads
    /// `self.selected_room` to know which room it's acting on.
    pub fn open_thread(&mut self, room_id: String, root: TimelineEvent) {
        self.unread_threads.remove(&(room_id.clone(), root.event_id.clone()));
        if self.selected_room.as_deref() != Some(room_id.as_str()) {
            self.select_room(&room_id);
        }
        self.tx
            .send(Command::LoadThread {
                room_id,
                thread_root_id: root.event_id.clone(),
            })
            .ok();
        self.right_panel = RightPanel::Thread {
            root,
            events: Vec::new(),
        };
    }

    /// Opens the global threads popup — one list across every joined room,
    /// Element-style, not scoped to whichever room happens to be selected.
    pub fn open_global_threads(&mut self) {
        for room in self.rooms.clone() {
            if room.is_space || room.is_invite {
                continue;
            }
            self.tx
                .send(Command::ListThreads { room_id: room.room_id })
                .ok();
        }
        self.right_panel = RightPanel::ThreadsList(None);
    }

    /// Opens the threads popup scoped to just `room_id` — the per-room
    /// "[ threads ]" button in the timeline header.
    pub fn open_room_threads(&mut self, room_id: String) {
        self.tx
            .send(Command::ListThreads { room_id: room_id.clone() })
            .ok();
        self.right_panel = RightPanel::ThreadsList(Some(room_id));
    }

    /// Whether any room has a thread with an unread live reply — drives
    /// the Element-style badge dot on the global "[ threads ]" button.
    pub fn has_unread_threads(&self) -> bool {
        !self.unread_threads.is_empty()
    }

    /// Whether `room_id` specifically has a thread with an unread live
    /// reply — drives the badge dot on that room's own "[ threads ]"
    /// button in the timeline header.
    pub fn room_has_unread_threads(&self, room_id: &str) -> bool {
        self.unread_threads.iter().any(|(r, _)| r == room_id)
    }

    /// Whether the specific thread rooted at `thread_root_id` (in
    /// `room_id`) has an unread live reply — drives the badge dot next to
    /// each row in the threads list.
    pub fn thread_has_unread(&self, room_id: &str, thread_root_id: &str) -> bool {
        self.unread_threads
            .contains(&(room_id.to_string(), thread_root_id.to_string()))
    }

    pub fn summarize_current_room(&mut self) {
        if let Some(room_id) = self.selected_room.clone() {
            self.summarizing = true;
            self.tx.send(Command::Summarize { room_id }).ok();
        }
    }

    /// Downloads the video and opens it in the system's default player.
    /// Doesn't touch `image_cache` — videos can be large and there's no
    /// inline playback in egui, so there's no point caching bytes in RAM
    /// here; the worker downloads straight to a temp file.
    pub fn play_video(&mut self, mxc_uri: &str, filename: &str) {
        self.status_line = Some(format!("opening video: {filename}"));
        self.tx
            .send(Command::PlayVideo {
                mxc_uri: mxc_uri.to_string(),
                filename: filename.to_string(),
            })
            .ok();
    }

    pub fn accept_invite(&mut self, room_id: &str) {
        self.tx
            .send(Command::AcceptInvite {
                room_id: room_id.to_string(),
            })
            .ok();
    }

    pub fn decline_invite(&mut self, room_id: &str) {
        self.tx
            .send(Command::DeclineInvite {
                room_id: room_id.to_string(),
            })
            .ok();
    }

    /// Leaves a room (or Space) you've joined. Clears it from selection if
    /// it was the one currently open.
    pub fn leave_room(&mut self, room_id: &str) {
        if self.selected_room.as_deref() == Some(room_id) {
            self.selected_room = None;
        }
        if self.selected_space.as_deref() == Some(room_id) {
            self.selected_space = None;
        }
        self.tx
            .send(Command::LeaveRoom {
                room_id: room_id.to_string(),
            })
            .ok();
    }

    /// Opens the "create room" dialog, resetting it to blank first.
    pub fn open_create_room_dialog(&mut self) {
        self.create_room_name.clear();
        self.create_room_is_public = false;
        self.create_room_is_space = false;
        self.show_create_room_dialog = true;
    }

    /// Submits the "create room" dialog's current fields, if the name
    /// isn't blank, and closes it either way.
    pub fn submit_create_room(&mut self) {
        let name = self.create_room_name.trim().to_string();
        self.show_create_room_dialog = false;
        if name.is_empty() {
            return;
        }
        self.tx
            .send(Command::CreateRoom {
                name,
                is_public: self.create_room_is_public,
                is_space: self.create_room_is_space,
            })
            .ok();
    }

    /// Selects a Space to filter the room list by (`None` = show every
    /// room). Fetches its child rooms the first time it's selected.
    pub fn select_space(&mut self, space_room_id: Option<String>) {
        if let Some(space_room_id) = &space_room_id {
            if !self.space_children.contains_key(space_room_id) {
                self.tx
                    .send(Command::ListSpaceChildren {
                        space_room_id: space_room_id.clone(),
                    })
                    .ok();
            }
        }
        self.selected_space = space_room_id;
    }

    /// The notification mode for a room (`"all"` | `"mentions"` |
    /// `"mute"`), fetching it the first time it's asked for. Returns
    /// `None` while that first fetch is still in flight.
    pub fn notification_mode(&mut self, room_id: &str) -> Option<&str> {
        if !self.room_notification_modes.contains_key(room_id)
            && self.notification_mode_requested.insert(room_id.to_string())
        {
            self.tx
                .send(Command::GetNotificationMode {
                    room_id: room_id.to_string(),
                })
                .ok();
        }
        self.room_notification_modes.get(room_id).map(|s| s.as_str())
    }

    /// Sets a room's notification mode (`"all"` | `"mentions"` | `"mute"`).
    pub fn set_notification_mode(&mut self, room_id: &str, mode: &str) {
        self.tx
            .send(Command::SetNotificationMode {
                room_id: room_id.to_string(),
                mode: mode.to_string(),
            })
            .ok();
    }

    pub fn start_self_verification(&mut self) {
        self.security_status = Some("verification request sent — approve on your other device".to_string());
        self.tx.send(Command::StartSelfVerification).ok();
    }

    pub fn confirm_verification(&mut self) {
        self.tx.send(Command::ConfirmVerification).ok();
        self.verification_emojis = None;
    }

    pub fn cancel_verification(&mut self) {
        self.tx.send(Command::CancelVerification).ok();
        self.verification_emojis = None;
    }

    pub fn recover_with_key(&mut self) {
        let key = std::mem::take(&mut self.recovery_key_input);
        if key.trim().is_empty() {
            return;
        }
        self.security_status = Some("recovering...".to_string());
        self.tx
            .send(Command::RecoverWithKey {
                recovery_key: key.trim().to_string(),
            })
            .ok();
    }

    /// Requests the bytes for an image message, at most once per event id.
    /// Call this from the UI layer for every visible image message; it's a
    /// no-op if the bytes are already cached or already in flight.
    pub fn request_image(&mut self, event_id: &str, mxc_uri: &str, ctx: &egui::Context) {
        self.touch_image(event_id, ctx);
        if self.image_cache.contains_key(event_id) || self.image_requested.contains(event_id) {
            return;
        }
        self.image_requested.insert(event_id.to_string());
        self.tx
            .send(Command::FetchImage {
                key: event_id.to_string(),
                mxc_uri: mxc_uri.to_string(),
            })
            .ok();
    }

    /// Room members whose display name contains `query` (case-insensitive),
    /// for the @mention autocomplete list. Empty `query` matches everyone.
    /// Capped at 8 results — the compose box only has room to show a few.
    pub fn mention_candidates(&self, room_id: &str, query: &str) -> Vec<(String, String)> {
        let Some(members) = self.room_members.get(room_id) else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        let mut matches: Vec<(String, String)> = members
            .iter()
            .filter(|(_, name)| !name.is_empty() && name.to_lowercase().contains(&query))
            .cloned()
            .collect();
        matches.sort_by(|a, b| a.1.cmp(&b.1));
        matches.truncate(8);
        matches
    }
}

/// If `text` ends with an in-progress "@query" (an `@` with no whitespace
/// after it up to the end), returns that query — including the empty
/// string right after typing a bare `@`, which should list everyone.
pub fn active_mention_query(text: &str) -> Option<&str> {
    let at = text.rfind('@')?;
    let query = &text[at + 1..];
    if query.contains(char::is_whitespace) {
        return None;
    }
    Some(query)
}

/// Replaces the trailing "@query" that `active_mention_query` matched with
/// the chosen member's full "@DisplayName ", cursor-ready for the rest of
/// the message.
pub fn insert_mention(text: &mut String, display_name: &str) {
    if let Some(at) = text.rfind('@') {
        text.truncate(at);
    }
    text.push('@');
    text.push_str(display_name);
    text.push(' ');
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn mime_from_extension(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "application/octet-stream",
    }
    .to_string()
}
