//! The open room: its timeline in a recycling `gtk::ListView` (one store
//! per room visited, so switching back is instant), the message menu, and
//! the composer.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use chosua_core::models::{ReactionSummary, TimelineEvent};
use chosua_core::Command;
use gtk::{gdk, gio, glib};

use crate::composer::{Composer, Outgoing};
use crate::window::Ctx;

/// Largest size (logical px) an image is shown at in the timeline.
const IMG_W: f64 = 400.0;
const IMG_H: f64 = 320.0;

/// The quick reactions offered in the message menu.
const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

struct RoomTimeline {
    store: gio::ListStore,
    reached_start: Cell<bool>,
    paginating: Cell<bool>,
    /// pagination answers in a row that added nothing (see `prepend`)
    empty_pages: Cell<u8>,
}

pub struct TimelinePane {
    pub root: gtk::Widget,
    ctx: Rc<Ctx>,
    title: adw::WindowTitle,
    header: adw::HeaderBar,
    list: gtk::ListView,
    pub scroller: gtk::ScrolledWindow,
    pub composer: Rc<Composer>,
    side: adw::ToolbarView,
    paned: gtk::Paned,
    side_stack: gtk::Stack,
    side_title: gtk::Label,
    threads_button: gtk::ToggleButton,
    threads_view: gtk::ListView,
    threads_store: gio::ListStore,
    threads_status: gtk::Label,
    threads_more: gtk::Button,
    /// keeps the threads list at its bottom (newest) until scrolled up
    threads_pin: Rc<Cell<bool>>,
    thread_list: gtk::ListView,
    thread_store: gio::ListStore,
    thread_composer: Rc<Composer>,
    /// (room id, root event) of the thread in the side panel
    open_thread: RefCell<Option<(String, TimelineEvent)>>,
    rooms: RefCell<HashMap<String, Rc<RoomTimeline>>>,
    current: Rc<RefCell<Option<String>>>,
    next_local_id: Cell<u64>,
    toast: RefCell<Option<Box<dyn Fn(&str)>>>,
    jump: RefCell<Option<Box<dyn Fn(&str)>>>,
    /// our own profile, for the local echo of what we send
    me: RefCell<Option<chosua_core::models::OwnProfile>>,
    /// (user id, name) per room, for @-completion; asked once per room
    members: RefCell<HashMap<String, Vec<(String, String)>>>,
    /// room -> user id -> avatar mxc, for the "seen by" avatars
    member_avatars: RefCell<HashMap<String, HashMap<String, String>>>,
    /// messages shared by link, fetched from the server (`None`: gone or
    /// not visible to us), and which shown messages show each one
    previews: RefCell<HashMap<(String, String), Option<TimelineEvent>>>,
    preview_holders: RefCell<HashMap<(String, String), Vec<String>>>,
    /// a shared message being looked for (opened from a link)
    jump_target: RefCell<Option<JumpTarget>>,
    /// the message just jumped to, highlighted for a moment
    highlight: RefCell<Option<String>>,
    /// the sticker/meme picker of each composer (main, thread)
    sticker_grids: RefCell<Vec<(gtk::FlowBox, bool)>>,
    /// every room's threads seen so far (ListThreads), for the
    /// all-rooms panel and the unread dots
    threads_by_room: RefCell<HashMap<String, Vec<TimelineEvent>>>,
    /// the threads panel shows every room's, not just this room's
    threads_global: Cell<bool>,
    /// the all-rooms panel lists only threads with unread replies
    threads_unread_only: Cell<bool>,
    /// rooms whose thread list is about to be re-read from the server
    threads_refresh: RefCell<std::collections::HashSet<String>>,
    /// told how many threads are unread, whenever that may have changed
    on_unread_threads: RefCell<Option<Box<dyn Fn(usize)>>>,
    /// root event id -> its room, for the per-row lookups
    thread_rooms: RefCell<HashMap<String, String>>,
    /// rooms whose threads are still to be asked for (a slow drip: all at
    /// once crowded out loading the room you're looking at)
    scan_queue: RefCell<std::collections::VecDeque<String>>,
    scan_running: Cell<bool>,
    /// coalesced redraws of the threads panel / unread badges
    threads_dirty: Cell<bool>,
    /// stay at the bottom while rows are measured / echoes replaced / the
    /// composer grows: on after a send or once at the bottom, off when the
    /// user scrolls up (timeline, thread panel)
    pin_main: Rc<Cell<bool>>,
    pin_thread: Rc<Cell<bool>>,
    room_name: RefCell<Option<Box<dyn Fn(&str) -> Option<String>>>>,
}

/// Below this width the room and its thread panel no longer fit side by side
/// (room 360 + panel 280 + handle, with room to read): the panel replaces the room.
const NARROW_SIDE_BY_SIDE: i32 = 760;

fn obj(ev: TimelineEvent) -> glib::BoxedAnyObject {
    glib::BoxedAnyObject::new(ev)
}

fn event(o: &glib::Object) -> std::cell::Ref<'_, TimelineEvent> {
    o.downcast_ref::<glib::BoxedAnyObject>().expect("timeline item").borrow()
}

/// Widgets of one recycled message row, and the event it shows now.
struct Row {
    ev: RefCell<Option<TimelineEvent>>,
    /// shown in the thread panel (no thread of a thread)
    in_thread: Cell<bool>,
    thread_btn: gtk::Button,
    bubble: gtk::Box,
    avatar: adw::Avatar,
    reply: gtk::Label,
    reactions: gtk::Box,
    /// avatars of who has read up to this message
    seen: gtk::Box,
    thread: gtk::Label,
    header: gtk::Box,
    sender: gtk::Label,
    time: gtk::Label,
    body: gtk::Label,
    picture: gtk::Picture,
    media: gtk::Button,
    status: gtk::Label,
    /// previews of messages shared as bare matrix.to links
    shared: gtk::Box,
}

const ROW: &str = "chosua-row";

fn row_of(line: &gtk::Widget) -> &Row {
    unsafe { line.data::<Row>(ROW).unwrap().as_ref() }
}

