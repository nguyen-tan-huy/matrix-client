//! The room list: `RoomListService`'s diffs applied straight onto a
//! `gio::ListStore`, shown by a recycling `gtk::ListView`; plus search,
//! filters (unread, favourites, a space), invites and the room menu.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

use adw::prelude::*;
use chosua_core::event::RoomListOp;
use chosua_core::models::RoomSummary;
use chosua_core::Command;
use gtk::{gdk, gio, glib};

use crate::window::Ctx;

#[derive(Clone, Copy, PartialEq)]
enum Show {
    All,
    Unread,
    Mentions,
    Favorites,
}

#[derive(Default)]
struct FilterState {
    query: String,
    /// rooms of the picked space (`None` = no space picked)
    space: Option<HashSet<String>>,
}

pub struct RoomsPane {
    pub root: gtk::Widget,
    header: adw::HeaderBar,
    footer: gtk::Box,
    search: gtk::SearchEntry,
    ctx: Rc<Ctx>,
    store: gio::ListStore,
    invites: gio::ListStore,
    invites_box: gtk::ListBox,
    selection: gtk::SingleSelection,
    filter: gtk::CustomFilter,
    state: Rc<RefCell<FilterState>>,
    show: Rc<Cell<Show>>,
    spaces: RefCell<Vec<RoomSummary>>,
    space_pick: gtk::DropDown,
    picked_space: RefCell<Option<String>>,
    on_selected: RefCell<Option<Box<dyn Fn(&RoomSummary)>>>,
    /// the open room when it isn't in the list (a favourite, opened from its tab)
    outside: RefCell<Option<String>>,
}

fn obj(room: RoomSummary) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(room)
}

fn summary(o: &glib::Object) -> std::cell::Ref<'_, RoomSummary> {
    o.downcast_ref::<glib::BoxedAnyObject>().expect("room item").borrow()
}

/// Case- and accent-insensitive key for searching Vietnamese names.
pub fn fold(s: &str) -> String {
    use glib::translate::{from_glib_full, ToGlibPtr};
    let lower = s.to_lowercase().replace(['đ', 'Đ'], "d");
    // g_str_to_ascii: "Phùng Hợp" -> "Phung Hop" (no Rust binding)
    unsafe {
        let out: glib::GString =
            from_glib_full(glib::ffi::g_str_to_ascii(lower.to_glib_none().0, std::ptr::null()));
        out.to_string()
    }
}

impl RoomsPane {
    pub fn new(ctx: Rc<Ctx>) -> Rc<Self> {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let state: Rc<RefCell<FilterState>> = Rc::default();
        let show = Rc::new(Cell::new(Show::All));
        let filter = {
            let (state, show) = (state.clone(), show.clone());
            gtk::CustomFilter::new(move |o| {
                let room = summary(o);
                if room.is_space {
                    return false;
                }
                let st = state.borrow();
                // favourites have their own tabs in the header: not in the
                // list too, except under the Favourites filter or a search
                if room.is_favorite && show.get() != Show::Favorites && st.query.is_empty() {
                    return false;
                }
                match show.get() {
                    Show::Unread if room.unread_count == 0 && room.mention_count == 0 => return false,
                    Show::Mentions if room.mention_count == 0 => return false,
                    Show::Favorites if !room.is_favorite => return false,
                    _ => {}
                }
                if let Some(space) = &st.space {
                    if !space.contains(&room.room_id) {
                        return false;
                    }
                }
                st.query.is_empty() || fold(&room.name).contains(&st.query)
            })
        };
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        let selection = gtk::SingleSelection::builder()
            .model(&filtered)
            .autoselect(false)
            .can_unselect(true)
            .build();
        selection.set_selected(gtk::INVALID_LIST_POSITION);

        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(|_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            item.set_child(Some(&room_row_widget()));
        });
        {
            let ctx = ctx.clone();
            factory.connect_bind(move |_, item| {
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let row = item.child().unwrap();
                let obj = item.item().unwrap();
                bind_room_row(&ctx, &row, &summary(&obj));
            });
        }

        let list = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .css_classes(["navigation-sidebar", "room-list"])
            .build();

