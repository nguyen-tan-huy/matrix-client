//! The main window and the one place `Event`s are dispatched.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use chosua_core::event::RoomListKind;
use chosua_core::{Command, Event};
use gtk::{gio, glib};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::config;
use crate::favtabs::FavTabs;
use crate::images::Images;
use crate::login::LoginPage;
use crate::rooms::RoomsPane;
use crate::roomtools::RoomTools;
use crate::settings::Settings;
use crate::timeline::TimelinePane;

/// What every pane needs: a way to talk to the worker, and the image cache.
pub struct Ctx {
    cmd: UnboundedSender<Command>,
    pub images: Rc<Images>,
}

impl Ctx {
    pub fn send(&self, cmd: Command) {
        if self.cmd.send(cmd).is_err() {
            tracing::error!("worker is gone; command dropped");
        }
    }
}

const STYLE: &str = include_str!("style.css");

pub fn build(
    app: &adw::Application,
    cmd: UnboundedSender<Command>,
    mut rx: UnboundedReceiver<Event>,
) {
    let display = gtk::gdk::Display::default().expect("no display");
    // no animation of its own (page crossfades, revealers, CSS transitions): the compositor
    // (swayctl-fx) animates windows, and its settings are the ones every app follows
    gtk::Settings::for_display(&display).set_gtk_enable_animations(false);
    let provider = gtk::CssProvider::new();
    provider.load_from_string(STYLE);
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    // the user's text size, on top of the app's style
    let look = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(&display, &look, gtk::STYLE_PROVIDER_PRIORITY_USER);
    apply_look(&look);

    let cfg = config::get();
    let ctx = Rc::new(Ctx { cmd: cmd.clone(), images: Rc::new(Images::new(cmd)) });

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("ChoSua")
        .default_width(cfg.window_width)
        .default_height(cfg.window_height)
        .hide_on_close(cfg.close_to_tray)
        .build();
    // the sync keeps running while the window is hidden (notifications);
    // quitting is the tray's, or closing with "close to tray" off
    std::mem::forget(app.hold());
    window.connect_close_request(|w| {
        let (width, height) = w.default_size();
        if !w.is_maximized() && width > 0 && height > 0 {
            config::update(|c| {
                c.window_width = width;
                c.window_height = height;
            });
        }
        if !config::get().close_to_tray {
            std::process::exit(0);
        }
        glib::Propagation::Proceed
    });

    let toasts = adw::ToastOverlay::new();
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::None).build();

    let spinner = adw::Spinner::builder().width_request(48).height_request(48).build();
    let loading = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .build();
    loading.append(&spinner);
    stack.add_named(&loading, Some("loading"));

    let login = LoginPage::new(ctx.clone());
    stack.add_named(&login.root, Some("login"));

    let rooms = RoomsPane::new(ctx.clone());
    let timeline = TimelinePane::new(ctx.clone());
    let tools = RoomTools::new(ctx.clone());
    let settings = Settings::new(ctx.clone());
    rooms.add_header_start(&settings.button);
    // the chats menu (app.js's "⋯" next to CHATS) and every room's threads
    let chats_menu = gio::Menu::new();
    chats_menu.append(Some("Tìm tin nhắn ở mọi phòng"), Some("chats.search"));
    chats_menu.append(Some("Tìm tin của một người"), Some("chats.user-search"));
    chats_menu.append(Some("Thread của mọi phòng"), Some("chats.threads"));
    let more = gio::Menu::new();
    more.append(Some("Cài đặt"), Some("chats.settings"));
    more.append(Some("Phím tắt"), Some("chats.shortcuts"));
    chats_menu.append_section(None, &more);
    let chats_button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .menu_model(&chats_menu)
        .tooltip_text("Thêm")
        .css_classes(["circular", "glass-button"])
        .build();
    let all_threads = gtk::Button::builder()
        .icon_name("mail-reply-all-symbolic")
        .tooltip_text("Thread của mọi phòng")
        .css_classes(["circular", "glass-button"])
        .build();
    rooms.add_header_end(&chats_button);
    rooms.add_header_end(&all_threads);
    timeline.add_header_button(&tools.button);
    let empty = adw::StatusPage::builder()
        .icon_name("chat-message-new-symbolic")
        .title("Chọn một cuộc trò chuyện")
        .build();
    let content = gtk::Stack::builder().transition_type(gtk::StackTransitionType::None).build();
    content.add_named(&empty, Some("empty"));
    content.add_named(&timeline.root, Some("room"));
    // rooms | room: drag the divider; its place is remembered
    rooms.root.set_width_request(220);
    content.set_width_request(360);
    content.set_vexpand(true);
    // favourites as tabs above the room (app.js's #favorite-tabs)
    let favtabs = FavTabs::new(ctx.clone());
    let main = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    main.append(&favtabs.root);
    main.append(&content);
    let refresh_tabs = {
        let (favtabs, rooms) = (favtabs.clone(), rooms.clone());
        move || favtabs.update(rooms.favorites(), rooms.selected_room().as_deref())
    };
    {
        let rooms = rooms.clone();
        favtabs.connect_open(move |room_id| rooms.select(room_id));
    }
    {
        let rooms = rooms.clone();
        favtabs.connect_menu(move |room_id, tab| rooms.menu_for(room_id, tab));
    }
    // Modern Minimal: the room list and the chat are two rounded cards on a
    // soft grey page
    main.set_spacing(12);
    for card in [rooms.root.upcast_ref::<gtk::Widget>(), favtabs.root.upcast_ref(), empty.upcast_ref()] {
        card.add_css_class("mm-card");
        card.set_overflow(gtk::Overflow::Hidden);
    }
    let split = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&rooms.root)
        .end_child(&main)
        .resize_start_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(cfg.rooms_width)
        .css_classes(["main-split"])
        .build();
    remember_position(&split, |c, pos| c.rooms_width = pos);
    stack.add_named(&split, Some("chat"));
    stack.set_visible_child_name("loading");

    {
        let chats = gio::SimpleActionGroup::new();
        let add = |name: &str, f: Box<dyn Fn()>| {
            let a = gio::SimpleAction::new(name, None);
            a.connect_activate(move |_, _| f());
            chats.add_action(&a);
        };
        let t = tools.clone();
        add("search", Box::new(move || t.show_search()));
        let (t, w) = (tools.clone(), window.clone());
        add("user-search", Box::new(move || t.show_user_search(&w)));
        let (t, r, c) = (timeline.clone(), rooms.clone(), content.clone());
        add(
            "threads",
            Box::new(move || {
                c.set_visible_child_name("room");
                t.show_all_threads(r.room_ids(), false);
            }),
        );
        let s = settings.clone();
        add("settings", Box::new(move || s.show(&s.button)));
        let s = settings.clone();
        add("shortcuts", Box::new(move || s.show_shortcuts(&s.button)));
        chats_button.insert_action_group("chats", Some(&chats));
    }
    {
        let (t, r, c) = (timeline.clone(), rooms.clone(), content.clone());
        all_threads.connect_clicked(move |_| {
            c.set_visible_child_name("room");
            t.show_all_threads(r.room_ids(), false);
        });
    }
    // bottom-left: threads with unread replies, with their count
    {
        let badge = gtk::Label::builder().visible(false).css_classes(["unread-badge", "mention"]).build();
        let inner = gtk::Box::builder().spacing(8).build();
        inner.append(&gtk::Image::from_icon_name("mail-reply-all-symbolic"));
        inner.append(&gtk::Label::new(Some("Thread chưa đọc")));
        inner.append(&badge);
        let unread_threads = gtk::Button::builder()
            .child(&inner)
            .hexpand(true)
            .tooltip_text("Xem các thread chưa đọc")
            .css_classes(["flat"])
            .build();
        rooms.add_footer(&unread_threads);
        {
            let (t, r, c) = (timeline.clone(), rooms.clone(), content.clone());
            unread_threads.connect_clicked(move |_| {
                c.set_visible_child_name("room");
                t.show_all_threads(r.room_ids(), true);
            });
        }
        timeline.connect_unread_threads(move |n| {
            badge.set_visible(n > 0);
            badge.set_label(&if n > 99 { "99+".into() } else { n.to_string() });
        });
        // keep the count fresh: ask for the threads of rooms not yet seen
        let (t, r) = (timeline.clone(), rooms.clone());
        glib::timeout_add_seconds_local(30, move || {
            t.scan_threads(r.room_ids());
            glib::ControlFlow::Continue
        });
        let (t, r) = (timeline.clone(), rooms.clone());
        glib::timeout_add_seconds_local_once(5, move || t.scan_threads(r.room_ids()));
    }
    {
        let r = rooms.clone();
        timeline.connect_room_name(move |id| r.name_of(id));
    }
    {
        let timeline = timeline.clone();
        let ctx = ctx.clone();
        let (tools, window) = (tools.clone(), window.clone());
        let refresh_tabs = refresh_tabs.clone();
        rooms.connect_selected(move |room| {
            refresh_tabs();
            timeline.open(&room.room_id, &room.name);
            tools.set_room(&room.room_id, &room.name, &window);
            content.set_visible_child_name("room");
            ctx.send(Command::GetNotificationMode { room_id: room.room_id.clone() });
        });
    }


    {
        let toasts = toasts.clone();
        timeline.connect_toast(move |msg| toasts.add_toast(adw::Toast::new(msg)));
    }
    {
        let rooms = rooms.clone();
        tools.connect_jump(move |room_id| rooms.select(room_id));
    }
    {
        let rooms = rooms.clone();
        timeline.connect_jump(move |room_id| rooms.select(room_id));
    }

    // keyboard shortcuts: every action of config::ACTIONS, rebindable in
    // the settings
    let mut actions: HashMap<&'static str, Rc<dyn Fn()>> = HashMap::new();
    {
        let r = rooms.clone();
        actions.insert("search-rooms", Rc::new(move || r.focus_search()));
        let r = rooms.clone();
        actions.insert("prev-room", Rc::new(move || r.select_relative(-1)));
        let r = rooms.clone();
        actions.insert("next-room", Rc::new(move || r.select_relative(1)));
        let t = tools.clone();
        actions.insert("search-messages", Rc::new(move || t.show_search()));
        let t = timeline.clone();
        actions.insert("threads", Rc::new(move || t.toggle_threads()));
        let (t, c) = (timeline.clone(), ctx.clone());
        actions.insert(
            "mark-read",
            Rc::new(move || {
                if let Some(room_id) = t.current() {
                    c.send(Command::MarkRoomRead { room_id });
                }
            }),
        );
        let s = settings.clone();
        actions.insert("settings", Rc::new(move || s.show(&s.button)));
        let s = settings.clone();
        actions.insert("shortcuts", Rc::new(move || s.show_shortcuts(&s.button)));
        let r = rooms.root.clone();
        actions.insert("toggle-sidebar", Rc::new(move || r.set_visible(!r.is_visible())));
        actions.insert("quit", Rc::new(|| std::process::exit(0)));
    }
    let keys = gtk::ShortcutController::new();
    keys.set_scope(gtk::ShortcutScope::Managed);
    window.add_controller(keys.clone());
    let reload_keys = {
        let (keys, actions) = (keys.clone(), Rc::new(actions));
        move || {
            while let Some(old) = keys.item(0).and_downcast::<gtk::Shortcut>() {
                keys.remove_shortcut(&old);
            }
            let cfg = config::get();
            for (id, ..) in config::ACTIONS {
                let (Some(f), Some(trigger)) = (actions.get(id).cloned(), gtk::ShortcutTrigger::parse_string(&cfg.accel(id)))
                else {
                    continue;
                };
                let action = gtk::CallbackAction::new(move |_, _| {
                    f();
                    glib::Propagation::Stop
                });
                keys.add_shortcut(gtk::Shortcut::new(Some(trigger), Some(action)));
            }
        }
    };
    reload_keys();
    {
        let (window, look) = (window.clone(), look.clone());
        settings.connect_changed(move || {
            apply_look(&look);
            window.set_hide_on_close(config::get().close_to_tray);
            reload_keys();
        });
    }

    toasts.set_child(Some(&stack));
    window.set_content(Some(&toasts));
    if !cfg.start_hidden {
        window.present();
    }

    let (tray_tx, mut tray_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::tray::spawn(tray_tx);
    {
        let (window, ctx) = (window.clone(), ctx.clone());
        glib::spawn_future_local(async move {
            while let Some(action) = tray_rx.recv().await {
                match action {
                    crate::tray::TrayAction::Toggle => {
                        if window.is_visible() && window.is_active() {
                            window.set_visible(false);
                        } else {
                            window.present();
                        }
                    }
                    crate::tray::TrayAction::Resumed => ctx.send(Command::ResumeSync),
                }
            }
        });
    }

    let modes: Rc<RefCell<HashMap<String, String>>> = Rc::default();
    // app.js's `maybeNotify`: not our own, not what's on screen, and as the
    // room's notification mode allows
    let notify = {
        let (ctx, window, timeline, rooms) = (ctx.clone(), window.clone(), timeline.clone(), rooms.clone());
        let modes = modes.clone();
        move |room_id: &str, event: &chosua_core::models::TimelineEvent, thread_id: Option<String>| {
            if !config::get().notifications {
                return;
            }
            let mode = modes.borrow().get(room_id).cloned().unwrap_or_else(|| "all".into());
            let on_screen = match &thread_id {
                Some(root) => timeline.is_thread_open(room_id, root),
                None => timeline.current().as_deref() == Some(room_id),
            };
            let seen = window.is_active() && on_screen;
            let wanted = mode != "mute" && (mode != "mentions" || event.mentions_me);
            if event.is_own || seen || !wanted {
                return;
            }
            let title = match rooms.name_of(room_id) {
                Some(room) if room != event.sender_name => format!("{} ({room})", event.sender_name),
                _ => event.sender_name.clone(),
            };
            let body = match event.msg_type.as_str() {
                "image" => "đã gửi một ảnh".to_string(),
                "video" => "đã gửi một video".to_string(),
                "audio" => "đã gửi tin nhắn thoại".to_string(),
                "file" => "đã gửi một tệp".to_string(),
                _ => event.body.chars().take(120).collect(),
            };
            ctx.send(Command::ShowNotification {
                room_id: room_id.to_string(),
                thread_id,
                title,
                body,
                icon_mxc: event.sender_avatar_url.clone(),
                icon_png: None,
            });
        }
    };
    let grow = Rc::new(GrowRoomList::default());
    glib::spawn_future_local(async move {
        while let Some(event) = rx.recv().await {
            match event {
                Event::SessionChecked(true) | Event::LoggedIn => {
                    stack.set_visible_child_name("chat");
                    ctx.send(Command::StartSync);
                    ctx.send(Command::GetOwnProfile);
                }
                Event::SessionChecked(false) => stack.set_visible_child_name("login"),
                Event::LoginError(e) => login.show_error(&e),
                Event::SessionExpired => {
                    login.show_error("Phiên đăng nhập đã hết hạn — hãy đăng nhập lại");
                    stack.set_visible_child_name("login");
                }
                Event::LoggedOut => {
                    settings.close();
                    stack.set_visible_child_name("login");
                }
                Event::OwnProfile(p) => {
                    timeline.set_me(p.clone());
                    settings.own_profile(p)
                }
                Event::VerificationEmojis(emojis) => settings.verification_emojis(emojis),
                Event::VerificationDone => settings.verification_done(true, ""),
                Event::VerificationCancelled(reason) => settings.verification_done(false, &reason),
                Event::RecoveryStatus(status) => settings.recovery_status(&status),
                Event::RoomKeysImported { imported, total } => settings.keys_imported(imported, total),
                Event::RecoveryBackupExists => settings.recovery_backup_exists(),
                Event::RecoveryKeyCreated { key } => settings.recovery_key_created(&key),
                Event::RoomKeysExported { bytes } => settings.keys_exported(bytes),
                Event::LvxApiKeyStatus { configured } => settings.lvx_status(configured),
                Event::RoomListUpdate { list: RoomListKind::Rooms, ops } => {
                    rooms.apply(ops);
                    refresh_tabs();
                    grow.schedule(&ctx, rooms.len());
                }
                Event::RoomListUpdate { list: RoomListKind::Invites, ops } => rooms.apply_invites(ops),
                Event::Spaces(spaces) => rooms.set_spaces(spaces),
                Event::SpaceChildren { space_room_id, room_ids } => rooms.set_space_children(&space_room_id, room_ids),
                Event::RoomFavoriteSet { room_id, favorite } => {
                    rooms.set_favorite(&room_id, favorite);
                    refresh_tabs();
                }
                Event::ReadReceipt { room_id, user_id, event_id } => {
                    timeline.read_receipt(&room_id, &user_id, &event_id)
                }
                Event::UnreadCountChanged { room_id, unread_count, mention_count } => {
                    // our own receipt moving (here or on another device)
                    // changes which threads are unread
                    timeline.refresh_threads(&room_id);
                    rooms.set_unread(&room_id, unread_count, mention_count);
                    refresh_tabs();
                }
                Event::Timeline { room_id, events } => timeline.set_events(&room_id, events),
                Event::TimelinePrepend { room_id, events, reached_start } => {
                    timeline.prepend(&room_id, events, reached_start)
                }
                Event::NewMessage { room_id, event } => {
                    notify(&room_id, &event, None);
                    timeline.push(&room_id, event)
                }
                Event::RoomInfo(info) => tools.room_info(info),
                Event::ImagePacks { room_id, images } => timeline.image_packs(&room_id, images),
                Event::Members { room_id, members } => {
                    timeline.set_members(&room_id, &members);
                    tools.members(&room_id, members)
                }
                Event::SharedEventResolved { room_id, event_id, found, thread_root_id } => {
                    timeline.shared_event(&room_id, &event_id, found, thread_root_id)
                }
                Event::DirectoryUsers { users, .. } => tools.directory_users(users),
                Event::PinnedEvents { room_id, event_ids } => tools.pinned(&room_id, event_ids),
                Event::EventPreview { room_id, event_id, event } => {
                    timeline.event_preview(&room_id, &event_id, event.clone());
                    tools.event_preview(&room_id, &event_id, event)
                }
                Event::MessageSearchResult { results, truncated, .. } => tools.search_results(results, truncated),
                Event::AllUsers { users } => tools.all_users(users),
                Event::UserMessagesSearchResult { user_id, results, truncated } => {
                    tools.user_messages(&user_id, results, truncated)
                }
                Event::Summary { room_id, thread_root_id: None, text } => tools.summary(&room_id, &text),
                Event::PollsList { room_id, polls } => tools.polls(&room_id, polls),
                Event::PollUpdated(poll) => tools.poll_updated(poll),
                Event::NotificationMode { room_id, mode } => {
                    tools.notification_mode(&room_id, &mode);
                    modes.borrow_mut().insert(room_id, mode);
                }
                Event::NotificationClicked { room_id, thread_id } => {
                    window.present();
                    rooms.select(&room_id);
                    if let Some(root) = thread_id {
                        timeline.open_thread_by_id(&room_id, &root);
                    }
                }
                Event::MessageEdited { room_id, event_id, new_body } => {
                    timeline.edit(&room_id, &event_id, new_body)
                }
                Event::MessageDeleted { room_id, event_id } => timeline.remove(&room_id, &event_id),
                Event::Reactions { room_id, event_id, reactions } => {
                    timeline.set_reactions(&room_id, &event_id, reactions)
                }
                Event::MessageSent { local_id, .. } => timeline.sent(&local_id),
                Event::ThreadsList { room_id, threads, reached_end } => {
                    timeline.threads_list(&room_id, threads, false, reached_end)
                }
                Event::ThreadsListAppend { room_id, threads, reached_end } => {
                    timeline.threads_list(&room_id, threads, true, reached_end)
                }
                Event::ThreadEvents { room_id, thread_root_id, events } => {
                    timeline.thread_events(&room_id, &thread_root_id, events)
                }
                Event::ThreadEventsPrepend { room_id, thread_root_id, events, reached_start } => {
                    timeline.thread_prepend(&room_id, &thread_root_id, events, reached_start)
                }
                Event::ThreadReply { room_id, thread_root_id, event } => {
                    if !timeline.is_thread_open(&room_id, &thread_root_id) {
                        notify(&room_id, &event, Some(thread_root_id.clone()));
                    }
                    timeline.refresh_threads(&room_id);
                    timeline.thread_reply(&room_id, &thread_root_id, event)
                }
                Event::MessageSendFailed { local_id, error, .. } => timeline.send_failed(&local_id, &error),
                Event::TypingUsers { room_id, user_ids } => timeline.set_typing(&room_id, &user_ids),
                Event::MediaDownloaded { path } => {
                    toasts.add_toast(adw::Toast::new(&format!("Đã lưu: {path}")))
                }
                Event::ImageBytes { key, bytes } => ctx.images.loaded(key, bytes),
                Event::ImageFetchFailed { key, error } => ctx.images.failed(&key, &error),
                Event::GlassConfig(cfg) => {
                    // a blinking caret redraws the composer twice a second,
                    // and over glass every redraw makes the compositor
                    // re-render the glass around it (~10 ms on a 3K screen):
                    // a cursor hitch each blink. Steady caret while on glass.
                    if let Some(s) = gtk::Settings::default() {
                        s.set_gtk_cursor_blink(!cfg.on);
                    }
                    // the text is light over glass; the compositor darkens the
                    // glass behind it where the backdrop is too bright
                    if cfg.on {
                        window.add_css_class("glass");
                    } else {
                        window.remove_css_class("glass");
                    }
                    // key ink: text is drawn in the key colour and the
                    // compositor inks it light or dark from what's behind
                    if cfg.on && cfg.ink {
                        window.add_css_class("ink-key");
                    } else {
                        window.remove_css_class("ink-key");
                    }
                }
                Event::Error(e) => toasts.add_toast(adw::Toast::new(&e)),
                _ => {}
            }
        }
    });
}