impl TimelinePane {
    pub fn new(ctx: Rc<Ctx>) -> Rc<Self> {
        let title = adw::WindowTitle::new("", "");
        title.add_css_class("pane-title");
        // the window's controls sit on the far right: the side panel's bar
        // when it's open, this one otherwise
        let header = adw::HeaderBar::builder().title_widget(&title).show_start_title_buttons(false).build();

        let list = gtk::ListView::builder().css_classes(["timeline"]).build();
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vexpand(true)
            .child(&list)
            .build();

        let composer = Composer::new();
        let threads_button = gtk::ToggleButton::builder()
            .icon_name("mail-reply-all-symbolic")
            .tooltip_text("Các thread của phòng")
            .css_classes(["circular", "glass-button"])
            .build();
        header.pack_end(&threads_button);

        let view = adw::ToolbarView::builder().hexpand(true).build();
        view.add_top_bar(&header);
        view.set_content(Some(&scroller));
        view.add_bottom_bar(&composer.root);

        // the side panel: the room's threads, or one thread
        let side_title = gtk::Label::builder().css_classes(["pane-title"]).build();
        let side_close = gtk::Button::builder().icon_name("window-close-symbolic").css_classes(["circular", "glass-button"]).build();
        let side_back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Về danh sách thread")
            .visible(false)
            .css_classes(["circular", "glass-button"])
            .build();
        let side_header = adw::HeaderBar::builder()
            .title_widget(&side_title)
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .build();
        side_header.pack_end(&side_close);
        side_header.pack_start(&side_back);
        // both lists are their scrolled window's own child: only the rows
        // on screen are built (a ListView anywhere else builds them all)
        let threads_view = gtk::ListView::builder().css_classes(["threads-list"]).build();
        let threads_status = gtk::Label::builder().css_classes(["dim-label"]).margin_top(24).visible(false).build();
        let threads_more = gtk::Button::builder()
            .label("Tải thêm thread cũ hơn")
            .css_classes(["pill", "glass-button"])
            .halign(gtk::Align::Center)
            .margin_top(6)
            .margin_bottom(6)
            .visible(false)
            .build();
        let threads_list_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&threads_view)
            .build();
        let threads_scroll = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        // newest at the bottom like the timeline, so older ones load above
        let threads_pin = Rc::new(Cell::new(true));
        pin_to_bottom(&threads_list_scroll, &threads_pin);
        threads_scroll.append(&threads_status);
        threads_scroll.append(&threads_more);
        threads_scroll.append(&threads_list_scroll);
        // a thread: its root is the list's first row
        let thread_list = gtk::ListView::builder().css_classes(["timeline"]).build();
        let thread_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&thread_list)
            .build();
        let thread_composer = Composer::new();
        let thread_view = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        thread_view.append(&thread_scroll);
        thread_view.append(&thread_composer.root);
        let side_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::None).build();
        side_stack.add_named(&threads_scroll, Some("threads"));
        side_stack.add_named(&thread_view, Some("thread"));
        let side_view = adw::ToolbarView::builder().width_request(380).css_classes(["side-panel"]).build();
        side_view.add_top_bar(&side_header);
        side_view.set_content(Some(&side_stack));
        for card in [view.upcast_ref::<gtk::Widget>(), side_view.upcast_ref()] {
            card.add_css_class("mm-card");
            card.set_overflow(gtk::Overflow::Hidden);
        }
        let side = side_view.clone();
        side.set_visible(false);
        side.set_width_request(280);
        view.set_width_request(320);
        // room | thread: drag the divider; the panel's width is remembered
        let root = gtk::Paned::builder()
            .orientation(gtk::Orientation::Horizontal)
            .start_child(&view)
            .end_child(&side)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .resize_end_child(false)
            .css_classes(["thread-split"])
            .build();
        // narrow window (half a screen): the thread panel takes the place of
        // the room instead of squeezing it; closing the panel brings it back
        {
            let (view, side) = (view.clone(), side.clone());
            let shown = Cell::new((0, false));
            root.add_tick_callback(move |p, _| {
                let state = (p.width(), side.is_visible());
                if shown.get() != state {
                    shown.set(state);
                    let narrow = state.1 && state.0 > 0 && state.0 < NARROW_SIDE_BY_SIDE;
                    if view.is_visible() == narrow {
                        view.set_visible(!narrow);
                    }
                }
                glib::ControlFlow::Continue
            });
        }
        {
            let (side, header) = (side.clone(), header.clone());
            side.connect_visible_notify(move |s| header.set_show_end_title_buttons(!s.is_visible()));
        }
        {
            let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
            root.connect_position_notify(move |p| {
                if !p.is_mapped() || !p.end_child().is_some_and(|c| c.is_visible()) {
                    return;
                }
                if let Some(t) = pending.borrow_mut().take() {
                    t.remove();
                }
                let (p, pending2) = (p.clone(), pending.clone());
                let id = glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                    pending2.borrow_mut().take();
                    let w = p.width() - p.position();
                    if w > 0 {
                        crate::config::update(|c| c.side_width = w);
                    }
                });
                *pending.borrow_mut() = Some(id);
            });
        }

        let this = Rc::new(TimelinePane {
            root: root.clone().upcast(),
            paned: root,
            ctx,
            title,
            header: header.clone(),
            list,
            scroller: scroller.clone(),
            composer: composer.clone(),
            side,
            side_stack,
            side_title,
            threads_button: threads_button.clone(),
            threads_view: threads_view.clone(),
            threads_store: gio::ListStore::new::<glib::BoxedAnyObject>(),
            threads_status,
            threads_more: threads_more.clone(),
            threads_pin,
            thread_list,
            thread_store: gio::ListStore::new::<glib::BoxedAnyObject>(),
            thread_composer: thread_composer.clone(),
            open_thread: RefCell::default(),

            rooms: RefCell::default(),
            current: Rc::default(),
            next_local_id: Cell::new(0),
            toast: RefCell::default(),
            jump: RefCell::default(),
            me: RefCell::default(),
            members: RefCell::default(),
            member_avatars: RefCell::default(),
            previews: RefCell::default(),
            preview_holders: RefCell::default(),
            jump_target: RefCell::default(),
            highlight: RefCell::default(),
            sticker_grids: RefCell::default(),
            threads_by_room: RefCell::default(),
            threads_global: Cell::new(false),
            threads_unread_only: Cell::new(false),
            threads_refresh: RefCell::default(),
            on_unread_threads: RefCell::new(None),
            thread_rooms: RefCell::default(),
            scan_queue: RefCell::default(),
            scan_running: Cell::new(false),
            threads_dirty: Cell::new(false),
            pin_main: Rc::new(Cell::new(true)),
            pin_thread: Rc::new(Cell::new(true)),
            room_name: RefCell::default(),
        });

        this.add_sticker_picker(&composer, false);
        this.add_sticker_picker(&thread_composer, true);
        this.list.set_factory(Some(&this.message_factory(&this.list)));
        this.thread_list.set_factory(Some(&this.message_factory(&this.thread_list)));
        this.threads_view.set_factory(Some(&this.threads_factory()));
        this.threads_view.set_model(Some(&gtk::NoSelection::new(Some(this.threads_store.clone()))));
        {
            let this = Rc::downgrade(&this);
            threads_more.connect_clicked(move |b| {
                let Some(this) = this.upgrade() else { return };
                if let Some(room_id) = this.current() {
                    b.set_sensitive(false);
                    this.ctx.send(Command::LoadMoreThreads { room_id });
                }
            });
        }
        {
            let this = Rc::downgrade(&this);
            composer.connect_send(move |out| {
                if let Some(this) = this.upgrade() {
                    this.outgoing(out, None);
                }
            });
        }
        {
            let this = Rc::downgrade(&this);
            thread_composer.connect_send(move |out| {
                if let Some(this) = this.upgrade() {
                    let root = this.open_thread.borrow().as_ref().map(|(_, root)| root.event_id.clone());
                    if root.is_some() {
                        this.outgoing(out, root);
                    }
                }
            });
        }
        pin_to_bottom(&scroller, &this.pin_main);
        pin_to_bottom(&thread_scroll, &this.pin_thread);
        {
            let weak = Rc::downgrade(&this);
            // older messages load ahead, while there's still ~1.5 screens
            // left above, not only once the very top is hit
            let adj = scroller.vadjustment();
            adj.connect_value_changed(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.maybe_paginate();
                }
            });
            // ... and again when rows were added/measured (a short room
            // that doesn't fill the view keeps loading)
            let weak = Rc::downgrade(&this);
            adj.connect_upper_notify(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.maybe_paginate();
                }
            });
        }
        // drop files anywhere on the room to send them
        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        {
            let composer = composer.clone();
            drop.connect_drop(move |_, value, _, _| {
                let Ok(files) = value.get::<gdk::FileList>() else { return false };
                let composer = composer.clone();
                glib::spawn_future_local(async move { composer.attach_files(files.files()).await });
                true
            });
        }
        view.add_controller(drop);

        {
            let this = Rc::downgrade(&this);
            threads_button.connect_toggled(move |b| {
                let Some(this) = this.upgrade() else { return };
                if b.is_active() {
                    this.show_threads();
                } else if this.side_stack.visible_child_name().as_deref() == Some("threads") {
                    this.close_side();
                }
            });
        }
        {
            let back = side_back.clone();
            this.side_stack.connect_visible_child_name_notify(move |s| {
                back.set_visible(s.visible_child_name().as_deref() == Some("thread"));
            });
        }
        {
            let this = Rc::downgrade(&this);
            side_back.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.back_to_threads();
                }
            });
        }
        {
            let this = Rc::downgrade(&this);
            side_close.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.close_side();
                }
            });
        }
        this
    }

    /// A factory for message rows in `list` (the room's timeline or a
    /// thread): grouping looks at the previous row of the same list.
    fn message_factory(self: &Rc<Self>, list: &gtk::ListView) -> gtk::SignalListItemFactory {
        let in_thread = list == &self.thread_list;
        let factory = gtk::SignalListItemFactory::new();
        {
            let weak = Rc::downgrade(self);
            factory.connect_setup(move |_, item| {
                let Some(this) = weak.upgrade() else { return };
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                item.set_activatable(false);
                item.set_focusable(false);
                item.set_child(Some(&this.build_row()));
            });
        }
        {
            let (weak, list) = (Rc::downgrade(self), list.downgrade());
            factory.connect_bind(move |_, item| {
                let (Some(this), Some(list)) = (weak.upgrade(), list.upgrade()) else { return };
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let line = item.child().unwrap();
                let model = list.model().unwrap();
                let pos = item.position();
                let obj = item.item().unwrap();
                let ev = event(&obj);
                // consecutive messages from one sender within 5 min: no header
                let grouped = pos > 0
                    && model.item(pos - 1).is_some_and(|p| {
                        let p = event(&p);
                        p.sender == ev.sender && ev.timestamp - p.timestamp < 5 * 60 * 1000
                    });
                row_of(&line).in_thread.set(in_thread);
                this.bind_row(&line, &ev, grouped);
            });
        }
        factory
    }

    /// Open another room (a matrix.to link, a resolved shared event).
    pub fn connect_jump(&self, f: impl Fn(&str) + 'static) {
        *self.jump.borrow_mut() = Some(Box::new(f));
    }

    fn jump(&self, room_id: &str) {
        if let Some(f) = self.jump.borrow().as_ref() {
            f(room_id);
        }
    }

    /// A matrix.to link to an event, once the worker found it: open its
    /// room (and thread), then find it — loading older history page by page
    /// if it isn't loaded yet — scroll to it and highlight it (app.js's
    /// `findAndScrollToMessage` / `resolveThreadLink`).
    pub fn shared_event(self: &Rc<Self>, room_id: &str, event_id: &str, found: bool, thread_root_id: Option<String>) {
        if !found {
            self.toast("Không tìm thấy tin nhắn này — có thể đã bị xoá, hoặc bạn không xem được");
            return;
        }
        *self.jump_target.borrow_mut() = Some(JumpTarget {
            room_id: room_id.to_string(),
            event_id: event_id.to_string(),
            thread_root: thread_root_id.filter(|r| r != event_id),
            pages: 0,
            thread_opened: false,
        });
        if !self.is_current(room_id) {
            self.jump(room_id);
        }
        let this = self.clone();
        glib::idle_add_local_once(move || this.try_jump());
    }

    /// One more step towards the jump target, whenever messages arrive.
    fn try_jump(self: &Rc<Self>) {
        let Some(mut t) = self.jump_target.borrow().clone() else { return };
        if !self.is_current(&t.room_id) {
            return; // the room is still opening
        }
        // in a thread: open it, then find the reply in it
        if let Some(root) = t.thread_root.clone() {
            if !t.thread_opened {
                t.thread_opened = true;
                *self.jump_target.borrow_mut() = Some(t);
                self.open_thread_by_id(&t_room(&self.jump_target), &root);
                return;
            }
            if let Some(i) = find(&self.thread_store, &t.event_id) {
                self.jump_target.borrow_mut().take();
                self.flash(&t.event_id);
                self.scroll_thread_to(i);
            }
            return; // more replies are still loading: thread_prepend calls back
        }
        let Some(room) = self.room(&t.room_id) else { return };
        if let Some(i) = find(&room.store, &t.event_id) {
            self.jump_target.borrow_mut().take();
            self.flash(&t.event_id);
            let list = self.list.clone();
            glib::idle_add_local_once(move || {
                let info = gtk::ScrollInfo::new();
                info.set_enable_vertical(true);
                list.scroll_to(i, gtk::ListScrollFlags::NONE, Some(info));
            });
            return;
        }
        if room.reached_start.get() {
            self.jump_target.borrow_mut().take();
            self.toast("Không tìm thấy tin này — có thể đã bị xoá, hoặc gửi trước khi bạn vào phòng");
            return;
        }
        t.pages += 1;
        if t.pages > JUMP_MAX_PAGES {
            self.jump_target.borrow_mut().take();
            self.toast("Đã tìm ngược khá xa mà chưa thấy tin này — dừng tìm");
            return;
        }
        if t.pages == 1 {
            self.toast("Đang tải tin cũ hơn để tìm tin được chia sẻ…");
        } else if t.pages % 10 == 0 {
            self.toast(&format!("Vẫn đang tìm… ({} trang)", t.pages));
        }
        *self.jump_target.borrow_mut() = Some(t);
        self.paginate();
    }

    /// Highlight one message for a moment.
    fn flash(self: &Rc<Self>, event_id: &str) {
        *self.highlight.borrow_mut() = Some(event_id.to_string());
        let rerender = |this: &TimelinePane, id: &str| {
            let lists = [(&this.list, &this.pin_main), (&this.thread_list, &this.pin_thread)];
            keep_scroll(&lists, || {
                if let Some(room) = this.current().and_then(|r| this.room(&r)) {
                    if let Some(i) = find(&room.store, id) {
                        room.store.items_changed(i, 1, 1);
                    }
                }
                if let Some(i) = find(&this.thread_store, id) {
                    this.thread_store.items_changed(i, 1, 1);
                }
            });
        };
        rerender(self, event_id);
        let (this, id) = (Rc::downgrade(self), event_id.to_string());
        glib::timeout_add_local_once(std::time::Duration::from_millis(2500), move || {
            let Some(this) = this.upgrade() else { return };
            if this.highlight.borrow().as_deref() == Some(id.as_str()) {
                this.highlight.borrow_mut().take();
                rerender(&this, &id);
            }
        });
    }

    /// Scroll the thread panel so its `i`th row is in view.
    fn scroll_thread_to(&self, i: u32) {
        let list = self.thread_list.clone();
        glib::idle_add_local_once(move || {
            let info = gtk::ScrollInfo::new();
            info.set_enable_vertical(true);
            list.scroll_to(i, gtk::ListScrollFlags::NONE, Some(info));
        });
    }
    pub fn set_me(&self, me: chosua_core::models::OwnProfile) {
        *self.me.borrow_mut() = Some(me);
    }

    pub fn add_header_button(&self, w: &impl IsA<gtk::Widget>) {
        self.header.pack_end(w);
    }

    pub fn connect_toast(self: &Rc<Self>, f: impl Fn(&str) + 'static) {
        *self.toast.borrow_mut() = Some(Box::new(f));
        for c in [&self.composer, &self.thread_composer] {
            let this = Rc::downgrade(self);
            c.connect_error(move |msg| {
                if let Some(this) = this.upgrade() {
                    this.toast(msg);
                }
            });
        }
    }

    fn toast(&self, msg: &str) {
        if let Some(f) = self.toast.borrow().as_ref() {
            f(msg);
        }
    }

    fn build_row(self: &Rc<Self>) -> gtk::Widget {
        let sender = gtk::Label::builder().xalign(0.0).css_classes(["sender"]).build();
        let time = gtk::Label::builder().xalign(0.0).css_classes(["dim-label", "caption"]).build();
        let header = gtk::Box::builder().spacing(8).build();
        header.append(&sender);
        header.append(&time);
        let body = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .max_width_chars(64)
            .use_markup(true)
            .selectable(true)
            .css_classes(["body"])
            .build();
        {
            let ctx = self.ctx.clone();
            let this = Rc::downgrade(self);
            body.connect_activate_link(move |_, uri| {
                if let (Some(this), Some((room_id, event_id))) = (this.upgrade(), matrix_to_room(uri)) {
                    match event_id {
                        Some(event_id) => ctx.send(Command::ResolveSharedEvent { room_id, event_id }),
                        None => this.jump(&room_id),
                    }
                } else {
                    ctx.send(Command::OpenUrl { url: uri.to_string() });
                }
                glib::Propagation::Stop
            });
        }
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .halign(gtk::Align::Start)
            .css_classes(["media"])
            .cursor(&gdk::Cursor::from_name("pointer", None).unwrap())
            .build();
        // the shown size follows the decoded image (stickers often carry no
        // size, and a guessed box cropped them); the event's own size only
        // reserves the space until it arrives
        picture.connect_paintable_notify(|p| {
            if let Some(pt) = p.paintable() {
                let (w, h) = (pt.intrinsic_width(), pt.intrinsic_height());
                if w > 0 && h > 0 {
                    let (w, h) = fit(Some((w as u32, h as u32)));
                    p.set_size_request(w, h);
                }
            }
        });
        let media = gtk::Button::builder().halign(gtk::Align::Start).css_classes(["pill", "small"]).build();
        let reply = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .css_classes(["reply-quote", "caption"])
            .build();
        let reactions = gtk::Box::builder().spacing(4).css_classes(["reactions"]).build();
        let seen = gtk::Box::builder().spacing(0).halign(gtk::Align::Start).valign(gtk::Align::Center).visible(false).css_classes(["read-receipts"]).build();
        // reactions and "seen" avatars share one line under the message
        let footer = gtk::Box::builder().spacing(6).build();
        footer.append(&reactions);
        footer.append(&seen);
        let thread = gtk::Label::builder().xalign(0.0).halign(gtk::Align::Start).css_classes(["thread-info", "caption"]).build();
        let status = gtk::Label::builder().xalign(1.0).visible(false).css_classes(["caption", "send-status"]).build();
        let shared = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).visible(false).build();
        // each message is its own pane of glass: a bubble sized to its
        // content, on an otherwise see-through window
        let bubble = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .css_classes(["bubble"])
            .build();
        for w in [
            header.upcast_ref::<gtk::Widget>(),
            reply.upcast_ref(),
            body.upcast_ref(),
            shared.upcast_ref(),
            picture.upcast_ref(),
            media.upcast_ref(),
            footer.upcast_ref(),
            thread.upcast_ref(),
            status.upcast_ref(),
        ] {
            bubble.append(w);
        }
        let avatar = adw::Avatar::builder().size(32).show_initials(true).valign(gtk::Align::Start).build();
        let line = gtk::Box::builder().spacing(10).css_classes(["message"]).build();
        line.append(&avatar);
        line.append(&bubble);
        // reply in a thread: on every message of the room, shown on hover
        let thread_btn = gtk::Button::builder()
            .icon_name("mail-reply-all-symbolic")
            .tooltip_text("Trả lời trong thread")
            .valign(gtk::Align::Center)
            .css_classes(["circular", "glass-button", "row-action"])
            .build();
        {
            let (this, line) = (Rc::downgrade(self), line.clone().upcast::<gtk::Widget>().downgrade());
            thread_btn.connect_clicked(move |_| {
                let (Some(this), Some(line)) = (this.upgrade(), line.upgrade()) else { return };
                let ev = row_of(&line).ev.borrow().clone();
                if let Some(ev) = ev {
                    this.open_thread(&ev);
                }
            });
        }
        line.append(&thread_btn);

        // right click / long press: the message menu
        let line_w: gtk::Widget = line.clone().upcast();
        // capture phase: the message menu wins over the selectable label's
        // own copy/select-all popup
        let click = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        {
            let (this, line) = (Rc::downgrade(self), line_w.downgrade());
            click.connect_pressed(move |g, _, x, y| {
                if let (Some(this), Some(line)) = (this.upgrade(), line.upgrade()) {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    this.show_menu(&line, x, y);
                }
            });
        }
        bubble.add_controller(click);
        let press = gtk::GestureLongPress::new();
        {
            let (this, line) = (Rc::downgrade(self), line_w.downgrade());
            press.connect_pressed(move |_, x, y| {
                if let (Some(this), Some(line)) = (this.upgrade(), line.upgrade()) {
                    this.show_menu(&line, x, y);
                }
            });
        }
        bubble.add_controller(press);
        // "💬 N replies": open the thread
        let open_thread = gtk::GestureClick::new();
        {
            let (this, line) = (Rc::downgrade(self), line_w.downgrade());
            open_thread.connect_released(move |_, _, _, _| {
                let (Some(this), Some(line)) = (this.upgrade(), line.upgrade()) else { return };
                let ev = row_of(&line).ev.borrow().clone();
                if let Some(ev) = ev {
                    this.open_thread(&ev);
                }
            });
        }
        thread.add_controller(open_thread);
        thread.set_cursor(gdk::Cursor::from_name("pointer", None).as_ref());
        // click an image: look at it big
        let open = gtk::GestureClick::new();
        {
            let (this, line) = (Rc::downgrade(self), line_w.downgrade());
            open.connect_released(move |_, _, _, _| {
                if let (Some(this), Some(line)) = (this.upgrade(), line.upgrade()) {
                    this.show_image(&line);
                }
            });
        }
        picture.add_controller(open);
        // video/file/full image: open with the desktop's default app
        {
            let (ctx, line) = (self.ctx.clone(), line_w.downgrade());
            media.connect_clicked(move |_| {
                let Some(line) = line.upgrade() else { return };
                let Some(ev) = row_of(&line).ev.borrow().clone() else { return };
                let Some(url) = ev.media_url.clone() else { return };
                let (filename, media_encryption) = (ev.body.clone(), ev.media_encryption.clone());
                ctx.send(if matches!(ev.msg_type.as_str(), "video" | "audio") {
                    Command::PlayVideo { mxc_uri: url, filename, media_encryption }
                } else {
                    Command::OpenMediaExternally { mxc_uri: url, filename, media_encryption }
                });
            });
        }

        unsafe {
            line.set_data(
                ROW,
                Row {
                    ev: RefCell::new(None),
                    in_thread: Cell::new(false),
                    thread_btn,
                    bubble,
                    avatar,
                    reply,
                    reactions,
                    seen,
                    thread,
                    header,
                    sender,
                    time,
                    body,
                    picture,
                    media,
                    status,
                    shared,
                },
            );
        }
        line.upcast()
    }

    fn bind_row(&self, line: &gtk::Widget, ev: &TimelineEvent, grouped: bool) {
        let row = row_of(line);
        *row.ev.borrow_mut() = Some(ev.clone());
        toggle_class(&row.bubble, "jump-highlight", self.highlight.borrow().as_deref() == Some(ev.event_id.as_str()));
        let pending = ev.local_id.as_deref() == Some(ev.event_id.as_str());
        row.thread_btn.set_visible(!row.in_thread.get() && !pending && !ev.event_id.is_empty());
        // everyone's messages look the same: on the left, with avatar and
        // name (ours too)
        line.set_halign(gtk::Align::Start);
        toggle_class(line, "grouped", grouped);
        row.header.set_visible(!grouped);
        // grouped rows keep the avatar's column, just empty
        row.avatar.set_opacity(if grouped { 0.0 } else { 1.0 });
        if !grouped {
            row.avatar.set_text(Some(&ev.sender_name));
            self.ctx.images.avatar(&row.avatar, ev.sender_avatar_url.as_deref());
        }
        row.sender.set_label(&ev.sender_name);
        // a stable colour per person, like Element's name colours
        let hue = ev.sender.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 8;
        for i in 0..8 {
            toggle_class(&row.sender, &format!("name-{i}"), i == hue);
        }
        row.time.set_label(&format_time(ev.timestamp));

        // a local echo: sending, or failed (its event_id is the local id)
        let pending = ev.local_id.as_deref() == Some(ev.event_id.as_str());
        let failed = pending && ev.msg_type == "failed";
        toggle_class(&row.bubble, "own", ev.is_own);
        toggle_class(&row.bubble, "pending", pending && !failed);
        toggle_class(&row.bubble, "failed", failed);
        row.status.set_visible(pending);
        row.status.set_label(if failed { "⚠ Không gửi được" } else { "Đang gửi…" });

        // the server only resolves the replied-to message sometimes (not for
        // our own echo or a live /sync one): find it among the loaded rows
        let reply = ev.reply_to_event_id.as_deref().map(|id| {
            ev.reply_to_preview.clone().or_else(|| self.preview_of(id)).unwrap_or_else(|| "Trả lời một tin nhắn".into())
        });
        row.reply.set_visible(reply.is_some());
        row.reply.set_label(reply.as_deref().unwrap_or(""));
        let (replies, _) = self.thread_summary(ev);
        row.thread.set_visible(replies > 0);
        if replies > 0 {
            let dot = if self.thread_unread(ev) { "<span foreground=\"#e5484d\">●</span> " } else { "" };
            let text = format!("{dot}💬 {replies} trả lời");
            row.thread.set_markup(&text);
            toggle_class(&row.thread, "unread", !dot.is_empty());
        }
        self.bind_reactions(row, ev);
        self.bind_seen(row, ev);
        if let Some(footer) = row.seen.parent() {
            footer.set_visible(!ev.read_by.is_empty() || !ev.reactions.is_empty());
        }

        let is_media = matches!(ev.msg_type.as_str(), "image" | "video" | "file");
        let caption = !is_media || (ev.msg_type != "file" && !looks_like_filename(&ev.body));
        // bare matrix.to links to a message become preview cards (app.js's
        // `decorateSharedLinks`); a link with its own text stays a link
        let (text, links) = split_shared_links(&ev.body);
        row.body.set_visible(caption && !text.trim().is_empty());
        let mut markup = crate::markdown::to_pango(&text);
        {
            // tagged people from m.mentions / pills; when the sender's client
            // set neither, any "@Member" in the text still counts as a tag
            let tagged = !ev.mentioned_user_ids.is_empty();
            let names: Vec<String> = {
                let members = self.members.borrow();
                let room = self.current().unwrap_or_default();
                members
                    .get(&room)
                    .map(|m| {
                        m.iter()
                            .filter(|(id, n)| n.chars().count() >= 2 && (!tagged || ev.mentioned_user_ids.contains(id)))
                            .map(|(_, n)| n.clone())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            markup = crate::markdown::highlight_mentions(&markup, &names, ev.is_own, tagged);
        }
        row.body.set_markup(&markup);
        self.bind_shared(row, &ev.event_id, &links);
        toggle_class(&row.body, "dim-label", ev.msg_type == "notice");

        let thumb = match ev.msg_type.as_str() {
            "image" => ev.media_url.as_deref(),
            "video" => ev.thumbnail_url.as_deref(),
            _ => None,
        };
        match thumb {
            Some(mxc) => {
                let (w, h) = fit(ev.media_size);
                row.picture.set_size_request(w, h);
                row.picture.set_visible(true);
                self.ctx.images.show(&row.picture, mxc, ev.media_encryption.as_deref());
            }
            None => {
                row.picture.set_visible(false);
                row.picture.set_paintable(None::<&gdk::Paintable>);
            }
        }
        let media_label = match (ev.msg_type.as_str(), ev.media_url.is_some()) {
            ("video", true) => Some(format!("▶ {}", ev.body)),
            ("file", true) => Some(format!("📎 {}", ev.body)),
            ("audio", true) => Some("▶ Tin nhắn thoại".to_string()),
            _ => None,
        };
        row.media.set_visible(media_label.is_some());
        row.media.set_label(media_label.as_deref().unwrap_or(""));
    }

    /// The room a row's event belongs to: its thread's room in the
    /// all-rooms panel, else the open one.
    fn row_room(&self, ev: &TimelineEvent) -> String {
        self.thread_rooms.borrow().get(&ev.event_id).cloned().or_else(|| self.current()).unwrap_or_default()
    }

    fn member_name(&self, room: &str, user_id: &str) -> String {
        self.members
            .borrow()
            .get(room)
            .and_then(|m| m.iter().find(|(id, _)| id == user_id).map(|(_, n)| n.clone()))
            .unwrap_or_else(|| user_id.to_string())
    }

    /// Whether a thread root has replies this account hasn't read: the
    /// server-derived flag on the `/threads` entry, else on the event.
    fn thread_unread(&self, ev: &TimelineEvent) -> bool {
        let room = self.thread_rooms.borrow().get(&ev.event_id).cloned();
        let map = self.threads_by_room.borrow();
        let listed = room.and_then(|r| map.get(&r)).and_then(|ts| ts.iter().find(|t| t.event_id == ev.event_id));
        listed.and_then(|t| t.is_unread).or(ev.is_unread).unwrap_or(false)
    }

    /// The small avatar stack of who has read up to this message.
    fn bind_seen(&self, row: &Row, ev: &TimelineEvent) {
        while let Some(child) = row.seen.first_child() {
            row.seen.remove(&child);
        }
        row.seen.set_visible(!ev.read_by.is_empty());
        let room = self.row_room(ev);
        let avatars = self.member_avatars.borrow();
        let urls = avatars.get(&room);
        // a crowd would stretch the bubble: a few avatars, then "+N" whose
        // tooltip names everyone
        const SHOWN: usize = 7;
        for id in ev.read_by.iter().take(SHOWN) {
            let name = self.member_name(&room, id);
            let avatar = adw::Avatar::builder().size(14).text(&name).show_initials(true).build();
            avatar.set_tooltip_text(Some(&format!("Đã xem bởi {name}")));
            self.ctx.images.avatar(&avatar, urls.and_then(|u| u.get(id)).map(String::as_str));
            row.seen.append(&avatar);
        }
        if ev.read_by.len() > SHOWN {
            let names = ev.read_by.iter().map(|id| self.member_name(&room, id)).collect::<Vec<_>>().join(", ");
            let more = gtk::Label::builder()
                .label(format!("+{}", ev.read_by.len() - SHOWN))
                .css_classes(["caption", "dim-label"])
                .tooltip_text(format!("Đã xem bởi {names}"))
                .build();
            row.seen.append(&more);
        }
    }

    fn bind_reactions(&self, row: &Row, ev: &TimelineEvent) {
        while let Some(child) = row.reactions.first_child() {
            row.reactions.remove(&child);
        }
        row.reactions.set_visible(!ev.reactions.is_empty());
        let room = self.row_room(ev);
        for r in &ev.reactions {
            let pill = gtk::ToggleButton::builder()
                .label(format!("{} {}", r.emoji, r.count))
                .active(r.by_me)
                .css_classes(["reaction"])
                .build();
            let who = r.senders.iter().map(|id| self.member_name(&room, id)).collect::<Vec<_>>().join(", ");
            let action = if r.by_me { "bấm để bỏ reaction" } else { "bấm để react" };
            pill.set_tooltip_text(Some(&if who.is_empty() { action.to_string() } else { format!("{who} — {action}") }));
            let (ctx, emoji, event_id) = (self.ctx.clone(), r.emoji.clone(), ev.event_id.clone());
            let current = self.current.clone();
            pill.connect_clicked(move |_| {
                if let Some(room_id) = current.borrow().clone() {
                    ctx.send(Command::ToggleReaction { room_id, event_id: event_id.clone(), emoji: emoji.clone() });
                }
            });
            row.reactions.append(&pill);
        }
    }

    /// The message menu: quick reactions, reply, edit, copy, pin, save, delete.
    fn show_menu(self: &Rc<Self>, line: &gtk::Widget, x: f64, y: f64) {
        let row = row_of(line);
        let Some(ev) = row.ev.borrow().clone() else { return };
        if ev.local_id.as_deref() == Some(ev.event_id.as_str()) {
            return; // not sent yet: nothing to act on
        }
        let Some(room_id) = self.current() else { return };

        let popover = gtk::Popover::builder().has_arrow(false).css_classes(["menu", "message-menu"]).build();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        let quick = gtk::Box::builder().spacing(2).margin_bottom(4).build();
        for emoji in QUICK_REACTIONS {
            let b = gtk::Button::builder().label(emoji).css_classes(["flat", "quick-reaction"]).build();
            let (ctx, room_id, event_id, p) = (self.ctx.clone(), room_id.clone(), ev.event_id.clone(), popover.clone());
            b.connect_clicked(move |_| {
                ctx.send(Command::ToggleReaction { room_id: room_id.clone(), event_id: event_id.clone(), emoji: emoji.into() });
                p.popdown();
            });
            quick.append(&b);
        }
        let more = gtk::MenuButton::builder().icon_name("list-add-symbolic").tooltip_text("Emoji khác").css_classes(["flat", "quick-reaction"]).build();
        let chooser = gtk::EmojiChooser::new();
        {
            let (ctx, room_id, event_id, p) = (self.ctx.clone(), room_id.clone(), ev.event_id.clone(), popover.clone());
            chooser.connect_emoji_picked(move |_, emoji| {
                ctx.send(Command::ToggleReaction { room_id: room_id.clone(), event_id: event_id.clone(), emoji: emoji.into() });
                p.popdown();
            });
        }
        more.set_popover(Some(&chooser));
        quick.append(&more);
        col.append(&quick);

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

        // a message in the thread panel is answered (and edited) in the
        // thread's composer, so the reply stays in the thread; from the
        // threads list, its thread opens first
        let in_thread = row.in_thread.get();
        let composer_for = {
            let this = Rc::downgrade(self);
            let ev = ev.clone();
            move || -> Option<Rc<Composer>> {
                let this = this.upgrade()?;
                if !in_thread {
                    return Some(this.composer.clone());
                }
                if this.open_thread.borrow().is_none() {
                    this.open_thread(&ev);
                }
                Some(this.thread_composer.clone())
            }
        };
        {
            let (pick, e) = (composer_for.clone(), ev.clone());
            item(
                "↩  Trả lời",
                Box::new(move || {
                    if let Some(c) = pick() {
                        c.reply_to(&e.event_id, &e.sender_name, &e.body);
                    }
                }),
            );
        }
        if ev.is_own && matches!(ev.msg_type.as_str(), "text" | "notice") {
            let (pick, e) = (composer_for.clone(), ev.clone());
            item(
                "✎  Sửa",
                Box::new(move || {
                    if let Some(c) = pick() {
                        c.edit(&e.event_id, &e.body);
                    }
                }),
            );
        }
        if !row.in_thread.get() {
            let (this, e) = (Rc::downgrade(self), ev.clone());
            item(
                "💬  Trả lời trong thread",
                Box::new(move || {
                    if let Some(this) = this.upgrade() {
                        this.open_thread(&e);
                    }
                }),
            );
        }
        {
            let (l, body) = (line.clone(), ev.body.clone());
            item("⧉  Sao chép", Box::new(move || l.clipboard().set_text(&body)));
        }
        {
            // a reply inside a thread links with its thread (app.js's
            // `buildMatrixToLink`), so opening it lands in the thread
            let thread_root = row
                .in_thread
                .get()
                .then(|| self.open_thread.borrow().as_ref().map(|(_, root)| root.event_id.clone()))
                .flatten()
                .filter(|root| *root != ev.event_id);
            let mut link = format!("https://matrix.to/#/{}/{}", room_id, ev.event_id);
            if let Some(root) = thread_root {
                link.push_str(&format!("?thread={root}"));
            }
            let (l, this) = (line.clone(), Rc::downgrade(self));
            item(
                "🔗  Sao chép liên kết (để chia sẻ)",
                Box::new(move || {
                    l.clipboard().set_text(&link);
                    if let Some(this) = this.upgrade() {
                        this.toast("Đã sao chép liên kết — dán vào phòng bất kỳ để chia sẻ");
                    }
                }),
            );
        }
        {
            let (ctx, r, e) = (self.ctx.clone(), room_id.clone(), ev.event_id.clone());
            item("📌  Ghim", Box::new(move || ctx.send(Command::PinMessage { room_id: r.clone(), event_id: e.clone() })));
        }
        if let Some(url) = ev.media_url.clone() {
            let (ctx, name, enc) = (self.ctx.clone(), ev.body.clone(), ev.media_encryption.clone());
            item(
                "⬇  Lưu vào Tải xuống",
                Box::new(move || {
                    ctx.send(Command::DownloadMedia {
                        mxc_uri: url.clone(),
                        filename: name.clone(),
                        media_encryption: enc.clone(),
                    })
                }),
            );
        }
        if ev.is_own {
            let (ctx, r, e) = (self.ctx.clone(), room_id.clone(), ev.event_id.clone());
            item("🗑  Xoá", Box::new(move || ctx.send(Command::DeleteMessage { room_id: r.clone(), event_id: e.clone() })));
        }

        popover.set_child(Some(&col));
        popover.set_parent(&row.bubble);
        let (bx, by) = line
            .compute_point(&row.bubble, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|p| (p.x() as i32, p.y() as i32))
            .unwrap_or((0, 0));
        popover.set_pointing_to(Some(&gdk::Rectangle::new(bx, by, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    /// An image, big: the thumbnail at once, the original once fetched,
    /// with zoom (see viewer.rs).
    fn show_image(&self, line: &gtk::Widget) {
        let row = row_of(line);
        let Some(ev) = row.ev.borrow().clone() else { return };
        if ev.msg_type != "image" {
            return;
        }
        let Some(url) = ev.media_url.clone() else { return };
        crate::viewer::show(
            &self.ctx,
            line,
            &ev.sender_name,
            &url,
            &ev.body,
            ev.media_encryption.clone(),
            ev.media_size,
            row.picture.paintable(),
        );
    }

    // ---- stickers / custom emoji (MSC2545 image packs) ----------------

    fn add_sticker_picker(self: &Rc<Self>, composer: &Rc<Composer>, in_thread: bool) {
        let grid = gtk::FlowBox::builder()
            .max_children_per_line(6)
            .min_children_per_line(4)
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .build();
        let add = gtk::Button::builder().label("Thêm meme của tôi…").css_classes(["pill"]).build();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        col.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::External)
                .min_content_height(260)
                .max_content_height(360)
                .propagate_natural_height(true)
                .child(&grid)
                .build(),
        );
        col.append(&add);
        let popover = gtk::Popover::builder().child(&col).build();
        let button = gtk::MenuButton::builder()
            .icon_name("insert-image-symbolic")
            .tooltip_text("Meme / emoji của phòng")
            .valign(gtk::Align::End)
            .popover(&popover)
            .css_classes(["circular", "glass-button"])
            .build();
        composer.add_tool(&button);
        {
            let this = Rc::downgrade(self);
            popover.connect_show(move |_| {
                let Some(this) = this.upgrade() else { return };
                if let Some(room_id) = this.current() {
                    this.ctx.send(Command::ListImagePacks { room_id });
                }
            });
        }
        {
            let (ctx, current) = (self.ctx.clone(), self.current.clone());
            add.connect_clicked(move |b| {
                let Some(room_id) = current.borrow().clone() else { return };
                let (ctx, window) = (ctx.clone(), b.root().and_downcast::<gtk::Window>());
                glib::spawn_future_local(async move {
                    let filter = gtk::FileFilter::new();
                    filter.add_mime_type("image/*");
                    let filters = gio::ListStore::new::<gtk::FileFilter>();
                    filters.append(&filter);
                    let dialog = gtk::FileDialog::builder().title("Ảnh meme").filters(&filters).build();
                    let Ok(file) = dialog.open_future(window.as_ref()).await else { return };
                    let Ok((bytes, _)) = file.load_contents_future().await else { return };
                    let name = file.basename().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "meme".into());
                    let shortcode: String = name
                        .rsplit_once('.')
                        .map(|(stem, _)| stem)
                        .unwrap_or(&name)
                        .chars()
                        .map(|c| if c.is_alphanumeric() { c } else { '_' })
                        .collect();
                    let (ctype, _) = gio::content_type_guess(Some(&name), Some(&bytes[..]));
                    let mime = gio::content_type_get_mime_type(&ctype).map(|m| m.to_string()).unwrap_or_else(|| "image/png".into());
                    ctx.send(Command::AddImagePackEmoji { room_id, scope: "personal".into(), shortcode, bytes: bytes.to_vec(), mime });
                });
            });
        }
        self.sticker_grids.borrow_mut().push((grid, in_thread));
    }

    pub fn image_packs(self: &Rc<Self>, room_id: &str, images: Vec<chosua_core::models::EmojiImage>) {
        if !self.is_current(room_id) {
            return;
        }
        for &(ref grid, in_thread) in self.sticker_grids.borrow().iter() {
            grid.remove_all();
            for img in &images {
                let pic = gtk::Picture::builder().content_fit(gtk::ContentFit::Contain).width_request(64).height_request(64).build();
                self.ctx.images.thumb(&pic, &img.url);
                let b = gtk::Button::builder()
                    .child(&pic)
                    .tooltip_text(format!(":{}: · {}", img.shortcode, img.pack_name))
                    .css_classes(["flat", "sticker"])
                    .build();
                let (this, url, shortcode) = (Rc::downgrade(self), img.url.clone(), img.shortcode.clone());
                b.connect_clicked(move |b| {
                    let Some(this) = this.upgrade() else { return };
                    let Some(room_id) = this.current() else { return };
                    let thread_id = if in_thread {
                        this.open_thread.borrow().as_ref().map(|(_, root)| root.event_id.clone())
                    } else {
                        None
                    };
                    this.ctx.send(Command::SendMeme { room_id, thread_id, url: url.clone(), shortcode: shortcode.clone() });
                    if let Some(p) = b.ancestor(gtk::Popover::static_type()).and_downcast::<gtk::Popover>() {
                        p.popdown();
                    }
                });
                if img.scope == "personal" {
                    // right click: remove one of my own
                    let click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
                    let (ctx, current, sc) = (self.ctx.clone(), self.current.clone(), img.shortcode.clone());
                    click.connect_pressed(move |_, _, _, _| {
                        if let Some(room_id) = current.borrow().clone() {
                            ctx.send(Command::RemoveImagePackEmoji { room_id, scope: "personal".into(), shortcode: sc.clone() });
                        }
                    });
                    b.add_controller(click);
                }
                grid.insert(&b, -1);
            }
            if images.is_empty() {
                grid.insert(&gtk::Label::builder().label("Chưa có meme nào").css_classes(["dim-label"]).build(), -1);
            }
        }
    }

    // ---- side panel: threads ------------------------------------------

    fn reveal_side(&self) {
        if self.side.is_visible() {
            return;
        }
        self.side.set_visible(true);
        let width = crate::config::get().side_width.max(280);
        let total = self.paned.width();
        if total > width {
            self.paned.set_position(total - width);
        }
    }

    pub fn toggle_threads(&self) {
        let showing = self.side.is_visible() && self.side_stack.visible_child_name().as_deref() == Some("threads");
        self.threads_button.set_active(!showing);
    }

    pub fn close_side(&self) {
        self.side.set_visible(false);
        self.threads_button.set_active(false);
        *self.open_thread.borrow_mut() = None;
        self.threads_pin.set(true);
        self.thread_composer.reset();
    }

    pub fn connect_room_name(&self, f: impl Fn(&str) -> Option<String> + 'static) {
        *self.room_name.borrow_mut() = Some(Box::new(f));
    }




    /// Ask for every room's threads once (app.js's `scanAllRoomThreads`),
    /// so the dots and the all-rooms panel have something from the start.
    pub fn scan_threads(self: &Rc<Self>, room_ids: Vec<String>) {
        {
            let known = self.threads_by_room.borrow();
            let mut queue = self.scan_queue.borrow_mut();
            for room_id in room_ids {
                if !known.contains_key(&room_id) && !queue.contains(&room_id) {
                    queue.push_back(room_id);
                }
            }
        }
        if self.scan_running.replace(true) {
            return;
        }
        let this = Rc::downgrade(self);
        glib::timeout_add_local(std::time::Duration::from_millis(600), move || {
            let Some(this) = this.upgrade() else { return glib::ControlFlow::Break };
            loop {
                let next = this.scan_queue.borrow_mut().pop_front();
                let Some(room_id) = next else {
                    this.scan_running.set(false);
                    return glib::ControlFlow::Break;
                };
                if this.threads_by_room.borrow().contains_key(&room_id) {
                    continue; // asked meanwhile (opened)
                }
                this.ctx.send(Command::ListThreads { room_id });
                return glib::ControlFlow::Continue;
            }
        });
    }

    /// Redraw the threads panel and the unread badges soon, once for a
    /// burst of updates.
    fn threads_dirty(self: &Rc<Self>) {
        if self.threads_dirty.replace(true) {
            return;
        }
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || {
            let Some(this) = this.upgrade() else { return };
            this.threads_dirty.set(false);
            let showing = this.side.is_visible() && this.side_stack.visible_child_name().as_deref() == Some("threads");
            if showing && this.threads_global.get() {
                this.render_threads();
            }
            this.notify_unread_threads();
        });
    }

    pub fn connect_unread_threads(&self, f: impl Fn(usize) + 'static) {
        *self.on_unread_threads.borrow_mut() = Some(Box::new(f));
    }

    /// How many known threads (every room) have replies not yet read.
    fn notify_unread_threads(&self) {
        let n = self.threads_by_room.borrow().values().flatten().filter(|t| t.is_unread == Some(true)).count();
        if let Some(f) = self.on_unread_threads.borrow().as_ref() {
            f(n);
        }
    }

    /// Re-read a room's thread list from the server soon (once for a burst
    /// of replies/receipts): counts, last replier and unread state all come
    /// from there, the local bumps in `thread_reply` only bridge the wait.
    /// Rooms whose threads were never listed are left to the scan.
    pub fn refresh_threads(self: &Rc<Self>, room_id: &str) {
        if !self.threads_by_room.borrow().contains_key(room_id) || !self.threads_refresh.borrow_mut().insert(room_id.to_string()) {
            return;
        }
        let (this, room_id) = (Rc::downgrade(self), room_id.to_string());
        glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
            let Some(this) = this.upgrade() else { return };
            this.threads_refresh.borrow_mut().remove(&room_id);
            this.ctx.send(Command::ListThreads { room_id });
        });
    }

    fn all_threads_title(&self) -> &'static str {
        if self.threads_unread_only.get() { "Thread chưa đọc" } else { "Thread của mọi phòng" }
    }

    /// Threads with recent replies across every room.
    pub fn show_all_threads(self: &Rc<Self>, room_ids: Vec<String>, unread_only: bool) {
        self.threads_global.set(true);
        self.threads_pin.set(true);
        self.threads_unread_only.set(unread_only);
        self.side_title.set_label(self.all_threads_title());
        self.side_stack.set_visible_child_name("threads");
        *self.open_thread.borrow_mut() = None;
        self.reveal_side();
        self.scan_threads(room_ids);
        self.render_threads();
    }

    fn render_threads(self: &Rc<Self>) {
        let global = self.threads_global.get();
        let current = self.current();
        let mut items: Vec<(String, TimelineEvent)> = {
            let map = self.threads_by_room.borrow();
            map.iter()
                .filter(|(r, _)| global || current.as_deref() == Some(r.as_str()))
                .flat_map(|(r, ts)| ts.iter().map(move |t| (r.clone(), t.clone())))
                .filter(|(_, t)| !(global && self.threads_unread_only.get()) || t.is_unread == Some(true))
                .collect()
        };
        // latest activity last, like the timeline
        items.sort_by_key(|(_, t)| t.latest_reply_ts.unwrap_or(t.timestamp));
        self.threads_status.set_visible(items.is_empty());
        self.threads_status.set_label(if global && self.threads_unread_only.get() { "Không có thread chưa đọc" } else { "Chưa có thread nào" });
        let n = self.threads_store.n_items();
        let rows = items.into_iter().map(glib::BoxedAnyObject::new).collect::<Vec<_>>();
        // newest is last: stay at the bottom unless the user scrolled up
        keep_scroll(&[(&self.threads_view, &self.threads_pin)], || self.threads_store.splice(0, n, &rows));
    }

    /// Rows of the threads list: (room id, root), the room's name above in
    /// the all-rooms list; a click opens the thread.
    fn threads_factory(self: &Rc<Self>) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        {
            let weak = Rc::downgrade(self);
            factory.connect_setup(move |_, item| {
                let Some(this) = weak.upgrade() else { return };
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                item.set_activatable(false);
                item.set_focusable(false);
                let room = gtk::Label::builder()
                    .xalign(0.0)
                    .css_classes(["caption", "heading", "thread-room"])
                    .build();
                let line = this.build_row();
                row_of(&line).in_thread.set(true);
                let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["thread-row"]).build();
                col.append(&room);
                col.append(&line);
                let click = gtk::GestureClick::new();
                let (w, c) = (weak.clone(), col.downgrade());
                click.connect_released(move |g, _, x, y| {
                    let (Some(this), Some(col)) = (w.upgrade(), c.upgrade()) else { return };
                    // a button in the row (reaction, media) does its own thing
                    let on_button = g
                        .widget()
                        .and_then(|w| w.pick(x, y, gtk::PickFlags::DEFAULT))
                        .is_some_and(|w| w.ancestor(gtk::Button::static_type()).is_some() || w.is::<gtk::Button>());
                    if on_button {
                        return;
                    }
                    let picked = unsafe { col.data::<(String, TimelineEvent)>(THREAD_ITEM).map(|p| p.as_ref().clone()) };
                    let Some((room_id, root)) = picked else { return };
                    if !this.is_current(&room_id) {
                        this.jump(&room_id);
                    }
                    this.open_thread(&root);
                });
                col.add_controller(click);
                item.set_child(Some(&col));
            });
        }
        {
            let weak = Rc::downgrade(self);
            factory.connect_bind(move |_, item| {
                let Some(this) = weak.upgrade() else { return };
                let item = item.downcast_ref::<gtk::ListItem>().unwrap();
                let col = item.child().unwrap();
                let obj = item.item().unwrap();
                let (room_id, root) = obj.downcast_ref::<glib::BoxedAnyObject>().unwrap().borrow::<(String, TimelineEvent)>().clone();
                let room = col.first_child().and_downcast::<gtk::Label>().unwrap();
                let line = room.next_sibling().unwrap();
                let global = this.threads_global.get();
                room.set_visible(global);
                if global {
                    let name = this.room_name.borrow().as_ref().and_then(|f| f(&room_id)).unwrap_or_else(|| room_id.clone());
                    room.set_label(&name);
                }
                this.bind_row(&line, &root, false);
                unsafe { col.set_data(THREAD_ITEM, (room_id, root)) };
            });
        }
        factory
    }

    /// From one thread back to the list it was opened from (this room's,
    /// or every room's).
    fn back_to_threads(self: &Rc<Self>) {
        *self.open_thread.borrow_mut() = None;
        self.thread_composer.reset();
        self.side_title.set_label(if self.threads_global.get() { self.all_threads_title() } else { "Các thread" });
        self.side_stack.set_visible_child_name("threads");
        self.render_threads();
        if !self.threads_global.get() {
            if let Some(room_id) = self.current() {
                self.ctx.send(Command::ListThreads { room_id });
            }
        }
    }

    /// The room's threads, newest activity last (`Command::ListThreads`).
    /// The room's threads: what's known right away, refreshed from the
    /// server a moment later — after the room's own timeline, which is what
    /// you're waiting for, and not at all if you've moved on by then.
    fn show_threads(self: &Rc<Self>) {
        let Some(room_id) = self.current() else { return };
        self.threads_global.set(false);
        self.threads_pin.set(true);
        self.side_title.set_label("Các thread");
        self.side_stack.set_visible_child_name("threads");
        self.reveal_side();
        *self.open_thread.borrow_mut() = None;
        let cached = self.threads_by_room.borrow().contains_key(&room_id);
        if cached {
            self.render_threads();
        } else {
            let n = self.threads_store.n_items();
            self.threads_store.splice(0, n, &[] as &[glib::Object]);
            self.threads_status.set_label("Đang tải…");
            self.threads_status.set_visible(true);
            self.threads_more.set_visible(false);
        }
        let this = Rc::downgrade(self);
        glib::timeout_add_local_once(std::time::Duration::from_millis(if cached { 1500 } else { 800 }), move || {
            let Some(this) = this.upgrade() else { return };
            let still_here = this.is_current(&room_id)
                && this.side.is_visible()
                && this.side_stack.visible_child_name().as_deref() == Some("threads");
            if still_here {
                this.ctx.send(Command::ListThreads { room_id });
            }
        });
    }

    /// A thread root's reply count and last replier. The count bundled in
    /// the event (`unsigned.m.relations.m.thread`) is often missing or
    /// stale — paginated history, our own messages — so the `/threads` list
    /// (and the live replies counted onto it) wins when it knows more, the
    /// same fallback as app.js.
    fn thread_summary(&self, ev: &TimelineEvent) -> (u64, Option<String>) {
        let bundled = ev.thread_count.unwrap_or(0);
        let map = self.threads_by_room.borrow();
        let room = self.thread_rooms.borrow().get(&ev.event_id).cloned();
        let listed = room.and_then(|r| map.get(&r)).and_then(|ts| ts.iter().find(|t| t.event_id == ev.event_id));
        match listed {
            Some(t) if t.thread_count.unwrap_or(0) >= bundled => {
                let last = t.latest_reply_sender_name.clone().or_else(|| ev.latest_reply_sender_name.clone());
                (t.thread_count.unwrap_or(0), last)
            }
            _ => (bundled, ev.latest_reply_sender_name.clone()),
        }
    }

    /// Re-show these roots in the room's timeline (their counts changed).
    fn refresh_roots(&self, room_id: &str, root_ids: &[String]) {
        let Some(room) = self.room(room_id) else { return };
        let lists = [(&self.list, &self.pin_main)];
        keep_scroll(&lists, || {
            for id in root_ids {
                if let Some(i) = find(&room.store, id) {
                    room.store.items_changed(i, 1, 1);
                }
            }
        });
    }

    pub fn threads_list(self: &Rc<Self>, room_id: &str, threads: Vec<TimelineEvent>, append: bool, reached_end: bool) {
        let ids: Vec<String> = threads.iter().map(|t| t.event_id.clone()).collect();
        {
            let mut map = self.threads_by_room.borrow_mut();
            let known = map.entry(room_id.to_string()).or_default();
            if append {
                for t in &threads {
                    if !known.iter().any(|k| k.event_id == t.event_id) {
                        known.push(t.clone());
                    }
                }
            } else {
                *known = threads.clone();
            }
        }
        {
            let mut index = self.thread_rooms.borrow_mut();
            for id in &ids {
                index.insert(id.clone(), room_id.to_string());
            }
        }
        if self.is_current(room_id) {
            self.refresh_roots(room_id, &ids);
        }
        self.threads_dirty();
        let showing = self.side.is_visible() && self.side_stack.visible_child_name().as_deref() == Some("threads");
        if showing && self.threads_global.get() {
            return; // threads_dirty redraws it
        }
        if !self.is_current(room_id) || !showing {
            return;
        }
        self.render_threads();
        self.threads_more.set_visible(!reached_end);
        self.threads_more.set_sensitive(true);
    }

    /// One thread in the side panel: its root, then the replies.
    pub fn open_thread(self: &Rc<Self>, root: &TimelineEvent) {
        let Some(room_id) = self.current() else { return };
        *self.open_thread.borrow_mut() = Some((room_id.clone(), root.clone()));
        self.mark_thread_read_locally(&room_id, &root.event_id);
        self.side_title.set_label("Thread");
        self.side_stack.set_visible_child_name("thread");
        self.reveal_side();
        self.threads_button.set_active(false);
        let mut shown = root.clone();
        shown.thread_count = None; // the panel is the thread
        self.thread_store.remove_all();
        self.thread_store.append(&obj(shown));
        self.thread_list.set_model(Some(&gtk::NoSelection::new(Some(self.thread_store.clone()))));
        self.thread_composer.reset();
        self.thread_composer.grab_focus();
        self.ctx.send(Command::LoadThread { room_id, thread_root_id: root.event_id.clone() });
    }

    fn mark_thread_read_locally(&self, room_id: &str, root_id: &str) {
        if let Some(t) = self.threads_by_room.borrow_mut().get_mut(room_id).and_then(|l| l.iter_mut().find(|t| t.event_id == root_id)) {
            t.is_unread = Some(false);
        }
        self.update(room_id, root_id, |root| root.is_unread = Some(false));
        self.notify_unread_threads();
    }

    pub fn is_thread_open(&self, room_id: &str, root_id: &str) -> bool {
        self.is_open_thread(room_id, root_id)
    }

    /// Open a thread by its root's id (a notification click): from the
    /// loaded timeline, or once the room's timeline has it.
    pub fn open_thread_by_id(self: &Rc<Self>, room_id: &str, root_id: &str) {
        let root = self.room(room_id).and_then(|r| find(&r.store, root_id).map(|i| event(&r.store.item(i).unwrap()).clone()));
        match root {
            Some(root) => self.open_thread(&root),
            None => {
                // not loaded yet: a stub root is enough to load the replies
                let stub = TimelineEvent { event_id: root_id.into(), body: "…".into(), msg_type: "notice".into(), ..Default::default() };
                self.open_thread(&stub);
            }
        }
    }

    fn is_open_thread(&self, room_id: &str, root_id: &str) -> bool {
        self.open_thread.borrow().as_ref().is_some_and(|(r, root)| r == room_id && root.event_id == root_id)
    }

    pub fn thread_events(self: &Rc<Self>, room_id: &str, root_id: &str, events: Vec<TimelineEvent>) {
        // whatever happens below, a pending jump looks again afterwards
        let _jump = JumpAfter(self.clone());
        if !self.is_open_thread(room_id, root_id) {
            return;
        }
        // row 0 is the root
        let events = keep_newer(&self.thread_store, 1, events);
        let last = events.last().map(|e| e.event_id.clone()).filter(|id| id.starts_with('$'));
        let n = self.thread_store.n_items();
        self.thread_store.splice(1, n.saturating_sub(1), &events.into_iter().map(obj).collect::<Vec<_>>());
        self.thread_scroll_to_end();
        self.ctx.send(Command::LoadMoreThreadReplies { room_id: room_id.into(), thread_root_id: root_id.into() });
        if let Some(event_id) = last {
            self.ctx.send(Command::MarkThreadRead { room_id: room_id.into(), thread_root_id: root_id.into(), event_id });
        }
    }

    /// An older page of replies; keep asking until the start (a thread is
    /// short enough to hold whole, and app.js does the same).
    pub fn thread_prepend(self: &Rc<Self>, room_id: &str, root_id: &str, events: Vec<TimelineEvent>, reached_start: bool) {
        // whatever happens below, a pending jump looks again afterwards
        let _jump = JumpAfter(self.clone());
        if !self.is_open_thread(room_id, root_id) {
            return;
        }
        // after the root (row 0)
        let at = self.thread_store.n_items().min(1);
        self.thread_store.splice(at, 0, &events.into_iter().map(obj).collect::<Vec<_>>());
        if !reached_start {
            self.ctx.send(Command::LoadMoreThreadReplies { room_id: room_id.into(), thread_root_id: root_id.into() });
        }
    }

    /// A new reply: into the open thread, and onto its root's summary in
    /// the room timeline.




    pub fn thread_reply(&self, room_id: &str, root_id: &str, ev: TimelineEvent) {
        let root_in_timeline = self.room(room_id).and_then(|r| find(&r.store, root_id).map(|i| event(&r.store.item(i).unwrap()).clone()));
        let unread = (!self.is_open_thread(room_id, root_id) && !ev.is_own).then_some(true);
        {
            let mut map = self.threads_by_room.borrow_mut();
            let list = map.entry(room_id.to_string()).or_default();
            if !list.iter().any(|t| t.event_id == root_id) {
                if let Some(root) = root_in_timeline {
                    list.push(root);
                    self.thread_rooms.borrow_mut().insert(root_id.to_string(), room_id.to_string());
                }
            }
            if let Some(t) = list.iter_mut().find(|t| t.event_id == root_id) {
                t.thread_count = Some(t.thread_count.unwrap_or(0) + 1);
                t.latest_reply_sender_name = Some(ev.sender_name.clone());
                t.latest_reply_ts = Some(ev.timestamp);
                t.is_unread = unread.or(t.is_unread);
                if ev.event_id.starts_with('$') {
                    t.latest_reply_event_id = Some(ev.event_id.clone());
                }
            }
        }
        self.update(room_id, root_id, |root| {
            root.thread_count = Some(root.thread_count.unwrap_or(0) + 1);
            root.latest_reply_sender_name = Some(ev.sender_name.clone());
            root.latest_reply_body = Some(ev.body.clone());
            root.latest_reply_ts = Some(ev.timestamp);
            root.is_unread = unread.or(root.is_unread);
        });
        self.notify_unread_threads();
        if !self.is_open_thread(room_id, root_id) {
            return;
        }
        let store = &self.thread_store;
        if let Some(i) = ev.local_id.as_deref().and_then(|l| find(store, l)) {
            store.splice(i, 1, &[obj(ev)]);
            return;
        }
        if find(store, &ev.event_id).is_some() {
            return;
        }
        let event_id = ev.event_id.clone();
        let follow = ev.is_own || self.pin_thread.get();
        store.append(&obj(ev));
        if follow {
            self.thread_scroll_to_end();
        }
        self.ctx.send(Command::MarkThreadRead { room_id: room_id.into(), thread_root_id: root_id.into(), event_id });
    }

    fn thread_scroll_to_end(&self) {
        self.pin_thread.set(true);
        let list = self.thread_list.clone();
        glib::idle_add_local_once(move || {
            if let Some(n) = list.model().map(|m| m.n_items()).filter(|n| *n > 0) {
                list.scroll_to(n - 1, gtk::ListScrollFlags::NONE, None);
            }
        });
    }

    // ---- shared messages ----------------------------------------------

    /// One card per shared message: "🔗 [room] Sender: snippet".
    fn bind_shared(&self, row: &Row, holder: &str, links: &[(String, String)]) {
        while let Some(c) = row.shared.first_child() {
            row.shared.remove(&c);
        }
        row.shared.set_visible(!links.is_empty());
        for (room_id, event_id) in links {
            let key = (room_id.clone(), event_id.clone());
            let label = gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .max_width_chars(56)
                .build();
            match self.lookup_shared(room_id, event_id) {
                Some(Some(ev)) => {
                    let room = (self.current().as_deref() != Some(room_id.as_str()))
                        .then(|| self.room_name.borrow().as_ref().and_then(|f| f(room_id)))
                        .flatten()
                        .map(|n| format!("<span alpha=\"75%\">{}</span> · ", glib::markup_escape_text(&n)))
                        .unwrap_or_default();
                    let snippet: String = ev.body.chars().take(120).collect();
                    label.set_markup(&format!(
                        "🔗 {room}<b>{}</b>: {}",
                        glib::markup_escape_text(&ev.sender_name),
                        glib::markup_escape_text(&snippet)
                    ));
                }
                Some(None) => label.set_markup("🔗 <span alpha=\"70%\">Tin nhắn không còn hoặc bạn không xem được</span>"),
                None => {
                    label.set_markup("🔗 <span alpha=\"70%\">Đang tải tin được chia sẻ…</span>");
                    let mut holders = self.preview_holders.borrow_mut();
                    let list = holders.entry(key.clone()).or_default();
                    let first = list.is_empty();
                    if !list.iter().any(|h| h == holder) {
                        list.push(holder.to_string());
                    }
                    if first {
                        self.ctx.send(Command::GetEventPreview { room_id: room_id.clone(), event_id: event_id.clone() });
                    }
                }
            }
            let card = gtk::Button::builder().child(&label).css_classes(["shared-card"]).halign(gtk::Align::Start).build();
            let (ctx, room_id, event_id) = (self.ctx.clone(), room_id.clone(), event_id.clone());
            card.connect_clicked(move |_| ctx.send(Command::ResolveSharedEvent { room_id: room_id.clone(), event_id: event_id.clone() }));
            row.shared.append(&card);
        }
    }

    /// A shared message from what's loaded here, or the server's answer:
    /// `None` = not known yet.
    fn lookup_shared(&self, room_id: &str, event_id: &str) -> Option<Option<TimelineEvent>> {
        if let Some(room) = self.room(room_id) {
            if let Some(i) = find(&room.store, event_id) {
                return Some(Some(event(&room.store.item(i).unwrap()).clone()));
            }
        }
        if let Some(i) = find(&self.thread_store, event_id) {
            return Some(Some(event(&self.thread_store.item(i).unwrap()).clone()));
        }
        let listed = self.threads_by_room.borrow().get(room_id).and_then(|ts| ts.iter().find(|t| t.event_id == event_id).cloned());
        if let Some(t) = listed {
            return Some(Some(t));
        }
        self.previews.borrow().get(&(room_id.to_string(), event_id.to_string())).cloned()
    }

    /// The server's answer to `GetEventPreview`: re-show the messages that
    /// share it.
    pub fn event_preview(&self, room_id: &str, event_id: &str, ev: Option<TimelineEvent>) {
        let key = (room_id.to_string(), event_id.to_string());
        self.previews.borrow_mut().insert(key.clone(), ev);
        let holders = self.preview_holders.borrow_mut().remove(&key).unwrap_or_default();
        if let Some(current) = self.current() {
            self.refresh_roots(&current, &holders);
        }
        let lists = [(&self.thread_list, &self.pin_thread)];
        keep_scroll(&lists, || {
            for h in &holders {
                if let Some(i) = find(&self.thread_store, h) {
                    self.thread_store.items_changed(i, 1, 1);
                }
            }
        });
    }

    /// `user_id` has now read up to `event_id`: their avatar leaves
    /// whichever row had it and shows under that one.
    pub fn read_receipt(&self, room_id: &str, user_id: &str, event_id: &str) {
        let mut stores = Vec::new();
        if let Some(room) = self.room(room_id) {
            stores.push(room.store.clone());
        }
        if self.open_thread.borrow().as_ref().is_some_and(|(r, _)| r == room_id) {
            stores.push(self.thread_store.clone());
        }
        let lists = [(&self.list, &self.pin_main), (&self.thread_list, &self.pin_thread)];
        keep_scroll(&lists, || for store in stores {
            // a receipt for an event this list doesn't hold isn't its business
            if find(&store, event_id).is_none() {
                continue;
            }
            for i in 0..store.n_items() {
                let mut ev = event(&store.item(i).unwrap()).clone();
                let has = ev.read_by.iter().any(|u| u == user_id);
                if has == (ev.event_id == event_id) {
                    continue;
                }
                if has {
                    ev.read_by.retain(|u| u != user_id);
                } else {
                    ev.read_by.push(user_id.to_string());
                }
                store.splice(i, 1, &[obj(ev)]);
            }
        });
    }

    pub fn set_reactions(&self, room_id: &str, event_id: &str, reactions: Vec<ReactionSummary>) {
        self.update(room_id, event_id, |ev| ev.reactions = reactions.clone());
    }

    fn update(&self, room_id: &str, event_id: &str, f: impl Fn(&mut TimelineEvent)) {
        let mut stores = Vec::new();
        if let Some(room) = self.room(room_id) {
            stores.push(room.store.clone());
        }
        if self.open_thread.borrow().as_ref().is_some_and(|(r, _)| r == room_id) {
            stores.push(self.thread_store.clone());
        }
        let lists = [(&self.list, &self.pin_main), (&self.thread_list, &self.pin_thread)];
        keep_scroll(&lists, || {
            for store in stores {
                if let Some(i) = find(&store, event_id) {
                    let mut ev = event(&store.item(i).unwrap()).clone();
                    f(&mut ev);
                    store.splice(i, 1, &[obj(ev)]);
                }
            }
        });
    }

    pub fn open(&self, room_id: &str, name: &str) {
        self.title.set_title(name);
        if self.current.borrow().as_deref() == Some(room_id) {
            return;
        }
        let (room, fresh) = {
            let mut rooms = self.rooms.borrow_mut();
            let fresh = !rooms.contains_key(room_id);
            let room = rooms
                .entry(room_id.to_string())
                .or_insert_with(|| {
                    Rc::new(RoomTimeline {
                        store: gio::ListStore::new::<glib::BoxedAnyObject>(),
                        reached_start: Cell::new(false),
                        paginating: Cell::new(false),
                        empty_pages: Cell::new(0),
                    })
                })
                .clone();
            (room, fresh)
        };
        *self.current.borrow_mut() = Some(room_id.to_string());
        self.close_side();
        if crate::config::get().auto_threads {
            self.threads_button.set_active(true);
        }
        self.list.set_model(Some(&gtk::NoSelection::new(Some(room.store.clone()))));
        if fresh {
            self.ctx.send(Command::LoadTimeline { room_id: room_id.to_string() });
        }
        self.composer.reset();
        self.ctx.send(Command::WatchTyping { room_id: room_id.to_string() });
        match self.members.borrow().get(room_id) {
            Some(m) => {
                self.composer.set_members(m.clone());
                self.thread_composer.set_members(m.clone());
            }
            None => self.ctx.send(Command::ListMembers { room_id: room_id.to_string() }),
        }
        self.ctx.send(Command::MarkRoomRead { room_id: room_id.to_string() });
        self.scroll_to_end();
        self.composer.grab_focus();
    }

    pub fn current(&self) -> Option<String> {
        self.current.borrow().clone()
    }

    fn room(&self, room_id: &str) -> Option<Rc<RoomTimeline>> {
        self.rooms.borrow().get(room_id).cloned()
    }

    fn is_current(&self, room_id: &str) -> bool {
        self.current.borrow().as_deref() == Some(room_id)
    }

    pub fn set_events(self: &Rc<Self>, room_id: &str, events: Vec<TimelineEvent>) {
        // whatever happens below, a pending jump looks again afterwards
        let _jump = JumpAfter(self.clone());
        let Some(room) = self.room(room_id) else { return };
        let events = keep_newer(&room.store, 0, events);
        let n = room.store.n_items();
        room.store.splice(0, n, &events.into_iter().map(obj).collect::<Vec<_>>());
        if self.is_current(room_id) {
            self.scroll_to_end();
        }
        // a short first page doesn't mean the room is short: once the rows
        // are measured, keep loading until the view is filled
        self.paginate_soon(room_id, 300);
    }

    /// `maybe_paginate` after `ms`, if this room is still the open one. The
    /// scroll signals only fire when the list's size changes, so a page that
    /// brought nothing (or arrived before the view had a size) would
    /// otherwise leave the room stuck with its few latest messages.
    fn paginate_soon(self: &Rc<Self>, room_id: &str, ms: u64) {
        let (weak, room_id) = (Rc::downgrade(self), room_id.to_string());
        glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            if let Some(this) = weak.upgrade() {
                if this.is_current(&room_id) {
                    this.maybe_paginate();
                }
            }
        });
    }

    /// `events` is the whole timeline again, older messages first; only the
    /// new head is inserted, so the rows on screen stay where they are.
    pub fn prepend(self: &Rc<Self>, room_id: &str, events: Vec<TimelineEvent>, reached_start: bool) {
        // whatever happens below, a pending jump looks again afterwards
        let _jump = JumpAfter(self.clone());
        let Some(room) = self.room(room_id) else { return };
        room.paginating.set(false);
        room.reached_start.set(reached_start);
        let n = room.store.n_items() as usize;
        let tail_matches = events.len() >= n
            && (n == 0
                || room.store.item(0).is_some_and(|first| event(&first).event_id == events[events.len() - n].event_id));
        let added = events.len() > n;
        if tail_matches {
            let new = events.len() - n;
            room.store.splice(0, 0, &events.into_iter().take(new).map(obj).collect::<Vec<_>>());
        } else {
            room.store.splice(0, n as u32, &events.into_iter().map(obj).collect::<Vec<_>>());
        }
        // an answer with nothing new while the start isn't reached: try
        // again a few times rather than waiting for the user to reopen the room
        if added || reached_start {
            room.empty_pages.set(0);
        } else if room.empty_pages.get() < 4 {
            room.empty_pages.set(room.empty_pages.get() + 1);
            self.paginate_soon(room_id, 800);
        }
    }

    pub fn push(&self, room_id: &str, ev: TimelineEvent) {
        let Some(room) = self.room(room_id) else { return };
        let n = room.store.n_items();
        // the real echo of a message sent from here replaces its local echo
        if let Some(local) = ev.local_id.as_deref() {
            if let Some(i) = find(&room.store, local) {
                room.store.splice(i, 1, &[obj(ev)]);
                return;
            }
        }
        if (n.saturating_sub(20)..n).any(|i| event(&room.store.item(i).unwrap()).event_id == ev.event_id) {
            return;
        }
        let follow = self.is_current(room_id) && self.at_bottom();
        room.store.append(&obj(ev));
        if self.is_current(room_id) {
            self.ctx.send(Command::MarkRoomRead { room_id: room_id.to_string() });
            if follow {
                self.scroll_to_end();
            }
        }
    }

    pub fn edit(&self, room_id: &str, event_id: &str, new_body: String) {
        self.update(room_id, event_id, |ev| ev.body = new_body.clone());
    }

    pub fn remove(&self, room_id: &str, event_id: &str) {
        if let Some(room) = self.room(room_id) {
            if let Some(i) = find(&room.store, event_id) {
                room.store.remove(i);
            }
        }
        if let Some(i) = find(&self.thread_store, event_id) {
            self.thread_store.remove(i);
        }
    }

    /// The send went through; the real event (with its own id) replaces the
    /// local echo when it comes down the sync.
    pub fn sent(&self, _local_id: &str) {}

    pub fn send_failed(&self, local_id: &str, error: &str) {
        let Some(room_id) = self.current() else { return };
        self.update(&room_id, local_id, |ev| ev.msg_type = "failed".into());
        self.toast(&format!("Gửi không được: {error}"));
    }

    pub fn set_members(&self, room_id: &str, members: &[(String, String, Option<String>)]) {
        let m: Vec<(String, String)> = members.iter().map(|(id, name, _)| (id.clone(), name.clone())).collect();
        if self.is_current(room_id) {
            self.composer.set_members(m.clone());
            self.thread_composer.set_members(m.clone());
        }
        let urls = members.iter().filter_map(|(id, _, url)| url.clone().map(|u| (id.clone(), u))).collect();
        self.member_avatars.borrow_mut().insert(room_id.to_string(), urls);
        self.members.borrow_mut().insert(room_id.to_string(), m);
        // rows bound before the names arrived: redraw so @tags get highlighted
        if self.is_current(room_id) {
            let lists = [(&self.list, &self.pin_main), (&self.thread_list, &self.pin_thread)];
            keep_scroll(&lists, || {
                if let Some(room) = self.room(room_id) {
                    let n = room.store.n_items();
                    if n > 0 {
                        room.store.items_changed(0, n, n);
                    }
                }
                let n = self.thread_store.n_items();
                if n > 0 {
                    self.thread_store.items_changed(0, n, n);
                }
            });
        }
    }

    pub fn set_typing(&self, room_id: &str, user_ids: &[String]) {
        if !self.is_current(room_id) {
            return;
        }
        let names: Vec<String> = user_ids.iter().map(|id| self.display_name(room_id, id)).collect();
        self.composer.set_typing(&names);
    }

    /// A member's name as seen in the loaded timeline, else their localpart.
    fn display_name(&self, room_id: &str, user_id: &str) -> String {
        if let Some(room) = self.room(room_id) {
            for i in (0..room.store.n_items()).rev() {
                let item = room.store.item(i).unwrap();
                let ev = event(&item);
                if ev.sender == user_id {
                    return ev.sender_name.clone();
                }
            }
        }
        user_id.trim_start_matches('@').split(':').next().unwrap_or(user_id).to_string()
    }

    fn maybe_paginate(&self) {
        let adj = self.scroller.vadjustment();
        let page = adj.page_size();
        if page <= 0.0 {
            return;
        }
        let not_filled = adj.upper() <= page + 1.0;
        if not_filled || adj.value() < page * 1.5 {
            self.paginate();
        }
    }

    fn paginate(&self) {
        let Some(room_id) = self.current.borrow().clone() else { return };
        let Some(room) = self.room(&room_id) else { return };
        if room.reached_start.get() || room.paginating.replace(true) {
            return;
        }
        self.ctx.send(Command::PaginateBack { room_id });
    }

    fn local_id(&self) -> String {
        let n = self.next_local_id.get() + 1;
        self.next_local_id.set(n);
        format!("gtk-{}-{n}", std::process::id())
    }

    fn outgoing(&self, out: Outgoing, thread_id: Option<String>) {
        let Some(room_id) = self.current() else { return };
        let store = match &thread_id {
            Some(_) => self.thread_store.clone(),
            None => match self.room(&room_id) {
                Some(r) => r.store.clone(),
                None => return,
            },
        };
        match out {
            Outgoing::Text { body, reply_to, mentions, html } => {
                let local_id = self.local_id();
                // local echo until the server's copy arrives
                let echo = TimelineEvent {
                    event_id: local_id.clone(),
                    local_id: Some(local_id.clone()),
                    body: body.clone(),
                    msg_type: "text".into(),
                    is_own: true,
                    sender: self.me.borrow().as_ref().map(|m| m.user_id.clone()).unwrap_or_default(),
                    sender_name: self.me.borrow().as_ref().map(|m| m.display_name.clone()).unwrap_or_default(),
                    sender_avatar_url: self.me.borrow().as_ref().and_then(|m| m.avatar_url.clone()),
                    timestamp: glib::real_time() / 1000,
                    reply_to_event_id: reply_to.clone(),
                    ..Default::default()
                };
                store.append(&obj(echo));
                if thread_id.is_some() {
                    self.thread_scroll_to_end();
                } else {
                    self.scroll_to_end();
                }
                self.ctx.send(Command::SetTyping { room_id: room_id.clone(), typing: false });
                self.ctx.send(Command::SendMessage {
                    room_id,
                    body,
                    thread_id,
                    mentions,
                    html_body: html,
                    local_id,
                    reply_to_event_id: reply_to,
                });
            }
            Outgoing::Edit { event_id, body } => {
                self.edit(&room_id, &event_id, body.clone());
                self.ctx.send(Command::EditMessage { room_id, event_id, body, mentions: Vec::new(), html_body: None });
            }
            Outgoing::Attachment { filename, bytes, mime } => {
                self.toast(&format!("Đang gửi {filename}…"));
                let local_id = self.local_id();
                self.ctx.send(if mime.starts_with("image/") {
                    Command::SendImage { room_id, thread_id, filename, bytes, mime, local_id }
                } else {
                    Command::SendFile { room_id, thread_id, filename, bytes, mime, local_id }
                });
            }
            Outgoing::Voice { ogg, duration_ms, waveform } => {
                self.toast("Đang gửi tin nhắn thoại…");
                let local_id = self.local_id();
                self.ctx.send(Command::SendVoiceMessage {
                    room_id,
                    thread_id,
                    bytes: ogg,
                    mime: "audio/ogg".into(),
                    duration_ms,
                    waveform,
                    local_id,
                });
            }
            Outgoing::Typing(typing) => self.ctx.send(Command::SetTyping { room_id, typing }),
        }
    }

    fn at_bottom(&self) -> bool {
        let adj = self.scroller.vadjustment();
        adj.value() + adj.page_size() >= adj.upper() - 32.0
    }

    fn scroll_to_end(&self) {
        self.pin_main.set(true);
        let list = self.list.clone();
        // after the new rows have been measured (pin_to_bottom keeps it
        // there as they grow)
        glib::idle_add_local_once(move || {
            if let Some(n) = list.model().map(|m| m.n_items()).filter(|n| *n > 0) {
                list.scroll_to(n - 1, gtk::ListScrollFlags::NONE, None);
            }
        });
    }
}