        // invites, above the rooms
        let invites = gio::ListStore::new::<glib::BoxedAnyObject>();
        let invites_box = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .visible(false)
            .css_classes(["invites"])
            .build();

        // the list must be the scrolled window's own child, or it isn't
        // virtualized (a row built for every room); invites sit above it
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vexpand(true)
            .child(&list)
            .build();
        let invites_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_height(true)
            .max_content_height(260)
            .visible(false)
            .child(&invites_box)
            .build();
        // an empty invites strip must not take any room above the list
        {
            let scroll = invites_scroll.clone();
            invites_box.connect_visible_notify(move |b| scroll.set_visible(b.is_visible()));
        }
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        col.append(&invites_scroll);
        col.append(&scroller);

        // search + filters
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Tìm phòng")
            .hexpand(true)
            .css_classes(["room-search"])
            .build();
        let chips = gtk::Box::builder().spacing(6).css_classes(["filter-chips"]).build();
        let all = gtk::ToggleButton::builder().label("Tất cả").active(true).css_classes(["chip"]).build();
        let unread = gtk::ToggleButton::builder().label("Chưa đọc").group(&all).css_classes(["chip"]).build();
        let mentions = gtk::ToggleButton::builder().label("@Nhắc tôi").group(&all).css_classes(["chip"]).build();
        let favs = gtk::ToggleButton::builder().label("Yêu thích").group(&all).css_classes(["chip"]).build();
        chips.append(&all);
        chips.append(&unread);
        chips.append(&mentions);
        chips.append(&favs);
        let space_names = gtk::StringList::new(&["Mọi space"]);
        let space_pick = gtk::DropDown::builder()
            .model(&space_names)
            .tooltip_text("Lọc theo space")
            .css_classes(["chip"])
            .build();
        chips.append(&space_pick);
        let tools = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .css_classes(["room-tools"])
            .build();
        tools.append(&search);
        tools.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::External)
                .vscrollbar_policy(gtk::PolicyType::Never)
                .child(&chips)
                .build(),
        );

        let view = adw::ToolbarView::new();
        let header = adw::HeaderBar::builder().show_end_title_buttons(false).build();
        header.set_title_widget(Some(
            &gtk::Label::builder().label("ChoSua").css_classes(["pane-title"]).build(),
        ));
        let menu = gio::Menu::new();
        menu.append(Some("Tạo phòng…"), Some("rooms.create-room"));
        menu.append(Some("Tạo space…"), Some("rooms.create-space"));
        menu.append(Some("Tải lại danh sách"), Some("rooms.refresh"));
        let more = gtk::MenuButton::builder()
            .icon_name("list-add-symbolic")
            .menu_model(&menu)
            .tooltip_text("Tạo phòng")
            .css_classes(["circular", "glass-button"])
            .build();
        header.pack_start(&more);
        view.add_top_bar(&header);
        view.add_top_bar(&tools);
        view.set_content(Some(&col));
        // bottom-left bar (Element Classic's threads shortcut lives here)
        let footer = gtk::Box::builder().spacing(6).margin_start(8).margin_end(8).margin_top(6).margin_bottom(6).build();
        view.add_bottom_bar(&footer);

        let this = Rc::new(RoomsPane {
            root: view.clone().upcast(),
            header: header.clone(),
            footer,
            search: search.clone(),
            ctx: ctx.clone(),
            store,
            invites,
            invites_box,
            selection: selection.clone(),
            filter,
            state,
            show,
            spaces: RefCell::default(),
            space_pick: space_pick.clone(),
            picked_space: RefCell::default(),
            on_selected: RefCell::new(None),
            outside: RefCell::new(None),
        });

        // actions of the "+" menu
        let actions = gio::SimpleActionGroup::new();
        for (name, space) in [("create-room", false), ("create-space", true)] {
            let a = gio::SimpleAction::new(name, None);
            let this = Rc::downgrade(&this);
            a.connect_activate(move |_, _| {
                if let Some(this) = this.upgrade() {
                    this.create_dialog(space);
                }
            });
            actions.add_action(&a);
        }
        {
            let a = gio::SimpleAction::new("refresh", None);
            let ctx = ctx.clone();
            a.connect_activate(move |_, _| ctx.send(Command::RefreshRooms));
            actions.add_action(&a);
        }
        view.insert_action_group("rooms", Some(&actions));

        {
            let this = Rc::downgrade(&this);
            search.connect_search_changed(move |e| {
                if let Some(this) = this.upgrade() {
                    this.state.borrow_mut().query = fold(e.text().trim());
                    this.refilter();
                }
            });
        }
        for (button, mode) in [(&all, Show::All), (&unread, Show::Unread), (&mentions, Show::Mentions), (&favs, Show::Favorites)] {
            let this = Rc::downgrade(&this);
            button.connect_toggled(move |b| {
                if let (true, Some(this)) = (b.is_active(), this.upgrade()) {
                    this.show.set(mode);
                    this.refilter();
                }
            });
        }
        {
            let this = Rc::downgrade(&this);
            space_pick.connect_selected_notify(move |d| {
                let Some(this) = this.upgrade() else { return };
                let i = d.selected() as usize;
                let space = if i == 0 { None } else { this.spaces.borrow().get(i - 1).map(|s| s.room_id.clone()) };
                *this.picked_space.borrow_mut() = space.clone();
                match space {
                    None => {
                        this.state.borrow_mut().space = None;
                        this.refilter();
                    }
                    Some(space_room_id) => this.ctx.send(Command::ListSpaceChildren { space_room_id }),
                }
            });
        }

        let weak = Rc::downgrade(&this);
        selection.connect_selected_item_notify(move |sel| {
            let (Some(this), Some(item)) = (weak.upgrade(), sel.selected_item()) else { return };
            let room = summary(&item).clone();
            this.outside.replace(None);
            let cb = this.on_selected.borrow();
            if let Some(cb) = cb.as_ref() {
                cb(&room);
            }
        });

        // right click / long press on a room: its menu
        let click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        {
            let (this, list) = (Rc::downgrade(&this), list.clone());
            click.connect_pressed(move |_, _, x, y| {
                if let Some(this) = this.upgrade() {
                    this.room_menu_at(&list, x, y);
                }
            });
        }
        list.add_controller(click);
        this
    }

    pub fn add_footer(&self, w: &impl IsA<gtk::Widget>) {
        self.footer.append(w);
    }

    pub fn add_header_end(&self, w: &impl IsA<gtk::Widget>) {
        self.header.pack_end(w);
    }

    /// Ids of the joined rooms (not spaces or invites).
    pub fn room_ids(&self) -> Vec<String> {
        (0..self.store.n_items())
            .map(|i| summary(&self.store.item(i).unwrap()).clone())
            .filter(|r| !r.is_space && !r.is_invite)
            .map(|r| r.room_id)
            .collect()
    }

    pub fn add_header_start(&self, w: &impl IsA<gtk::Widget>) {
        self.header.pack_start(w);
    }

    fn refilter(&self) {
        let keep = self.selected_id();
        self.filter.changed(gtk::FilterChange::Different);
        if let Some(id) = keep {
            self.select_quietly(&id);
        }
    }

    pub fn connect_selected(&self, f: impl Fn(&RoomSummary) + 'static) {
        *self.on_selected.borrow_mut() = Some(Box::new(f));
    }

    pub fn len(&self) -> usize {
        self.store.n_items() as usize
    }

    /// The selected room's id, so a diff that moves it keeps it selected.
    fn selected_id(&self) -> Option<String> {
        self.selection.selected_item().map(|o| summary(&o).room_id.clone()).or_else(|| self.outside.borrow().clone())
    }

    pub fn apply(&self, ops: Vec<RoomListOp>) {
        let keep = self.selected_id();
        apply_ops(&self.store, ops);
        if let Some(id) = keep {
            if self.selected_id().as_deref() != Some(id.as_str()) {
                self.select_quietly(&id);
            }
        }
    }

    pub fn apply_invites(self: &Rc<Self>, ops: Vec<RoomListOp>) {
        apply_ops(&self.invites, ops);
        self.invites_box.remove_all();
        self.invites_box.set_visible(self.invites.n_items() > 0);
        for i in 0..self.invites.n_items() {
            let room = summary(&self.invites.item(i).unwrap()).clone();
            let row = room_row_widget();
            bind_room_row(&self.ctx, &row, &room);
            let accept = gtk::Button::builder().label("Tham gia").css_classes(["suggested-action", "pill", "small"]).build();
            let decline = gtk::Button::builder().label("Từ chối").css_classes(["pill", "small"]).build();
            let buttons = gtk::Box::builder().spacing(6).halign(gtk::Align::End).margin_top(4).build();
            buttons.append(&decline);
            buttons.append(&accept);
            let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["invite"]).build();
            col.append(&row);
            col.append(&buttons);
            let (ctx, id) = (self.ctx.clone(), room.room_id.clone());
            accept.connect_clicked(move |b| {
                b.set_sensitive(false);
                ctx.send(Command::AcceptInvite { room_id: id.clone() });
            });
            let (ctx, id) = (self.ctx.clone(), room.room_id.clone());
            decline.connect_clicked(move |b| {
                b.set_sensitive(false);
                ctx.send(Command::DeclineInvite { room_id: id.clone() });
            });
            self.invites_box.append(&col);
        }
    }

    pub fn set_spaces(&self, spaces: Vec<RoomSummary>) {
        let names: Vec<String> = std::iter::once("Mọi space".to_string()).chain(spaces.iter().map(|s| s.name.clone())).collect();
        let picked = self.picked_space.borrow().clone();
        *self.spaces.borrow_mut() = spaces;
        let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
        self.space_pick.set_model(Some(&model));
        let i = picked
            .and_then(|p| self.spaces.borrow().iter().position(|s| s.room_id == p))
            .map(|i| i as u32 + 1)
            .unwrap_or(0);
        self.space_pick.set_selected(i);
    }

    pub fn set_space_children(&self, space_room_id: &str, room_ids: Vec<String>) {
        if self.picked_space.borrow().as_deref() == Some(space_room_id) {
            self.state.borrow_mut().space = Some(room_ids.into_iter().collect());
            self.refilter();
        }
    }

    /// Favourite rooms (not invites or spaces), for the tab strip.
    pub fn favorites(&self) -> Vec<RoomSummary> {
        (0..self.store.n_items())
            .map(|i| summary(&self.store.item(i).unwrap()).clone())
            .filter(|r| r.is_favorite && !r.is_invite && !r.is_space)
            .collect()
    }

    pub fn selected_room(&self) -> Option<String> {
        self.selected_id()
    }

    pub fn focus_search(&self) {
        self.search.grab_focus();
    }

    /// The room above (-1) or below (+1) the selected one, as filtered.
    pub fn select_relative(&self, step: i32) {
        let n = self.selection.n_items() as i32;
        if n == 0 {
            return;
        }
        let cur = self.selection.selected();
        let next = if cur == gtk::INVALID_LIST_POSITION { 0 } else { (cur as i32 + step).clamp(0, n - 1) };
        self.selection.set_selected(next as u32);
    }

    pub fn select(&self, room_id: &str) {
        self.select_quietly(room_id);
    }

    pub fn name_of(&self, room_id: &str) -> Option<String> {
        self.find(room_id).map(|(_, r)| r.name)
    }

    fn find(&self, room_id: &str) -> Option<(u32, RoomSummary)> {
        (0..self.store.n_items()).find_map(|i| {
            let item = self.store.item(i).unwrap();
            let r = summary(&item);
            (r.room_id == room_id).then(|| (i, r.clone()))
        })
    }

    fn select_quietly(&self, room_id: &str) {
        let model = self.selection.model().unwrap();
        for i in 0..model.n_items() {
            if summary(&model.item(i).unwrap()).room_id == room_id {
                // re-selecting the same room re-fires the handler, which
                // only re-shows what's already open (no reload)
                self.selection.set_selected(i);
                return;
            }
        }
        // not in the list (a favourite: those are the header's tabs): open
        // it all the same, with nothing selected in the list
        if let Some((_, room)) = self.find(room_id) {
            self.outside.replace(Some(room_id.to_string()));
            self.selection.set_selected(gtk::INVALID_LIST_POSITION);
            if let Some(cb) = self.on_selected.borrow().as_ref() {
                cb(&room);
            }
        }
    }

    fn modify(&self, room_id: &str, f: impl FnOnce(&mut RoomSummary)) {
        if let Some((i, mut room)) = self.find(room_id) {
            f(&mut room);
            let keep = self.selected_id();
            self.store.splice(i, 1, &[obj(room)]);
            if let Some(id) = keep {
                if self.selected_id().as_deref() != Some(id.as_str()) {
                    self.select_quietly(&id);
                }
            }
        }
    }

    pub fn set_unread(&self, room_id: &str, unread: u64, mentions: u64) {
        self.modify(room_id, |r| {
            r.unread_count = unread;
            r.mention_count = mentions;
        });
    }

    pub fn set_favorite(&self, room_id: &str, favorite: bool) {
        self.modify(room_id, |r| r.is_favorite = favorite);
    }

    /// The room under (x, y) of the list, and its menu.
    fn room_menu_at(self: &Rc<Self>, list: &gtk::ListView, x: f64, y: f64) {
        let Some(picked) = list.pick(x, y, gtk::PickFlags::DEFAULT) else { return };
        let row = std::iter::successors(Some(picked), |w| w.parent()).find(|w| w.has_css_class("room-row"));
        let Some(row) = row else { return };
        let Some(room_id) = (unsafe { row.data::<String>(ROOM_ID).map(|p| p.as_ref().clone()) }) else { return };
        self.menu_for(&room_id, &row);
    }

    /// The room's menu, popped up on `parent` (a list row or a favourite tab).
    pub fn menu_for(self: &Rc<Self>, room_id: &str, parent: &gtk::Widget) {
        let row = parent;
        let Some((_, room)) = self.find(room_id) else { return };

        let popover = gtk::Popover::builder().has_arrow(false).css_classes(["menu"]).build();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        let item = |label: &str, f: Box<dyn Fn()>| {
            let b = gtk::Button::builder().label(label).css_classes(["flat"]).build();
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
            let p = popover.clone();
            b.connect_clicked(move |_| {
                p.popdown();
                f();
            });
            col.append(&b);
        };
        let ctx = self.ctx.clone();
        {
            let (ctx, id, fav) = (ctx.clone(), room.room_id.clone(), !room.is_favorite);
            item(
                if fav { "☆  Thêm vào yêu thích" } else { "★  Bỏ yêu thích" },
                Box::new(move || ctx.send(Command::SetRoomFavorite { room_id: id.clone(), favorite: fav })),
            );
        }
        {
            let (ctx, id) = (ctx.clone(), room.room_id.clone());
            item("✓  Đánh dấu đã đọc", Box::new(move || ctx.send(Command::MarkRoomRead { room_id: id.clone() })));
        }
        col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        for (label, mode) in [("🔔  Mọi tin nhắn", "all"), ("@  Chỉ khi được nhắc", "mentions"), ("🔕  Tắt thông báo", "mute")] {
            let (ctx, id) = (ctx.clone(), room.room_id.clone());
            item(label, Box::new(move || ctx.send(Command::SetNotificationMode { room_id: id.clone(), mode: mode.into() })));
        }
        col.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        {
            let (this, room) = (Rc::downgrade(self), room.clone());
            item(
                "⎋  Rời phòng…",
                Box::new(move || {
                    if let Some(this) = this.upgrade() {
                        this.confirm_leave(&room);
                    }
                }),
            );
        }
        popover.set_child(Some(&col));
        popover.set_parent(row);
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn confirm_leave(&self, room: &RoomSummary) {
        let dialog = adw::AlertDialog::builder()
            .heading("Rời phòng?")
            .body(format!("Bạn sẽ rời “{}”. Muốn quay lại phải được mời lại (nếu phòng riêng tư).", room.name))
            .build();
        dialog.add_responses(&[("cancel", "Huỷ"), ("leave", "Rời phòng")]);
        dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
        let (ctx, id) = (self.ctx.clone(), room.room_id.clone());
        dialog.connect_response(Some("leave"), move |_, _| ctx.send(Command::LeaveRoom { room_id: id.clone() }));
        dialog.present(Some(&self.root));
    }

    fn create_dialog(&self, space: bool) {
        let name = adw::EntryRow::builder().title(if space { "Tên space" } else { "Tên phòng" }).build();
        let public = adw::SwitchRow::builder().title("Công khai").subtitle("Ai cũng tìm thấy và tham gia được").build();
        let group = adw::PreferencesGroup::new();
        group.add(&name);
        group.add(&public);
        let dialog = adw::AlertDialog::builder()
            .heading(if space { "Tạo space" } else { "Tạo phòng" })
            .extra_child(&group)
            .build();
        dialog.add_responses(&[("cancel", "Huỷ"), ("create", "Tạo")]);
        dialog.set_response_appearance("create", adw::ResponseAppearance::Suggested);
        let ctx = self.ctx.clone();
        dialog.connect_response(Some("create"), move |_, _| {
            let n = name.text().trim().to_string();
            if !n.is_empty() {
                ctx.send(Command::CreateRoom { name: n, is_public: public.is_active(), is_space: space });
            }
        });
        dialog.present(Some(&self.root));
    }
}

