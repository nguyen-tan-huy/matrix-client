use matrix_sdk::Client;
use matrix_sdk_ui::timeline::{
    MsgLikeKind, TimelineDetails, TimelineItem, TimelineItemContent, VirtualTimelineItem,
};
use std::sync::Arc;

use crate::models::TimelineEvent;

fn mxc_of(source: &matrix_sdk::ruma::events::room::MediaSource) -> Option<String> {
    match source {
        matrix_sdk::ruma::events::room::MediaSource::Plain(uri) => Some(uri.to_string()),
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(file) => Some(file.url.to_string()),
    }
}

/// Same idea as `mxc_of`, but for a sticker's own (differently-typed, if
/// structurally identical) media source — `m.sticker` doesn't carry
/// encryption info in this app (no `Encrypted` handling below, matching
/// this app's pre-existing sticker support), so any variant other than
/// `Plain` is treated as "no URL" rather than guessed at.
pub fn sticker_mxc_of(source: &matrix_sdk::ruma::events::sticker::StickerMediaSource) -> Option<String> {
    match source {
        matrix_sdk::ruma::events::sticker::StickerMediaSource::Plain(uri) => Some(uri.to_string()),
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

/// `(mxc url, encryption info)` — for `MediaSource::Encrypted`, the second
/// element is the `EncryptedFile` (AES key/iv/hashes) JSON-encoded so it
/// can ride along through `TimelineEvent`/`Command::FetchImage` etc. as a
/// plain string, then get deserialized back and used to actually decrypt
/// the downloaded bytes (see `download_media_bytes` in worker.rs).
/// Without this, an encrypted room's images/files/videos downloaded as
/// raw ciphertext and got handed to the webview/video player as if they
/// were the real file — which is exactly why every image in an encrypted
/// room rendered as a broken-image icon regardless of its actual format.
fn media_source_fields(
    source: &matrix_sdk::ruma::events::room::MediaSource,
) -> (Option<String>, Option<String>) {
    match source {
        matrix_sdk::ruma::events::room::MediaSource::Plain(uri) => (Some(uri.to_string()), None),
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(file) => (
            Some(file.url.to_string()),
            serde_json::to_string(file.as_ref()).ok(),
        ),
    }
}

/// Turns a typed `MessageType` into `(body, msg_type, media_url,
/// thumbnail_url)` — shared between `convert_item` (timeline items, which
/// additionally fall back to `raw_media_url` when this returns `"other"`,
/// since ruma's enum doesn't recognize every content shape in the wild)
/// and `register_new_message_handler` in worker.rs (live `/sync` messages,
/// which previously hardcoded every new message to `msg_type: "text"`
/// regardless of its real type — newly sent/received images had no
/// `media_url` at all and so no way to render or view them).
pub(crate) fn message_type_fields(
    msgtype: &matrix_sdk::ruma::events::room::message::MessageType,
) -> (String, String, Option<String>, Option<String>, Option<String>, Option<String>) {
    use matrix_sdk::ruma::events::room::message::MessageType;
    match msgtype {
        MessageType::Text(t) => (t.body.clone(), "text".to_string(), None, None, None, None),
        MessageType::Image(img) => {
            // Store the raw mxc:// URI (not an http URL) — the real
            // download happens later via `client.media()`, which
            // attaches the access token. See Command::FetchImage in
            // worker.rs / Event::ImageBytes.
            let (mxc, encryption) = media_source_fields(&img.source);
            let mime = img.info.as_ref().and_then(|info| info.mimetype.clone());
            (img.body.clone(), "image".to_string(), mxc, None, mime, encryption)
        }
        MessageType::Video(vid) => {
            let (video_mxc, encryption) = media_source_fields(&vid.source);
            // Thumbnail lives in `info.thumbnail_source`, may be absent.
            let thumb_mxc = vid
                .info
                .as_ref()
                .and_then(|info| info.thumbnail_source.as_ref())
                .and_then(mxc_of);
            (vid.body.clone(), "video".to_string(), video_mxc, thumb_mxc, None, encryption)
        }
        MessageType::File(f) => {
            let (mxc, encryption) = media_source_fields(&f.source);
            let mime = f.info.as_ref().and_then(|info| info.mimetype.clone());
            (f.body.clone(), "file".to_string(), mxc, None, mime, encryption)
        }
        MessageType::Audio(a) => {
            let (mxc, encryption) = media_source_fields(&a.source);
            let mime = a.info.as_ref().and_then(|info| info.mimetype.clone());
            (a.body.clone(), "audio".to_string(), mxc, None, mime, encryption)
        }
        MessageType::Notice(n) => (n.body.clone(), "notice".to_string(), None, None, None, None),
        other => (other.body().to_string(), "other".to_string(), None, None, None, None),
    }
}

/// Fallback for message shapes ruma's typed `MessageType` enum fails to
/// recognize (e.g. clients/bridges that send `m.image`/`m.file` content
/// with fields ruma's strict variant doesn't expect) — read the mxc URL
/// straight out of the raw event JSON instead of giving up on the
/// attachment entirely. Handles both plain (`content.url`) and encrypted
/// (`content.file.url`) shapes.
fn raw_media_url(event: &matrix_sdk_ui::timeline::EventTimelineItem) -> Option<String> {
    let raw = event.latest_json()?;
    let value: serde_json::Value = raw.deserialize_as().ok()?;
    let content = value.get("content")?;
    if let Some(url) = content.get("url").and_then(|v| v.as_str()) {
        return Some(url.to_string());
    }
    content
        .get("file")
        .and_then(|f| f.get("url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Converts the timeline item's own bundled reaction aggregation
/// (`content.reactions()` — already loaded from the local store/sync, no
/// `/relations` fetch needed) into what the frontend renders. Same shape
/// as `summarize_reactions` in `worker.rs`, which covers the live-update
/// path (`Command::ToggleReaction`/`register_reaction_handler`); this is
/// what actually fills the pills on a normal timeline load, including
/// right after reopening the app, since that path never touches this
/// function's caller (`convert_item`) at all.
fn reactions_from_bundled(
    reactions: Option<&matrix_sdk_ui::timeline::ReactionsByKeyBySender>,
    my_id: Option<&matrix_sdk::ruma::UserId>,
) -> Vec<crate::models::ReactionSummary> {
    let Some(reactions) = reactions else {
        return Vec::new();
    };
    reactions
        .iter()
        .filter(|(_, by_sender)| !by_sender.is_empty())
        .map(|(emoji, by_sender)| {
            let senders: Vec<String> = by_sender.keys().map(|u| u.to_string()).collect();
            let by_me = my_id.is_some_and(|id| by_sender.contains_key(id));
            crate::models::ReactionSummary {
                emoji: emoji.clone(),
                count: senders.len() as u64,
                by_me,
                senders,
            }
        })
        .collect()
}

pub async fn convert_item(client: &Client, item: &Arc<TimelineItem>) -> Option<TimelineEvent> {
    let event = item.as_event()?;

    // Excludes anything with an `m.thread` relation from the main
    // timeline, checked generically on the *raw* event JSON regardless of
    // event type — not just `m.room.message`, which is all the typed
    // `event_filter` in `Command::LoadTimeline` can ever check (it only
    // sees ruma's typed `Relation` enum, and that only exists on
    // `RoomMessageEventContent`). A sticker sent into a thread has no
    // typed `relates_to` field at all — `StickerEventContent` doesn't
    // declare one, so ruma silently drops it while decoding, before any
    // typed check could ever see it, no matter where that check lives.
    // Reading the raw JSON here instead — the same way Element itself
    // does — is what actually lets a thread-targeted sticker (or any
    // other non-`m.room.message` event a thread might one day carry) stay
    // out of the main room view, matching Element's own behavior exactly
    // instead of only handling the one event type ruma gives typed
    // support for.
    let has_thread_relation = event
        .latest_json()
        .and_then(|raw| raw.deserialize_as::<serde_json::Value>().ok())
        .and_then(|v| {
            v.pointer("/content/m.relates_to/rel_type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .as_deref()
        == Some("m.thread");
    if has_thread_relation {
        return None;
    }

    let sender = event.sender().to_string();
    let is_own = client
        .user_id()
        .map(|id| id == event.sender())
        .unwrap_or(false);

    let (sender_name, sender_avatar_url) = match event.sender_profile() {
        TimelineDetails::Ready(profile) => (
            profile.display_name.clone(),
            profile.avatar_url.as_ref().map(|u| u.to_string()),
        ),
        _ => (None, None),
    };
    let sender_name = sender_name.unwrap_or_else(|| sender.clone());

    let (body, msg_type, media_url, thumbnail_url, media_mime, media_encryption) = match event.content() {
        // 0.18 groups `Message`/`Sticker`/redactions/UTDs together under
        // one `MsgLike` variant (previously each was its own top-level
        // `TimelineItemContent` variant) — see `MsgLikeKind`.
        TimelineItemContent::MsgLike(msg_like) => match &msg_like.kind {
            MsgLikeKind::Message(msg) => {
                let msgtype = msg.msgtype();
                let fields = message_type_fields(msgtype);
                if fields.1 == "other" {
                    let raw_msgtype = msgtype.msgtype();
                    let body = fields.0;
                    let mxc = raw_media_url(event);
                    tracing::debug!(
                        msgtype = raw_msgtype,
                        body,
                        recovered_media_url = mxc.is_some(),
                        "unrecognized MessageType"
                    );
                    match (raw_msgtype, mxc) {
                        (t, Some(mxc)) if t == "m.image" => {
                            (body, "image".to_string(), Some(mxc), None, None, None)
                        }
                        (t, Some(mxc)) if t == "m.video" => {
                            (body, "video".to_string(), Some(mxc), None, None, None)
                        }
                        (_, Some(mxc)) => (body, "file".to_string(), Some(mxc), None, None, None),
                        (_, None) => (body, "other".to_string(), None, None, None, None),
                    }
                } else {
                    fields
                }
            }
            MsgLikeKind::Redacted => (
                "[message removed]".to_string(),
                "notice".to_string(),
                None,
                None,
                None,
                None,
            ),
            MsgLikeKind::Sticker(sticker) => {
                let content = sticker.content();
                (
                    content.body.clone(),
                    "image".to_string(),
                    sticker_mxc_of(&content.source),
                    None,
                    content.info.mimetype.clone(),
                    None,
                )
            }
            MsgLikeKind::UnableToDecrypt(_) => (
                "[unable to decrypt]".to_string(),
                "notice".to_string(),
                None,
                None,
                None,
                None,
            ),
            // Polls, live locations, and custom message-like events aren't
            // rendered by this app — same "hide, don't dump raw structs"
            // treatment as membership/profile/other-state changes below.
            MsgLikeKind::Poll(_) | MsgLikeKind::LiveLocation(_) | MsgLikeKind::Other(_) => {
                return None;
            }
        },
        TimelineItemContent::FailedToParseMessageLike { event_type, .. } => (
            format!("[unsupported event: {event_type}]"),
            "notice".to_string(),
            None,
            None,
            None,
            None,
        ),
        TimelineItemContent::FailedToParseState { event_type, .. } => (
            format!("[unsupported state event: {event_type}]"),
            "notice".to_string(),
            None,
            None,
            None,
            None,
        ),
        // Room membership changes (joins/invites/leaves/kicks), profile
        // changes (display name/avatar tweaks), and other state events
        // (room settings, power levels, ...) used to leak as a raw `{:?}`
        // debug dump of the internal SDK struct straight into the chat —
        // e.g. `MembershipChange(RoomMembershipChange { user_id: ... })`.
        // Hide them from the timeline entirely instead, same as Element's
        // default "collapse membership events" behavior; there's nothing
        // useful to a user in the raw struct anyway.
        TimelineItemContent::MembershipChange(_)
        | TimelineItemContent::ProfileChange(_)
        | TimelineItemContent::OtherState(_) => return None,
        other => {
            tracing::debug!(content = ?other, "unhandled TimelineItemContent, hiding from timeline");
            return None;
        }
    };

    // matrix-sdk-ui 0.7's typed API doesn't expose per-item thread
    // summaries, but the server still bundles them into the raw event's
    // `unsigned.m.relations.m.thread` (MSC3440) — read it straight from
    // there. Lets the main timeline show "[ thread: N replies ]" directly
    // on thread-root messages, and tells `merge_thread_replies` which
    // already-loaded messages are actually thread roots worth fetching
    // replies for, without a separate full-room `/threads` scan.
    let thread_count = event
        .latest_json()
        .and_then(|raw| raw.deserialize_as::<serde_json::Value>().ok())
        .and_then(|v| {
            v.pointer("/unsigned/m.relations/m.thread/count")
                .and_then(|c| c.as_u64())
        });

    let (reply_to_event_id, reply_to_preview) = reply_preview(event);
    let mentioned_user_ids = mentioned_user_ids_from_raw(event);
    let reactions = reactions_from_bundled(event.content().reactions(), client.user_id());
    let own_id = client.user_id();
    let read_by: Vec<String> = event
        .read_receipts()
        .keys()
        .filter(|id| Some(id.as_ref()) != own_id)
        .map(|id| id.to_string())
        .collect();

    Some(TimelineEvent {
        event_id: event.event_id()?.to_string(),
        sender,
        sender_name,
        sender_avatar_url,
        body,
        msg_type,
        media_url,
        media_mime,
        media_encryption,
        thumbnail_url,
        timestamp: event.timestamp().0.into(),
        thread_count,
        is_own,
        reply_to_event_id,
        reply_to_preview,
        mentions_me: event.is_highlighted(),
        mentioned_user_ids,
        reactions,
        read_by,
        // Only populated for the `/threads`-endpoint path (see
        // `parse_thread_root` in `worker.rs`) that backs the threads-list
        // panel — a thread root as it appears inline in the main timeline
        // only ever shows its own reply-count button, not a preview of the
        // latest reply, so there's nothing to fill in here.
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
    })
}

/// Same "read it out of the raw event JSON" approach as `thread_count`
/// just above — matrix-sdk-ui's typed `TimelineItemContent::Message`
/// exposes `mentions()` too, but reading the raw JSON keeps this in one
/// spot with `parse_raw_message_event`'s identical logic for the
/// thread/search code paths (worker.rs), which have no typed item to call
/// a method on at all. Also folds in any user pill found in
/// `formatted_body` (see `user_ids_from_formatted_body`) that
/// `m.mentions` alone missed, deduplicated.
fn mentioned_user_ids_from_raw(event: &matrix_sdk_ui::timeline::EventTimelineItem) -> Vec<String> {
    let Some(value) = event
        .latest_json()
        .and_then(|raw| raw.deserialize_as::<serde_json::Value>().ok())
    else {
        return Vec::new();
    };
    mentioned_user_ids_from_content(value.get("content"))
}

/// Shared by every code path that has a raw `content` object handy
/// (`mentioned_user_ids_from_raw` above and `worker.rs`'s
/// `parse_raw_message_event`) — the two actual sources are `m.mentions`
/// and any matrix.to user pill in `formatted_body`, deduplicated.
pub(crate) fn mentioned_user_ids_from_content(content: Option<&serde_json::Value>) -> Vec<String> {
    let Some(content) = content else {
        return Vec::new();
    };
    let mut ids: Vec<String> = content
        .pointer("/m.mentions/user_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|id| id.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if let Some(html) = content.get("formatted_body").and_then(|v| v.as_str()) {
        for id in user_ids_from_formatted_body(html) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// The HTML alternate body of a typed `MessageType`, when it has one —
/// used by `register_new_message_handler` (worker.rs) to feed
/// `user_ids_from_formatted_body` from a live `/sync` message, which only
/// has this typed enum handy rather than the raw JSON the other two
/// mentioned-user-ids call sites read from directly.
pub(crate) fn formatted_html_of(
    msgtype: &matrix_sdk::ruma::events::room::message::MessageType,
) -> Option<&str> {
    use matrix_sdk::ruma::events::room::message::MessageType;
    match msgtype {
        MessageType::Text(t) => t.formatted.as_ref(),
        MessageType::Notice(n) => n.formatted.as_ref(),
        MessageType::Emote(e) => e.formatted.as_ref(),
        _ => None,
    }
    .map(|f| f.body.as_str())
}

/// Pulls `@user:server` IDs out of matrix.to user links
/// (`https://matrix.to/#/@user:server` or, since `encodeURIComponent`
/// escapes `@`/`:`, this app's own `%40user%3Aserver` shape) in a
/// message's HTML body — a fallback source of "who's actually tagged
/// here" for `mentioned_user_ids_from_content`. Needed because not every
/// client sets the newer (MSC3952) `m.mentions` content field for a
/// mention it otherwise renders as a pill — without this, a real tag from
/// one of those just showed as plain "@Name" text here instead of a pill,
/// even in a room where `m.mentions`-based ones worked fine. No general
/// percent-decoder, just those two escapes — a Matrix user id's character
/// set has little else that would ever end up percent-encoded in
/// practice, and anything that fails to parse as a real user ID is
/// dropped rather than guessed at.
pub(crate) fn user_ids_from_formatted_body(html: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let needle = "matrix.to/#/";
    let mut search_from = 0usize;
    while let Some(rel) = html[search_from..].find(needle) {
        let start = search_from + rel + needle.len();
        let rest = &html[start..];
        let Some(id_part) = rest.strip_prefix("%40").or_else(|| rest.strip_prefix('@')) else {
            search_from = start;
            continue;
        };
        let end = id_part
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c.is_whitespace())
            .unwrap_or(id_part.len());
        let raw = id_part[..end].replace("%3A", ":").replace("%3a", ":");
        let id = format!("@{raw}");
        if matrix_sdk::ruma::UserId::parse(&id).is_ok() && !ids.contains(&id) {
            ids.push(id);
        }
        search_from = start + end;
    }
    ids
}

/// Pulls "what message is this replying to" out of a timeline item, when
/// it's available. Covers both a plain reply (`Relation::Reply`) and the
/// `m.in_reply_to` a genuine in-thread reply also carries — but not
/// `Thread`'s auto-generated fallback `m.in_reply_to` (pointing at
/// whatever the latest thread message happened to be, for clients that
/// don't understand threads), which isn't an intentional reply and would
/// be misleading to show as one. Distinguishing the two needs the raw
/// `is_falling_back` flag, which matrix-sdk-ui's typed `Message::in_reply_to()`
/// doesn't expose — so the event ID comes from the raw JSON instead.
///
/// The preview text is only available when matrix-sdk-ui already resolved
/// the target locally (`TimelineDetails::Ready`) — otherwise just the
/// event ID comes back, with no snippet.
fn reply_preview(
    event: &matrix_sdk_ui::timeline::EventTimelineItem,
) -> (Option<String>, Option<String>) {
    let Some(event_id) = reply_to_event_id_from_raw(event) else {
        return (None, None);
    };

    let TimelineItemContent::MsgLike(msg_like) = event.content() else {
        return (Some(event_id), None);
    };
    let Some(in_reply_to) = &msg_like.in_reply_to else {
        return (Some(event_id), None);
    };
    let TimelineDetails::Ready(replied) = &in_reply_to.event else {
        return (Some(event_id), None);
    };

    let sender = replied.sender.to_string();
    let snippet = match &replied.content {
        TimelineItemContent::MsgLike(reply_msg_like) => match &reply_msg_like.kind {
            MsgLikeKind::Message(m) => m.msgtype().body().to_string(),
            MsgLikeKind::Redacted => "[message removed]".to_string(),
            MsgLikeKind::Sticker(s) => s.content().body.clone(),
            _ => "[attachment]".to_string(),
        },
        _ => "[attachment]".to_string(),
    };
    (Some(event_id), Some(format!("{sender}: {snippet}")))
}

fn reply_to_event_id_from_raw(
    event: &matrix_sdk_ui::timeline::EventTimelineItem,
) -> Option<String> {
    let raw = event.latest_json()?;
    let value: serde_json::Value = raw.deserialize_as().ok()?;
    let relates_to = value.pointer("/content/m.relates_to")?;

    if relates_to.get("rel_type").and_then(|v| v.as_str()) == Some("m.thread")
        && relates_to.get("is_falling_back").and_then(|v| v.as_bool()) == Some(true)
    {
        return None;
    }

    relates_to
        .pointer("/m.in_reply_to/event_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub async fn convert_items<'a>(
    client: &Client,
    items: impl IntoIterator<Item = &'a Arc<TimelineItem>>,
) -> Vec<TimelineEvent> {
    let mut out = Vec::new();
    for item in items {
        if matches!(item.as_virtual(), Some(VirtualTimelineItem::DateDivider(_))) {
            continue;
        }
        if let Some(event) = convert_item(client, item).await {
            out.push(event);
        }
    }
    out
}
