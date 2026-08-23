use crate::models::{RoomSummary, TimelineEvent};

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

    Rooms(Vec<RoomSummary>),
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
    Summary {
        room_id: String,
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

    Error(String),
}