/// `RoomListService` only grows its window when asked: ask again shortly
/// after every update, backing off (to 5 s) while nothing new shows up —
/// the same schedule as app.js's `scheduleGrowRoomList`.
#[derive(Default)]
struct GrowRoomList {
    last_len: Cell<usize>,
    streak: Cell<u32>,
    timer: RefCell<Option<glib::SourceId>>,
}

impl GrowRoomList {
    fn schedule(self: &Rc<Self>, ctx: &Rc<Ctx>, len: usize) {
        if let Some(t) = self.timer.borrow_mut().take() {
            t.remove();
        }
        let grew = len != self.last_len.replace(len);
        self.streak.set(if grew { 0 } else { self.streak.get() + 1 });
        let delay = (150u64 << self.streak.get().min(8)).min(5000);
        let (this, ctx) = (self.clone(), ctx.clone());
        let id = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            this.timer.borrow_mut().take();
            ctx.send(Command::GrowRoomList);
        });
        *self.timer.borrow_mut() = Some(id);
    }
}


/// Theme and text size from the settings.
fn apply_look(look: &gtk::CssProvider) {
    let cfg = config::get();
    adw::StyleManager::default().set_color_scheme(match cfg.theme.as_str() {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    });
    let mut rules = String::new();
    if !cfg.font_family.is_empty() {
        rules.push_str(&format!("font-family: \"{}\", sans-serif; ", cfg.font_family.replace('"', "")));
    }
    if (12..=22).contains(&cfg.font_size) {
        rules.push_str(&format!("font-size: {}px; ", cfg.font_size));
    }
    let mut css = if rules.is_empty() {
        String::new()
    } else {
        format!("window.background, window.background popover, window.background dialog, window.background entry, window.background textview {{ {rules}}}\n")
    };
    // message text weight (on top of style.css)
    let weight = cfg.message_weight.clamp(300, 800) / 100 * 100;
    css.push_str(&format!(".bubble .body, .bubble .reply-quote, .composer-text {{ font-weight: {weight}; }}\n"));
    look.load_from_string(&css);
}

/// Save where a divider was left, once dragging settles.
pub fn remember_position(paned: &gtk::Paned, set: fn(&mut config::Config, i32)) {
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    paned.connect_position_notify(move |p| {
        if !p.is_mapped() {
            return;
        }
        if let Some(t) = pending.borrow_mut().take() {
            t.remove();
        }
        let (p, pending2) = (p.clone(), pending.clone());
        let id = glib::timeout_add_local_once(Duration::from_millis(600), move || {
            pending2.borrow_mut().take();
            let pos = p.position();
            config::update(|c| set(c, pos));
        });
        *pending.borrow_mut() = Some(id);
    });
}
