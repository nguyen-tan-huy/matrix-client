use crate::models::{OwnProfile, RoomSummary, TimelineEvent};

/// Which of the two dynamic filtered views of `RoomListService::all_rooms()`
/// a `RoomListUpdate` came from — the growing main room list, or the
/// (unpaged, since invite counts are always small) invites view. Each is
/// its own `entries_with_dynamic_adapters` call (see `matrix/worker.rs`'s
/// `Command::StartSync`), so each has its own independent index space —
/// the frontend keeps two separate arrays and concatenates them (invites
/// first) into the flat `state.rooms` the rest of the UI reads.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub enum RoomListKind {
    Rooms,
    Invites,
}

/// One incremental change to a room list, mapped 1:1 from
/// `eyeball_im::VectorDiff<matrix_sdk_ui::room_list_service::Room>` (see
/// `matrix/worker.rs`'s `run_room_list_listener`). Unlike the older
/// MSC3575-era sliding sync this app briefly targeted, MSC4186 (what this
/// app's homeserver actually speaks) has no concept of an unsynced
/// "placeholder" slot — every entry that reaches this stream is a real,
/// resolved room, so (unlike an earlier draft of this protocol) there's no
/// `Option`/loading-slot case to represent here.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "op")]
pub enum RoomListOp {
    Append { values: Vec<RoomSummary> },
    Clear,
    PushFront { value: RoomSummary },
    PushBack { value: RoomSummary },
    PopFront,
    PopBack,
    Insert { index: u32, value: RoomSummary },
    Set { index: u32, value: RoomSummary },
    Remove { index: u32 },
    Truncate { length: u32 },
    Reset { values: Vec<RoomSummary> },
}

/// Sent from the tokio worker thread to the egui UI thread. Mirrors the
/// `app.emit(...)` calls from the Tauri version. The UI thread polls its
/// receiver once per frame (non-blocking) in `App::update` and folds these
/// into its state, then calls `ctx.request_repaint()`.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "data")]
pub enum Event {
    SessionChecked(bool),
    LoggedIn,
    LoginError(String),
    /// The running GTK theme's resolved colors (Linux desktop only — see
    /// `gtk_theme.rs`), sent once at startup and again on every live theme
    /// switch. `app.js` applies these directly as `:root` custom-property
    /// overrides so the UI matches the system theme (Sway/GTK) exactly,
    /// auto-dark-mode included, instead of the static built-in palette.
    SystemTheme(crate::models::SystemTheme),

    /// Incremental room-list change, driven by `RoomListService`'s sliding
    /// sync — replaces the old full-snapshot `Rooms` event so an account
    /// with thousands of rooms never has to build/send/re-render the whole
    /// list at once. See `matrix/worker.rs`'s room-list diff-listener tasks.
    RoomListUpdate {
        list: RoomListKind,
        ops: Vec<RoomListOp>,
    },
    /// Full snapshot of Space rooms (`m.room.create`'s `type: m.space`) —
    /// these are deliberately excluded from `RoomListService`'s sliding-sync
    /// lists server-side, so they're tracked via a small dedicated
    /// second sliding-sync session instead (see `refresh_spaces` in
    /// `matrix/worker.rs`). Full replace, not diffed, since the number of
    /// spaces on any account is always small.
    Spaces(Vec<RoomSummary>),
    Timeline {
        room_id: String,
        events: Vec<TimelineEvent>,
    },
    TimelinePrepend {
        room_id: String,
        events: Vec<TimelineEvent>,
        /// True once the room's full history has been reached — the UI
        /// stops issuing further `PaginateBack` requests for this room.
        reached_start: bool,
    },
    NewMessage {
        room_id: String,
        event: TimelineEvent,
    },
    /// A thread reply arrived via live `/sync`. Unlike `NewMessage`, this
    /// never touches the main room timeline — it either appends to the
    /// thread panel if that exact thread is open, or marks the thread
    /// unread (Element-style badge) otherwise.
    ThreadReply {
        room_id: String,
        thread_root_id: String,
        event: TimelineEvent,
    },
    ThreadEvents {
        room_id: String,
        thread_root_id: String,
        events: Vec<TimelineEvent>,
    },
    /// A page of older replies for a thread already open in the panel —
    /// response to `Command::LoadMoreThreadReplies`. Mirrors
    /// `Event::TimelinePrepend`.
    ThreadEventsPrepend {
        room_id: String,
        thread_root_id: String,
        events: Vec<TimelineEvent>,
        /// True once the thread's oldest reply has been reached — the UI
        /// stops issuing further `LoadMoreThreadReplies` requests for it.
        reached_start: bool,
    },
    /// The first page of a room's thread list — response to
    /// `Command::ListThreads`. Replaces whatever the UI had for this room
    /// (a fresh load, not an append — see `ThreadsListAppend` for that).
    ThreadsList {
        room_id: String,
        threads: Vec<TimelineEvent>,
        /// True once the room's oldest thread has been reached — the UI
        /// stops issuing further `LoadMoreThreads` requests for it (and
        /// doesn't show a "load more" control for it in the first place).
        reached_end: bool,
    },
    /// A further (older-activity) page of a room's thread list — response
    /// to `Command::LoadMoreThreads`. Mirrors `Event::ThreadEventsPrepend`.
    ThreadsListAppend {
        room_id: String,
        threads: Vec<TimelineEvent>,
        reached_end: bool,
    },
    /// A room's joined members, for the @mention autocomplete. `members` is
    /// `(user_id, display_name)` pairs.
    Members {
        room_id: String,
        members: Vec<(String, String)>,
    },
    /// Response to `Command::Summarize` — echoes `thread_root_id` back
    /// (as opposed to just carrying `room_id` alone) so the UI knows
    /// whether this answers the main-timeline summarize dialog or a
    /// specific thread's, since either can be requested independently
    /// and their results shouldn't cross-populate the wrong popup.
    Summary {
        room_id: String,
        thread_root_id: Option<String>,
        text: String,
    },
    /// A message was redacted — either by us (`Command::DeleteMessage`,
    /// applied optimistically before the server round trip) or live via
    /// `/sync` (someone else deleted it, or the server echoed ours back).
    /// Applying it twice is harmless, so no de-duplication needed between
    /// those two sources.
    MessageDeleted {
        room_id: String,
        event_id: String,
    },
    /// A message was edited — same dual-source/idempotent story as
    /// `MessageDeleted`. `new_body` is the replacement's actual new text,
    /// not the "* fallback" text `m.replace` events carry in their
    /// top-level body for clients that don't understand edits.
    MessageEdited {
        room_id: String,
        event_id: String,
        new_body: String,
    },
    /// `Command::SendMessage`/`SendImage` succeeded — clears the
    /// "sending…" indicator for `local_id`'s compose box. The message
    /// itself shows up separately once the live `/sync` echo arrives
    /// (`Event::NewMessage`); this is only about the indicator.
    MessageSent {
        thread_id: Option<String>,
        local_id: String,
    },
    /// `Command::SendMessage`/`SendImage` failed — same indicator-clearing
    /// role as `MessageSent`, plus the error to show.
    MessageSendFailed {
        thread_id: Option<String>,
        local_id: String,
        error: String,
    },

