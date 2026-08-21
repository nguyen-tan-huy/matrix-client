/// Sent from the egui UI thread to the tokio worker thread. Mirrors the
/// `#[tauri::command]` functions from the Tauri version — same operations,
/// now just enum variants pushed through an `mpsc` channel instead of IPC.
#[derive(Debug, Clone)]
pub enum Command {
    CheckSession,
    LoginPassword {
        homeserver: String,
        username: String,
        password: String,
    },
    LoginOAuth {
        homeserver: String,
    },
    StartSync,
    LoadTimeline {
        room_id: String,
    },
    PaginateBack {
        room_id: String,
    },
    LoadThread {
        room_id: String,
        thread_root_id: String,
    },
    /// Loads the next (older) page of a thread's replies, continuing from
    /// wherever `LoadThread`'s first page (or the last `LoadMoreThreadReplies`
    /// call) left off. Mirrors `PaginateBack` for the main timeline.
    LoadMoreThreadReplies {
        room_id: String,
        thread_root_id: String,
    },
    ListThreads {
        room_id: String,
    },
    /// Fetches the room's joined member list, for the @mention autocomplete
    /// in the compose box.
    ListMembers {
        room_id: String,
    },
    AcceptInvite {
        room_id: String,
    },
    DeclineInvite {
        room_id: String,
    },
    SendMessage {
        room_id: String,
        body: String,
        thread_id: Option<String>,
        /// User IDs (`@user:server`) mentioned in `body` via the compose
        /// box's @mention autocomplete — resolved into a proper
        /// `m.mentions` field so the mentioned users actually get
        /// pinged/highlighted, not just plain "@DisplayName" text.
        mentions: Vec<String>,
        /// Pre-built HTML with `@DisplayName` occurrences turned into
        /// matrix.to pill links, when `mentions` is non-empty (built in
        /// `App::send_message`, which already has the display-name cache
        /// this needs). `None` for a plain-text message.
        html_body: Option<String>,
        /// Opaque id `App::send_message` generated for this attempt, so
        /// `Event::MessageSent`/`MessageSendFailed` can say which "sending…"
        /// indicator to clear.
        local_id: String,
        /// Set via the compose box's "reply" action (`App::start_reply`) —
        /// quotes this event as `m.in_reply_to` (or, inside a thread,
        /// `Thread::reply`, a genuine in-thread reply rather than the
        /// automatic fallback every thread reply also carries).
        reply_to_event_id: Option<String>,
    },
    Summarize {
        room_id: String,
    },
    /// Redacts (deletes) a message. Only actually succeeds server-side for
    /// your own messages or if you have redaction power in the room — the
    /// server rejects it otherwise, surfaced as `Event::Error`.
    DeleteMessage {
        room_id: String,
        event_id: String,
    },
    /// Sends an `m.replace` edit of an existing message.
    EditMessage {
        room_id: String,
        event_id: String,
        body: String,
        mentions: Vec<String>,
        html_body: Option<String>,
    },
    /// Uploads an image (picked from disk or pasted from the clipboard)
    /// and sends it as an `m.image` message.
    SendImage {
        room_id: String,
        thread_id: Option<String>,
        filename: String,
        bytes: Vec<u8>,
        /// MIME type, e.g. "image/png" — used both for the upload request
        /// and to pick the right encoder for clipboard pastes.
        mime: String,
        /// Same idea as `SendMessage`'s `local_id`.
        local_id: String,
    },
    StartSelfVerification,
    ConfirmVerification,
    CancelVerification,
    RecoverWithKey {
        recovery_key: String,
    },
    /// Downloads an encrypted/auth-required media item's bytes through
    /// `client.media()`, which attaches the access token. `key` is just an
    /// identifier the UI picked (typically the event id) to match the
    /// result back to the right message.
    FetchImage {
        key: String,
        mxc_uri: String,
    },
    /// Drops a room's cached `Timeline` object from the worker's memory —
    /// sent by the UI's LRU eviction when a room falls out of the "recently
    /// viewed" window. Safe to call any time; re-opening the room later
    /// just rebuilds the Timeline (backed by the on-disk sqlite store, so
    /// no data is actually lost, only the in-memory cache).
    CloseRoomTimeline {
        room_id: String,
    },
    /// Downloads a video's full bytes via `client.media()` (auth attached),
    /// writes them to a temp file, then opens it with the system's default
    /// video player. Runs entirely in the worker — bytes are never sent
    /// back to the UI thread or kept in the RAM cache, since videos can be
    /// large and there's no point loading them into egui (no video widget).
    PlayVideo {
        mxc_uri: String,
        filename: String,
    },
    /// Leaves a room (or space) you've joined.
    LeaveRoom {
        room_id: String,
    },
    /// Sends a read receipt for the latest event in the room — the
    /// server/client-computed `unread_count` in `RoomSummary` only drops
    /// once one of these actually goes out, which nothing did before this
    /// existed.
    MarkRoomRead {
        room_id: String,
    },
    /// Creates a new room, or a new Space if `is_space` is set (a Space is
    /// just a room with `m.room.create`'s `type` set to `m.space` — it
    /// holds no messages of its own, only `m.space.child` links to other
    /// rooms).
    CreateRoom {
        name: String,
        is_public: bool,
        is_space: bool,
    },
    /// Reads a room's own `m.space.child` state events — the rooms grouped
    /// under it, if it's a Space.
    ListSpaceChildren {
        space_room_id: String,
    },
    /// Adds a room as a child of a Space (posts that room's
    /// `m.space.child` state event) — "put this room in this space".
    AddRoomToSpace {
        space_room_id: String,
        room_id: String,
    },
    GetNotificationMode {
        room_id: String,
    },
    /// `mode` is `"all"` | `"mentions"` | `"mute"`.
    SetNotificationMode {
        room_id: String,
        mode: String,
    },
}
