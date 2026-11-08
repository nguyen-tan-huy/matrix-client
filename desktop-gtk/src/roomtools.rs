//! The open room's menu (title bar): room info and settings, members and
//! invites, pinned messages, message search, the LLM summary, and polls.
//! Each is a dialog the worker's answers are rendered into while it's open.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use chosua_core::models::{DirectoryUser, PollData, RoomInfo, SearchHit, TimelineEvent};
use chosua_core::Command;
use gtk::{gio, glib};

use crate::window::Ctx;

/// What the open dialog shows, and where its answers go.
enum Open {
    Info { room_id: String },
    Members { room_id: String, list: gtk::ListBox, found: gtk::ListBox },
    Pinned { room_id: String, list: gtk::ListBox, rows: HashMap<String, gtk::Box> },
    Search { list: gtk::ListBox, status: gtk::Label },
    UserMessages { user_id: String, list: gtk::ListBox, status: gtk::Label },
    Users { list: gtk::ListBox, entry: gtk::SearchEntry, users: Vec<(String, String)> },
    Summary { room_id: String, label: gtk::Label },
    Polls { room_id: String, list: gtk::ListBox, polls: HashMap<String, PollData> },
}

pub struct RoomTools {
    ctx: Rc<Ctx>,
    room: RefCell<Option<(String, String)>>,
    open: RefCell<Option<(adw::Dialog, Open)>>,
    parent: RefCell<Option<gtk::Widget>>,
    on_jump: RefCell<Option<Box<dyn Fn(&str)>>>,
    pub button: gtk::MenuButton,
    me: RefCell<std::rc::Weak<RoomTools>>,
}

fn page(title: &str, content: &impl IsA<gtk::Widget>, w: i32, h: i32) -> adw::Dialog {
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(content));
    adw::Dialog::builder().title(title).content_width(w).content_height(h).child(&view).build()
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(child)
        .build()
}

fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(12)
        .build()
}

fn placeholder(list: &gtk::ListBox, text: &str) {
    list.remove_all();
    list.append(&gtk::Label::builder().label(text).css_classes(["dim-label"]).margin_top(18).margin_bottom(18).build());
}

/// A message, one line: "Sender: body", with its time.
fn event_row(ev: &TimelineEvent, extra: Option<&str>) -> adw::ActionRow {
    let body: String = ev.body.chars().take(240).collect();
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&ev.sender_name).as_str())
        .subtitle(glib::markup_escape_text(&body).as_str())
        .subtitle_lines(4)
        .build();
    if let Some(extra) = extra {
        row.set_title(&format!("{} · {}", glib::markup_escape_text(&ev.sender_name), glib::markup_escape_text(extra)));
    }
    row
}

impl RoomTools {
    pub fn new(ctx: Rc<Ctx>) -> Rc<Self> {
        let menu = gio::Menu::new();
        let info = gio::Menu::new();
        info.append(Some("Thông tin phòng"), Some("room.info"));
        info.append(Some("Thành viên"), Some("room.members"));
        menu.append_section(None, &info);
        let content = gio::Menu::new();
        content.append(Some("Tin đã ghim"), Some("room.pinned"));
        content.append(Some("Tìm tin nhắn"), Some("room.search"));
        content.append(Some("Tóm tắt cuộc trò chuyện"), Some("room.summary"));
        content.append(Some("Bình chọn (poll)"), Some("room.polls"));
        menu.append_section(None, &content);
        let notify = gio::Menu::new();
        notify.append(Some("Thông báo: mọi tin"), Some("room.notify::all"));
        notify.append(Some("Thông báo: chỉ khi được nhắc"), Some("room.notify::mentions"));
        notify.append(Some("Thông báo: tắt"), Some("room.notify::mute"));
        menu.append_section(None, &notify);

        let button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .tooltip_text("Phòng")
            .css_classes(["circular", "glass-button"])
            .build();

        let this = Rc::new(RoomTools {
            ctx,
            room: RefCell::default(),
            open: RefCell::default(),
            parent: RefCell::default(),
            on_jump: RefCell::default(),
            button: button.clone(),
            me: RefCell::default(),
        });
        *this.me.borrow_mut() = Rc::downgrade(&this);

        let group = gio::SimpleActionGroup::new();
        for name in ["info", "members", "pinned", "search", "summary", "polls"] {
            let a = gio::SimpleAction::new(name, None);
            let this = Rc::downgrade(&this);
            a.connect_activate(move |_, _| {
                let Some(this) = this.upgrade() else { return };
                match name {
                    "info" => this.show_info(),
                    "members" => this.show_members(),
                    "pinned" => this.show_pinned(),
                    "search" => this.show_search(),
                    "summary" => this.show_summary(),
                    _ => this.show_polls(),
                }
            });
            group.add_action(&a);
        }
        let notify_mode = gio::SimpleAction::new_stateful("notify", Some(glib::VariantTy::STRING), &"all".to_variant());
        {
            let this = Rc::downgrade(&this);
            notify_mode.connect_activate(move |a, v| {
                let (Some(this), Some(v)) = (this.upgrade(), v) else { return };
                let Some(mode) = v.str() else { return };
                a.set_state(v);
                let room_id = this.room_id();
                if let Some(room_id) = room_id {
                    this.ctx.send(Command::SetNotificationMode { room_id, mode: mode.into() });
                }
            });
        }
        group.add_action(&notify_mode);
        button.insert_action_group("room", Some(&group));
        unsafe { button.set_data("notify-action", notify_mode) };
        this
    }

