use eframe::egui;

use crate::app::App;

/// Draws the emoji-compare popup (when a SAS verification is in progress)
/// and the security settings window (verify device / restore history).
/// Called every frame from main.rs; each `egui::Window` only actually shows
/// itself when its `open` flag / `Option` is set.
pub fn show(app: &mut App, ctx: &egui::Context) {
    show_verification_popup(app, ctx);
    show_security_window(app, ctx);
}

fn show_verification_popup(app: &mut App, ctx: &egui::Context) {
    let Some(emojis) = app.verification_emojis.clone() else {
        return;
    };

    egui::Window::new("verify this device")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.label("Confirm these emoji match on your other device:");
            ui.add_space(8.0);

            egui::Grid::new("emoji_grid").num_columns(4).show(ui, |ui| {
                for (i, (symbol, description)) in emojis.iter().enumerate() {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new(symbol).size(22.0));
                        ui.label(egui::RichText::new(description).small().weak());
                    });
                    if (i + 1) % 4 == 0 {
                        ui.end_row();
                    }
                }
            });

            ui.add_space(12.0);
            ui.horizontal(|ui| {
                if ui.button("they don't match").clicked() {
                    app.cancel_verification();
                }
                if ui.button("they match").clicked() {
                    app.confirm_verification();
                }
            });
        });
}

fn show_security_window(app: &mut App, ctx: &egui::Context) {
    if !app.show_security_panel {
        return;
    }

    let mut open = true;
    egui::Window::new("security")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_width(360.0);

            ui.label(egui::RichText::new("VERIFY THIS DEVICE").strong().small());
            ui.label(
                "Approve this device from your phone or another signed-in \
                 client so it's trusted for new messages.",
            );
            if ui.button("verify this device").clicked() {
                app.start_self_verification();
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(12.0);

            ui.label(egui::RichText::new("RESTORE HISTORY").strong().small());
            ui.label(
                "Enter your Secure Backup recovery key to decrypt messages \
                 sent before this device existed.",
            );
            ui.text_edit_singleline(&mut app.recovery_key_input);
            if ui.button("restore history").clicked() {
                app.recover_with_key();
            }

            if let Some(status) = &app.security_status {
                ui.add_space(10.0);
                ui.label(egui::RichText::new(status).weak().small());
            }
        });

    if !open {
        app.show_security_panel = false;
    }
}