impl TimelinePane {
    /// "Sender: snippet" of a loaded message (this room's or the open thread's).
    fn preview_of(&self, event_id: &str) -> Option<String> {
        let mut stores = vec![self.thread_store.clone()];
        if let Some(room) = self.current().and_then(|id| self.room(&id)) {
            stores.insert(0, room.store.clone());
        }
        stores.iter().find_map(|store| {
            let i = find(store, event_id)?;
            let o = store.item(i)?;
            let ev = event(&o);
            let body: String = ev.body.lines().next().unwrap_or("").chars().take(80).collect();
            let body = if body.is_empty() { "[tệp đính kèm]".to_string() } else { body };
            Some(format!("{}: {body}", ev.sender_name))
        })
    }
}

/// Keep a scrolled list at its bottom while `pin` is on: whenever its
/// content or the view changes size (rows measured, an echo replaced by the
/// server's copy, the composer growing). Scrolling up turns it off, back at
/// the bottom turns it on.
fn pin_to_bottom(scroller: &gtk::ScrolledWindow, pin: &Rc<Cell<bool>>) {
    let adj = scroller.vadjustment();
    let bottom = |a: &gtk::Adjustment| a.value() + a.page_size() >= a.upper() - 32.0;
    // The user is scrolling right now (wheel/touchpad/keys within the last
    // moment, or a mouse button held: scrollbar drag). While true we never
    // move the view ourselves — fighting the user's scroll is what made the
    // list jump — and only the user's own movement may turn the pin off.
    let active = Rc::new(Cell::new(None::<std::time::Instant>));
    let held = Rc::new(Cell::new(false));
    let busy = {
        let (active, held) = (active.clone(), held.clone());
        move || held.get() || active.get().is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(300))
    };
    {
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL | gtk::EventControllerScrollFlags::KINETIC);
        scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
        let a1 = active.clone();
        scroll.connect_scroll(move |_, _, _| {
            a1.set(Some(std::time::Instant::now()));
            glib::Propagation::Proceed
        });
        scroller.add_controller(scroll);
        let click = gtk::GestureClick::new();
        click.set_propagation_phase(gtk::PropagationPhase::Capture);
        let h = held.clone();
        click.connect_pressed(move |_, _, _, _| h.set(true));
        let h = held.clone();
        click.connect_released(move |_, _, _, _| h.set(false));
        click.connect_stopped({
            let h = held.clone();
            move |_| h.set(false)
        });
        scroller.add_controller(click);
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let a2 = active.clone();
        keys.connect_key_pressed(move |_, _, _, _| {
            a2.set(Some(std::time::Instant::now()));
            glib::Propagation::Proceed
        });
        scroller.add_controller(keys);
    }
    {
        let pin = pin.clone();
        let busy = busy.clone();
        let last = Cell::new(adj.value());
        adj.connect_value_changed(move |a| {
            let v = a.value();
            if bottom(a) {
                pin.set(true);
            } else if busy() && v < last.get() - 1.0 {
                pin.set(false); // the user went up
            }
            last.set(v);
        });
    }
    let stick = {
        let pin = pin.clone();
        move |a: &gtk::Adjustment| {
            if pin.get() && !busy() {
                let end = a.upper() - a.page_size();
                if a.value() < end {
                    a.set_value(end);
                }
            }
        }
    };
    let s2 = stick.clone();
    adj.connect_upper_notify(move |a| stick(a));
    adj.connect_page_size_notify(move |a| s2(a));
}