    VerificationEmojis(Vec<(String, String)>),
    VerificationDone,
    VerificationCancelled(String),
    RecoveryStatus(String),
    /// Response to `Command::SetLvxApiKey`/`GetLvxApiKeyStatus` — never
    /// carries the actual key, just whether one is currently set.
    LvxApiKeyStatus {
        configured: bool,
    },
    /// Result of `Command::ImportRoomKeys` — how many of the sessions in
    /// the file were new/usable vs. the file's total session count.
    RoomKeysImported {
        imported: usize,
        total: usize,
    },

    /// Raw image bytes for a message, fetched with proper auth. `key`
    /// matches whatever was passed in `Command::FetchImage`.
    ImageBytes {
        key: String,
        bytes: Vec<u8>,
    },
    /// `Command::FetchImage` failed — carries `key` (unlike the generic
    /// `Error` event) specifically so the frontend can un-stick that one
    /// image and retry it, instead of it staying permanently broken. A
    /// plain `Error` alone left no way to tell which image to retry, and
    /// the frontend's own "already requested, don't ask again" guard
    /// (there to avoid spamming duplicate requests on every re-render)
    /// meant a single transient failure broke that image forever.
    ImageFetchFailed {
        key: String,
        error: String,
    },
    /// `Command::DownloadMedia` succeeded — `path` is the full path the
    /// file was saved to, for a "saved to ..." toast.
    MediaDownloaded {
        path: String,
    },

    /// A Space's child room IDs — response to `Command::ListSpaceChildren`.
    SpaceChildren {
        space_room_id: String,
        room_ids: Vec<String>,
    },
    /// A room's notification mode — response to `Command::GetNotificationMode`
    /// (and sent again after `Command::SetNotificationMode` succeeds).
    /// `mode` is `"all"` | `"mentions"` | `"mute"`.
    NotificationMode {
        room_id: String,
        mode: String,
    },
    /// The user clicked a notification shown for `Command::ShowNotification`
    /// (desktop) or tapped one on Android (routed back in via
    /// `Command::HandleNotificationClick`).
    NotificationClicked {
        room_id: String,
        thread_id: Option<String>,
    },
    /// The current reaction state of one message — response to
    /// `Command::ToggleReaction`, and also pushed live when someone else's
    /// reaction to a message arrives via sync (see `register_reaction_handler`).
    Reactions {
        room_id: String,
        event_id: String,
        reactions: Vec<crate::models::ReactionSummary>,
    },
    /// A room's custom emoji/sticker images — response to `Command::ListImagePacks`.
    ImagePacks {
        room_id: String,
        images: Vec<crate::models::EmojiImage>,
    },

    /// Full results for `Command::SearchUserMessages`. `truncated` means
    /// at least one room hit `search_user_messages`'s safety page cap
    /// before running out of history within the requested date range —
    /// results for that room may be missing older messages; narrowing the
    /// date range avoids it.
    UserMessagesSearchResult {
        user_id: String,
        results: Vec<crate::models::UserSearchHit>,
        truncated: bool,
    },
    /// Response to `Command::ListAllUsers` — `(user_id, display_name)`
    /// pairs, one per distinct joined member across every joined room.
    AllUsers {
        users: Vec<(String, String)>,
    },

    /// Response to `Command::ResolveSharedEvent`. `found: false` means the
    /// event doesn't exist (deleted, or no permission) — not the same as
    /// `found: true, thread_root_id: None`, which means it exists and is a
    /// plain (non-thread) timeline event.
    SharedEventResolved {
        room_id: String,
        event_id: String,
        found: bool,
        thread_root_id: Option<String>,
    },

    /// Response to `Command::GetOwnProfile`, and again after
    /// `Command::SetDisplayName`/`SetAvatar` succeed — this app never
    /// touches per-room membership state directly, so it can't (and
    /// shouldn't try to) suppress the "changed name/avatar" timeline
    /// notices a homeserver fans out to other members' clients when a
    /// global profile changes; that fan-out is server-side, not something
    /// this client controls. This event only reports the profile write
    /// itself succeeding.
    OwnProfile(OwnProfile),

    Error(String),
}