    fn self_weak(&self) -> std::rc::Weak<RoomTools> {
        self.me.borrow().clone()
    }

    pub fn set_room(&self, room_id: &str, name: &str, parent: &impl IsA<gtk::Widget>) {
        *self.room.borrow_mut() = Some((room_id.into(), name.into()));
        *self.parent.borrow_mut() = Some(parent.clone().upcast());
    }

    /// Open a room (and message) from a search hit.
    pub fn connect_jump(&self, f: impl Fn(&str) + 'static) {
        *self.on_jump.borrow_mut() = Some(Box::new(f));
    }

    pub fn notification_mode(&self, room_id: &str, mode: &str) {
        if self.room.borrow().as_ref().is_some_and(|(r, _)| r == room_id) {
            let action = unsafe { self.button.data::<gio::SimpleAction>("notify-action").map(|p| p.as_ref().clone()) };
            if let Some(a) = action {
                a.set_state(&mode.to_variant());
            }
        }
    }

    fn present(self: &Rc<Self>, dialog: adw::Dialog, open: Open) {
        let this = Rc::downgrade(self);
        dialog.connect_closed(move |d| {
            let Some(this) = this.upgrade() else { return };
            // only forget it if it's still the open one (closing an old
            // dialog can land after the next one opened)
            let mine = this.open.borrow().as_ref().is_some_and(|(open, _)| open == d);
            if mine {
                this.open.borrow_mut().take();
            }
        });
        let parent = self.parent.borrow().clone();
        *self.open.borrow_mut() = Some((dialog.clone(), open));
        dialog.present(parent.as_ref());
    }

    fn room_id(&self) -> Option<String> {
        self.room.borrow().as_ref().map(|(r, _)| r.clone())
    }

    // ---- room info --------------------------------------------------

    fn show_info(self: &Rc<Self>) {
        let Some(room_id) = self.room_id() else { return };
        let group = adw::PreferencesGroup::builder().margin_start(12).margin_end(12).margin_top(12).build();
        group.add(&adw::ActionRow::builder().title("Đang tải…").build());
        let dialog = page("Thông tin phòng", &scrolled(&group), 480, 420);
        self.present(dialog, Open::Info { room_id: room_id.clone() });
        self.ctx.send(Command::GetRoomInfo { room_id });
    }