const ROOM_ID: &str = "chosua-room-id";

fn room_row_widget() -> gtk::Widget {
    let avatar = adw::Avatar::new(36, None, true);
    let name = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .build();
    let last = gtk::Label::builder()
        .xalign(0.0)
        .single_line_mode(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["dim-label", "caption"])
        .build();
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(&name);
    text.append(&last);
    let badge = gtk::Label::builder().valign(gtk::Align::Center).css_classes(["unread-badge"]).build();
    let row = gtk::Box::builder().spacing(10).css_classes(["room-row"]).build();
    row.append(&avatar);
    row.append(&text);
    row.append(&badge);
    row.upcast()
}

fn bind_room_row(ctx: &Ctx, row: &gtk::Widget, room: &RoomSummary) {
    unsafe { row.set_data(ROOM_ID, room.room_id.clone()) };
    let avatar = row.first_child().and_downcast::<adw::Avatar>().unwrap();
    let text = avatar.next_sibling().unwrap();
    let badge = text.next_sibling().and_downcast::<gtk::Label>().unwrap();
    let name = text.first_child().and_downcast::<gtk::Label>().unwrap();
    let last = name.next_sibling().and_downcast::<gtk::Label>().unwrap();
    avatar.set_text(Some(&room.name));
    ctx.images.avatar(&avatar, room.avatar_url.as_deref());
    name.set_label(&if room.is_favorite { format!("★ {}", room.name) } else { room.name.clone() });
    last.set_label(room.last_message.as_deref().unwrap_or(if room.is_invite { "Lời mời tham gia" } else { "" }));
    let unread = room.unread_count;
    badge.set_visible(unread > 0);
    badge.set_label(&if unread > 99 { "99+".into() } else { unread.to_string() });
    if room.mention_count > 0 {
        badge.add_css_class("mention");
    } else {
        badge.remove_css_class("mention");
    }
}

