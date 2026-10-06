/// Data models shared between the tokio worker (talks to matrix-rust-sdk)
/// and the egui UI thread. These replace the serde-IPC structs from the
/// Tauri version — same shape, but now just passed by value through a
/// channel instead of being (de)serialized across a process boundary.

#[derive(Debug, Clone, serde::Serialize)]
pub struct RoomSummary {
    pub room_id: String,
    pub name: String,
    /// The room's own `mxc://` avatar (its `m.room.avatar`, or the other
    /// member's for a DM whose room has none set — see
    /// `resolve_room_avatar_url` in `matrix::worker`), if any. Fetched
    /// through the same
    /// `FetchImage`/`state.imageCache` pipeline every other avatar in this
    /// app uses. `None` falls back to a colored-initial circle, same as a
    /// member with no avatar set.
    pub avatar_url: Option<String>,
    pub last_message: Option<String>,
    pub last_message_ts: i64,
    pub unread_count: u64,
    /// Unread messages that ping the logged-in user specifically — an
    /// `m.mentions` mention or a keyword/room-notify highlight, computed
    /// client-side the same way as `unread_count` (see `entry_to_summary`
    /// in `matrix/worker.rs`). Drives the room list's red mention badge and
    /// the "mentions" filter, as opposed to `unread_count`'s grey badge for
    /// "just unread".
    pub mention_count: u64,
    pub is_encrypted: bool,
    pub is_invite: bool,
    /// The room's `m.favourite` tag (`Command::SetRoomFavorite`) — the
    /// room list sorts these to the top, same as Element's own "Favourites"
    /// treatment. Read straight from `Room::is_favourite()`'s already-
    /// synced local cache, not a live server round-trip.
    pub is_favorite: bool,
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
    /// Emoji reactions on this message, aggregated by emoji. Populated from
    /// the timeline item's own bundled aggregation (`content.reactions()`)
    /// at load time — no extra request per message needed, since the SDK
    /// already carries this — and refreshed via `Command::ToggleReaction`
    /// / `Event::Reactions` (see those for the live-update path).
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
    /// Same bundled-aggregation source as `latest_reply_sender_name` — the
    /// most recent reply's event ID. Lets the UI mark a thread read (send
    /// a threaded receipt for its latest reply) straight from a
    /// threads-list row, without first opening the thread to learn which
    /// event that receipt should point at.
    pub latest_reply_event_id: Option<String>,
    /// Same bundled-aggregation source as `latest_reply_sender_name` —
    /// whether the thread's most recent reply itself pings this account
    /// (an `m.mentions` mention, same check as `mentions_me` above, just
    /// against the reply's content instead of this event's own). A thread
    /// can ping you through any of its replies, not only through its root
    /// message — this is what lets the threads-list panel's "you were
    /// mentioned" indicator/filter catch that case too, instead of only
    /// ever looking at `mentions_me` (the root's own mention).
    pub latest_reply_mentions_me: bool,
    /// Same bundled-aggregation source as `latest_reply_sender_name` —
    /// `"image"` | `"video"` | `"audio"` | `"file"` | `"notice"` when the
    /// latest reply itself is an attachment/notice, `None` for a plain
    /// text reply (or when there's no latest reply at all). Lets the
    /// threads-list panel render an actual image thumbnail for "latest
    /// reply: [photo]" instead of just the filename text `latest_reply_body`
    /// alone would show.
    pub latest_reply_msg_type: Option<String>,
    /// The latest reply's `mxc://` media, when `latest_reply_msg_type` is
    /// one of the attachment kinds.
    pub latest_reply_media_url: Option<String>,
    /// The latest reply's declared MIME type for `latest_reply_media_url`,
    /// same role as `media_mime` above.
    pub latest_reply_media_mime: Option<String>,
    /// JSON-encoded `EncryptedFile` for `latest_reply_media_url` when the
    /// latest reply is an encrypted-room attachment, same role as
    /// `media_encryption` above.
    pub latest_reply_media_encryption: Option<String>,
    /// For a thread root only: whether this account's own threaded read
    /// receipt (server state, not a session-local guess — see
    /// `thread_is_unread` in `worker.rs`) is behind the thread's latest
    /// reply. `None` for every other kind of event, and for a thread root
    /// whose read state couldn't be determined.
    pub is_unread: Option<bool>,
    /// The client-generated `Command::SendMessage.local_id` this event's
    /// sender attached as the event's transaction ID, echoed back by the
    /// homeserver on `unsigned.transaction_id` — but *only* down the sync
    /// stream reaching the exact device that sent it (see
    /// `register_new_message_handler`). Lets `app.js` match a live event
    /// against the optimistic "sending…" bubble it already rendered for
    /// that `local_id` and reconcile the two instead of showing the
    /// message twice. `None` for every event this app didn't itself just
    /// send with a transaction ID (which is every path but that live
    /// handler).
    pub local_id: Option<String>,
    /// User IDs (`@user:server`) whose read receipt (`m.receipt`) points
    /// at exactly this event — i.e. this is the newest message that user
    /// has read. Populated from the timeline item's own bundled
    /// `read_receipts()` (see `convert.rs`), same "already loaded, no
    /// extra fetch" reasoning as `reactions` above. Excludes the logged-in
    /// user's own receipt — nothing useful to show yourself that you've
    /// read your own messages. Drives the small "seen by" avatar stack
    /// Element shows under the last message each person has read.
    pub read_by: Vec<String>,
}

