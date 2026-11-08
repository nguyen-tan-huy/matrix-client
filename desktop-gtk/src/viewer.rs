//! The image viewer: the thumbnail at once, the original as soon as it's
//! fetched, zoom in/out (buttons, +/-/0/1 keys, Ctrl+wheel, pinch,
//! double-click), drag to pan, open the original in the desktop's viewer, save.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use chosua_core::Command;
use gtk::{gdk, glib};

use crate::window::Ctx;

const MIN_ZOOM: f64 = 0.05;
const MAX_ZOOM: f64 = 16.0;
const STEP: f64 = 1.25;

struct View {
    scroller: gtk::ScrolledWindow,
    picture: gtk::Picture,
    label: gtk::Button,
    /// the image's own size in pixels (what 100% means)
    size: Cell<(f64, f64)>,
    /// 0 = fit the window, else the scale factor
    zoom: Cell<f64>,
}

impl View {
    fn fit_scale(&self) -> f64 {
        let (iw, ih) = self.size.get();
        let (vw, vh) = (self.scroller.width() as f64, self.scroller.height() as f64);
        if iw < 1.0 || ih < 1.0 || vw < 1.0 || vh < 1.0 {
            return 1.0;
        }
        (vw / iw).min(vh / ih)
    }

    fn scale(&self) -> f64 {
        match self.zoom.get() {
            z if z > 0.0 => z,
            _ => self.fit_scale(),
        }
    }

    fn apply(&self) {
        let z = self.zoom.get();
        let (iw, ih) = self.size.get();
        if z <= 0.0 {
            self.picture.set_can_shrink(true);
            self.picture.set_content_fit(gtk::ContentFit::Contain);
            self.picture.set_size_request(-1, -1);
            self.picture.set_halign(gtk::Align::Fill);
            self.picture.set_valign(gtk::Align::Fill);
            self.scroller.set_hscrollbar_policy(gtk::PolicyType::Never);
            self.scroller.set_vscrollbar_policy(gtk::PolicyType::Never);
        } else {
            self.picture.set_can_shrink(false);
            self.picture.set_content_fit(gtk::ContentFit::Fill);
            self.picture.set_size_request((iw * z).round() as i32, (ih * z).round() as i32);
            self.picture.set_halign(gtk::Align::Center);
            self.picture.set_valign(gtk::Align::Center);
            self.scroller.set_hscrollbar_policy(gtk::PolicyType::Automatic);
            self.scroller.set_vscrollbar_policy(gtk::PolicyType::Automatic);
        }
        self.label.set_label(&format!("{}%", (self.scale() * 100.0).round() as i32));
    }

    /// Zoom to `to`, keeping the point at the middle of the window in place.
    fn zoom_to(&self, to: f64) {
        let (h, v) = (self.scroller.hadjustment(), self.scroller.vadjustment());
        let centre = |a: &gtk::Adjustment| if a.upper() > 0.0 { (a.value() + a.page_size() / 2.0) / a.upper() } else { 0.5 };
        let (cx, cy) = (centre(&h), centre(&v));
        self.zoom.set(to.clamp(MIN_ZOOM, MAX_ZOOM));
        self.apply();
        let scroller = self.scroller.clone();
        glib::idle_add_local_once(move || {
            for (a, c) in [(scroller.hadjustment(), cx), (scroller.vadjustment(), cy)] {
                a.set_value((c * a.upper() - a.page_size() / 2.0).max(0.0));
            }
        });
    }

    fn by(&self, factor: f64) {
        self.zoom_to(self.scale() * factor);
    }

    fn fit(&self) {
        self.zoom.set(0.0);
        self.apply();
    }

    fn toggle(&self) {
        if self.zoom.get() > 0.0 && (self.zoom.get() - 1.0).abs() < 1e-6 { self.fit() } else { self.zoom_to(1.0) }
    }

    fn show(&self, tex: &impl IsA<gdk::Paintable>, size: Option<(f64, f64)>) {
        self.picture.set_paintable(Some(tex));
        let (w, h) = (tex.intrinsic_width() as f64, tex.intrinsic_height() as f64);
        self.size.set(size.unwrap_or((w, h)));
        self.apply();
    }
}