/// Run `f` (which splices rows of the lists in `lists`) without the view
/// jumping: replacing a row makes the ListView reset its scroll, which also
/// reads as "the user scrolled up" to `pin_to_bottom`. Put the pin and the
/// position back afterwards, again once the rows are measured.
fn keep_scroll(lists: &[(&gtk::ListView, &Rc<Cell<bool>>)], f: impl FnOnce()) {
    let saved: Vec<(gtk::Adjustment, Rc<Cell<bool>>, f64, bool)> = lists
        .iter()
        .filter_map(|(l, pin)| {
            let adj = l.parent()?.downcast::<gtk::ScrolledWindow>().ok()?.vadjustment();
            let v = adj.value();
            Some((adj, (*pin).clone(), v, pin.get()))
        })
        .collect();
    f();
    for (adj, pin, v, was) in saved {
        let restore = move |adj: &gtk::Adjustment| {
            pin.set(was);
            let target = if was { adj.upper() - adj.page_size() } else { v.min(adj.upper() - adj.page_size()) };
            if (adj.value() - target).abs() > 0.5 {
                adj.set_value(target.max(0.0));
            }
        };
        restore(&adj);
        let adj2 = adj.clone();
        let restore2 = restore.clone();
        glib::idle_add_local_once(move || restore2(&adj2));
    }
}

