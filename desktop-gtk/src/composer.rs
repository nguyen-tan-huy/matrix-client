//! The compose box: multi-line markdown text (Enter sends, Shift+Enter is a
//! new line), a reply/edit banner, attachments (button, paste, drop) and
//! typing notices. It doesn't know the room; it hands `Outgoing` to the
//! timeline, which does.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, gio, glib};

pub enum Outgoing {
    /// `mentions`: user ids pinged; `html`: the body with them as pills
    Text { body: String, reply_to: Option<String>, mentions: Vec<String>, html: Option<String> },
    Edit { event_id: String, body: String },
    Attachment { filename: String, bytes: Vec<u8>, mime: String },
    Voice { ogg: Vec<u8>, duration_ms: u64, waveform: Vec<f32> },
    Typing(bool),
}

#[derive(Clone, Default)]
enum Mode {
    #[default]
    Normal,
    Reply { event_id: String },
    Edit { event_id: String },
}

pub struct Composer {
    pub root: gtk::Box,
    text: gtk::TextView,
    banner: gtk::Box,
    banner_label: gtk::Label,
    typing: gtk::Label,
    mode: RefCell<Mode>,
    on_send: RefCell<Option<Box<dyn Fn(Outgoing)>>>,
    typing_sent: Cell<Option<Instant>>,
    shift: Cell<bool>,
    ctrl: Cell<bool>,
    /// (user id, display name) of the room's members, for @-completion
    members: RefCell<Vec<(String, String)>>,
    /// members picked from the completion while writing this message
    mentioned: RefCell<Vec<(String, String)>>,
    mention_pop: gtk::Popover,
    mention_list: gtk::ListBox,
    candidates: RefCell<Vec<(String, String)>>,
    recorder: RefCell<Option<crate::voice::Recorder>>,
    mic: gtk::Button,
    tools: gtk::Box,
    rec_label: gtk::Label,
    error: RefCell<Option<Box<dyn Fn(&str)>>>,
    /// images waiting for the user's OK before they're sent, one dialog at a time
    pending: RefCell<std::collections::VecDeque<(String, Vec<u8>, String)>>,
    confirming: Cell<bool>,
}

/// Typing notices are refreshed this often while typing (the server drops
/// them after ~30 s; app.js re-sends on a similar cadence).
const TYPING_EVERY: Duration = Duration::from_secs(4);