/// Open `ev`'s image big over `anchor`. `thumb` is what the timeline shows
/// (instant); the original replaces it once fetched.
pub fn show(ctx: &Rc<Ctx>, anchor: &impl IsA<gtk::Widget>, title: &str, mxc: &str, filename: &str,
            encryption: Option<String>, size: Option<(u32, u32)>, thumb: Option<gdk::Paintable>) {
    let picture = gtk::Picture::builder().hexpand(true).vexpand(true).build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&picture)
        .hexpand(true)
        .vexpand(true)
        .css_classes(["image-viewer"])
        .build();
    let label = gtk::Button::builder().css_classes(["flat"]).tooltip_text("Vừa khung hình").build();
    let view = Rc::new(View {
        scroller: scroller.clone(),
        picture: picture.clone(),
        label: label.clone(),
        size: Cell::new(size.map(|(w, h)| (w as f64, h as f64)).unwrap_or((0.0, 0.0))),
        zoom: Cell::new(0.0),
    });
    let known = size.map(|(w, h)| (w as f64, h as f64));
    if let Some(t) = &thumb {
        view.show(t, known);
    }

    let btn = |icon: &str, tip: &str| gtk::Button::builder().icon_name(icon).tooltip_text(tip).css_classes(["flat"]).build();
    let out = btn("zoom-out-symbolic", "Thu nhỏ (−)");
    let inn = btn("zoom-in-symbolic", "Phóng to (+)");
    let actual = gtk::Button::builder().label("1:1").tooltip_text("Kích thước thật (1)").css_classes(["flat"]).build();
    let spinner = gtk::Spinner::builder().spinning(true).tooltip_text("Đang tải ảnh gốc…").build();
    let original = gtk::Button::builder().label("Mở ảnh gốc").css_classes(["pill"]).build();
    let save = gtk::Button::builder().label("Lưu").css_classes(["pill"]).build();
    let bar = gtk::Box::builder().spacing(4).halign(gtk::Align::Center).margin_bottom(10).margin_top(4).build();
    for w in [out.upcast_ref::<gtk::Widget>(), label.upcast_ref(), inn.upcast_ref(), actual.upcast_ref(),
              spinner.upcast_ref(), original.upcast_ref(), save.upcast_ref()] {
        bar.append(w);
    }
    let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    col.append(&scroller);
    col.append(&bar);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&col));
    let dialog = adw::Dialog::builder().title(title).content_width(1000).content_height(760).child(&toolbar).build();

    {
        let v = view.clone();
        out.connect_clicked(move |_| v.by(1.0 / STEP));
        let v = view.clone();
        inn.connect_clicked(move |_| v.by(STEP));
        let v = view.clone();
        actual.connect_clicked(move |_| v.zoom_to(1.0));
        let v = view.clone();
        label.connect_clicked(move |_| v.fit());
    }
    // the fit scale (and so the label) follows the window
    {
        let v = view.clone();
        scroller.connect_notify_local(Some("width"), move |_, _| v.apply());
        let v = view.clone();
        scroller.connect_notify_local(Some("height"), move |_, _| v.apply());
    }
    // Ctrl + wheel
    {
        let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
        let v = view.clone();
        wheel.connect_scroll(move |c, _, dy| {
            if c.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
                v.by(if dy < 0.0 { STEP } else { 1.0 / STEP });
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
        scroller.add_controller(wheel);
    }
    // pinch
    {
        let pinch = gtk::GestureZoom::new();
        let start = Rc::new(Cell::new(1.0));
        let (v, s) = (view.clone(), start.clone());
        pinch.connect_begin(move |_, _| s.set(v.scale()));
        let v = view.clone();
        pinch.connect_scale_changed(move |_, scale| v.zoom_to(start.get() * scale));
        scroller.add_controller(pinch);
    }
    // drag to pan
    {
        let drag = gtk::GestureDrag::new();
        let from = Rc::new(Cell::new((0.0, 0.0)));
        let (sc, f) = (scroller.clone(), from.clone());
        drag.connect_drag_begin(move |_, _, _| f.set((sc.hadjustment().value(), sc.vadjustment().value())));
        let sc = scroller.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let (h0, v0) = from.get();
            sc.hadjustment().set_value(h0 - dx);
            sc.vadjustment().set_value(v0 - dy);
        });
        scroller.add_controller(drag);
    }
    // double-click: fit <-> 100%
    {
        let click = gtk::GestureClick::new();
        let v = view.clone();
        click.connect_pressed(move |_, n, _, _| {
            if n == 2 {
                v.toggle();
            }
        });
        picture.add_controller(click);
    }
    // keys
    {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let v = view.clone();
        keys.connect_key_pressed(move |_, key, _, _| {
            match key {
                gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => v.by(STEP),
                gdk::Key::minus | gdk::Key::KP_Subtract => v.by(1.0 / STEP),
                gdk::Key::_0 | gdk::Key::KP_0 => v.fit(),
                gdk::Key::_1 | gdk::Key::KP_1 => v.zoom_to(1.0),
                _ => return glib::Propagation::Proceed,
            }
            glib::Propagation::Stop
        });
        dialog.add_controller(keys);
    }

    {
        let (c, u, n, e) = (ctx.clone(), mxc.to_string(), filename.to_string(), encryption.clone());
        original.connect_clicked(move |_| {
            c.send(Command::OpenMediaExternally { mxc_uri: u.clone(), filename: n.clone(), media_encryption: e.clone() })
        });
        let (c, u, n, e) = (ctx.clone(), mxc.to_string(), filename.to_string(), encryption.clone());
        save.connect_clicked(move |_| {
            c.send(Command::DownloadMedia { mxc_uri: u.clone(), filename: n.clone(), media_encryption: e.clone() })
        });
    }

    // the original, when it arrives
    {
        let (v, sp) = (Rc::downgrade(&view), spinner.clone());
        ctx.images.fetch_full(mxc, encryption.as_deref(), move |tex| {
            sp.set_spinning(false);
            sp.set_visible(false);
            if let (Some(v), Some(tex)) = (v.upgrade(), tex) {
                v.show(&tex, None);
            }
        });
    }
    dialog.present(Some(anchor));
    view.apply();
}