/// Resolved system-theme colors, straight from the running GTK theme (see
/// `gtk_theme.rs`) — every field is a `#rrggbb` hex string, matching what
/// `app.js` sets as CSS custom properties directly. Named to mirror the
/// `--bg`/`--bg-alt`/etc. custom properties in `style.css` one-to-one.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SystemTheme {
    pub bg: String,
    pub bg_alt: String,
    pub border: String,
    pub text: String,
    pub text_weak: String,
    pub accent: String,
    pub accent_strong: String,
}

/// swayctl-center's liquid glass settings as the webview needs them (see
/// `sway_glass.rs`): glass on behind the window, the panes' white body alpha
/// (effects.glass_opacity) and frost (effects.glass_blur, 0..1).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GlassConfig {
    pub on: bool,
    pub tint: f64,
    pub frost: f64,
}

/// What's behind the window right now (the compositor's glass probe), as a
/// `cols` x `rows` grid of relative luminance over a `w` x `h` (logical px)
/// window, plus each cell's darkest/brightest pixel.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GlassBackdrop {
    pub w: f64,
    pub h: f64,
    pub cols: u32,
    pub rows: u32,
    pub lum: Vec<f32>,
    pub lmin: Vec<f32>,
    pub lmax: Vec<f32>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReactionSummary {
    pub emoji: String,
    pub count: u64,
    /// Whether the logged-in user is one of the reactors — drives
    /// highlighting the pill and determines whether clicking it removes
    /// vs. adds a reaction.
    pub by_me: bool,
    /// User IDs (`@user:server`) of everyone who reacted with this emoji,
    /// in reaction order. Lets the frontend show "who reacted" (e.g. a
    /// hover tooltip on the pill) instead of just a bare count.
    pub senders: Vec<String>,
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

/// One hit from `Command::SearchDirectoryUsers` — the homeserver's user
/// directory search, so (unlike `Command::ListAllUsers`) this can surface
/// people the logged-in user has no room in common with yet, which is
/// exactly who "invite by name" needs to find.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DirectoryUser {
    pub user_id: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
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

/// A room's editable metadata plus whether the logged-in user is actually
/// allowed to change each field (`RoomMember::can_send_state`) — response to
/// `Command::GetRoomInfo`, and re-sent after `SetRoomName`/`SetRoomTopic`/
/// `SetRoomAvatar` succeed, same "always re-read the real state back" idea
/// as `OwnProfile`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RoomInfo {
    pub room_id: String,
    pub name: Option<String>,
    pub topic: Option<String>,
    pub avatar_url: Option<String>,
    pub can_set_name: bool,
    pub can_set_topic: bool,
    pub can_set_avatar: bool,
}

/// One answer option of a poll, with its current vote tally — part of
/// `PollData`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PollOptionResult {
    pub id: String,
    pub text: String,
    pub votes: u64,
}

/// A poll's full current state — the question, its options with tallied
/// votes, and whether it's ended. Sent as `Event::PollUpdated` both as the
/// direct response to `Command::StartPoll`/`VotePoll`/`EndPoll` and pushed
/// live whenever anyone else's vote/end arrives via sync (see
/// `register_poll_response_handler`/`register_poll_end_handler` in
/// `worker.rs`). Always a full recompute from every response event on the
/// poll (via `ruma::events::poll::compile_unstable_poll_results`), same
/// "re-fetch rather than incrementally patch" approach `Reactions` uses, so
/// there's no risk of drift from a missed/duplicate event.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PollData {
    pub room_id: String,
    /// The thread this poll was started in, if any — so the frontend knows
    /// whether to update the main timeline or a thread panel.
    pub thread_id: Option<String>,
    pub poll_event_id: String,
    pub question: String,
    pub options: Vec<PollOptionResult>,
    pub total_votes: u64,
    /// Answer ID(s) the logged-in user has themselves selected, if any —
    /// drives which option(s) show as "your vote" in the UI.
    pub my_vote_ids: Vec<String>,
    pub ended: bool,
    pub max_selections: u64,
}

/// One message found by `Command::SearchMessages` — same shape as
/// `UserSearchHit`, kept as its own type since it's matched by message
/// *content* rather than sender, and the two searches may grow different
/// fields later.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub room_id: String,
    pub room_name: String,
    pub event: TimelineEvent,
}

/// One room member's live presence — response to `Command::GetPresence`,
/// and pushed again whenever a fresh `m.presence` arrives for that user via
/// sync (see `register_presence_handler`). `presence` is `"online"` |
/// `"offline"` | `"unavailable"`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PresenceInfo {
    pub user_id: String,
    pub presence: String,
    pub currently_active: Option<bool>,
    pub last_active_ago_ms: Option<u64>,
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