impl Composer {
    pub fn new() -> Rc<Self> {
        let typing = gtk::Label::builder()
            .xalign(0.0)
            .visible(false)
            .css_classes(["typing", "caption"])
            .build();

        let banner_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .build();
        let banner_close = gtk::Button::builder().icon_name("window-close-symbolic").css_classes(["flat", "circular"]).build();
        let banner = gtk::Box::builder()
            .spacing(8)
            .visible(false)
            .css_classes(["compose-banner"])
            .build();
        banner.append(&banner_label);
        banner.append(&banner_close);

        let text = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(4)
            .right_margin(4)
            .css_classes(["composer-text"])
            .build();
        let placeholder = gtk::Label::builder()
            .label("Nhập tin nhắn…")
            .xalign(0.0)
            .halign(gtk::Align::Start)
            // on the text's first line (its top margin), not centred on its
            // own: the caret and the placeholder always line up
            .valign(gtk::Align::Start)
            .margin_top(8)
            .can_target(false)
            .margin_start(4)
            .css_classes(["dim-label"])
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_height(true)
            .max_content_height(220)
            .hexpand(true)
            .child(&text)
            .build();
        // one line sits in the middle of the pill; more lines grow it from there
        let line = gtk::Overlay::builder().child(&scroll).valign(gtk::Align::Center).hexpand(true).build();
        line.add_overlay(&placeholder);
        let field = gtk::Overlay::builder().child(&line).css_classes(["composer"]).build();

        let attach = gtk::Button::builder()
            .icon_name("mail-attachment-symbolic")
            .tooltip_text("Đính kèm ảnh hoặc tệp")
            .valign(gtk::Align::End)
            .css_classes(["circular", "glass-button"])
            .build();
        let send = gtk::Button::builder()
            .icon_name("paper-plane-symbolic")
            .tooltip_text("Gửi (Enter)")
            .valign(gtk::Align::End)
            .css_classes(["suggested-action", "circular"])
            .build();
        let emoji_chooser = gtk::EmojiChooser::new();
        let emoji = gtk::MenuButton::builder()
            .icon_name("face-smile-symbolic")
            .tooltip_text("Emoji")
            .valign(gtk::Align::End)
            .popover(&emoji_chooser)
            .css_classes(["circular", "glass-button"])
            .build();
        // extra buttons the timeline adds (stickers)
        let tools = gtk::Box::builder().spacing(8).build();
        let mic = gtk::Button::builder()
            .icon_name("audio-input-microphone-symbolic")
            .tooltip_text("Ghi tin nhắn thoại")
            .valign(gtk::Align::End)
            .css_classes(["circular", "glass-button"])
            .build();
        let rec_label = gtk::Label::builder()
            .xalign(0.0)
            .visible(false)
            .css_classes(["recording"])
            .build();
        let line = gtk::Box::builder().spacing(8).build();
        line.append(&attach);
        line.append(&field);
        line.append(&emoji);
        line.append(&tools);
        line.append(&mic);
        line.append(&send);

        let mention_list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).build();
        let mention_pop = gtk::Popover::builder()
            .child(&mention_list)
            .autohide(false)
            .has_arrow(false)
            .position(gtk::PositionType::Top)
            .halign(gtk::Align::Start)
            .build();
        mention_pop.set_parent(&field);

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .css_classes(["compose-row"])
            .build();
        root.append(&typing);
        root.append(&rec_label);
        root.append(&banner);
        let toolbar = gtk::Box::builder().spacing(4).css_classes(["md-toolbar"]).build();
        root.append(&toolbar);
        root.append(&line);

