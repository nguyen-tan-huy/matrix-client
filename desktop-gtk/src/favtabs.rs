//! Favourite rooms as a strip of tabs above the room (app.js's
//! `renderFavoriteTabs`): alphabetical so they never jump around, each with
//! its unread badge and a × to unfavourite; middle click does that too.
//! Tabs are kept per room and only updated, never rebuilt. Full names;
//! when they don't fit on one line the strip wraps onto more.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use chosua_core::models::RoomSummary;
use chosua_core::Command;
use gtk::gdk;

use crate::window::Ctx;

struct Tab {
    root: gtk::Box,
    sort_key: RefCell<String>,
    avatar: adw::Avatar,
    name: gtk::Label,
    badge: gtk::Label,
    avatar_url: RefCell<Option<String>>,
}

pub struct FavTabs {
    pub root: adw::WrapBox,
    strip: adw::WrapBox,
    ctx: Rc<Ctx>,
    tabs: RefCell<HashMap<String, Tab>>,
    on_open: RefCell<Option<Rc<dyn Fn(&str)>>>,
    on_menu: RefCell<Option<Rc<dyn Fn(&str, &gtk::Widget)>>>,
}

impl FavTabs {
    pub fn new(ctx: Rc<Ctx>) -> Rc<Self> {
        // tabs follow one another at their own width and wrap onto the next
        // line when the row is full (a flow box would align them in columns)
        let strip = adw::WrapBox::builder()
            .child_spacing(8)
            .line_spacing(8)
            .valign(gtk::Align::Start)
            .visible(false)
            .css_classes(["fav-tabs"])
            .build();
        Rc::new(FavTabs { root: strip.clone(), strip, ctx, tabs: RefCell::default(), on_open: RefCell::default(), on_menu: RefCell::default() })
    }

    pub fn connect_open(&self, f: impl Fn(&str) + 'static) {
        *self.on_open.borrow_mut() = Some(Rc::new(f));
    }

    /// Right click on a tab: its room menu (room id, the tab).
    pub fn connect_menu(&self, f: impl Fn(&str, &gtk::Widget) + 'static) {
        *self.on_menu.borrow_mut() = Some(Rc::new(f));
    }

    /// Show these favourites (any order), `selected` highlighted.
    pub fn update(self: &Rc<Self>, mut favs: Vec<RoomSummary>, selected: Option<&str>) {
        favs.retain(|r| !r.room_id.is_empty());
        let mut tabs = self.tabs.borrow_mut();
        tabs.retain(|id, tab| {
            let keep = favs.iter().any(|r| &r.room_id == id);
            if !keep {
                self.strip.remove(&tab.root);
            }
            keep
        });
        let mut resort = false;
        for room in &favs {
            let tab = tabs.entry(room.room_id.clone()).or_insert_with(|| {
                let tab = self.make_tab(&room.room_id);
                self.strip.append(&tab.root);
                tab
            });
            if tab.name.label() != room.name {
                tab.name.set_label(&room.name);
                tab.avatar.set_text(Some(&room.name));
                let key = format!("{}\u{0}{}", crate::rooms::fold(&room.name), room.room_id);
                *tab.sort_key.borrow_mut() = key;
                resort = true;
            }
            if *tab.avatar_url.borrow() != room.avatar_url {
                *tab.avatar_url.borrow_mut() = room.avatar_url.clone();
                self.ctx.images.avatar(&tab.avatar, room.avatar_url.as_deref());
            }
            let unread = room.unread_count;
            tab.badge.set_visible(unread > 0);
            tab.badge.set_label(&if unread > 99 { "99+".into() } else { unread.to_string() });
            toggle(&tab.badge, "mention", room.mention_count > 0);
            toggle(&tab.name, "unread", unread > 0);
            toggle(&tab.root, "selected", selected == Some(room.room_id.as_str()));
        }
        if resort {
            // alphabetical: put each tab right after the previous one
            let mut order: Vec<&Tab> = tabs.values().collect();
            order.sort_by(|a, b| a.sort_key.borrow().cmp(&*b.sort_key.borrow()));
            let mut prev: Option<&gtk::Box> = None;
            for tab in order {
                self.strip.reorder_child_after(&tab.root, prev);
                prev = Some(&tab.root);
            }
        }
        self.root.set_visible(!favs.is_empty());
    }

    fn make_tab(self: &Rc<Self>, room_id: &str) -> Tab {
        let avatar = adw::Avatar::builder().size(24).show_initials(true).build();
        let name = gtk::Label::builder().css_classes(["fav-tab-name"]).build();
        let badge = gtk::Label::builder().visible(false).css_classes(["unread-badge"]).build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Bỏ khỏi yêu thích")
            .css_classes(["flat", "circular", "fav-tab-close"])
            .build();
        // hugs its name: never stretched wider than its content
        let root = gtk::Box::builder()
            .spacing(8)
            .halign(gtk::Align::Start)
            .css_classes(["fav-tab"])
            .build();
        root.append(&avatar);
        root.append(&name);
        root.append(&badge);
        root.append(&close);
        root.set_cursor(gdk::Cursor::from_name("pointer", None).as_ref());

        let unfav = {
            let (ctx, id) = (self.ctx.clone(), room_id.to_string());
            move || ctx.send(Command::SetRoomFavorite { room_id: id.clone(), favorite: false })
        };
        {
            let unfav = unfav.clone();
            close.connect_clicked(move |_| unfav());
        }
        let click = gtk::GestureClick::builder().button(0).build();
        {
            let (this, id) = (Rc::downgrade(self), room_id.to_string());
            click.connect_released(move |g, _, _, _| match g.current_button() {
                gdk::BUTTON_MIDDLE => unfav(),
                gdk::BUTTON_PRIMARY => {
                    let open = this.upgrade().and_then(|t| t.on_open.borrow().clone());
                    if let Some(open) = open {
                        open(&id);
                    }
                }
                _ => {}
            });
        }
        root.add_controller(click);
        let menu = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        {
            let (this, id) = (Rc::downgrade(self), room_id.to_string());
            menu.connect_pressed(move |g, _, _, _| {
                let on_menu = this.upgrade().and_then(|t| t.on_menu.borrow().clone());
                if let (Some(on_menu), Some(w)) = (on_menu, g.widget()) {
                    on_menu(&id, &w);
                }
            });
        }
        root.add_controller(menu);
        Tab { root, sort_key: RefCell::default(), avatar, name, badge, avatar_url: RefCell::new(None) }
    }
}

fn toggle(w: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}
