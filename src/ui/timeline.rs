use eframe::egui;

use crate::app::App;
use crate::models::TimelineEvent;

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(room_id) = app.selected_room.clone() else {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.centered_and_justified(|ui| {
                ui.label(egui::RichText::new("select a conversation").weak());
            });
        });
        return;
    };

    // A `TopBottomPanel` (declared before the `CentralPanel`) reserves
    // exactly the height its own content needs — including the mention
    // suggestion row, which only shows up sometimes — and the timeline's
    // `ScrollArea` below just fills whatever's left. A fixed
    // `ui.available_height() - 40.0` on the ScrollArea (the previous
    // approach) didn't know about the mention row's extra height and ended
    // up pushing the compose box itself off the bottom of the window.
    egui::TopBottomPanel::bottom("timeline_compose").show(ctx, |ui| {
        ui.add_space(4.0);
        if draw_pending_image_preview(ui, app, &room_id, None) {
            ui.add_space(4.0);
            return;
        }
        draw_mention_suggestions(ui, app, &room_id, true);
        draw_editing_indicator(ui, app, None);
        draw_reply_indicator(ui, app, None);
        ui.horizontal(|ui| {
            if ui.button("📎").on_hover_text("send an image").clicked() {
                app.pick_image(ctx, &room_id, None);
            }
            let response = ui.add(
                egui::TextEdit::singleline(&mut app.compose_text)
                    .desired_width(ui.available_width() - 60.0)
                    .hint_text("message... (@ to mention, Ctrl+V to paste an image)"),
            );
            let enter_pressed =
                response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let paste_pressed = response.has_focus()
                && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::V));

            if ui.button("send").clicked() || enter_pressed {
                app.send_message(&room_id, None);
            }
            if paste_pressed {
                app.paste_image_from_clipboard(ctx, &room_id, None);
            }
            if app.is_sending(None) {
                ui.label(egui::RichText::new("sending...").weak().small());
            }
        });
        ui.add_space(4.0);
    });

    egui::CentralPanel::default().show(ctx, |ui| {
        let room_name = app
            .rooms
            .iter()
            .find(|r| r.room_id == room_id)
            .map(|r| r.name.clone())
            .unwrap_or_else(|| room_id.clone());

        // ---- Header ----
        ui.horizontal(|ui| {
            ui.heading(&room_name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let summarizing = app.summarizing;
                if ui
                    .add_enabled(
                        !summarizing,
                        egui::Button::new(if summarizing {
                            "summarizing..."
                        } else {
                            "[ summarize ]"
                        }),
                    )
                    .clicked()
                {
                    app.summarize_current_room();
                }
                let threads_label = if app.room_has_unread_threads(&room_id) {
                    "[ threads ● ]"
                } else {
                    "[ threads ]"
                };
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new(threads_label).color(
                            if app.room_has_unread_threads(&room_id) {
                                egui::Color32::from_rgb(90, 200, 120)
                            } else {
                                ui.visuals().text_color()
                            },
                        ),
                    ))
                    .clicked()
                {
                    app.open_room_threads(room_id.clone());
                }

                if ui
                    .button("[ leave ]")
                    .on_hover_text("leave this room")
                    .clicked()
                {
                    app.leave_room(&room_id);
                }

                draw_notification_mode_picker(ui, app, &room_id);
            });
        });
        ui.separator();

        if let Some(summary) = app.summaries.get(&room_id) {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new("SUMMARY").strong().small());
                ui.label(summary);
            });
            ui.add_space(6.0);
        }

        // ---- Timeline ----
        let events = app.timelines.get(&room_id).cloned().unwrap_or_default();

        let mut clicked_thread: Option<TimelineEvent> = None;

        let scroll_output = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .id_salt("main_timeline_scroll")
            .show(ui, |ui| {
                // Feedback for what's happening at the top of the list —
                // otherwise scrolling up either silently does nothing
                // (indistinguishable from "no more history") or silently
                // starts a fetch with no sign anything is happening.
                if app.is_paginating(&room_id) {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("loading older messages...").weak().small());
                    });
                    ui.add_space(4.0);
                } else if app.reached_start(&room_id) && !events.is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("— beginning of conversation —")
                                .weak()
                                .small(),
                        );
                    });
                    ui.add_space(4.0);
                }

                for event in &events {
                    draw_message(ui, app, event, &mut clicked_thread, &room_id, None);
                }
            });

        // Infinite scroll: ask for older messages once the scroll position
        // is near the top, instead of eagerly loading a large batch upfront.
        //
        // Guarded on actually overflowing the viewport: right after opening
        // a room (or right after each small batch arrives), the loaded
        // messages may not yet fill the visible area, so there's nothing to
        // scroll and `offset.y` reads 0 same as "scrolled to the top" —
        // without this check that reads as "near top" every single frame
        // and keeps fetching batch after batch until the whole room history
        // is loaded, which is exactly what lazy-loading is meant to avoid.
        //
        // Two thresholds (hysteresis), not one: prepending older messages
        // doesn't adjust the scroll offset for the height just added above
        // the viewport, so right after a batch loads the offset is still
        // "near the top" — a single threshold would immediately fire
        // another load, cascading into batch after batch with no further
        // scrolling from the user. `app.maybe_paginate_back` won't fire
        // again until `rearm_paginate_back` clears its cooldown, which only
        // happens once the offset passes back out beyond `REARM_PX` —
        // i.e. the user has to actually scroll away and back for each
        // additional batch, same as "kéo lên nữa thì load thêm".
        const NEAR_TOP_PX: f32 = 200.0;
        const REARM_PX: f32 = 500.0;
        let overflowing = scroll_output.content_size.y > scroll_output.inner_rect.height();
        let offset_y = scroll_output.state.offset.y;
        if offset_y > REARM_PX {
            app.rearm_paginate_back(&room_id);
        }
        if !events.is_empty() && overflowing && offset_y < NEAR_TOP_PX {
            app.maybe_paginate_back(&room_id);
        }

        if let Some(root) = clicked_thread {
            app.open_thread(room_id.clone(), root);
        }
    });
}

