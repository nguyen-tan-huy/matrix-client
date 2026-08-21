use matrix_sdk::Client;
use matrix_sdk_ui::timeline::{TimelineDetails, TimelineItem, TimelineItemContent, VirtualTimelineItem};
use std::sync::Arc;

use crate::models::TimelineEvent;

fn mxc_of(source: &matrix_sdk::ruma::events::room::MediaSource) -> Option<String> {
    match source {
        matrix_sdk::ruma::events::room::MediaSource::Plain(uri) => Some(uri.to_string()),
        matrix_sdk::ruma::events::room::MediaSource::Encrypted(file) => Some(file.url.to_string()),
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
) -> (String, String, Option<String>, Option<String>) {
    use matrix_sdk::ruma::events::room::message::MessageType;
    match msgtype {
        MessageType::Text(t) => (t.body.clone(), "text".to_string(), None, None),
        MessageType::Image(img) => {
            // Store the raw mxc:// URI (not an http URL) — the real
            // download happens later via `client.media()`, which
            // attaches the access token. See Command::FetchImage in
            // worker.rs / Event::ImageBytes.
            let mxc = mxc_of(&img.source);
            (img.body.clone(), "image".to_string(), mxc, None)
        }
        MessageType::Video(vid) => {
            let video_mxc = mxc_of(&vid.source);
            // Thumbnail lives in `info.thumbnail_source`, may be absent.
            let thumb_mxc = vid
                .info
                .as_ref()
                .and_then(|info| info.thumbnail_source.as_ref())
                .and_then(mxc_of);
            (vid.body.clone(), "video".to_string(), video_mxc, thumb_mxc)
        }
        MessageType::File(f) => {
            let mxc = mxc_of(&f.source);
            (f.body.clone(), "file".to_string(), mxc, None)
        }
        MessageType::Notice(n) => (n.body.clone(), "notice".to_string(), None, None),
        other => (other.body().to_string(), "other".to_string(), None, None),
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

pub async fn convert_item(client: &Client, item: &Arc<TimelineItem>) -> Option<TimelineEvent> {
    let event = item.as_event()?;

    let sender = event.sender().to_string();
    let is_own = client
        .user_id()
        .map(|id| id == event.sender())
        .unwrap_or(false);

    let sender_name = match event.sender_profile() {
        TimelineDetails::Ready(profile) => profile.display_name.clone(),
        _ => None,
    }
    .unwrap_or_else(|| sender.clone());

    let (body, msg_type, media_url, thumbnail_url) = match event.content() {
        TimelineItemContent::Message(msg) => {
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
                    (t, Some(mxc)) if t == "m.image" => (body, "image".to_string(), Some(mxc), None),
                    (t, Some(mxc)) if t == "m.video" => (body, "video".to_string(), Some(mxc), None),
                    (_, Some(mxc)) => (body, "file".to_string(), Some(mxc), None),
                    (_, None) => (body, "other".to_string(), None, None),
                }
            } else {
                fields
            }
        }
        TimelineItemContent::RedactedMessage => (
            "[message removed]".to_string(),
            "notice".to_string(),
            None,
            None,
        ),
        TimelineItemContent::Sticker(sticker) => {
            let content = sticker.content();
            (
                content.body.clone(),
                "image".to_string(),
                Some(content.url.to_string()),
                None,
            )
        }
        TimelineItemContent::UnableToDecrypt(_) => (
            "[unable to decrypt]".to_string(),
            "notice".to_string(),
            None,
            None,
        ),
        TimelineItemContent::FailedToParseMessageLike { event_type, .. } => (
            format!("[unsupported event: {event_type}]"),
            "notice".to_string(),
            None,
            None,
        ),
        TimelineItemContent::FailedToParseState { event_type, .. } => (
            format!("[unsupported state event: {event_type}]"),
            "notice".to_string(),
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

    Some(TimelineEvent {
        event_id: event.event_id()?.to_string(),
        sender,
        sender_name,
        body,
        msg_type,
        media_url,
        thumbnail_url,
        timestamp: event.timestamp().0.into(),
        thread_count,
        is_own,
        reply_to_event_id,
        reply_to_preview,
    })
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
fn reply_preview(event: &matrix_sdk_ui::timeline::EventTimelineItem) -> (Option<String>, Option<String>) {
    let Some(event_id) = reply_to_event_id_from_raw(event) else {
        return (None, None);
    };

    let TimelineItemContent::Message(msg) = event.content() else {
        return (Some(event_id), None);
    };
    let Some(in_reply_to) = msg.in_reply_to() else {
        return (Some(event_id), None);
    };
    let TimelineDetails::Ready(replied) = &in_reply_to.event else {
        return (Some(event_id), None);
    };

    let sender = replied.sender().to_string();
    let snippet = match replied.content() {
        TimelineItemContent::Message(m) => m.msgtype().body().to_string(),
        TimelineItemContent::RedactedMessage => "[message removed]".to_string(),
        TimelineItemContent::Sticker(s) => s.content().body.clone(),
        _ => "[attachment]".to_string(),
    };
    (Some(event_id), Some(format!("{sender}: {snippet}")))
}

fn reply_to_event_id_from_raw(event: &matrix_sdk_ui::timeline::EventTimelineItem) -> Option<String> {
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
        if matches!(item.as_virtual(), Some(VirtualTimelineItem::DayDivider(_))) {
            continue;
        }
        if let Some(event) = convert_item(client, item).await {
            out.push(event);
        }
    }
    out
}