fn toggle_class(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

fn find(store: &gio::ListStore, event_id: &str) -> Option<u32> {
    (0..store.n_items()).rev().find(|&i| event(&store.item(i).unwrap()).event_id == event_id)
}

/// The size an image is shown at: its own, scaled down to fit IMG_W x IMG_H.
fn fit(size: Option<(u32, u32)>) -> (i32, i32) {
    let (w, h) = match size {
        Some((w, h)) if w > 0 && h > 0 => (w as f64, h as f64),
        _ => (IMG_W, IMG_H * 0.75),
    };
    let k = (IMG_W / w).min(IMG_H / h).min(1.0);
    ((w * k).round().max(48.0) as i32, (h * k).round().max(48.0) as i32)
}

/// An image/video event's body is usually just its file name.
fn looks_like_filename(body: &str) -> bool {
    !body.contains(' ') && body.rsplit_once('.').is_some_and(|(_, ext)| (1..=5).contains(&ext.len()))
}

fn format_time(ts_ms: i64) -> String {
    glib::DateTime::from_unix_local(ts_ms / 1000)
        .and_then(|t| {
            let today = glib::DateTime::now_local()?;
            let fmt = if t.ymd() == today.ymd() { "%H:%M" } else { "%d/%m %H:%M" };
            t.format(fmt)
        })
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// `https://matrix.to/#/!room:server[/$event]` -> (room id, event id).
fn matrix_to_room(uri: &str) -> Option<(String, Option<String>)> {
    let rest = uri.strip_prefix("https://matrix.to/#/")?;
    let rest = rest.split('?').next()?;
    let mut parts = rest.split('/');
    let room = glib::Uri::unescape_string(parts.next()?, None::<&str>)?.to_string();
    if !room.starts_with('!') {
        return None;
    }
    let event = parts.next().and_then(|e| glib::Uri::unescape_string(e, None::<&str>)).map(|e| e.to_string());
    Some((room, event.filter(|e| e.starts_with('$'))))
}


/// The body without bare matrix.to message links (each on its own: the
/// link *is* the text), and those links as (room id, event id). A link
/// inside `[text](...)` is left alone.
fn split_shared_links(body: &str) -> (String, Vec<(String, String)>) {
    const PREFIX: &str = "https://matrix.to/#/!";
    let mut text = String::with_capacity(body.len());
    let mut links = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find(PREFIX) {
        let (before, tail) = rest.split_at(i);
        let end = tail.find(|c: char| c.is_whitespace() || c == ')' || c == '>' || c == '"').unwrap_or(tail.len());
        let url = &tail[..end];
        let markdown_target = before.ends_with("](") || before.ends_with('<');
        match (markdown_target, matrix_to_room(url)) {
            (false, Some((room_id, Some(event_id)))) => {
                text.push_str(before.trim_end_matches([' ', '\t']));
                if !links.iter().any(|(r, e)| r == &room_id && e == &event_id) {
                    links.push((room_id, event_id));
                }
            }
            _ => {
                text.push_str(before);
                text.push_str(url);
            }
        }
        rest = &tail[end..];
    }
    text.push_str(rest);
    (text, links)
}

#[cfg(test)]
mod tests {
    use super::split_shared_links;

    #[test]
    fn shared_links_become_cards() {
        let (t, l) = split_shared_links("xem cái này https://matrix.to/#/!abc:s.vn/$ev1?via=s.vn nhé");
        assert_eq!(t, "xem cái này nhé");
        assert_eq!(l, vec![("!abc:s.vn".to_string(), "$ev1".to_string())]);
        // custom link text, a room link, other urls: untouched
        let body = "[đây](https://matrix.to/#/!abc:s.vn/$ev1) https://matrix.to/#/!abc:s.vn https://x.y";
        assert_eq!(split_shared_links(body), (body.to_string(), vec![]));
    }
}

/// A message being looked for after clicking a shared link.
#[derive(Clone)]
struct JumpTarget {
    room_id: String,
    event_id: String,
    /// the thread it's in, if any
    thread_root: Option<String>,
    /// history pages loaded while looking
    pages: u32,
    thread_opened: bool,
}

const THREAD_ITEM: &str = "chosua-thread-item";

/// How far back (pages of history) a jump looks before giving up.
const JUMP_MAX_PAGES: u32 = 60;

fn t_room(t: &RefCell<Option<JumpTarget>>) -> String {
    t.borrow().as_ref().map(|t| t.room_id.clone()).unwrap_or_default()
}

/// On drop (the end of a handler that added messages): take the next step
/// of a pending jump, once the new rows exist.
struct JumpAfter(Rc<TimelinePane>);

impl Drop for JumpAfter {
    fn drop(&mut self) {
        if self.0.jump_target.borrow().is_some() {
            let this = self.0.clone();
            glib::idle_add_local_once(move || this.try_jump());
        }
    }
}

/// A freshly fetched list, plus whatever `store` already had that came
/// after it: messages that arrived live (sync) while the fetch was on its
/// way, and our own local echoes. Replacing the store with the fetch
/// alone dropped those — a new message missing from a thread or room.
fn keep_newer(store: &gio::ListStore, skip: u32, mut fetched: Vec<TimelineEvent>) -> Vec<TimelineEvent> {
    let newest = fetched.iter().map(|e| e.timestamp).max().unwrap_or(i64::MIN);
    let known: std::collections::HashSet<String> = fetched.iter().map(|e| e.event_id.clone()).collect();
    for i in skip..store.n_items() {
        let ev = event(&store.item(i).unwrap()).clone();
        let pending = ev.local_id.as_deref() == Some(ev.event_id.as_str());
        if !known.contains(&ev.event_id) && (pending || ev.timestamp >= newest) {
            fetched.push(ev);
        }
    }
    fetched
}
