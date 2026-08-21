use eframe::egui;

use crate::app::App;

pub fn show(app: &mut App, ctx: &egui::Context) {
    egui::SidePanel::left("room_list")
        .resizable(true)
        .default_width(220.0)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("CHATS").weak().small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("[sec]").clicked() {
                        app.show_security_panel = true;
                    }
                    if ui
                        .small_button("[+]")
                        .on_hover_text("create a room or space")
                        .clicked()
                    {
                        app.open_create_room_dialog();
                    }
                    let threads_label = if app.has_unread_threads() {
                        "[ threads ● ]"
                    } else {
                        "[ threads ]"
                    };
                    if ui
                        .add(egui::Button::new(egui::RichText::new(threads_label).color(
                            if app.has_unread_threads() {
                                egui::Color32::from_rgb(90, 200, 120)
                            } else {
                                ui.visuals().text_color()
                            },
                        )).small())
                        .on_hover_text("threads with new replies, across all rooms")
                        .clicked()
                    {
                        app.open_global_threads();
                    }
                });
            });

            draw_space_picker(ui, app);

            ui.add(
                egui::TextEdit::singleline(&mut app.room_filter).hint_text("search rooms..."),
            );
            ui.separator();

            egui::ScrollArea::vertical().show(ui, |ui| {
                let filter = app.room_filter.trim().to_lowercase();
                let space_filter = app
                    .selected_space
                    .as_ref()
                    .and_then(|s| app.space_children.get(s).cloned());

                let room_ids: Vec<(String, String, Option<String>, u64, bool, bool)> = app
                    .rooms
                    .iter()
                    // A Space itself doesn't belong in the message-room
                    // list — it's picked from the row above instead.
                    .filter(|r| !r.is_space)
                    .filter(|r| filter.is_empty() || r.name.to_lowercase().contains(&filter))
                    .filter(|r| {
                        space_filter
                            .as_ref()
                            .is_none_or(|children| children.contains(&r.room_id))
                    })
                    .map(|r| {
                        (
                            r.room_id.clone(),
                            r.name.clone(),
                            r.last_message.clone(),
                            r.unread_count,
                            r.is_encrypted,
                            r.is_invite,
                        )
                    })
                    .collect();

                let mut accept: Option<String> = None;
                let mut decline: Option<String> = None;

                for (room_id, name, last_message, unread, encrypted, is_invite) in room_ids {
                    if is_invite {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.label(egui::RichText::new(format!("invite: {name}")).strong());
                            ui.horizontal(|ui| {
                                if ui.small_button("accept").clicked() {
                                    accept = Some(room_id.clone());
                                }
                                if ui.small_button("decline").clicked() {
                                    decline = Some(room_id.clone());
                                }
                            });
                        });
                        ui.add_space(4.0);
                        continue;
                    }

                    let is_selected = app.selected_room.as_deref() == Some(room_id.as_str());

                    let lock = if encrypted { "[e] " } else { "" };
                    let has_unread = unread > 0;
                    let label = format!("{lock}{name}");

                    let response = ui.horizontal(|ui| {
                        // Unread rooms get bold text, Element-style, so
                        // they stand out at a glance instead of relying on
                        // a small "(N)" suffix easy to miss while scanning.
                        let text = if has_unread {
                            egui::RichText::new(&label).strong()
                        } else {
                            egui::RichText::new(&label)
                        };
                        let response = ui.selectable_label(is_selected, text);
                        if has_unread {
                            let badge_text = if unread > 99 {
                                "99+".to_string()
                            } else {
                                unread.to_string()
                            };
                            let badge_color = if app.notification_mode(&room_id) == Some("mute") {
                                egui::Color32::GRAY
                            } else {
                                egui::Color32::from_rgb(220, 70, 70)
                            };
                            egui::Frame::none()
                                .fill(badge_color)
                                .rounding(8.0)
                                .inner_margin(egui::Margin::symmetric(6.0, 1.0))
                                .show(ui, |ui| {
                                    ui.label(
                                        egui::RichText::new(badge_text)
                                            .color(egui::Color32::WHITE)
                                            .small(),
                                    );
                                });
                        }
                        // Brief flash when a live message just arrived via
                        // /sync for this room — visible feedback that sync
                        // is actually delivering data, even for a room
                        // that isn't the one currently open.
                        if app.room_activity_flash(&room_id) {
                            ui.label(
                                egui::RichText::new("●")
                                    .color(egui::Color32::from_rgb(90, 200, 120)),
                            );
                            ctx.request_repaint_after(std::time::Duration::from_millis(80));
                        }
                        response
                    })
                    .inner;
                    if let Some(preview) = &last_message {
                        ui.label(
                            egui::RichText::new(truncate(preview, 40))
                                .weak()
                                .small(),
                        );
                    }
                    ui.add_space(4.0);

                    if response.clicked() {
                        app.select_room(&room_id);
                    }
                }

                if let Some(room_id) = accept {
                    app.accept_invite(&room_id);
                }
                if let Some(room_id) = decline {
                    app.decline_invite(&room_id);
                }
            });
        });

    draw_create_room_dialog(ctx, app);
}

/// A row of Space buttons ("[ All ]" plus one per joined Space) that
/// filters the room list below to that Space's children. Matrix Spaces are
/// just rooms with `m.room.create`'s `type` set to `m.space`
/// (`RoomSummary::is_space`) that group other rooms via `m.space.child`
/// state events — this is the standard way Matrix organizes rooms into
/// something like Discord servers/categories.
fn draw_space_picker(ui: &mut egui::Ui, app: &mut App) {
    let spaces: Vec<(String, String)> = app
        .rooms
        .iter()
        .filter(|r| r.is_space && !r.is_invite)
        .map(|r| (r.room_id.clone(), r.name.clone()))
        .collect();

    if spaces.is_empty() {
        return;
    }

    ui.horizontal_wrapped(|ui| {
        if ui
            .selectable_label(app.selected_space.is_none(), "[ all ]")
            .clicked()
        {
            app.select_space(None);
        }
        for (room_id, name) in spaces {
            let is_selected = app.selected_space.as_deref() == Some(room_id.as_str());
            if ui.selectable_label(is_selected, truncate(&name, 14)).clicked() {
                app.select_space(Some(room_id));
            }
        }
    });
    ui.add_space(2.0);
}

fn draw_create_room_dialog(ctx: &egui::Context, app: &mut App) {
    if !app.show_create_room_dialog {
        return;
    }

    let mut open = true;
    egui::Window::new("create room")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label("name:");
            ui.text_edit_singleline(&mut app.create_room_name);
            ui.checkbox(&mut app.create_room_is_public, "public");
            ui.checkbox(
                &mut app.create_room_is_space,
                "this is a space (groups other rooms, not for messages)",
            );
            ui.horizontal(|ui| {
                if ui.button("[ create ]").clicked() {
                    app.submit_create_room();
                }
                if ui.button("[ cancel ]").clicked() {
                    app.show_create_room_dialog = false;
                }
            });
        });
    if !open {
        app.show_create_room_dialog = false;
    }
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
