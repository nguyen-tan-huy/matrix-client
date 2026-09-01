/// Data models shared between the tokio worker (talks to matrix-rust-sdk)
/// and the egui UI thread. These replace the serde-IPC structs from the
/// Tauri version — same shape, but now just passed by value through a
/// channel instead of being (de)serialized across a process boundary.

#[derive(Debug, Clone, serde::Serialize)]
pub struct RoomSummary {
    pub room_id: String,
    pub name: String,
    pub last_message: Option<String>,
    pub last_message_ts: i64,
    pub unread_count: u64,
    pub is_encrypted: bool,
    pub is_invite: bool,
    /// True for a Matrix Space (`m.room.create`'s `type` is `m.space`) —
    /// holds no messages of its own, just `m.space.child` links to other
    /// rooms. Drives the space-picker row in the room list.
    pub is_space: bool,
    /// True when this summary came from a `RoomListEntry::Invalidated` —
    /// data that was synced at some point but just fell out of the
    /// server's synced range (e.g. scrolled away from) and may be stale.
    /// `RoomListEntry::Empty` (never synced at all) isn't represented as a
    /// `RoomSummary` in the first place — see `entry_to_summary` in
    /// `matrix/worker.rs`.
    pub is_loading: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineEvent {
    pub event_id: String,
    pub sender: String,
    pub sender_name: String,
    /// The sender's `mxc://` avatar, resolved the same way `sender_name`
    /// is (room-specific member profile, or the raw event's bundled
    /// sender profile for a live/raw-JSON-parsed event) — `None` when
    /// they have no avatar set. Fetched through the same `FetchImage` /
    /// `state.imageCache` path as message attachments in `app.js`, keyed
    /// by this URL.
    pub sender_avatar_url: Option<String>,
    pub body: String,
    pub msg_type: String, // "text" | "image" | "video" | "file" | "notice" | "other"
    pub media_url: Option<String>, // mxc:// of the main media (image/video/file)
    /// The sender's declared MIME type for `media_url` (`content.info.mimetype`),
    /// e.g. "image/heic" — used as the `data:` URL type when the fetched
    /// bytes are rendered, since guessing from magic bytes alone misses
    /// formats like HEIC/AVIF/TIFF entirely (falls back to that guess only
    /// when this is absent, e.g. a sender that didn't set it).
    pub media_mime: Option<String>,
    /// JSON-encoded `EncryptedFile` (AES key/iv/hashes) when `media_url`
    /// points at an encrypted-room attachment — `None` for a plain/public
    /// room's media. Round-tripped back through `Command::FetchImage` /
    /// `PlayVideo` / `OpenMediaExternally` so the worker can decrypt the
    /// downloaded bytes instead of handing back raw ciphertext.
    pub media_encryption: Option<String>,
    pub thumbnail_url: Option<String>, // mxc:// of a preview thumbnail, if any (video)
    pub timestamp: i64,
    pub thread_count: Option<u64>,
    pub is_own: bool,
    /// Set when this message is a reply (`m.in_reply_to`, whether a plain
    /// reply or the fallback on a threaded reply) — the event ID of the
    /// message it's replying to.
    pub reply_to_event_id: Option<String>,
    /// A short "Sender: body snippet" preview of the replied-to message,
    /// when it was available at conversion time. `None` doesn't mean
    /// there's no reply — check `reply_to_event_id` for that — just that
    /// no preview text could be resolved (e.g. a live `/sync` message,
    /// where resolving the target would need an extra fetch).
    pub reply_to_preview: Option<String>,
    /// True when this message pings the logged-in user — an `m.mentions`
    /// user-id mention, or (via matrix-sdk's own push-rule evaluation for
    /// timeline items) a keyword/room-notify highlight. Drives the
    /// highlighted-message styling in the timeline.
    pub mentions_me: bool,
    /// User IDs (`@user:server`) named in this message's `m.mentions` —
    /// the *specific* people it tags, as opposed to `mentions_me` (which
    /// only says whether the logged-in user happens to be one of them).
    /// Drives rendering an `@DisplayName` occurrence in the body as a
    /// pill instead of plain text, same as Element does — see `app.js`'s
    /// `applyMentionPills`.
    pub mentioned_user_ids: Vec<String>,
    /// Emoji reactions on this message, aggregated by emoji. Starts empty
    /// on initial load (fetched on demand — see `Command::ToggleReaction`
    /// and `Event::Reactions` — rather than eagerly for every message,
    /// which would mean an extra request per message just to render a
    /// list) and gets filled in once something reacts to it this session.
    pub reactions: Vec<ReactionSummary>,
    /// For a thread root only (`thread_count.is_some()`): the display name
    /// of whoever sent the thread's most recent reply, straight from the
    /// server's bundled aggregation on that root event — no separate
    /// `/relations` fetch needed just to preview it. `None` for every other
    /// kind of event this model represents (a normal message, an actual
    /// thread reply, or a thread root the server didn't bundle this for).
    pub latest_reply_sender_name: Option<String>,
    /// Same bundled-aggregation source as `latest_reply_sender_name` — the
    /// most recent reply's body text.
    pub latest_reply_body: Option<String>,
    /// Same bundled-aggregation source as `latest_reply_sender_name` — the
    /// most recent reply's `origin_server_ts`.
    pub latest_reply_ts: Option<i64>,
    /// For a thread root only: whether this account's own threaded read
    /// receipt (server state, not a session-local guess — see
    /// `thread_is_unread` in `worker.rs`) is behind the thread's latest
    /// reply. `None` for every other kind of event, and for a thread root
    /// whose read state couldn't be determined.
    pub is_unread: Option<bool>,
}

/// Resolved system-theme colors, straight from the running GTK theme (see
/// `gtk_theme.rs`) — every field is a `#rrggbb` hex string, matching what
/// `app.js` sets as CSS custom properties directly. Named to mirror the
/// `--bg`/`--bg-alt`/etc. custom properties in `style.css` one-to-one.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SystemTheme {
    pub bg: String,
    pub bg_alt: String,
    pub border: String,
    pub text: String,
    pub text_weak: String,
    pub accent: String,
    pub accent_strong: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReactionSummary {
    pub emoji: String,
    pub count: u64,
    /// Whether the logged-in user is one of the reactors — drives
    /// highlighting the pill and determines whether clicking it removes
    /// vs. adds a reaction.
    pub by_me: bool,
}

/// One message found by `Command::SearchUserMessages` — a `TimelineEvent`
/// plus which room it came from, since results are aggregated across every
/// joined room rather than scoped to one.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UserSearchHit {
    pub room_id: String,
    pub room_name: String,
    pub event: TimelineEvent,
}

/// The logged-in user's own profile — response to `Command::GetOwnProfile`,
/// and re-sent after `Command::SetDisplayName`/`SetAvatar` succeed so the
/// profile panel always reflects what the server actually has, rather than
/// the UI just assuming its own write went through.
#[derive(Debug, Clone, serde::Serialize)]
pub struct OwnProfile {
    pub user_id: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
}

/// One image in a room's custom emoji/sticker pack (`im.ponies.room_emotes`
/// state event, MSC2545 — the same format Element reads/writes, so a pack
/// set up there, or in any other MSC2545 client, shows up here too).
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmojiImage {
    pub shortcode: String,
    pub url: String, // mxc://
    pub pack_name: String,
    /// "room" or "personal" — which pack this came from, so the UI knows
    /// which one `Command::RemoveImagePackEmoji` needs to edit.
    pub scope: String,
}
