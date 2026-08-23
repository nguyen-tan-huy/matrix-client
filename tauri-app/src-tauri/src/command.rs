/// Sent from the egui UI thread to the tokio worker thread. Mirrors the
/// `#[tauri::command]` functions from the Tauri version — same operations,
/// now just enum variants pushed through an `mpsc` channel instead of IPC.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "type", content = "data")]
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
    /// Loads the next (older-activity) page of a room's thread list,
    /// continuing from wherever `ListThreads`'s first page (or the
    /// previous `LoadMoreThreads` call) left off. Mirrors
    /// `LoadMoreThreadReplies`/`PaginateBack`.
    LoadMoreThreads {
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
    /// Invites a user (`@name:server`) to a room you're in.
    InviteUser {
        room_id: String,
        user_id: String,
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
    /// Imports E2EE room keys from a file exported by Element (Settings →
    /// Security & Privacy → Export keys) — a passphrase-encrypted Megolm
    /// session export. Restores the ability to decrypt history the app's
    /// own device never received keys for. Surfaced as `Event::RoomKeysImported`
    /// or `Event::Error`.
    ImportRoomKeys {
        bytes: Vec<u8>,
        passphrase: String,
    },
    /// Downloads an encrypted/auth-required media item's bytes through
    /// `client.media()`, which attaches the access token. `key` is just an
    /// identifier the UI picked (typically the event id) to match the
    /// result back to the right message.
    FetchImage {
        key: String,
        mxc_uri: String,
        /// `TimelineEvent::media_encryption`, when the room is encrypted —
        /// needed to decrypt the downloaded bytes rather than handing back
        /// raw ciphertext.
        media_encryption: Option<String>,
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
        media_encryption: Option<String>,
    },
    /// Same download-to-temp-file-then-open-with-the-system-default-app
    /// approach as `PlayVideo`, for an image format the webview itself
    /// can't decode (HEIC/HEIF from an iPhone, TIFF, ...) — those would
    /// otherwise just render as the browser's broken-image icon.
    OpenMediaExternally {
        mxc_uri: String,
        filename: String,
        media_encryption: Option<String>,
    },
    /// Downloads an attachment (image/video/file) via `client.media()` and
    /// saves it into the user's Downloads folder (falling back to home if
    /// the OS has no such directory configured), rather than the temp file
    /// `PlayVideo`/`OpenMediaExternally` use — this is a real "save a copy"
    /// action, so the file needs to survive past this session. Answered
    /// with `Event::MediaDownloaded` or `Event::Error`.
    DownloadMedia {
        mxc_uri: String,
        filename: String,
        media_encryption: Option<String>,
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
    /// Shows a desktop notification for a new message. Handled with
    /// `notify-rust` directly rather than `tauri-plugin-notification` —
    /// that plugin's Linux `show()` call panics ("Cannot start a runtime
    /// from within a runtime") when the zbus dependency it shares with
    /// `ksni` (the tray icon) is built with the "tokio" feature, since it
    /// then bridges to async D-Bus via a fresh `Runtime::block_on` from
    /// inside a task that's already running on one. Calling
    /// `notify-rust`'s own `show_async()` from here avoids that entirely.
    ShowNotification {
        room_id: String,
        /// Set when this notification is for a reply inside a thread —
        /// clicking it should open that thread directly, not just the
        /// room, same as clicking the thread badge on the message itself.
        thread_id: Option<String>,
        title: String,
        body: String,
    },
    /// Not sent by the frontend directly — routed here from a
    /// `matrixtauriclient://notification?...` deep link, itself fired by
    /// tapping an Android notification (see `MainActivity.kt`'s
    /// `handleNotificationTap`). Just forwards to the same
    /// `Event::NotificationClicked` the desktop click path already sends,
    /// so `app.js` only needs the one handler for both platforms.
    HandleNotificationClick {
        room_id: String,
        thread_id: Option<String>,
    },
    /// Adds an emoji reaction to a message if the logged-in user hasn't
    /// already reacted with that emoji, or removes it if they have — same
    /// command works for a main-timeline message, a thread root, or a
    /// thread reply, since it fetches the event's current reactions via
    /// `/relations/{eventId}/m.annotation` rather than depending on
    /// whichever `Timeline` object (if any) happens to already have that
    /// event loaded. Answered with `Event::Reactions`.
    ToggleReaction {
        room_id: String,
        event_id: String,
        emoji: String,
    },
    /// Reads the room's custom emoji/sticker pack(s) — `im.ponies.room_emotes`
    /// state events (MSC2545, any state key) — plus the user's own
    /// personal pack (`im.ponies.user_emotes` account data, so it follows
    /// them into every room). Answered with `Event::ImagePacks`.
    ListImagePacks {
        room_id: String,
    },
    /// Sends one image from a pack as a standalone `m.sticker` message —
    /// the MSC2545 "usage: sticker" send path, as opposed to inserting the
    /// shortcode inline into text (not implemented; this app has no rich
    /// text editor to splice an inline `<img>` into).
    SendMeme {
        room_id: String,
        thread_id: Option<String>,
        url: String,
        shortcode: String,
    },
    /// Uploads an image and adds it to a pack under `shortcode` — either
    /// the room's own pack (`scope: "room"`, needs permission to send
    /// `im.ponies.room_emotes` state in that room) or the user's personal
    /// one (`scope: "personal"`, always allowed — it's their own account
    /// data). Answered with a fresh `Event::ImagePacks`, same as
    /// `Command::ListImagePacks`.
    AddImagePackEmoji {
        room_id: String,
        scope: String, // "room" | "personal"
        shortcode: String,
        bytes: Vec<u8>,
        mime: String,
    },
    /// Removes one shortcode from a pack — `scope` is `EmojiImage::scope`
    /// from whichever entry the UI is deleting, so it always edits the
    /// pack that entry actually came from. Answered with a fresh
    /// `Event::ImagePacks`, same as `Command::ListImagePacks`.
    RemoveImagePackEmoji {
        room_id: String,
        scope: String, // "room" | "personal"
        shortcode: String,
    },
}