fn apply_ops(s: &gio::ListStore, ops: Vec<RoomListOp>) {
    for op in ops {
        let n = s.n_items();
        match op {
            RoomListOp::Append { values } => s.splice(n, 0, &values.into_iter().map(obj).collect::<Vec<_>>()),
            RoomListOp::Clear => s.remove_all(),
            RoomListOp::PushFront { value } => s.insert(0, &obj(value)),
            RoomListOp::PushBack { value } => s.append(&obj(value)),
            RoomListOp::PopFront if n > 0 => s.remove(0),
            RoomListOp::PopBack if n > 0 => s.remove(n - 1),
            RoomListOp::Insert { index, value } => s.insert(index.min(n), &obj(value)),
            RoomListOp::Set { index, value } if index < n => s.splice(index, 1, &[obj(value)]),
            RoomListOp::Remove { index } if index < n => s.remove(index),
            RoomListOp::Truncate { length } if length < n => s.splice(length, n - length, &[] as &[glib::Object]),
            RoomListOp::Reset { values } => s.splice(0, n, &values.into_iter().map(obj).collect::<Vec<_>>()),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn fold_strips_vietnamese_marks() {
        assert_eq!(super::fold("Phùng Hợp"), "phung hop");
        assert_eq!(super::fold("Đồng bộ"), "dong bo");
    }
}
