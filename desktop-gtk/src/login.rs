//! Sign-in: password or SSO/OAuth (the worker opens the browser and waits
//! on its loopback redirect).

use std::rc::Rc;

use adw::prelude::*;
use chosua_core::Command;

use crate::window::Ctx;

pub struct LoginPage {
    pub root: gtk::Widget,
    error: gtk::Label,
    buttons: gtk::Box,
}

impl LoginPage {
    pub fn new(ctx: Rc<Ctx>) -> Rc<Self> {
        let homeserver = adw::EntryRow::builder().title("Máy chủ").text("matrix.org").build();
        let username = adw::EntryRow::builder().title("Tên đăng nhập").build();
        let password = adw::PasswordEntryRow::builder().title("Mật khẩu").build();
        let group = adw::PreferencesGroup::new();
        group.add(&homeserver);
        group.add(&username);
        group.add(&password);

        let error = gtk::Label::builder().wrap(true).visible(false).css_classes(["error"]).build();
        let login = gtk::Button::builder().label("Đăng nhập").css_classes(["suggested-action", "pill"]).build();
        let sso = gtk::Button::builder().label("Đăng nhập bằng SSO").css_classes(["pill"]).build();
        let buttons = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        buttons.append(&login);
        buttons.append(&sso);

        let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();
        body.append(&group);
        body.append(&error);
        body.append(&buttons);
        let page = adw::StatusPage::builder()
            .title("ChoSua")
            .description("Đăng nhập tài khoản Matrix")
            .icon_name("chat-message-new-symbolic")
            .child(&adw::Clamp::builder().maximum_size(420).child(&body).build())
            .build();

        let this = Rc::new(LoginPage { root: page.upcast(), error, buttons });
        let submit = {
            let (this, ctx, homeserver, username, password) =
                (this.clone(), ctx.clone(), homeserver.clone(), username.clone(), password.clone());
            move || {
                this.busy(true);
                ctx.send(Command::LoginPassword {
                    homeserver: homeserver.text().trim().to_string(),
                    username: username.text().trim().to_string(),
                    password: password.text().to_string(),
                });
            }
        };
        login.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        password.connect_entry_activated(move |_| submit());
        {
            let this = this.clone();
            sso.connect_clicked(move |_| {
                this.busy(true);
                ctx.send(Command::LoginOAuth { homeserver: homeserver.text().trim().to_string() });
            });
        }
        this
    }

    fn busy(&self, busy: bool) {
        self.buttons.set_sensitive(!busy);
        if busy {
            self.error.set_visible(false);
        }
    }

    pub fn show_error(&self, msg: &str) {
        self.busy(false);
        self.error.set_label(msg);
        self.error.set_visible(true);
    }
}
