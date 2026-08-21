use eframe::egui;

use crate::app::{App, RightPanel};
use crate::models::TimelineEvent;
use crate::ui::timeline::draw_message;

pub fn show(app: &mut App, ctx: &egui::Context) {
    match &app.right_panel {
        RightPanel::None => {}
        RightPanel::ThreadsList(_) => show_threads_list(app, ctx),
        RightPanel::Thread { .. } => show_thread(app, ctx),
    }
}

/// Threads popup — either scoped to one room (`App::open_room_threads`, the
/// per-room "[ threads ]" button) or global across every joined room
/// (`App::open_global_threads`), grouped here by room name either way.
fn show_threads_list(app: &mut App, ctx: &egui::Context) {
    let RightPanel::ThreadsList(scope) = &app.right_panel else {
        return;
    };
    let scope = scope.clone();

    let mut close = false;
    let mut selected: Option<(String, TimelineEvent)> = None;

    let title = if scope.is_some() { "threads" } else { "all threads" };

    egui::SidePanel::right("threads_list_panel")
        .resizable(true)
        .default_width(300.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("[x]").clicked() {
                        close = true;
                    }
                });
            });
            ui.separator();

            // The global popup (no scope) only makes sense as an
            // "activity" view — with every thread from every room in it
            // regardless of read state, it's just noise. The per-room
            // popup keeps showing everything, since there you're already
            // looking for a specific thread you remember, read or not.
            let unread_only = scope.is_none();

            let mut rooms: Vec<(String, String, Vec<TimelineEvent>)> = app
                .threads_by_room
                .iter()
                .filter(|(room_id, _)| scope.as_deref().is_none_or(|s| s == room_id.as_str()))
                .filter_map(|(room_id, threads)| {
                    let threads: Vec<TimelineEvent> = threads
                        .iter()
                        .filter(|t| !unread_only || app.thread_has_unread(room_id, &t.event_id))
                        .cloned()
                        .collect();
                    if threads.is_empty() {
                        return None;
                    }
                    let room_name = app
                        .rooms
                        .iter()
                        .find(|r| &r.room_id == room_id)
                        .map(|r| r.name.clone())
                        .unwrap_or_else(|| room_id.clone());
                    Some((room_id.clone(), room_name, threads))
                })
                .collect();
            rooms.sort_by(|a, b| a.1.cmp(&b.1));
            // Scoped to a single room — its name is already implied by
            // which room you're in, so skip the redundant heading below.
            let show_room_headings = scope.is_none();

            if rooms.is_empty() {
                let msg = if unread_only {
                    "no unread threads"
                } else {
                    "no threads yet"
                };
                ui.label(egui::RichText::new(msg).weak());
            }

            egui::ScrollArea::vertical().show(ui, |ui| {
                for (room_id, room_name, threads) in rooms {
                    if show_room_headings {
                        ui.label(egui::RichText::new(room_name).weak().small());
                    }
                    for thread in threads {
                        let unread = app.thread_has_unread(&room_id, &thread.event_id);
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if unread {
                                    ui.label(
                                        egui::RichText::new("●")
                                            .color(egui::Color32::from_rgb(90, 200, 120)),
                                    );
                                }
                                ui.label(
                                    egui::RichText::new(&thread.sender_name)
                                        .strong()
                                        .color(if unread {
                                            ui.visuals().strong_text_color()
                                        } else {
                                            ui.visuals().text_color()
                                        }),
                                );
                            });
                            ui.label(&thread.body);
                            let count = thread.thread_count.unwrap_or(0);
                            let reply_label = if unread {
                                format!("{count} replies (new) →")
                            } else {
                                format!("{count} replies →")
                            };
                            if ui.small_button(reply_label).clicked() {
                                selected = Some((room_id.clone(), thread.clone()));
                            }
                        });
                        ui.add_space(4.0);
                    }
                    ui.add_space(6.0);
                }
            });
        });

    if let Some((room_id, root)) = selected {
        app.open_thread(room_id, root);
    }
    if close {
        app.right_panel = RightPanel::None;
    }
}