/// Shows the staged image (if any) waiting for this exact compose box
/// (`room_id`/`thread_id`) to confirm or cancel, with a thumbnail preview.
/// Returns `true` if it drew anything — callers hide their normal
/// text-input row in that case, so an image can't accidentally get sent
/// alongside (or instead of) a stray text message while it's awaiting
/// confirmation.
pub(crate) fn draw_pending_image_preview(
    ui: &mut egui::Ui,
    app: &mut App,
    room_id: &str,
    thread_id: Option<&str>,
) -> bool {
    let matches = app
        .pending_image
        .as_ref()
        .is_some_and(|p| p.room_id == room_id && p.thread_id.as_deref() == thread_id);
    if !matches {
        return false;
    }

    let pending = app.pending_image.as_ref().expect("checked above");
    let texture = pending.texture.clone();
    let filename = pending.filename.clone();

    ui.horizontal(|ui| {
        ui.add(egui::Image::from_texture(&texture).max_width(96.0).max_height(96.0));
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(&filename).weak().small());
            ui.horizontal(|ui| {
                if ui.button("[ gửi ảnh ]").clicked() {
                    app.confirm_pending_image();
                }
                if ui.button("[ hủy ]").clicked() {
                    app.cancel_pending_image();
                }
            });
        });
    });
    true
}

/// If the compose box has an in-progress "@query" at the end, shows a row
/// of matching-member buttons; clicking one finishes the mention. `is_main`
/// picks which compose field (`App::compose_text` vs
/// `App::thread_compose_text`) to read/edit.
/// Shows "editing message... [cancel]" above the compose box while
/// `App::start_edit` has staged an edit for it — `thread_id` picks which
/// box, same convention as `draw_mention_suggestions`' `is_main`.
pub(crate) fn draw_editing_indicator(ui: &mut egui::Ui, app: &mut App, thread_id: Option<&str>) {
    if !app.is_editing(thread_id) {
        return;
    }
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("editing message...").weak().small());
        if ui.small_button("[ cancel ]").clicked() {
            app.cancel_edit(thread_id);
        }
    });
}

/// Shows "↩ replying to: {preview} [cancel]" above the compose box while
/// `App::start_reply` has one queued for it.
pub(crate) fn draw_reply_indicator(ui: &mut egui::Ui, app: &mut App, thread_id: Option<&str>) {
    let Some(preview) = app.replying_to_preview(thread_id).map(|s| truncate(s, 60)) else {
        return;
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("↩ replying to: {preview}")).weak().small());
        if ui.small_button("[ cancel ]").clicked() {
            app.cancel_reply(thread_id);
        }
    });
}

