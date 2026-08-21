use eframe::egui;

use crate::app::App;
use crate::command::Command;

pub fn show(app: &mut App, ctx: &egui::Context) {
    egui::CentralPanel::default().show(ctx, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(80.0);
            ui.heading("matrix login");
            ui.add_space(20.0);

            egui::Grid::new("login_grid")
                .num_columns(2)
                .spacing([8.0, 8.0])
                .show(ui, |ui| {
                    ui.label("homeserver");
                    ui.text_edit_singleline(&mut app.homeserver);
                    ui.end_row();

                    ui.label("username");
                    ui.text_edit_singleline(&mut app.username);
                    ui.end_row();

                    ui.label("password");
                    ui.add(egui::TextEdit::singleline(&mut app.password).password(true));
                    ui.end_row();
                });

            ui.add_space(12.0);

            if let Some(err) = &app.login_error {
                ui.colored_label(egui::Color32::RED, err);
                ui.add_space(8.0);
            }

            let login_clicked = ui
                .add_enabled(!app.logging_in, egui::Button::new(if app.logging_in {
                    "signing in..."
                } else {
                    "sign in"
                }))
                .clicked();

            if login_clicked {
                app.logging_in = true;
                app.login_error = None;
                app.tx
                    .send(Command::LoginPassword {
                        homeserver: app.homeserver.clone(),
                        username: app.username.clone(),
                        password: app.password.clone(),
                    })
                    .ok();
            }

            ui.add_space(8.0);
            ui.label(egui::RichText::new("— or —").weak().small());
            ui.add_space(8.0);

            if ui
                .add_enabled(!app.logging_in, egui::Button::new("continue with sso / oauth"))
                .clicked()
            {
                app.logging_in = true;
                app.login_error = None;
                app.tx
                    .send(Command::LoginOAuth {
                        homeserver: app.homeserver.clone(),
                    })
                    .ok();
            }
        });
    });
}