        let this = Rc::new(Composer {
            root,
            text: text.clone(),
            banner,
            banner_label,
            typing,
            mode: RefCell::default(),
            on_send: RefCell::default(),
            typing_sent: Cell::new(None),
            shift: Cell::new(false),
            ctrl: Cell::new(false),
            members: RefCell::default(),
            mentioned: RefCell::default(),
            mention_pop,
            mention_list: mention_list.clone(),
            candidates: RefCell::default(),
            recorder: RefCell::default(),
            mic: mic.clone(),
            tools: tools.clone(),
            rec_label,
            error: RefCell::default(),
            pending: RefCell::default(),
            confirming: Cell::new(false),
        });
        {
            let this = Rc::downgrade(&this);
            mic.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.toggle_recording();
                }
            });
        }
        for (label, tip, action) in [
            ("<b>B</b>", "In đậm (Ctrl+B)", 0),
            ("<i>I</i>", "In nghiêng (Ctrl+I)", 1),
            ("<tt>`code`</tt>", "Code trong dòng (Ctrl+E)", 2),
            ("<tt>```</tt>", "Khối code (Ctrl+Shift+E)", 3),
            ("🔗", "Chèn link — hoặc chọn chữ rồi dán link", 4),
        ] {
            let b = gtk::Button::builder().tooltip_text(tip).css_classes(["flat", "md-button"]).build();
            let l = gtk::Label::new(None);
            l.set_markup(label);
            b.set_child(Some(&l));
            let this = Rc::downgrade(&this);
            b.connect_clicked(move |_| {
                let Some(this) = this.upgrade() else { return };
                match action {
                    0 => this.toggle_wrap("**", "**"),
                    1 => this.toggle_wrap("*", "*"),
                    2 => this.toggle_wrap("`", "`"),
                    3 => this.code_block(),
                    _ => this.insert_link(),
                };
            });
            toolbar.append(&b);
        }
        {
            let text = text.clone();
            emoji_chooser.connect_emoji_picked(move |_, e| {
                text.buffer().insert_at_cursor(e);
                text.grab_focus();
            });
        }
        {
            let this = Rc::downgrade(&this);
            mention_list.connect_row_activated(move |_, row| {
                if let Some(this) = this.upgrade() {
                    this.accept_mention(row.index() as usize);
                }
            });
        }

        let buffer = text.buffer();
        {
            let (this, placeholder) = (Rc::downgrade(&this), placeholder.clone());
            buffer.connect_changed(move |b| {
                let empty = b.char_count() == 0;
                placeholder.set_visible(empty);
                if let Some(this) = this.upgrade() {
                    this.typing_changed(empty);
                    this.update_mentions();
                }
            });
        }
        // Enter sends. Done on the inserted "\n" rather than the key press,
        // so an input method (fcitx5/Unikey) still gets Enter to commit its
        // preedit first; the key controller only remembers Shift.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let this = Rc::downgrade(&this);
            keys.connect_key_pressed(move |_, key, _, state| {
                let Some(this) = this.upgrade() else { return glib::Propagation::Proceed };
                if this.mention_pop.is_visible() {
                    let n = this.candidates.borrow().len() as i32;
                    let cur = this.mention_list.selected_row().map(|r| r.index()).unwrap_or(0);
                    match key {
                        gdk::Key::Down | gdk::Key::Up => {
                            let next = if key == gdk::Key::Down { (cur + 1) % n.max(1) } else { (cur + n - 1) % n.max(1) };
                            if let Some(r) = this.mention_list.row_at_index(next) {
                                this.mention_list.select_row(Some(&r));
                            }
                            return glib::Propagation::Stop;
                        }
                        gdk::Key::Tab | gdk::Key::Return | gdk::Key::KP_Enter => {
                            this.accept_mention(cur.max(0) as usize);
                            return glib::Propagation::Stop;
                        }
                        gdk::Key::Escape => {
                            this.mention_pop.popdown();
                            return glib::Propagation::Stop;
                        }
                        _ => {}
                    }
                }
                if state.contains(gdk::ModifierType::CONTROL_MASK) && !state.contains(gdk::ModifierType::ALT_MASK) {
                    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
                    let done = match key.to_lower() {
                        gdk::Key::b if !shift => this.toggle_wrap("**", "**"),
                        gdk::Key::i if !shift => this.toggle_wrap("*", "*"),
                        gdk::Key::e if !shift => this.toggle_wrap("`", "`"),
                        gdk::Key::e if shift => this.code_block(),
                        _ => false,
                    };
                    if done {
                        return glib::Propagation::Stop;
                    }
                }
                if key == gdk::Key::Escape && this.recorder.borrow().is_some() {
                    this.stop_recording(false);
                    return glib::Propagation::Stop;
                }
                if key == gdk::Key::Escape && !matches!(*this.mode.borrow(), Mode::Normal) {
                    this.cancel();
                    return glib::Propagation::Stop;
                }
                if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter)
                    && state.contains(gdk::ModifierType::CONTROL_MASK)
                    && !crate::config::get().enter_sends
                {
                    this.submit();
                    return glib::Propagation::Stop;
                }
                if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) {
                    this.shift.set(state.contains(gdk::ModifierType::SHIFT_MASK));
                    this.ctrl.set(state.contains(gdk::ModifierType::CONTROL_MASK));
                }
                glib::Propagation::Proceed
            });
        }
        text.add_controller(keys);
        {
            let this = Rc::downgrade(&this);
            buffer.connect_insert_text(move |b, _, s| {
                let Some(this) = this.upgrade() else { return };
                // Enter sends (Shift+Enter: new line), or with that setting
                // off, Ctrl+Enter sends and Enter is a new line
                let (shift, ctrl) = (this.shift.replace(false), this.ctrl.replace(false));
                let sends = if crate::config::get().enter_sends { !shift } else { ctrl };
                if s == "\n" && sends {
                    b.stop_signal_emission_by_name("insert-text");
                    let this = this.clone();
                    glib::idle_add_local_once(move || this.submit());
                }
            });
        }
        {
            let this = Rc::downgrade(&this);
            text.connect_paste_clipboard(move |view| {
                let Some(this) = this.upgrade() else { return };
                let clipboard = view.clipboard();
                // a URL pasted over selected text links that text (app.js's
                // `wrapSelectionAsLink`); anything else pastes as usual
                let buffer = view.buffer();
                if buffer.has_selection() && !clipboard.formats().contains_type(gdk::Texture::static_type()) {
                    view.stop_signal_emission_by_name("paste-clipboard");
                    let this = this.clone();
                    glib::spawn_future_local(async move {
                        let Ok(Some(pasted)) = clipboard.read_text_future().await else { return };
                        this.paste_over_selection(pasted.trim());
                    });
                    return;
                }
                if clipboard.formats().contains_type(gdk::Texture::static_type()) {
                    view.stop_signal_emission_by_name("paste-clipboard");
                    glib::spawn_future_local(async move {
                        match clipboard.read_texture_future().await {
                            Ok(Some(tex)) => {
                                let bytes = tex.save_to_png_bytes().to_vec();
                                this.confirm_attachment("image.png".into(), bytes, "image/png".into());
                            }
                            Ok(None) => {}
                            Err(e) => tracing::warn!(error = %e, "clipboard image unreadable"),
                        }
                    });
                }
            });
        }
        {
            let this = this.clone();
            send.connect_clicked(move |_| this.submit());
        }
        {
            let this = Rc::downgrade(&this);
            attach.connect_clicked(move |button| {
                let Some(this) = this.upgrade() else { return };
                let window = button.root().and_downcast::<gtk::Window>();
                glib::spawn_future_local(async move {
                    let dialog = gtk::FileDialog::builder().title("Đính kèm").modal(true).build();
                    if let Ok(files) = dialog.open_multiple_future(window.as_ref()).await {
                        let files: Vec<gio::File> =
                            (0..files.n_items()).filter_map(|i| files.item(i).and_downcast()).collect();
                        this.attach_files(files).await;
                    }
                });
            });
        }
        banner_close.connect_clicked({
            let this = Rc::downgrade(&this);
            move |_| {
                if let Some(this) = this.upgrade() {
                    this.cancel();
                }
            }
        });
        this
    }

    // ---- markdown editing (app.js's compose editor) -------------------

    /// The selection, or an empty range at the cursor.
    fn selection(&self) -> (gtk::TextIter, gtk::TextIter) {
        let b = self.text.buffer();
        b.selection_bounds().unwrap_or_else(|| {
            let at = b.iter_at_mark(&b.get_insert());
            (at, at)
        })
    }

    /// Wrap the selection in `before`/`after` (`**`, `*`, `` ` ``), or unwrap
    /// it if it already is. Keeps the inner text selected.
    fn toggle_wrap(&self, before: &str, after: &str) -> bool {
        let b = self.text.buffer();
        let (start, end) = self.selection();
        let (so, eo) = (start.offset(), end.offset());
        let (bl, al) = (before.chars().count() as i32, after.chars().count() as i32);
        let around = |from: i32, to: i32| {
            if from < 0 || to > b.char_count() {
                return String::new();
            }
            b.text(&b.iter_at_offset(from), &b.iter_at_offset(to), false).to_string()
        };
        let wrapped = around(so - bl, so) == before && around(eo, eo + al) == after;
        if wrapped {
            b.begin_user_action();
            let (mut s, mut e) = (b.iter_at_offset(eo), b.iter_at_offset(eo + al));
            b.delete(&mut s, &mut e);
            let (mut s, mut e) = (b.iter_at_offset(so - bl), b.iter_at_offset(so));
            b.delete(&mut s, &mut e);
            b.end_user_action();
            b.select_range(&b.iter_at_offset(so - bl), &b.iter_at_offset(eo - bl));
        } else {
            b.begin_user_action();
            b.insert(&mut b.iter_at_offset(eo), after);
            b.insert(&mut b.iter_at_offset(so), before);
            b.end_user_action();
            b.select_range(&b.iter_at_offset(so + bl), &b.iter_at_offset(eo + bl));
        }
        self.text.grab_focus();
        true
    }

    /// A fenced code block around the selection (or an empty one), each
    /// fence on its own line; the cursor ends up inside.
    fn code_block(&self) -> bool {
        let b = self.text.buffer();
        let (start, end) = self.selection();
        let (so, eo) = (start.offset(), end.offset());
        let selected = b.text(&start, &end, false).to_string();
        let lead = so > 0 && b.text(&b.iter_at_offset(so - 1), &start, false) != "\n";
        let trail = eo < b.char_count() && b.text(&end, &b.iter_at_offset(eo + 1), false) != "\n";
        let block = format!("{}```\n{}\n```{}", if lead { "\n" } else { "" }, selected, if trail { "\n" } else { "" });
        b.begin_user_action();
        let (mut s, mut e) = (start, end);
        b.delete(&mut s, &mut e);
        b.insert(&mut b.iter_at_offset(so), &block);
        b.end_user_action();
        let cursor = so + lead as i32 + 4 + selected.chars().count() as i32;
        b.place_cursor(&b.iter_at_offset(cursor));
        self.text.grab_focus();
        true
    }

    /// `[selection]()` with the cursor in the parentheses (or `[]()` with
    /// it in the brackets).
    fn insert_link(&self) -> bool {
        let b = self.text.buffer();
        let (start, end) = self.selection();
        let so = start.offset();
        let selected = b.text(&start, &end, false).to_string();
        b.begin_user_action();
        let (mut s, mut e) = (start, end);
        b.delete(&mut s, &mut e);
        b.insert(&mut b.iter_at_offset(so), &format!("[{selected}]()"));
        b.end_user_action();
        let n = selected.chars().count() as i32;
        let cursor = if n > 0 { so + n + 3 } else { so + 1 };
        b.place_cursor(&b.iter_at_offset(cursor));
        self.text.grab_focus();
        true
    }

    /// The clipboard's text over the selection: a link to it if it's a URL.
    fn paste_over_selection(&self, pasted: &str) {
        let b = self.text.buffer();
        let (start, end) = self.selection();
        let so = start.offset();
        let selected = b.text(&start, &end, false).to_string();
        let is_url = (pasted.starts_with("http://") || pasted.starts_with("https://")) && !pasted.contains(char::is_whitespace);
        let text = if is_url && !selected.is_empty() { format!("[{selected}]({pasted})") } else { pasted.to_string() };
        b.begin_user_action();
        let (mut s, mut e) = (start, end);
        b.delete(&mut s, &mut e);
        b.insert(&mut b.iter_at_offset(so), &text);
        b.end_user_action();
        b.place_cursor(&b.iter_at_offset(so + text.chars().count() as i32));
    }

    pub fn connect_send(&self, f: impl Fn(Outgoing) + 'static) {
        *self.on_send.borrow_mut() = Some(Box::new(f));
    }

    fn emit(&self, out: Outgoing) {
        if let Some(f) = self.on_send.borrow().as_ref() {
            f(out);
        }
    }

    pub fn grab_focus(&self) {
        self.text.grab_focus();
    }

    fn content(&self) -> String {
        let b = self.text.buffer();
        b.text(&b.start_iter(), &b.end_iter(), false).trim().to_string()
    }

    fn submit(&self) {
        let body = self.content();
        if body.is_empty() {
            return;
        }
        let (mentions, html) = mention_html(&body, &self.mentioned.take());
        let out = match self.mode.take() {
            Mode::Edit { event_id } => Outgoing::Edit { event_id, body },
            Mode::Reply { event_id } => Outgoing::Text { body, reply_to: Some(event_id), mentions, html },
            Mode::Normal => Outgoing::Text { body, reply_to: None, mentions, html },
        };
        self.mention_pop.popdown();
        self.banner.set_visible(false);
        self.text.buffer().set_text("");
        self.emit(out);
    }

    pub fn reply_to(&self, event_id: &str, sender: &str, preview: &str) {
        *self.mode.borrow_mut() = Mode::Reply { event_id: event_id.to_string() };
        self.banner_label.set_markup(&format!(
            "↩ Trả lời <b>{}</b>: {}",
            glib::markup_escape_text(sender),
            glib::markup_escape_text(&preview.chars().take(120).collect::<String>())
        ));
        self.banner.set_visible(true);
        self.text.grab_focus();
    }

    pub fn edit(&self, event_id: &str, body: &str) {
        *self.mode.borrow_mut() = Mode::Edit { event_id: event_id.to_string() };
        self.banner_label.set_markup("✎ <b>Sửa tin nhắn</b> — Esc để huỷ");
        self.banner.set_visible(true);
        self.text.buffer().set_text(body);
        self.text.grab_focus();
    }

    /// Leave reply/edit (an edit's text is dropped too).
    pub fn cancel(&self) {
        if matches!(self.mode.take(), Mode::Edit { .. }) {
            self.text.buffer().set_text("");
        }
        self.banner.set_visible(false);
    }

    /// Switching rooms: nothing half-typed follows along.
    pub fn add_tool(&self, w: &impl IsA<gtk::Widget>) {
        self.tools.append(w);
    }

    pub fn connect_error(&self, f: impl Fn(&str) + 'static) {
        *self.error.borrow_mut() = Some(Box::new(f));
    }

    fn error(&self, msg: &str) {
        if let Some(f) = self.error.borrow().as_ref() {
            f(msg);
        }
    }

    fn toggle_recording(self: &Rc<Self>) {
        if self.recorder.borrow().is_some() {
            self.stop_recording(true);
            return;
        }
        match crate::voice::Recorder::start() {
            Ok(r) => {
                *self.recorder.borrow_mut() = Some(r);
                self.mic.set_icon_name("media-playback-stop-symbolic");
                self.mic.set_tooltip_text(Some("Dừng và gửi"));
                self.mic.add_css_class("destructive-action");
                self.rec_label.set_visible(true);
                let this = Rc::downgrade(self);
                glib::timeout_add_local(Duration::from_millis(250), move || {
                    let Some(this) = this.upgrade() else { return glib::ControlFlow::Break };
                    let rec = this.recorder.borrow();
                    let Some(r) = rec.as_ref() else { return glib::ControlFlow::Break };
                    let s = r.elapsed().as_secs();
                    this.rec_label.set_label(&format!("● Đang ghi {}:{:02} — bấm ■ để gửi, Esc để huỷ", s / 60, s % 60));
                    glib::ControlFlow::Continue
                });
            }
            Err(e) => self.error(&format!("Không mở được micro: {e}")),
        }
    }

    fn stop_recording(&self, send: bool) {
        let Some(r) = self.recorder.borrow_mut().take() else { return };
        self.mic.set_icon_name("audio-input-microphone-symbolic");
        self.mic.set_tooltip_text(Some("Ghi tin nhắn thoại"));
        self.mic.remove_css_class("destructive-action");
        self.rec_label.set_visible(false);
        if !send {
            r.cancel();
            return;
        }
        match r.finish() {
            Ok(rec) => self.emit(Outgoing::Voice {
                ogg: rec.ogg,
                duration_ms: rec.duration.as_millis() as u64,
                waveform: rec.waveform,
            }),
            Err(e) => self.error(&format!("Không gửi được bản ghi: {e}")),
        }
    }

    pub fn set_members(&self, members: Vec<(String, String)>) {
        *self.members.borrow_mut() = members;
    }

    /// The "@partial" right before the cursor: (its start offset, the text).
    fn mention_token(&self) -> Option<(i32, String)> {
        let b = self.text.buffer();
        let cursor = b.iter_at_mark(&b.get_insert());
        let mut start = cursor;
        start.set_line_offset(0);
        let before = b.text(&start, &cursor, false).to_string();
        let at = before.rfind('@')?;
        let query = &before[at + 1..];
        let boundary = before[..at].chars().last().is_none_or(char::is_whitespace);
        if !boundary || query.chars().any(char::is_whitespace) || query.chars().count() > 32 {
            return None;
        }
        Some((start.offset() + before[..at].chars().count() as i32, query.to_string()))
    }

    fn update_mentions(&self) {
        let Some((_, query)) = self.mention_token() else {
            self.mention_pop.popdown();
            return;
        };
        let q = crate::rooms::fold(&query);
        let found: Vec<(String, String)> = self
            .members
            .borrow()
            .iter()
            .filter(|(id, name)| crate::rooms::fold(name).contains(&q) || id.to_lowercase().contains(&q))
            .take(8)
            .cloned()
            .collect();
        if found.is_empty() {
            self.mention_pop.popdown();
            return;
        }
        self.mention_list.remove_all();
        for (id, name) in &found {
            let row = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_start(6).margin_end(6).build();
            row.append(&gtk::Label::builder().label(name).xalign(0.0).css_classes(["heading"]).build());
            row.append(&gtk::Label::builder().label(id).xalign(0.0).css_classes(["dim-label", "caption"]).build());
            self.mention_list.append(&row);
        }
        if let Some(r) = self.mention_list.row_at_index(0) {
            self.mention_list.select_row(Some(&r));
        }
        *self.candidates.borrow_mut() = found;
        self.mention_pop.popup();
    }

    fn accept_mention(&self, i: usize) {
        let Some((id, name)) = self.candidates.borrow().get(i).cloned() else { return };
        let Some((start, _)) = self.mention_token() else { return };
        let b = self.text.buffer();
        let mut from = b.iter_at_offset(start);
        let mut to = b.iter_at_mark(&b.get_insert());
        b.delete(&mut from, &mut to);
        b.insert_at_cursor(&format!("@{name} "));
        self.mentioned.borrow_mut().push((id, name));
        self.mention_pop.popdown();
        self.text.grab_focus();
    }

    pub fn reset(&self) {
        self.mentioned.borrow_mut().clear();
        self.mention_pop.popdown();
        self.mode.take();
        self.banner.set_visible(false);
        self.text.buffer().set_text("");
        self.typing.set_visible(false);
    }

    pub fn set_typing(&self, names: &[String]) {
        self.typing.set_visible(!names.is_empty());
        self.typing.set_label(&match names {
            [] => String::new(),
            [a] => format!("{a} đang nhập…"),
            [a, b] => format!("{a} và {b} đang nhập…"),
            [a, rest @ ..] => format!("{a} và {} người khác đang nhập…", rest.len()),
        });
    }

    fn typing_changed(&self, empty: bool) {
        if empty {
            if self.typing_sent.take().is_some() {
                self.emit(Outgoing::Typing(false));
            }
        } else if crate::config::get().typing_notices
            && self.typing_sent.get().is_none_or(|t| t.elapsed() >= TYPING_EVERY)
        {
            self.typing_sent.set(Some(Instant::now()));
            self.emit(Outgoing::Typing(true));
        }
    }

    /// Images are looked at before they're sent: a preview with Gửi / Hủy.
    /// Other files go straight out. Several images are asked one by one.
    pub fn confirm_attachment(self: &Rc<Self>, filename: String, bytes: Vec<u8>, mime: String) {
        if !mime.starts_with("image/") {
            self.emit(Outgoing::Attachment { filename, bytes, mime });
            return;
        }
        self.pending.borrow_mut().push_back((filename, bytes, mime));
        if !self.confirming.replace(true) {
            self.confirm_next();
        }
    }

    fn confirm_next(self: &Rc<Self>) {
        let Some((filename, bytes, mime)) = self.pending.borrow_mut().pop_front() else {
            self.confirming.set(false);
            return;
        };
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from(&bytes[..])).ok();
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["send-preview"])
            .build();
        let mut facts = format_size(bytes.len());
        match &texture {
            Some(t) => {
                picture.set_paintable(Some(t));
                facts = format!("{}×{} · {facts}", t.width(), t.height());
            }
            None => picture.set_visible(false),
        }
        let name = gtk::Label::builder()
            .label(&filename)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["heading"])
            .build();
        let info = gtk::Label::builder().label(&facts).css_classes(["dim-label", "caption"]).build();
        let cancel = gtk::Button::builder().label("Hủy").css_classes(["pill"]).build();
        let send = gtk::Button::builder().label("Gửi").css_classes(["pill", "suggested-action"]).build();
        let buttons = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).margin_top(6).margin_bottom(12).build();
        buttons.append(&cancel);
        buttons.append(&send);
        let more = self.pending.borrow().len();
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_start(12).margin_end(12).build();
        col.append(&picture);
        col.append(&name);
        col.append(&info);
        if more > 0 {
            col.append(&gtk::Label::builder().label(format!("Còn {more} ảnh nữa")).css_classes(["dim-label", "caption"]).build());
        }
        col.append(&buttons);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&col));
        let dialog = adw::Dialog::builder()
            .title("Gửi ảnh này?")
            .content_width(640)
            .content_height(560)
            .child(&view)
            .default_widget(&send)
            .build();
        let sent = Rc::new(Cell::new(false));
        {
            let (this, sent, d) = (self.clone(), sent.clone(), dialog.clone());
            let payload = RefCell::new(Some((filename, bytes, mime)));
            send.connect_clicked(move |_| {
                if let Some((filename, bytes, mime)) = payload.borrow_mut().take() {
                    this.emit(Outgoing::Attachment { filename, bytes, mime });
                }
                sent.set(true);
                d.close();
            });
        }
        {
            let d = dialog.clone();
            cancel.connect_clicked(move |_| {
                d.close();
            });
        }
        {
            // Esc / closing the window cancels this one (and only this one)
            let this = self.clone();
            dialog.connect_closed(move |_| this.confirm_next());
        }
        dialog.present(Some(&self.root));
        send.grab_focus();
    }

    pub async fn attach_files(self: &Rc<Self>, files: Vec<gio::File>) {
        for file in files {
            match file.load_contents_future().await {
                Ok((bytes, _)) => {
                    let filename = file
                        .basename()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "file".into());
                    let (ctype, _) = gio::content_type_guess(Some(&filename), Some(&bytes[..]));
                    let mime = gio::content_type_get_mime_type(&ctype)
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "application/octet-stream".into());
                    self.confirm_attachment(filename, bytes.to_vec(), mime);
                }
                Err(e) => tracing::warn!(error = %e, "could not read attachment"),
            }
        }
    }
}