fn draw_notification_mode_picker(ui: &mut egui::Ui, app: &mut App, room_id: &str) {
    let current = app.notification_mode(room_id).map(|s| s.to_string());
    let label = match current.as_deref() {
        Some("all") => "🔔 all",
        Some("mentions") => "🔔 mentions",
        Some("mute") => "🔇 mute",
        _ => "🔔 ...",
    };
    egui::ComboBox::from_id_salt("notification_mode")
        .selected_text(label)
        .show_ui(ui, |ui| {
            for (mode, mode_label) in [
                ("all", "🔔 all messages"),
                ("mentions", "🔔 mentions only"),
                ("mute", "🔇 mute"),
            ] {
                if ui
                    .selectable_label(current.as_deref() == Some(mode), mode_label)
                    .clicked()
                {
                    app.set_notification_mode(room_id, mode);
                }
            }
        });
}

pub(crate) fn draw_mention_suggestions(ui: &mut egui::Ui, app: &mut App, room_id: &str, is_main: bool) {
    let text = if is_main { &app.compose_text } else { &app.thread_compose_text };
    let Some(query) = crate::app::active_mention_query(text).map(|s| s.to_string()) else {
        return;
    };
    let candidates = app.mention_candidates(room_id, &query);
    if candidates.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("mention:").weak().small());
        for (_, name) in &candidates {
            if ui.small_button(format!("@{name}")).clicked() {
                let text = if is_main {
                    &mut app.compose_text
                } else {
                    &mut app.thread_compose_text
                };
                crate::app::insert_mention(text, name);
            }
        }
    });
}