fn show_thread(app: &mut App, ctx: &egui::Context) {
    let RightPanel::Thread { root, events } = &app.right_panel else {
        return;
    };
    let root = root.clone();
    let events = events.clone();

    let mut close = false;
    let mut clicked_thread = None; // unused here, nested threads not supported

    let room_id = app.selected_room.clone().unwrap_or_default();

    egui::SidePanel::right("thread_panel")
        .resizable(true)
        .default_width(320.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("thread");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("[x]").clicked() {
                        close = true;
                    }
                });
            });
            ui.separator();

            // Same reasoning as the main timeline's compose box: a nested
            // bottom panel reserves exactly the height its own content
            // needs (including the mention row, which only shows up
            // sometimes) rather than a fixed guess that the mention row
            // could push past, hiding the input.
            egui::TopBottomPanel::bottom("thread_compose").show_inside(ui, |ui| {
                ui.add_space(4.0);
                if crate::ui::timeline::draw_pending_image_preview(
                    ui,
                    app,
                    &room_id,
                    Some(root.event_id.as_str()),
                ) {
                    ui.add_space(4.0);
                    return;
                }
                crate::ui::timeline::draw_mention_suggestions(ui, app, &room_id, false);
                crate::ui::timeline::draw_editing_indicator(ui, app, Some(root.event_id.as_str()));
                crate::ui::timeline::draw_reply_indicator(ui, app, Some(root.event_id.as_str()));
                ui.horizontal(|ui| {
                    if ui.button("📎").on_hover_text("send an image").clicked() {
                        app.pick_image(ctx, &room_id, Some(root.event_id.clone()));
                    }
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut app.thread_compose_text)
                            .desired_width(ui.available_width() - 60.0)
                            .hint_text("reply... (Ctrl+V to paste an image)"),
                    );
                    let enter_pressed =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let paste_pressed = response.has_focus()
                        && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::V));

                    if ui.button("send").clicked() || enter_pressed {
                        app.send_message(&room_id, Some(root.event_id.clone()));
                    }
                    if paste_pressed {
                        app.paste_image_from_clipboard(ctx, &room_id, Some(root.event_id.clone()));
                    }
                    if app.is_sending(Some(root.event_id.as_str())) {
                        ui.label(egui::RichText::new("sending...").weak().small());
                    }
                });
                ui.add_space(4.0);
            });

            let scroll_output = egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .id_salt("thread_replies_scroll")
                .show(ui, |ui| {
                    draw_message(
                        ui,
                        app,
                        &root,
                        &mut clicked_thread,
                        &room_id,
                        Some(root.event_id.as_str()),
                    );
                    ui.separator();

                    // Same loading/at-the-start feedback as the main
                    // timeline, and for the same reason: otherwise
                    // scrolling up either does nothing visible or silently
                    // starts a fetch.
                    if app.is_thread_paginating(&room_id, &root.event_id) {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("loading older replies...").weak().small(),
                            );
                        });
                        ui.add_space(4.0);
                    } else if app.thread_reached_start(&room_id, &root.event_id)
                        && !events.is_empty()
                    {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new("— beginning of thread —").weak().small(),
                            );
                        });
                        ui.add_space(4.0);
                    }

                    ui.label(
                        egui::RichText::new(format!("{} replies", events.len()))
                            .weak()
                            .small(),
                    );
                    for event in &events {
                        draw_message(
                            ui,
                            app,
                            event,
                            &mut clicked_thread,
                            &room_id,
                            Some(root.event_id.as_str()),
                        );
                    }
                });

            // Infinite scroll for older thread replies — same hysteresis
            // reasoning as the main timeline's (see its comment): without
            // it, loading a page while already sitting near the top would
            // immediately trigger another load with no further scrolling.
            const NEAR_TOP_PX: f32 = 200.0;
            const REARM_PX: f32 = 500.0;
            let overflowing = scroll_output.content_size.y > scroll_output.inner_rect.height();
            let offset_y = scroll_output.state.offset.y;
            if offset_y > REARM_PX {
                app.rearm_thread_pagination(&room_id, &root.event_id);
            }
            if !events.is_empty() && overflowing && offset_y < NEAR_TOP_PX {
                app.maybe_load_more_thread_replies(&room_id, &root.event_id);
            }
        });

    if close {
        app.right_panel = RightPanel::None;
    }
}
