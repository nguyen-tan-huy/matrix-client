//! Account settings: profile (name, avatar), security (verify this device
//! with emoji, recovery key, room-key import), the LVX API key the summary
//! uses, and sign-out. Opened from the own-avatar button in the sidebar.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use chosua_core::models::OwnProfile;
use chosua_core::Command;
use gtk::{gio, glib};

use crate::window::Ctx;

pub struct Settings {
    ctx: Rc<Ctx>,
    on_changed: RefCell<Option<Rc<dyn Fn()>>>,
    pub button: gtk::Button,
    avatar: adw::Avatar,
    profile: RefCell<Option<OwnProfile>>,
    open: RefCell<Option<Open>>,
    /// where the room-key export being prepared will be saved
    export_target: Rc<RefCell<Option<gio::File>>>,
}

/// Widgets of the open dialog that worker answers update.
struct Open {
    dialog: adw::PreferencesDialog,
    avatar: adw::Avatar,
    name: adw::EntryRow,
    user_id: adw::ActionRow,
    verify: adw::ActionRow,
    verify_box: gtk::Box,
    recovery: adw::ActionRow,
    import: adw::ActionRow,
    export: adw::ActionRow,
    lvx: adw::PasswordEntryRow,
}

async fn pick_file(parent: Option<gtk::Window>, title: &str, mime: Option<&str>) -> Option<gio::File> {
    let dialog = gtk::FileDialog::builder().title(title).modal(true).build();
    if let Some(mime) = mime {
        let filter = gtk::FileFilter::new();
        filter.add_mime_type(mime);
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
    }
    dialog.open_future(parent.as_ref()).await.ok()
}

/// Starting the account's encryption over is destructive and needs the password: ask first.
fn ask_reset_identity(ctx: Rc<Ctx>, parent: &gtk::Widget, recovery: adw::ActionRow) {
    let ask = adw::AlertDialog::builder()
        .heading("Bắt đầu lại mã hoá của tài khoản?")
        .body("Bản sao lưu khoá cũ và danh tính mã hoá cũ sẽ bị xoá, thay bằng bản mới; thiết bị này được xác minh lại (tài khoản OAuth/SSO sẽ được mở trình duyệt để duyệt, không cần mật khẩu). Tin nhắn đang đọc được trên thiết bị này vẫn đọc được; tin chỉ nằm trong bản sao lưu cũ sẽ không đọc lại được. Các thiết bị khác phải xác minh lại. Nên xuất khoá phòng ra tệp (mục bên dưới) trước.")
        .build();
    let password = gtk::PasswordEntry::builder().placeholder_text("Mật khẩu (bỏ trống nếu đăng nhập OAuth/SSO)").show_peek_icon(true).activates_default(true).build();
    ask.set_extra_child(Some(&password));
    ask.add_responses(&[("cancel", "Huỷ"), ("reset", "Bắt đầu lại")]);
    ask.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
    ask.set_default_response(Some("cancel"));
    ask.set_close_response("cancel");
    ask.connect_response(None, move |_, response| {
        if response == "reset" {
            recovery.set_subtitle("Đang bắt đầu lại…");
            ctx.send(Command::ResetIdentity { password: password.text().to_string() });
        }
    });
    ask.present(Some(parent));
}

impl Settings {
    pub fn new(ctx: Rc<Ctx>) -> Rc<Self> {
        let avatar = adw::Avatar::builder().size(28).show_initials(true).build();
        let button = gtk::Button::builder()
            .child(&avatar)
            .tooltip_text("Tài khoản và cài đặt")
            .css_classes(["flat", "circular", "own-avatar"])
            .build();
        let this = Rc::new(Settings {
            ctx,
            on_changed: RefCell::default(),
            button: button.clone(),
            avatar,
            profile: RefCell::default(),
            open: RefCell::default(),
            export_target: Rc::default(),
        });
        {
            let this = Rc::downgrade(&this);
            button.connect_clicked(move |b| {
                if let Some(this) = this.upgrade() {
                    this.show(b);
                }
            });
        }
        this
    }