fn format_size(n: usize) -> String {
    let n = n as f64;
    if n >= 1024.0 * 1024.0 {
        format!("{:.1} MB", n / 1024.0 / 1024.0)
    } else {
        format!("{:.0} KB", (n / 1024.0).max(1.0))
    }
}

/// The members still "@Name"d in `body`, and the body as HTML with each of
/// them a matrix.to pill (app.js's `buildMentionHtml`; longest name first so
/// "@An Nguyen" wins over "@An").
fn mention_html(body: &str, picked: &[(String, String)]) -> (Vec<String>, Option<String>) {
    let mut cands: Vec<&(String, String)> = picked.iter().collect();
    cands.sort_by_key(|(_, name)| std::cmp::Reverse(name.chars().count()));
    cands.dedup();
    let mut ids: Vec<String> = Vec::new();
    let mut html = String::new();
    let mut rest = body;
    'outer: while !rest.is_empty() {
        for (id, name) in &cands {
            let needle = format!("@{name}");
            if rest.starts_with(&needle) {
                if !ids.contains(id) {
                    ids.push(id.clone());
                }
                html.push_str(&format!(
                    "<a href=\"https://matrix.to/#/{}\">{}</a>",
                    glib::Uri::escape_string(id, None, false),
                    glib::markup_escape_text(&needle)
                ));
                rest = &rest[needle.len()..];
                continue 'outer;
            }
        }
        let c = rest.chars().next().unwrap();
        let s = c.to_string();
        html.push_str(&glib::markup_escape_text(&s));
        rest = &rest[c.len_utf8()..];
    }
    if ids.is_empty() {
        (ids, None)
    } else {
        (ids, Some(html.replace('\n', "<br>")))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mention_pills() {
        let picked = vec![("@an:x".to_string(), "An".to_string()), ("@ann:x".to_string(), "An Nguyen".to_string())];
        let (ids, html) = super::mention_html("hi @An Nguyen & @An", &picked);
        assert_eq!(ids, vec!["@ann:x", "@an:x"]);
        assert_eq!(
            html.unwrap(),
            "hi <a href=\"https://matrix.to/#/%40ann%3Ax\">@An Nguyen</a> &amp; <a href=\"https://matrix.to/#/%40an%3Ax\">@An</a>"
        );
        assert_eq!(super::mention_html("plain", &picked), (vec![], None));
    }
}