    pub fn room_info(self: &Rc<Self>, info: RoomInfo) {
        let open = self.open.borrow();
        let Some((dialog, Open::Info { room_id })) = open.as_ref() else { return };
        if *room_id != info.room_id {
            return;
        }
        let fresh = adw::PreferencesGroup::builder().margin_start(12).margin_end(12).margin_top(12).build();
        let avatar = adw::Avatar::builder().size(96).show_initials(true).text(info.name.as_deref().unwrap_or("")).build();
        self.ctx.images.avatar(&avatar, info.avatar_url.as_deref());
        let change = gtk::Button::builder().label("Đổi ảnh").css_classes(["pill"]).halign(gtk::Align::Center).visible(info.can_set_avatar).build();
        let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_bottom(12).build();
        top.append(&avatar);
        top.append(&change);
        let name = adw::EntryRow::builder()
            .title("Tên phòng")
            .text(info.name.as_deref().unwrap_or(""))
            .editable(info.can_set_name)
            .show_apply_button(info.can_set_name)
            .build();
        let topic = adw::EntryRow::builder()
            .title("Chủ đề")
            .text(info.topic.as_deref().unwrap_or(""))
            .editable(info.can_set_topic)
            .show_apply_button(info.can_set_topic)
            .build();
        let id_row = adw::ActionRow::builder().title("ID phòng").subtitle(&info.room_id).subtitle_selectable(true).build();
        fresh.add(&name);
        fresh.add(&topic);
        fresh.add(&id_row);
        let (ctx, rid) = (self.ctx.clone(), info.room_id.clone());
        name.connect_apply(move |e| ctx.send(Command::SetRoomName { room_id: rid.clone(), name: e.text().to_string() }));
        let (ctx, rid) = (self.ctx.clone(), info.room_id.clone());
        topic.connect_apply(move |e| ctx.send(Command::SetRoomTopic { room_id: rid.clone(), topic: e.text().to_string() }));
        let (ctx, rid) = (self.ctx.clone(), info.room_id.clone());
        change.connect_clicked(move |b| {
            let (ctx, rid, window) = (ctx.clone(), rid.clone(), b.root().and_downcast::<gtk::Window>());
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_mime_type("image/*");
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let dialog = gtk::FileDialog::builder().title("Ảnh phòng").filters(&filters).build();
                let Ok(file) = dialog.open_future(window.as_ref()).await else { return };
                let Ok((bytes, _)) = file.load_contents_future().await else { return };
                let (ctype, _) = gio::content_type_guess(file.basename().as_deref(), Some(&bytes[..]));
                let mime = gio::content_type_get_mime_type(&ctype).map(|m| m.to_string()).unwrap_or_else(|| "image/png".into());
                ctx.send(Command::SetRoomAvatar { room_id: rid, bytes: bytes.to_vec(), mime });
            });
        });
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_top(12).build();
        col.append(&top);
        col.append(&fresh);
        if let Some(view) = dialog.child().and_downcast::<adw::ToolbarView>() {
            view.set_content(Some(&scrolled(&col)));
        }
    }

    // ---- members ----------------------------------------------------

    fn show_members(self: &Rc<Self>) {
        let Some(room_id) = self.room_id() else { return };
        let invite = gtk::SearchEntry::builder()
            .placeholder_text("Mời người: tên hoặc @id:máy-chủ")
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .build();
        let found = boxed_list();
        found.set_visible(false);
        let list = boxed_list();
        placeholder(&list, "Đang tải…");
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        col.append(&invite);
        col.append(&found);
        col.append(&list);
        {
            let ctx = self.ctx.clone();
            invite.connect_search_changed(move |e| {
                let q = e.text().trim().to_string();
                if q.len() >= 2 {
                    ctx.send(Command::SearchDirectoryUsers { query: q });
                }
            });
        }
        let dialog = page("Thành viên", &scrolled(&col), 480, 600);
        self.present(dialog, Open::Members { room_id: room_id.clone(), list, found });
        self.ctx.send(Command::ListMembers { room_id });
    }

    pub fn members(&self, room_id: &str, members: Vec<(String, String, Option<String>)>) {
        let open = self.open.borrow();
        let Some((_, Open::Members { room_id: r, list, .. })) = open.as_ref() else { return };
        if r != room_id {
            return;
        }
        list.remove_all();
        let mut members = members;
        members.sort_by_key(|(_, name, _)| name.to_lowercase());
        for (user_id, name, avatar_url) in members {
            let avatar = adw::Avatar::builder().size(32).show_initials(true).text(&name).build();
            self.ctx.images.avatar(&avatar, avatar_url.as_deref());
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&name).as_str())
                .subtitle(glib::markup_escape_text(&user_id).as_str())
                .build();
            row.add_prefix(&avatar);
            row.set_activatable(true);
            let (this, uid, n) = (self.self_weak(), user_id.clone(), name.clone());
            row.connect_activated(move |_| {
                if let Some(this) = this.upgrade() {
                    this.show_user_messages(&uid, &n);
                }
            });
            list.append(&row);
        }
    }

    /// Everything one member wrote, across rooms (`SearchUserMessages`).
    pub fn show_user_messages(self: &Rc<Self>, user_id: &str, name: &str) {
        let status = gtk::Label::builder().label("Đang tìm…").css_classes(["dim-label", "caption"]).margin_top(12).build();
        let list = boxed_list();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        col.append(&status);
        col.append(&list);
        let old = self.open.borrow_mut().take();
        if let Some((old, _)) = old {
            old.close();
        }
        let dialog = page(&format!("Tin nhắn của {name}"), &scrolled(&col), 560, 640);
        self.present(dialog, Open::UserMessages { user_id: user_id.into(), list, status });
        self.ctx.send(Command::SearchUserMessages { user_id: user_id.into(), from_ts: None, to_ts: None });
    }

    pub fn user_messages(self: &Rc<Self>, user_id: &str, results: Vec<chosua_core::models::UserSearchHit>, truncated: bool) {
        let open = self.open.borrow();
        let Some((dialog, Open::UserMessages { user_id: u, list, status })) = open.as_ref() else { return };
        if u != user_id {
            return;
        }
        status.set_label(&match (results.len(), truncated) {
            (0, _) => "Không có tin nào".to_string(),
            (n, true) => format!("{n}+ tin (đã cắt bớt)"),
            (n, false) => format!("{n} tin"),
        });
        list.remove_all();
        for hit in results {
            let row = event_row(&hit.event, Some(&hit.room_name));
            row.set_activatable(true);
            let (this, room_id, dialog) = (Rc::downgrade(self), hit.room_id.clone(), dialog.clone());
            row.connect_activated(move |_| {
                if let Some(this) = this.upgrade() {
                    if let Some(f) = this.on_jump.borrow().as_ref() {
                        f(&room_id);
                    }
                }
                dialog.close();
            });
            list.append(&row);
        }
    }

    pub fn directory_users(&self, users: Vec<DirectoryUser>) {
        let open = self.open.borrow();
        let Some((_, Open::Members { room_id, found, .. })) = open.as_ref() else { return };
        found.remove_all();
        found.set_visible(!users.is_empty());
        for u in users {
            let name = u.display_name.clone().unwrap_or_else(|| u.user_id.clone());
            let avatar = adw::Avatar::builder().size(32).show_initials(true).text(&name).build();
            self.ctx.images.avatar(&avatar, u.avatar_url.as_deref());
            let invite = gtk::Button::builder().label("Mời").valign(gtk::Align::Center).css_classes(["suggested-action", "pill", "small"]).build();
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&name).as_str())
                .subtitle(glib::markup_escape_text(&u.user_id).as_str())
                .build();
            row.add_prefix(&avatar);
            row.add_suffix(&invite);
            let (ctx, room_id, user_id) = (self.ctx.clone(), room_id.clone(), u.user_id.clone());
            invite.connect_clicked(move |b| {
                b.set_sensitive(false);
                b.set_label("Đã mời");
                ctx.send(Command::InviteUser { room_id: room_id.clone(), user_id: user_id.clone() });
            });
            found.append(&row);
        }
    }

    // ---- pinned -------------------------------------------------------

    fn show_pinned(self: &Rc<Self>) {
        let Some(room_id) = self.room_id() else { return };
        let list = boxed_list();
        placeholder(&list, "Đang tải…");
        let dialog = page("Tin đã ghim", &scrolled(&list), 520, 560);
        self.present(dialog, Open::Pinned { room_id: room_id.clone(), list, rows: HashMap::new() });
        self.ctx.send(Command::GetPinnedEvents { room_id });
    }

    pub fn pinned(&self, room_id: &str, event_ids: Vec<String>) {
        let mut open = self.open.borrow_mut();
        let Some((_, Open::Pinned { room_id: r, list, rows })) = open.as_mut() else { return };
        if r != room_id {
            return;
        }
        if event_ids.is_empty() {
            placeholder(list, "Chưa ghim tin nào");
            return;
        }
        list.remove_all();
        rows.clear();
        for event_id in event_ids.into_iter().rev() {
            let slot = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
            slot.append(&adw::ActionRow::builder().title("…").build());
            list.append(&slot);
            rows.insert(event_id.clone(), slot);
            self.ctx.send(Command::GetEventPreview { room_id: room_id.into(), event_id });
        }
    }

    pub fn event_preview(&self, room_id: &str, event_id: &str, ev: Option<TimelineEvent>) {
        let open = self.open.borrow();
        let Some((_, Open::Pinned { room_id: r, rows, .. })) = open.as_ref() else { return };
        let (true, Some(slot)) = (r == room_id, rows.get(event_id)) else { return };
        while let Some(c) = slot.first_child() {
            slot.remove(&c);
        }
        let row = match &ev {
            Some(ev) => event_row(ev, None),
            None => adw::ActionRow::builder().title("(không tải được tin này)").build(),
        };
        let unpin = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Bỏ ghim").valign(gtk::Align::Center).css_classes(["flat", "circular"]).build();
        let (ctx, rid, eid, slot2) = (self.ctx.clone(), room_id.to_string(), event_id.to_string(), slot.clone());
        unpin.connect_clicked(move |_| {
            ctx.send(Command::UnpinMessage { room_id: rid.clone(), event_id: eid.clone() });
            slot2.set_visible(false);
        });
        row.add_suffix(&unpin);
        slot.append(&row);
    }

    // ---- search ------------------------------------------------------

    /// Search messages: in the open room, or (none open, or ticked) all.
    pub fn show_search(self: &Rc<Self>) {
        let room_id = self.room_id().unwrap_or_default();
        let entry = gtk::SearchEntry::builder().placeholder_text("Tìm trong tin nhắn").hexpand(true).build();
        let everywhere = gtk::CheckButton::builder().label("Mọi phòng").active(room_id.is_empty()).sensitive(!room_id.is_empty()).build();
        let bar = gtk::Box::builder().spacing(8).margin_start(12).margin_end(12).margin_top(12).build();
        bar.append(&entry);
        bar.append(&everywhere);
        let status = gtk::Label::builder().css_classes(["dim-label", "caption"]).margin_top(6).build();
        let list = boxed_list();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        col.append(&bar);
        col.append(&status);
        col.append(&list);
        {
            let (ctx, status) = (self.ctx.clone(), status.clone());
            let everywhere = everywhere.clone();
            entry.connect_activate(move |e| {
                let query = e.text().trim().to_string();
                if query.is_empty() {
                    return;
                }
                status.set_label("Đang tìm…");
                let room_id = (!everywhere.is_active()).then(|| room_id.clone());
                ctx.send(Command::SearchMessages { query, room_id, from_ts: None, to_ts: None });
            });
        }
        let dialog = page("Tìm tin nhắn", &scrolled(&col), 560, 640);
        self.present(dialog, Open::Search { list, status });
        entry.grab_focus();
    }

    pub fn search_results(self: &Rc<Self>, results: Vec<SearchHit>, truncated: bool) {
        let open = self.open.borrow();
        let Some((dialog, Open::Search { list, status })) = open.as_ref() else { return };
        status.set_label(&match (results.len(), truncated) {
            (0, _) => "Không thấy tin nào".to_string(),
            (n, true) => format!("{n}+ kết quả (đã cắt bớt)"),
            (n, false) => format!("{n} kết quả"),
        });
        list.remove_all();
        for hit in results {
            let row = event_row(&hit.event, Some(&hit.room_name));
            row.set_activatable(true);
            let (this, room_id, dialog) = (Rc::downgrade(self), hit.room_id.clone(), dialog.clone());
            row.connect_activated(move |_| {
                if let Some(this) = this.upgrade() {
                    if let Some(f) = this.on_jump.borrow().as_ref() {
                        f(&room_id);
                    }
                }
                dialog.close();
            });
            list.append(&row);
        }
    }

    /// Pick anyone you share a room with, then see everything they wrote.
    pub fn show_user_search(self: &Rc<Self>, parent: &impl IsA<gtk::Widget>) {
        *self.parent.borrow_mut() = Some(parent.clone().upcast());
        let entry = gtk::SearchEntry::builder()
            .placeholder_text("Tên hoặc @id")
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .build();
        let list = boxed_list();
        placeholder(&list, "Đang tải danh sách…");
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        col.append(&entry);
        col.append(&list);
        {
            let this = Rc::downgrade(self);
            entry.connect_search_changed(move |_| {
                if let Some(this) = this.upgrade() {
                    this.render_users();
                }
            });
        }
        let dialog = page("Tìm tin của một người", &scrolled(&col), 480, 600);
        self.present(dialog, Open::Users { list, entry: entry.clone(), users: Vec::new() });
        entry.grab_focus();
        self.ctx.send(Command::ListAllUsers);
    }

    pub fn all_users(self: &Rc<Self>, users: Vec<(String, String)>) {
        {
            let mut open = self.open.borrow_mut();
            let Some((_, Open::Users { users: known, .. })) = open.as_mut() else { return };
            *known = users;
        }
        self.render_users();
    }

    fn render_users(self: &Rc<Self>) {
        let open = self.open.borrow();
        let Some((_, Open::Users { list, entry, users })) = open.as_ref() else { return };
        let q = crate::rooms::fold(entry.text().trim());
        list.remove_all();
        let mut shown = 0;
        for (user_id, name) in users {
            if !q.is_empty() && !crate::rooms::fold(name).contains(&q) && !user_id.to_lowercase().contains(&q) {
                continue;
            }
            shown += 1;
            if shown > 100 {
                break;
            }
            let row = adw::ActionRow::builder()
                .title(glib::markup_escape_text(name).as_str())
                .subtitle(glib::markup_escape_text(user_id).as_str())
                .activatable(true)
                .build();
            let (this, uid, n) = (Rc::downgrade(self), user_id.clone(), name.clone());
            row.connect_activated(move |_| {
                if let Some(this) = this.upgrade() {
                    // after this handler: show_user_messages replaces the dialog
                    let (uid, n) = (uid.clone(), n.clone());
                    glib::idle_add_local_once(move || this.show_user_messages(&uid, &n));
                }
            });
            list.append(&row);
        }
        if shown == 0 && !users.is_empty() {
            placeholder(list, "Không thấy ai");
        }
    }

    // ---- summary -----------------------------------------------------

    fn show_summary(self: &Rc<Self>) {
        let Some(room_id) = self.room_id() else { return };
        let label = gtk::Label::builder()
            .label("Đang tóm tắt…")
            .wrap(true)
            .xalign(0.0)
            .yalign(0.0)
            .selectable(true)
            .margin_start(18)
            .margin_end(18)
            .margin_top(12)
            .margin_bottom(18)
            .build();
        let dialog = page("Tóm tắt", &scrolled(&label), 560, 600);
        self.present(dialog, Open::Summary { room_id: room_id.clone(), label });
        self.ctx.send(Command::Summarize { room_id, thread_root_id: None });
    }

    pub fn summary(&self, room_id: &str, text: &str) {
        let open = self.open.borrow();
        let Some((_, Open::Summary { room_id: r, label })) = open.as_ref() else { return };
        if r == room_id {
            label.set_markup(&crate::markdown::to_pango(text));
        }
    }

    // ---- polls -------------------------------------------------------

    fn show_polls(self: &Rc<Self>) {
        let Some(room_id) = self.room_id() else { return };
        let list = boxed_list();
        placeholder(&list, "Đang tải…");
        let create = gtk::Button::builder().label("Tạo bình chọn…").css_classes(["pill", "suggested-action"]).halign(gtk::Align::Center).margin_top(12).build();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        col.append(&create);
        col.append(&list);
        {
            let this = Rc::downgrade(self);
            create.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.create_poll();
                }
            });
        }
        let dialog = page("Bình chọn", &scrolled(&col), 520, 620);
        self.present(dialog, Open::Polls { room_id: room_id.clone(), list, polls: HashMap::new() });
        self.ctx.send(Command::ListPolls { room_id });
    }

    pub fn polls(&self, room_id: &str, polls: Vec<PollData>) {
        {
            let mut open = self.open.borrow_mut();
            let Some((_, Open::Polls { room_id: r, polls: known, .. })) = open.as_mut() else { return };
            if r != room_id {
                return;
            }
            for p in polls {
                known.insert(p.poll_event_id.clone(), p);
            }
        }
        self.render_polls();
    }

    pub fn poll_updated(&self, poll: PollData) {
        {
            let mut open = self.open.borrow_mut();
            let Some((_, Open::Polls { room_id, polls, .. })) = open.as_mut() else { return };
            if *room_id != poll.room_id {
                return;
            }
            polls.insert(poll.poll_event_id.clone(), poll);
        }
        self.render_polls();
    }

    fn render_polls(&self) {
        let open = self.open.borrow();
        let Some((_, Open::Polls { room_id, list, polls })) = open.as_ref() else { return };
        if polls.is_empty() {
            placeholder(list, "Chưa có bình chọn nào");
            return;
        }
        list.remove_all();
        let mut polls: Vec<&PollData> = polls.values().collect();
        polls.sort_by_key(|p| p.ended);
        for poll in polls {
            let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_start(12).margin_end(12).margin_top(10).margin_bottom(10).build();
            col.append(&gtk::Label::builder().label(&poll.question).wrap(true).xalign(0.0).css_classes(["heading"]).build());
            for opt in &poll.options {
                let mine = poll.my_vote_ids.contains(&opt.id);
                let frac = if poll.total_votes > 0 { opt.votes as f64 / poll.total_votes as f64 } else { 0.0 };
                let b = gtk::ToggleButton::builder().active(mine).sensitive(!poll.ended).css_classes(["poll-option"]).build();
                let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
                let line = gtk::Box::builder().spacing(8).build();
                line.append(&gtk::Label::builder().label(&opt.text).xalign(0.0).hexpand(true).wrap(true).build());
                line.append(&gtk::Label::builder().label(opt.votes.to_string()).css_classes(["dim-label"]).build());
                inner.append(&line);
                inner.append(&gtk::LevelBar::builder().value(frac).build());
                b.set_child(Some(&inner));
                let (ctx, rid, pid, oid) = (self.ctx.clone(), room_id.clone(), poll.poll_event_id.clone(), opt.id.clone());
                b.connect_clicked(move |_| {
                    ctx.send(Command::VotePoll { room_id: rid.clone(), poll_event_id: pid.clone(), answer_ids: vec![oid.clone()] })
                });
                col.append(&b);
            }
            let foot = gtk::Box::builder().spacing(8).build();
            foot.append(
                &gtk::Label::builder()
                    .label(format!("{} phiếu{}", poll.total_votes, if poll.ended { " · đã kết thúc" } else { "" }))
                    .css_classes(["dim-label", "caption"])
                    .hexpand(true)
                    .xalign(0.0)
                    .build(),
            );
            if !poll.ended {
                let end = gtk::Button::builder().label("Kết thúc").css_classes(["pill", "small"]).build();
                let (ctx, rid, pid) = (self.ctx.clone(), room_id.clone(), poll.poll_event_id.clone());
                end.connect_clicked(move |b| {
                    b.set_sensitive(false);
                    ctx.send(Command::EndPoll { room_id: rid.clone(), poll_event_id: pid.clone() });
                });
                foot.append(&end);
            }
            col.append(&foot);
            list.append(&col);
        }
    }

    fn create_poll(self: &Rc<Self>) {
        let Some(room_id) = self.room_id() else { return };
        let question = adw::EntryRow::builder().title("Câu hỏi").build();
        let options: Vec<adw::EntryRow> = (1..=4).map(|i| adw::EntryRow::builder().title(format!("Lựa chọn {i}")).build()).collect();
        let group = adw::PreferencesGroup::new();
        group.add(&question);
        for o in &options {
            group.add(o);
        }
        let dialog = adw::AlertDialog::builder().heading("Tạo bình chọn").extra_child(&group).build();
        dialog.add_responses(&[("cancel", "Huỷ"), ("create", "Tạo")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        let ctx = self.ctx.clone();
        dialog.connect_response(Some("create"), move |_, _| {
            let q = question.text().trim().to_string();
            let opts: Vec<String> = options.iter().map(|o| o.text().trim().to_string()).filter(|s| !s.is_empty()).collect();
            if !q.is_empty() && opts.len() >= 2 {
                ctx.send(Command::StartPoll { room_id: room_id.clone(), thread_id: None, question: q, options: opts, max_selections: 1 });
                ctx.send(Command::ListPolls { room_id: room_id.clone() });
            }
        });
        let parent = self.parent.borrow().clone();
        dialog.present(parent.as_ref());
    }
}