/// Shared between the main timeline and the thread panel. `room_id` and
/// `edit_thread_id` say which compose box "edit"/"reply" actions on this
/// message should act through — `edit_thread_id` is `None` when drawing in
/// the main timeline, or `Some(root_event_id)` when drawing inside that
/// thread's panel (not necessarily `event`'s own thread membership — e.g.
/// a thread's root message is drawn with `edit_thread_id: None` in the
/// main timeline and `Some(itself)` inside its own thread panel).
pub fn draw_message(
    ui: &mut egui::Ui,
    app: &mut App,
    event: &TimelineEvent,
    clicked_thread: &mut Option<TimelineEvent>,
    room_id: &str,
    edit_thread_id: Option<&str>,
) {
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(4.0, 4.0))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(&event.sender_name).strong());
                ui.label(egui::RichText::new(format_time(event.timestamp)).weak().small());
            });

            if let Some(reply_id) = &event.reply_to_event_id {
                // A preview snippet is only available straight from the
                // worker when matrix-sdk-ui already had the target loaded
                // locally (see `convert.rs`'s `reply_preview`) — e.g. not
                // for a live `/sync` message. Fall back to looking the
                // target up among whatever's already loaded on screen
                // (`App::find_event`) before giving up and just saying
                // *that* it's a reply — resolving it from the server would
                // need an extra fetch this app doesn't do.
                let owned_preview;
                let text = if let Some(preview) = &event.reply_to_preview {
                    preview.as_str()
                } else if let Some(target) = app.find_event(room_id, reply_id) {
                    owned_preview = format!("{}: {}", target.sender_name, target.body);
                    owned_preview.as_str()
                } else {
                    "replying to a message"
                };
                ui.label(
                    egui::RichText::new(format!("↩ {}", truncate(text, 60)))
                        .weak()
                        .small()
                        .italics(),
                );
            }

            match event.msg_type.as_str() {
                "notice" => {
                    ui.label(egui::RichText::new(&event.body).italics().weak());
                }
                "video" => {
                    ui.label(egui::RichText::new(&event.body).weak().small());

                    if let Some(thumb_mxc) = &event.thumbnail_url {
                        let thumb_key = format!("{}_thumb", event.event_id);
                        draw_lazy_image(ui, app, &thumb_key, thumb_mxc, 280.0, "thumbnail");
                    }

                    if let Some(video_mxc) = &event.media_url {
                        if ui.button("[ ▶ play video ]").clicked() {
                            app.play_video(video_mxc, &event.body);
                        }
                    }
                }
                // Element (and other clients) sometimes send images as a
                // generic "m.file" upload rather than "m.image" — e.g. when
                // dragged in from a file manager — which previously fell
                // through to plain body text with the attachment silently
                // dropped. Render those the same way as a proper image
                // message if the filename looks like one.
                "image" | "file" if event.msg_type == "image" || looks_like_image(&event.body) => {
                    ui.label(egui::RichText::new(&event.body).weak().small());
                    if let Some(mxc_uri) = &event.media_url {
                        draw_lazy_image(ui, app, &event.event_id, mxc_uri, 320.0, "ảnh");
                    }
                }
                "file" => {
                    ui.label(egui::RichText::new(format!("📎 {}", event.body)).weak().small());
                }
                // Covers "text" (the common case) and anything
                // unrecognized — rendered as Markdown (bold/italic/code/
                // links/lists/etc.) rather than plain text. The other arms
                // above keep plain `RichText` styling (italics for
                // notices, dim caption style for attachments) since
                // swapping those to the Markdown viewer would lose that
                // styling for comparatively little benefit — captions and
                // system notices are rarely formatted.
                _ => {
                    egui_commonmark::CommonMarkViewer::new().show(
                        ui,
                        &mut app.markdown_cache,
                        &event.body,
                    );
                }
            }

            ui.horizontal(|ui| {
                // Starting/opening a thread only makes sense from the main
                // timeline — nested threads aren't a thing in Matrix, so
                // this button doesn't appear on messages already inside a
                // thread panel (`edit_thread_id.is_some()`).
                if edit_thread_id.is_none() {
                    let thread_label = match event.thread_count {
                        Some(count) if count > 0 => format!("[ thread: {count} replies ]"),
                        _ => "[ reply in thread ]".to_string(),
                    };
                    if ui.small_button(thread_label).clicked() {
                        *clicked_thread = Some(event.clone());
                    }
                }

                // A plain reply, unlike "reply in thread" above, works
                // from anywhere — inside a thread panel too.
                if ui.small_button("[ reply ]").clicked() {
                    app.start_reply(room_id, event, edit_thread_id.map(|s| s.to_string()));
                }

                if event.is_own {
                    if matches!(event.msg_type.as_str(), "text" | "notice") {
                        if ui.small_button("[ edit ]").clicked() {
                            app.start_edit(room_id, event, edit_thread_id.map(|s| s.to_string()));
                        }
                    }
                    if ui.small_button("[ delete ]").clicked() {
                        app.delete_message(room_id, &event.event_id);
                    }
                }
            });
        });
    ui.add_space(2.0);
}

/// Renders a cached image, or — if not cached — a button the user has to
/// click to actually fetch and show it. Nothing is downloaded just because
/// a message scrolled into view; opening a room with many attachments
/// shouldn't kick off any network traffic until the user asks to see one.
fn draw_lazy_image(ui: &mut egui::Ui, app: &mut App, key: &str, mxc_uri: &str, max_width: f32, label: &str) {
    if let Some(texture) = app.image_cache.get(key).cloned() {
        let ctx = ui.ctx().clone();
        app.touch_image_public(key, &ctx);
        ui.add(egui::Image::from_texture(&texture).max_width(max_width));
        return;
    }

    if app.image_requested.contains(key) {
        ui.label(egui::RichText::new(format!("loading {label}...")).weak().small());
        return;
    }

    if ui.button(format!("[ xem {label} ]")).clicked() {
        let ctx = ui.ctx().clone();
        app.request_image(key, mxc_uri, &ctx);
    }
}

fn looks_like_image(filename: &str) -> bool {
    let lower = filename.to_lowercase();
    ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
        .iter()
        .any(|ext| lower.ends_with(&format!(".{ext}")))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max_chars).collect();
        t.push('…');
        t
    }
}

fn format_time(ts_millis: i64) -> String {
    // Minimal formatting without pulling in a chrono dependency: show
    // HH:MM in UTC. Swap for `chrono`/`time` if local-time display matters.
    let secs = ts_millis / 1000;
    let hours = (secs / 3600) % 24;
    let minutes = (secs / 60) % 60;
    format!("{hours:02}:{minutes:02}")
}
