//! Matrix images for `gtk::Picture`s: fetched once per mxc URI through the
//! worker (which decrypts E2EE media), decoded *and downscaled* off the main
//! thread to the size they're shown at, then kept in a bounded cache. A
//! chat full of 12 MP photos would otherwise hold ~48 MB of pixels each.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};

use chosua_core::Command;
use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf, gio, glib};
use tokio::sync::mpsc::UnboundedSender;

/// Decoded textures kept around (thumbnails, so a few hundred is ~100 MB).
const CACHE_LIMIT: usize = 240;
/// Longest side, in device pixels, an image is decoded to.
pub const MAX_PX: i32 = 1024;

pub struct Images {
    cmd: UnboundedSender<Command>,
    textures: RefCell<HashMap<String, gdk::Texture>>,
    order: RefCell<VecDeque<String>>,
    waiting: RefCell<HashMap<String, Vec<glib::WeakRef<gtk::Widget>>>>,
    requested: RefCell<HashSet<String>>,
    /// full-size fetches (the image viewer): callbacks per key
    full: RefCell<HashMap<String, Vec<Box<dyn FnOnce(Option<gdk::Texture>)>>>>,
}

/// Cache-key prefix of full-size (undownscaled) images for the viewer.
const FULL: &str = "full|";

const KEY: &str = "chosua-mxc";
/// Cache-key prefix of avatars, and the size they're decoded to.
const AVATAR: &str = "avatar|";
const AVATAR_PX: i32 = 96;

fn paint(target: &gtk::Widget, tex: &gdk::Texture) {
    if let Some(p) = target.downcast_ref::<gtk::Picture>() {
        p.set_paintable(Some(tex));
    } else if let Some(a) = target.downcast_ref::<adw::Avatar>() {
        a.set_custom_image(Some(tex));
    }
}

impl Images {
    pub fn new(cmd: UnboundedSender<Command>) -> Self {
        Images {
            cmd,
            textures: RefCell::default(),
            order: RefCell::default(),
            waiting: RefCell::default(),
            requested: RefCell::default(),
            full: RefCell::default(),
        }
    }

    /// The original, at full size and upright, for the image viewer. Not
    /// cached (it can be tens of MB); `done` gets None when it can't be had.
    pub fn fetch_full(&self, mxc: &str, encryption: Option<&str>, done: impl FnOnce(Option<gdk::Texture>) + 'static) {
        let key = format!("{FULL}{mxc}");
        let first = {
            let mut full = self.full.borrow_mut();
            let list = full.entry(key.clone()).or_default();
            list.push(Box::new(done));
            list.len() == 1
        };
        if first {
            let _ = self.cmd.send(Command::FetchImage {
                key,
                mxc_uri: mxc.to_string(),
                media_encryption: encryption.map(str::to_string),
            });
        }
    }

    fn finish_full(&self, key: &str, tex: Option<gdk::Texture>) {
        for cb in self.full.borrow_mut().remove(key).unwrap_or_default() {
            cb(tex.clone());
        }
    }

    /// Show `mxc` in `picture` (now if cached, else once it's fetched). A
    /// recycled row re-binds its picture to another URI; only the latest
    /// one ever lands in it.
    pub fn show(&self, picture: &gtk::Picture, mxc: &str, encryption: Option<&str>) {
        picture.set_paintable(None::<&gdk::Paintable>);
        self.request(picture.upcast_ref(), mxc.to_string(), mxc, encryption);
    }

    /// A small picture (custom emoji, sticker): decoded at avatar size.
    pub fn thumb(&self, picture: &gtk::Picture, mxc: &str) {
        picture.set_paintable(None::<&gdk::Paintable>);
        self.request(picture.upcast_ref(), format!("{AVATAR}{mxc}"), mxc, None);
    }

    /// An avatar image (decoded small, cached apart from the photos).
    pub fn avatar(&self, avatar: &adw::Avatar, mxc: Option<&str>) {
        avatar.set_custom_image(None::<&gdk::Paintable>);
        match mxc {
            Some(mxc) => self.request(avatar.upcast_ref(), format!("{AVATAR}{mxc}"), mxc, None),
            None => unsafe {
                avatar.steal_data::<String>(KEY);
            },
        }
    }

    fn request(&self, target: &gtk::Widget, key: String, mxc: &str, encryption: Option<&str>) {
        unsafe { target.set_data(KEY, key.clone()) };
        if let Some(tex) = self.textures.borrow().get(&key) {
            paint(target, tex);
            return;
        }
        self.waiting.borrow_mut().entry(key.clone()).or_default().push(target.downgrade());
        if self.requested.borrow_mut().insert(key.clone()) {
            let _ = self.cmd.send(Command::FetchImage {
                key,
                mxc_uri: mxc.to_string(),
                media_encryption: encryption.map(str::to_string),
            });
        }
    }

    pub fn loaded(self: &std::rc::Rc<Self>, key: String, bytes: Vec<u8>) {
        let this = self.clone();
        if key.starts_with(FULL) {
            glib::spawn_future_local(async move {
                let decoded = gio::spawn_blocking(move || decode_full(bytes)).await.ok().flatten();
                this.finish_full(&key, decoded);
            });
            return;
        }
        glib::spawn_future_local(async move {
            let max = if key.starts_with(AVATAR) { AVATAR_PX } else { MAX_PX };
            let decoded = gio::spawn_blocking(move || decode(bytes, max)).await;
            match decoded {
                Ok(Some(tex)) => this.store(key, tex),
                _ => this.failed(&key, "could not decode"),
            }
        });
    }

    pub fn failed(&self, key: &str, error: &str) {
        if key.starts_with(FULL) {
            tracing::warn!(key, error, "original image unavailable");
            self.finish_full(key, None);
            return;
        }
        tracing::warn!(key, error, "image unavailable");
        self.waiting.borrow_mut().remove(key);
        // allow a retry the next time a row shows it
        self.requested.borrow_mut().remove(key);
    }

    fn store(&self, key: String, tex: gdk::Texture) {
        for target in self.waiting.borrow_mut().remove(&key).unwrap_or_default() {
            if let Some(target) = target.upgrade() {
                let current = unsafe { target.data::<String>(KEY).map(|p| p.as_ref().clone()) };
                if current.as_deref() == Some(key.as_str()) {
                    paint(&target, &tex);
                }
            }
        }
        let mut order = self.order.borrow_mut();
        let mut textures = self.textures.borrow_mut();
        textures.insert(key.clone(), tex);
        order.push_back(key);
        while order.len() > CACHE_LIMIT {
            if let Some(old) = order.pop_front() {
                textures.remove(&old);
                self.requested.borrow_mut().remove(&old);
            }
        }
    }
}

/// Decode and fit within max x max (keeping the aspect ratio).
fn decode(bytes: Vec<u8>, max: i32) -> Option<gdk::Texture> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
    let pixbuf =
        gdk_pixbuf::Pixbuf::from_stream_at_scale(&stream, max, max, true, gio::Cancellable::NONE).ok()?;
    let format = if pixbuf.has_alpha() {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    let tex = gdk::MemoryTexture::new(
        pixbuf.width(),
        pixbuf.height(),
        format,
        &pixbuf.read_pixel_bytes(),
        pixbuf.rowstride() as usize,
    );
    Some(tex.upcast())
}

/// Decode at full size, rotated as its EXIF says.
fn decode_full(bytes: Vec<u8>) -> Option<gdk::Texture> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
    let pixbuf = gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()?;
    let pixbuf = pixbuf.apply_embedded_orientation().unwrap_or(pixbuf);
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}