    /// Something in the app settings changed (already saved): re-apply.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        *self.on_changed.borrow_mut() = Some(Rc::new(f));
    }

    fn changed(&self) {
        let f = self.on_changed.borrow().clone();
        if let Some(f) = f {
            f();
        }
    }

    pub fn show_shortcuts(self: &Rc<Self>, anchor: &gtk::Button) {
        self.show(anchor);
        if let Some(o) = self.open.borrow().as_ref() {
            o.dialog.set_visible_page_name("shortcuts");
        }
    }

    /// Look and behaviour of this app on this machine (`config.rs`).
    fn app_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let cfg = crate::config::get();
        let page = adw::PreferencesPage::builder().title("Ứng dụng").icon_name("applications-system-symbolic").name("app").build();

        let look = adw::PreferencesGroup::builder().title("Giao diện").build();
        let themes = gtk::StringList::new(&["Theo hệ thống", "Sáng", "Tối"]);
        let theme = adw::ComboRow::builder()
            .title("Chủ đề")
            .subtitle("Khi bật liquid glass, màu chữ vẫn tự theo nền phía sau")
            .model(&themes)
            .selected(match cfg.theme.as_str() {
                "light" => 1,
                "dark" => 2,
                _ => 0,
            })
            .build();
        let families = gtk::StringList::new(&crate::config::FONT_FAMILIES.iter().map(|(l, _)| *l).collect::<Vec<_>>());
        let family = adw::ComboRow::builder()
            .title("Kiểu chữ")
            .model(&families)
            .selected(crate::config::FONT_FAMILIES.iter().position(|(_, v)| *v == cfg.font_family).unwrap_or(0) as u32)
            .build();
        let size_on = adw::SwitchRow::builder()
            .title("Cỡ chữ riêng")
            .subtitle("Tắt: theo cỡ chữ của hệ thống")
            .active(cfg.font_size != 0)
            .build();
        let font = adw::SpinRow::builder()
            .title("Cỡ chữ (px)")
            .adjustment(&gtk::Adjustment::new(if cfg.font_size == 0 { 15.0 } else { cfg.font_size as f64 }, 12.0, 22.0, 1.0, 2.0, 0.0))
            .sensitive(cfg.font_size != 0)
            .build();
        let weights = gtk::StringList::new(&["Mảnh (300)", "Thường (400)", "Vừa (500)", "Đậm vừa (600)", "Đậm (700)", "Rất đậm (800)"]);
        let weight = adw::ComboRow::builder()
            .title("Độ đậm chữ tin nhắn")
            .model(&weights)
            .selected(((cfg.message_weight.clamp(300, 800) - 300) / 100) as u32)
            .build();
        {
            let this = Rc::downgrade(self);
            weight.connect_selected_notify(move |r| {
                let v = 300 + r.selected().min(5) * 100;
                crate::config::update(|c| c.message_weight = v);
                if let Some(this) = this.upgrade() {
                    this.changed();
                }
            });
        }
        look.add(&theme);
        look.add(&weight);
        look.add(&family);
        look.add(&size_on);
        look.add(&font);
        {
            let this = Rc::downgrade(self);
            family.connect_selected_notify(move |r| {
                let v = crate::config::FONT_FAMILIES.get(r.selected() as usize).map(|(_, v)| *v).unwrap_or("");
                crate::config::update(|c| c.font_family = v.into());
                if let Some(this) = this.upgrade() {
                    this.changed();
                }
            });
        }
        {
            let (this, font) = (Rc::downgrade(self), font.clone());
            size_on.connect_active_notify(move |r| {
                font.set_sensitive(r.is_active());
                let v = if r.is_active() { font.value() as u32 } else { 0 };
                crate::config::update(|c| c.font_size = v);
                if let Some(this) = this.upgrade() {
                    this.changed();
                }
            });
        }
        {
            let this = Rc::downgrade(self);
            theme.connect_selected_notify(move |r| {
                let v = ["system", "light", "dark"][r.selected().min(2) as usize];
                crate::config::update(|c| c.theme = v.into());
                if let Some(this) = this.upgrade() {
                    this.changed();
                }
            });
        }
        {
            let this = Rc::downgrade(self);
            font.connect_value_notify(move |r| {
                let v = r.value() as u32;
                crate::config::update(|c| c.font_size = v);
                if let Some(this) = this.upgrade() {
                    this.changed();
                }
            });
        }

        let chat = adw::PreferencesGroup::builder().title("Trò chuyện").build();
        let switch = |title: &str, subtitle: &str, on: bool, set: fn(&mut crate::config::Config, bool)| {
            let row = adw::SwitchRow::builder().title(title).subtitle(subtitle).active(on).build();
            let this = Rc::downgrade(self);
            row.connect_active_notify(move |r| {
                let v = r.is_active();
                crate::config::update(|c| set(c, v));
                if let Some(this) = this.upgrade() {
                    this.changed();
                }
            });
            row
        };
        chat.add(&switch(
            "Enter để gửi",
            "Tắt: Ctrl+Enter gửi, Enter xuống dòng",
            cfg.enter_sends,
            |c, v| c.enter_sends = v,
        ));
        chat.add(&switch(
            "Luôn mở thread khi vào phòng",
            "Khung danh sách thread của phòng mở sẵn bên phải",
            cfg.auto_threads,
            |c, v| c.auto_threads = v,
        ));
        chat.add(&switch(
            "Báo “đang nhập…”",
            "Cho người khác thấy khi bạn đang gõ",
            cfg.typing_notices,
            |c, v| c.typing_notices = v,
        ));

        let system = adw::PreferencesGroup::builder().title("Hệ thống").build();
        system.add(&switch("Thông báo", "Thông báo desktop khi có tin mới", cfg.notifications, |c, v| c.notifications = v));
        system.add(&switch(
            "Đóng vào khay hệ thống",
            "Nút đóng chỉ ẩn cửa sổ; app vẫn chạy để nhận thông báo",
            cfg.close_to_tray,
            |c, v| c.close_to_tray = v,
        ));
        system.add(&switch(
            "Khởi động ẩn",
            "Mở app chỉ với biểu tượng ở khay",
            cfg.start_hidden,
            |c, v| c.start_hidden = v,
        ));

        page.add(&look);
        page.add(&chat);
        page.add(&system);
        page
    }

    /// Every rebindable shortcut: click to record a new one.
    fn shortcuts_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::builder().title("Phím tắt").icon_name("input-keyboard-symbolic").name("shortcuts").build();
        let group = adw::PreferencesGroup::builder()
            .title("Phím tắt")
            .description("Bấm vào một dòng rồi nhấn tổ hợp phím mới. Esc để huỷ, Backspace để tắt phím tắt đó.")
            .build();
        let fixed = adw::PreferencesGroup::builder().title("Khi soạn tin").build();
        for (keys, what) in [
            ("Enter / Ctrl+Enter", "Gửi (theo tuỳ chọn “Enter để gửi”)"),
            ("Shift+Enter", "Xuống dòng"),
            ("Esc", "Huỷ trả lời, sửa hoặc ghi âm"),
            ("↑ ↓ Tab", "Chọn người trong gợi ý @mention"),
            ("Ctrl+V", "Dán ảnh từ clipboard để gửi"),
        ] {
            fixed.add(&adw::ActionRow::builder().title(what).subtitle(keys).build());
        }
        let cfg = crate::config::get();
        for (id, what, _) in crate::config::ACTIONS {
            let label = gtk::ShortcutLabel::builder().accelerator(cfg.accel(id)).disabled_text("Tắt").valign(gtk::Align::Center).build();
            let reset = gtk::Button::builder()
                .icon_name("edit-undo-symbolic")
                .tooltip_text("Về mặc định")
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            let row = adw::ActionRow::builder().title(*what).activatable(true).build();
            row.add_suffix(&label);
            row.add_suffix(&reset);
            {
                let (this, label) = (Rc::downgrade(self), label.clone());
                row.connect_activated(move |row| {
                    if let Some(this) = this.upgrade() {
                        this.record_shortcut(row, id, &label);
                    }
                });
            }
            {
                let (this, label) = (Rc::downgrade(self), label.clone());
                reset.connect_clicked(move |_| {
                    crate::config::update(|c| {
                        c.shortcuts.remove(*id);
                    });
                    label.set_accelerator(&crate::config::get().accel(id));
                    if let Some(this) = this.upgrade() {
                        this.changed();
                    }
                });
            }
            group.add(&row);
        }
        page.add(&group);
        page.add(&fixed);
        page
    }

    /// A small dialog that takes the next key combination.
    fn record_shortcut(self: &Rc<Self>, anchor: &adw::ActionRow, id: &'static str, label: &gtk::ShortcutLabel) {
        let dialog = adw::AlertDialog::builder()
            .heading("Nhấn tổ hợp phím mới")
            .body("Esc để huỷ · Backspace để tắt")
            .build();
        dialog.add_response("cancel", "Huỷ");
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let (this, label, dialog2) = (Rc::downgrade(self), label.clone(), dialog.clone());
            keys.connect_key_pressed(move |_, key, _, state| {
                use gtk::gdk;
                let mods = state & gtk::accelerator_get_default_mod_mask();
                if matches!(
                    key,
                    gdk::Key::Shift_L | gdk::Key::Shift_R | gdk::Key::Control_L | gdk::Key::Control_R | gdk::Key::Alt_L
                        | gdk::Key::Alt_R | gdk::Key::Super_L | gdk::Key::Super_R | gdk::Key::Meta_L | gdk::Key::Meta_R
                ) {
                    return glib::Propagation::Stop;
                }
                let accel = if key == gdk::Key::Escape && mods.is_empty() {
                    dialog2.close();
                    return glib::Propagation::Stop;
                } else if key == gdk::Key::BackSpace && mods.is_empty() {
                    String::new() // disabled
                } else if mods.is_empty() && key.to_unicode().is_some() {
                    // a bare letter would fire while typing a message
                    return glib::Propagation::Stop;
                } else {
                    gtk::accelerator_name(key, mods).to_string()
                };
                let taken = crate::config::ACTIONS
                    .iter()
                    .find(|(other, ..)| *other != id && !accel.is_empty() && crate::config::get().accel(other) == accel);
                if let Some((_, what, _)) = taken {
                    dialog2.set_body(&format!("Đã dùng cho “{what}”. Chọn tổ hợp khác."));
                    return glib::Propagation::Stop;
                }
                crate::config::update(|c| {
                    c.shortcuts.insert(id.into(), accel.clone());
                });
                label.set_accelerator(&accel);
                dialog2.close();
                if let Some(this) = this.upgrade() {
                    this.changed();
                }
                glib::Propagation::Stop
            });
        }
        dialog.add_controller(keys);
        dialog.present(Some(anchor));
    }

    pub fn own_profile(&self, p: OwnProfile) {
        self.avatar.set_text(Some(&p.display_name));
        self.ctx.images.avatar(&self.avatar, p.avatar_url.as_deref());
        if let Some(o) = self.open.borrow().as_ref() {
            o.avatar.set_text(Some(&p.display_name));
            self.ctx.images.avatar(&o.avatar, p.avatar_url.as_deref());
            o.name.set_text(&p.display_name);
            o.user_id.set_subtitle(&p.user_id);
        }
        *self.profile.borrow_mut() = Some(p);
    }

    pub fn show(self: &Rc<Self>, anchor: &gtk::Button) {
        let ctx = self.ctx.clone();
        let dialog = adw::PreferencesDialog::builder().title("Cài đặt").build();

        // ---- profile
        let profile_page = adw::PreferencesPage::builder().title("Hồ sơ").icon_name("avatar-default-symbolic").build();
        let p = self.profile.borrow().clone();
        let avatar = adw::Avatar::builder().size(96).show_initials(true).text(p.as_ref().map(|p| p.display_name.as_str()).unwrap_or("")).build();
        self.ctx.images.avatar(&avatar, p.as_ref().and_then(|p| p.avatar_url.as_deref()));
        let change = gtk::Button::builder().label("Đổi ảnh đại diện").css_classes(["pill"]).halign(gtk::Align::Center).build();
        let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
        top.append(&avatar);
        top.append(&change);
        let top_group = adw::PreferencesGroup::new();
        top_group.add(&top);
        let name = adw::EntryRow::builder()
            .title("Tên hiển thị")
            .text(p.as_ref().map(|p| p.display_name.as_str()).unwrap_or(""))
            .show_apply_button(true)
            .build();
        let user_id = adw::ActionRow::builder()
            .title("Matrix ID")
            .subtitle(p.as_ref().map(|p| p.user_id.as_str()).unwrap_or(""))
            .subtitle_selectable(true)
            .build();
        let who = adw::PreferencesGroup::new();
        who.add(&name);
        who.add(&user_id);
        profile_page.add(&top_group);
        profile_page.add(&who);
        {
            let ctx = ctx.clone();
            name.connect_apply(move |e| ctx.send(Command::SetDisplayName { name: e.text().trim().to_string() }));
        }
        {
            let ctx = ctx.clone();
            change.connect_clicked(move |b| {
                let (ctx, window) = (ctx.clone(), b.root().and_downcast::<gtk::Window>());
                glib::spawn_future_local(async move {
                    let Some(file) = pick_file(window, "Ảnh đại diện", Some("image/*")).await else { return };
                    let Ok((bytes, _)) = file.load_contents_future().await else { return };
                    let (ctype, _) = gio::content_type_guess(file.basename().as_deref(), Some(&bytes[..]));
                    let mime = gio::content_type_get_mime_type(&ctype).map(|m| m.to_string()).unwrap_or_else(|| "image/png".into());
                    ctx.send(Command::SetAvatar { bytes: bytes.to_vec(), mime });
                });
            });
        }

        // ---- security
        let security = adw::PreferencesPage::builder().title("Bảo mật").icon_name("security-high-symbolic").build();
        let verify_group = adw::PreferencesGroup::builder()
            .title("Xác minh thiết bị")
            .description("So khớp emoji với một thiết bị khác đã đăng nhập để đọc được tin nhắn mã hoá cũ.")
            .build();
        let verify_btn = gtk::Button::builder().label("Bắt đầu").valign(gtk::Align::Center).css_classes(["pill", "suggested-action"]).build();
        let verify = adw::ActionRow::builder().title("Xác minh thiết bị này").build();
        verify.add_suffix(&verify_btn);
        let verify_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).visible(false).margin_top(12).build();
        verify_group.add(&verify);
        verify_group.add(&verify_box);
        {
            let (ctx, verify) = (ctx.clone(), verify.clone());
            verify_btn.connect_clicked(move |_| {
                verify.set_subtitle("Đang chờ thiết bị kia chấp nhận…");
                ctx.send(Command::StartSelfVerification);
            });
        }

        let recovery_group = adw::PreferencesGroup::builder()
            .title("Khôi phục")
            .description("Dùng recovery key (security key) để tải lại khoá mã hoá từ bản sao lưu.")
            .build();
        let recovery_key = adw::PasswordEntryRow::builder().title("Recovery key").show_apply_button(true).build();
        let recovery = adw::ActionRow::builder().title("Trạng thái").subtitle("—").build();
        recovery_group.add(&recovery_key);
        recovery_group.add(&recovery);
        // lost the key (or never made one): mint a new one
        let new_key_btn = gtk::Button::builder().label("Tạo key mới…").valign(gtk::Align::Center).css_classes(["pill"]).build();
        let new_key = adw::ActionRow::builder()
            .title("Tạo lại security key")
            .subtitle("Dùng khi bạn mất key cũ. Key cũ sẽ không còn dùng được.")
            .build();
        new_key.add_suffix(&new_key_btn);
        recovery_group.add(&new_key);
        // lost the key and the verification: start the account's encryption over
        let reset_btn = gtk::Button::builder().label("Bắt đầu lại…").valign(gtk::Align::Center).css_classes(["pill", "destructive-action"]).build();
        let reset = adw::ActionRow::builder()
            .title("Bắt đầu lại từ đầu")
            .subtitle("Mất cả key lẫn xác minh thiết bị: đặt lại danh tính mã hoá và tạo key mới.")
            .build();
        reset.add_suffix(&reset_btn);
        recovery_group.add(&reset);
        {
            let (ctx, recovery) = (ctx.clone(), recovery.clone());
            reset_btn.connect_clicked(move |b| ask_reset_identity(ctx.clone(), b.upcast_ref(), recovery.clone()));
        }
        {
            let (ctx, recovery) = (ctx.clone(), recovery.clone());
            new_key_btn.connect_clicked(move |b| {
                let confirm = adw::AlertDialog::builder()
                    .heading("Tạo security key mới?")
                    .body("Key cũ sẽ ngừng hoạt động. Key mới chỉ hiện một lần — hãy lưu lại ngay.")
                    .build();
                confirm.add_responses(&[("cancel", "Huỷ"), ("create", "Tạo key mới")]);
                confirm.set_response_appearance("create", adw::ResponseAppearance::Suggested);
                confirm.set_default_response(Some("cancel"));
                confirm.set_close_response("cancel");
                let (ctx, recovery) = (ctx.clone(), recovery.clone());
                confirm.connect_response(None, move |_, response| {
                    if response == "create" {
                        recovery.set_subtitle("Đang tạo key mới…");
                        ctx.send(Command::CreateRecoveryKey);
                    }
                });
                confirm.present(Some(b));
            });
        }
        {
            let (ctx, recovery) = (ctx.clone(), recovery.clone());
            recovery_key.connect_apply(move |e| {
                recovery.set_subtitle("Đang khôi phục…");
                ctx.send(Command::RecoverWithKey { recovery_key: e.text().trim().to_string() });
                e.set_text("");
            });
        }

        let import_group = adw::PreferencesGroup::builder()
            .title("Nhập khoá phòng")
            .description("Tệp xuất khoá (megolm) từ Element hoặc client khác, cùng mật khẩu đã đặt khi xuất.")
            .build();
        let passphrase = adw::PasswordEntryRow::builder().title("Mật khẩu của tệp").build();
        let import_btn = gtk::Button::builder().label("Chọn tệp…").valign(gtk::Align::Center).css_classes(["pill"]).build();
        let import = adw::ActionRow::builder().title("Tệp khoá").build();
        import.add_suffix(&import_btn);
        import_group.add(&passphrase);
        import_group.add(&import);
        {
            let (ctx, import) = (ctx.clone(), import.clone());
            import_btn.connect_clicked(move |b| {
                let (ctx, import, pass, window) = (ctx.clone(), import.clone(), passphrase.text().to_string(), b.root().and_downcast::<gtk::Window>());
                glib::spawn_future_local(async move {
                    let Some(file) = pick_file(window, "Tệp xuất khoá", None).await else { return };
                    let Ok((bytes, _)) = file.load_contents_future().await else { return };
                    import.set_subtitle("Đang nhập…");
                    ctx.send(Command::ImportRoomKeys { bytes: bytes.to_vec(), passphrase: pass });
                });
            });
        }
        // log out and back in without losing the history: save the keys to a
        // password-protected file now, import it after signing in again
        let export_group = adw::PreferencesGroup::builder()
            .title("Xuất khoá phòng")
            .description("Lưu khoá giải mã vào một tệp có mật khẩu trước khi đăng xuất. Đăng nhập lại rồi dùng “Nhập khoá phòng” để đọc lại tin nhắn cũ.")
            .build();
        let export_pass = adw::PasswordEntryRow::builder().title("Mật khẩu bảo vệ tệp").build();
        let export_btn = gtk::Button::builder().label("Lưu thành…").valign(gtk::Align::Center).css_classes(["pill"]).build();
        let export = adw::ActionRow::builder().title("Tệp khoá").build();
        export.add_suffix(&export_btn);
        export_group.add(&export_pass);
        export_group.add(&export);
        {
            let (ctx, export, target) = (ctx.clone(), export.clone(), self.export_target.clone());
            export_btn.connect_clicked(move |b| {
                let pass = export_pass.text().to_string();
                if pass.is_empty() {
                    export.set_subtitle("Hãy đặt mật khẩu cho tệp trước");
                    return;
                }
                let (ctx, export, target, window) = (ctx.clone(), export.clone(), target.clone(), b.root().and_downcast::<gtk::Window>());
                glib::spawn_future_local(async move {
                    let dialog = gtk::FileDialog::builder().title("Lưu khoá phòng").modal(true).initial_name("chosua-room-keys.txt").build();
                    let Ok(file) = dialog.save_future(window.as_ref()).await else { return };
                    *target.borrow_mut() = Some(file);
                    export.set_subtitle("Đang xuất…");
                    ctx.send(Command::ExportRoomKeys { passphrase: pass });
                });
            });
        }
        security.add(&verify_group);
        security.add(&recovery_group);
        security.add(&export_group);
        security.add(&import_group);

        // ---- integrations + account
        let other = adw::PreferencesPage::builder().title("Khác").icon_name("preferences-other-symbolic").build();
        let lvx_group = adw::PreferencesGroup::builder()
            .title("Tóm tắt bằng AI")
            .description("Khoá API LVX dùng cho “Tóm tắt cuộc trò chuyện”.")
            .build();
        let lvx = adw::PasswordEntryRow::builder().title("Khoá API LVX").show_apply_button(true).build();
        lvx_group.add(&lvx);
        {
            let ctx = ctx.clone();
            lvx.connect_apply(move |e| {
                ctx.send(Command::SetLvxApiKey { api_key: e.text().trim().to_string() });
                e.set_text("");
                ctx.send(Command::GetLvxApiKeyStatus);
            });
        }
        let account = adw::PreferencesGroup::builder().title("Tài khoản").build();
        let logout_btn = gtk::Button::builder().label("Đăng xuất").valign(gtk::Align::Center).css_classes(["pill", "destructive-action"]).build();
        let logout = adw::ActionRow::builder().title("Đăng xuất khỏi thiết bị này").build();
        logout.add_suffix(&logout_btn);
        account.add(&logout);
        {
            let (ctx, dialog) = (ctx.clone(), dialog.clone());
            logout_btn.connect_clicked(move |_| {
                let confirm = adw::AlertDialog::builder()
                    .heading("Đăng xuất?")
                    .body("Tin nhắn mã hoá trên thiết bị này sẽ không đọc lại được nếu chưa có bản sao lưu khoá.")
                    .build();
                confirm.add_responses(&[("cancel", "Huỷ"), ("logout", "Đăng xuất")]);
                confirm.set_response_appearance("logout", adw::ResponseAppearance::Destructive);
                let ctx = ctx.clone();
                confirm.connect_response(Some("logout"), move |_, _| ctx.send(Command::Logout));
                confirm.present(Some(&dialog));
            });
        }
        other.add(&lvx_group);
        other.add(&account);

        dialog.add(&self.app_page());
        dialog.add(&profile_page);
        dialog.add(&security);
        dialog.add(&self.shortcuts_page());
        dialog.add(&other);
        {
            let this = Rc::downgrade(self);
            dialog.connect_closed(move |_| {
                if let Some(this) = this.upgrade() {
                    this.open.borrow_mut().take();
                }
            });
        }
        *self.open.borrow_mut() = Some(Open { dialog: dialog.clone(), avatar, name, user_id, verify, verify_box, recovery, import, export, lvx });
        ctx.send(Command::GetOwnProfile);
        ctx.send(Command::GetLvxApiKeyStatus);
        dialog.present(Some(anchor));
    }

    pub fn verification_emojis(&self, emojis: Vec<(String, String)>) {
        let open = self.open.borrow();
        let Some(o) = open.as_ref() else { return };
        o.verify.set_subtitle("Các emoji dưới đây có giống trên thiết bị kia không?");
        while let Some(c) = o.verify_box.first_child() {
            o.verify_box.remove(&c);
        }
        let grid = gtk::FlowBox::builder().max_children_per_line(7).selection_mode(gtk::SelectionMode::None).homogeneous(true).build();
        for (emoji, name) in emojis {
            let cell = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
            cell.append(&gtk::Label::builder().label(&emoji).css_classes(["title-1"]).build());
            cell.append(&gtk::Label::builder().label(&name).css_classes(["caption"]).build());
            grid.insert(&cell, -1);
        }
        let yes = gtk::Button::builder().label("Giống nhau").css_classes(["pill", "suggested-action"]).build();
        let no = gtk::Button::builder().label("Không giống").css_classes(["pill", "destructive-action"]).build();
        let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).build();
        buttons.append(&no);
        buttons.append(&yes);
        let ctx = self.ctx.clone();
        yes.connect_clicked(move |_| ctx.send(Command::ConfirmVerification));
        let ctx = self.ctx.clone();
        no.connect_clicked(move |_| ctx.send(Command::CancelVerification));
        o.verify_box.append(&grid);
        o.verify_box.append(&buttons);
        o.verify_box.set_visible(true);
    }

    pub fn verification_done(&self, ok: bool, reason: &str) {
        if let Some(o) = self.open.borrow().as_ref() {
            o.verify_box.set_visible(false);
            o.verify.set_subtitle(&if ok { "✓ Thiết bị đã được xác minh".to_string() } else { format!("Đã huỷ: {reason}") });
        }
    }

    pub fn recovery_status(&self, status: &str) {
        if let Some(o) = self.open.borrow().as_ref() {
            o.recovery.set_subtitle(&glib::markup_escape_text(status));
        }
    }

    pub fn keys_imported(&self, imported: usize, total: usize) {
        if let Some(o) = self.open.borrow().as_ref() {
            o.import.set_subtitle(&format!("Đã nhập {imported}/{total} khoá"));
        }
    }

    /// The server's key backup can't be opened (its key is lost): the only way on is to start the
    /// account's encryption over.
    pub fn recovery_backup_exists(&self) {
        let Some((dialog, recovery)) = self.open.borrow().as_ref().map(|o| (o.dialog.clone(), o.recovery.clone())) else { return };
        recovery.set_subtitle("Tài khoản đã có bản sao lưu khoá cũ mà thiết bị này không mở được.");
        ask_reset_identity(self.ctx.clone(), dialog.upcast_ref(), recovery);
    }

    /// The new security key: show it once, with a copy button.
    pub fn recovery_key_created(&self, key: &str) {
        let Some(o) = self.open.borrow().as_ref().map(|o| o.dialog.clone()) else { return };
        let shown = glib::markup_escape_text(key);
        let dialog = adw::AlertDialog::builder()
            .heading("Security key mới")
            .body_use_markup(true)
            .body(format!("Hãy lưu key này ở nơi an toàn — sẽ không xem lại được:\n\n<tt><big>{shown}</big></tt>"))
            .build();
        dialog.add_responses(&[("copy", "Sao chép"), ("done", "Đã lưu")]);
        dialog.set_response_appearance("done", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("done"));
        dialog.set_close_response("done");
        let key = key.to_string();
        dialog.connect_response(None, move |d, response| {
            if response == "copy" {
                d.clipboard().set_text(&key);
            }
        });
        dialog.present(Some(&o));
    }

    /// The export file's bytes arrived: write them where the user chose.
    pub fn keys_exported(&self, bytes: Vec<u8>) {
        let Some(file) = self.export_target.borrow_mut().take() else { return };
        let result = file.path().ok_or_else(|| "đường dẫn không hợp lệ".to_string()).and_then(|p| std::fs::write(&p, &bytes).map(|_| p).map_err(|e| e.to_string()));
        if let Some(o) = self.open.borrow().as_ref() {
            match result {
                Ok(path) => o.export.set_subtitle(&glib::markup_escape_text(&format!("Đã lưu: {}", path.display()))),
                Err(e) => o.export.set_subtitle(&glib::markup_escape_text(&format!("Không lưu được: {e}"))),
            }
        }
    }

    pub fn lvx_status(&self, configured: bool) {
        if let Some(o) = self.open.borrow().as_ref() {
            o.lvx.set_title(if configured { "Khoá API LVX (đã lưu)" } else { "Khoá API LVX (chưa có)" });
        }
    }

    pub fn close(&self) {
        if let Some(o) = self.open.borrow_mut().take() {
            o.dialog.close();
        }
    }
}
