/// Data models shared between the tokio worker (talks to matrix-rust-sdk)
/// and the egui UI thread. These replace the serde-IPC structs from the
/// Tauri version — same shape, but now just passed by value through a
/// channel instead of being (de)serialized across a process boundary.

#[derive(Debug, Clone)]
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
}

#[derive(Debug, Clone)]
pub struct TimelineEvent {
    pub event_id: String,
    pub sender: String,
    pub sender_name: String,
    pub body: String,
    pub msg_type: String, // "text" | "image" | "video" | "file" | "notice" | "other"
    pub media_url: Option<String>,     // mxc:// of the main media (image/video/file)
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
}
