//! Chat list + message thread + composer.
//!
//! TDLib pushes state rather than answering queries: chats, users and groups arrive as
//! updates and are kept here in plain maps, from which the sidebar is rebuilt (coalesced to
//! one rebuild per main-loop turn, since a login can bring hundreds of updates at once).
use super::state::{Media, action_text, fmt_size, fmt_time, media_of, presence, to_markup};
use crate::tg::client::{Client, FileKind};
use crate::tg::types::*;
use adw::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
struct State {
    chats: HashMap<i64, Chat>,
    users: HashMap<i64, User>,
    basic_groups: HashMap<i64, BasicGroup>,
    supergroups: HashMap<i64, Supergroup>,
    /// Default mute_for of private chats, groups and channels (`updateScopeNotificationSettings`).
    scope_mute: [i32; 3],
    me: i64,
    /// Loaded messages per chat, oldest first.
    messages: HashMap<i64, Vec<Message>>,
    /// Chats whose history has been paged back to the very first message.
    exhausted: HashSet<i64>,
    loading_older: HashSet<i64>,
    current: Option<i64>,
    rows: HashMap<i64, gtk4::ListBoxRow>,
    /// Who is doing what, per chat ("Ana is typing"), with when they last said so.
    actions: HashMap<i64, (String, &'static str, Instant)>,
    all_chats_loaded: bool,
    loading_chats: bool,
}

const PAGE: i32 = 50;

/// Where the thread scroller should settle after its contents change.
#[derive(Clone, Copy, PartialEq, Debug)]
enum ScrollTarget {
    /// The user has scrolled on their own; leave the position alone.
    Free,
    /// Stick to the newest message.
    Bottom,
    /// Keep this distance (in pixels) from the end, so prepended pages don't shift the view.
    FromBottom(f64),
}

pub struct ChatsView {
    pub widget: adw::NavigationSplitView,
    win: adw::ApplicationWindow,
    client: Arc<Client>,
    events: async_channel::Receiver<Update>,
    /// Set by the app: the client closed (logout, or the session was revoked elsewhere).
    on_closed: RefCell<Option<Box<dyn Fn()>>>,
    st: Rc<RefCell<State>>,
    list: gtk4::ListBox,
    list_scroll: gtk4::ScrolledWindow,
    list_dirty: Cell<bool>,
    thread: gtk4::ListBox,
    thread_scroll: gtk4::ScrolledWindow,
    /// Where the thread should sit once GTK has laid out the rows just added to it.
    scroll_target: Cell<ScrollTarget>,
    /// Decoded images by TDLib file id, so re-rendering a thread never re-decodes.
    textures: Rc<RefCell<HashMap<i32, gtk4::gdk::Texture>>>,
    /// Looping animations by file id for the open chat; dropped when another chat opens.
    animations: Rc<RefCell<HashMap<i32, gtk4::MediaFile>>>,
    /// Local paths of files downloaded this session.
    paths: Rc<RefCell<HashMap<i32, String>>>,
    /// Profile photos by file id. `None` marks a download in flight.
    avatars: Rc<RefCell<HashMap<i32, Option<gtk4::gdk::Texture>>>>,
    thread_title: adw::WindowTitle,
    entry: gtk4::TextView,
    emoji_btn: gtk4::Button,
    send: gtk4::Button,
    attach: gtk4::Button,
    gif_btn: gtk4::Button,
    toast: adw::ToastOverlay,
    banner: adw::Banner,
    side_stack: gtk4::Stack,
    content_stack: gtk4::Stack,
    composer: gtk4::Box,
    settings: Rc<RefCell<crate::settings::Settings>>,
    notifier: Option<Rc<crate::notify::Notifier>>,
    /// The "+" in the sidebar header that opens the new-chat picker.
    new_chat: gtk4::Button,
    /// Messages dated before this are backlog TDLib is catching up on, not news.
    started: i64,
}

/// Keep only what a dialler would: a leading `+` and digits. `None` if the text doesn't look
/// like a phone number at all (letters, or fewer than five digits).
fn normalise_number(input: &str) -> Option<String> {
    let t = input.trim();
    if t.is_empty() || !t.chars().all(|c| c.is_ascii_digit() || "+ ()-.".contains(c)) { return None; }
    let digits: String = t.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 5 { return None; }
    Some(format!("+{digits}"))
}

/// A Telegram username as typed, without the `@`, if it could be one (5–32 of a–z, 0–9, _).
fn as_username(input: &str) -> Option<String> {
    let t = input.trim().trim_start_matches("https://t.me/").trim_start_matches('@');
    let ok = (5..=32).contains(&t.len()) && t.starts_with(|c: char| c.is_ascii_alphabetic()) && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    ok.then(|| t.to_string())
}

/// A centred spinner with a caption, used while a pane is waiting on Telegram.
fn loading_page(text: &str) -> gtk4::Widget {
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    let label = gtk4::Label::builder().label(text).css_classes(["dim-label"]).build();
    let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(12).halign(gtk4::Align::Center).valign(gtk4::Align::Center).vexpand(true).hexpand(true).build();
    col.append(&spinner); col.append(&label);
    col.upcast()
}

/// Build a stack of named pages, crossfading between them.
fn stack(pages: &[(&str, &gtk4::Widget)]) -> gtk4::Stack {
    let st = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::Crossfade).transition_duration(150).vexpand(true).build();
    for (name, w) in pages { st.add_named(*w, Some(name)); }
    st
}

/// Telegram's seven name colours, picked by id the way its apps do.
fn name_color(id: i64) -> &'static str {
    ["#e17076", "#eda86c", "#a695e7", "#7bc862", "#6ec9cb", "#65aadd", "#ee7aae"][id.rem_euclid(7) as usize]
}

fn sender_key(s: &MessageSender) -> i64 { match s { MessageSender::User { user_id } => *user_id, MessageSender::Chat { chat_id } => *chat_id, MessageSender::Unknown => 0 } }

impl State {
    fn chat_title(&self, c: &Chat) -> String {
        if c.kind.user_id() == Some(self.me) && self.me != 0 { "Saved Messages".into() } else if c.title.is_empty() { "Unnamed".into() } else { c.title.clone() }
    }

    fn sender_name(&self, s: &MessageSender, short: bool) -> String {
        match s {
            MessageSender::User { user_id } => self.users.get(user_id).map(|u| if short { u.short_name() } else { u.full_name() }).unwrap_or_default(),
            MessageSender::Chat { chat_id } => self.chats.get(chat_id).map(|c| c.title.clone()).unwrap_or_default(),
            MessageSender::Unknown => String::new(),
        }
    }

    /// The photo to show for a message's sender.
    fn sender_photo(&self, s: &MessageSender) -> Option<File> {
        match s {
            MessageSender::User { user_id } => self.users.get(user_id)?.profile_photo.as_ref().map(|p| p.small.clone()),
            MessageSender::Chat { chat_id } => self.chats.get(chat_id)?.photo.as_ref().map(|p| p.small.clone()),
            MessageSender::Unknown => None,
        }
    }

    fn muted(&self, c: &Chat) -> bool {
        let ns = &c.notification_settings;
        let mute_for = if ns.use_default_mute_for {
            self.scope_mute[if c.kind.is_channel() { 2 } else if c.kind.is_group() { 1 } else { 0 }]
        } else { ns.mute_for };
        mute_for > 0
    }

    fn unread(&self, c: &Chat) -> bool { c.unread_count > 0 || c.is_marked_as_unread }

    /// "online", "12 members", "typing…" — the thread header's second line.
    fn subtitle(&self, c: &Chat) -> String {
        if let Some((who, what, at)) = self.actions.get(&c.id) && at.elapsed() < Duration::from_secs(6) {
            return if c.kind.user_id().is_some() || who.is_empty() { format!("{what}…") } else { format!("{who} is {what}…") };
        }
        let count = |n: i32, one: &str, many: &str| if n == 1 { format!("1 {one}") } else if n > 0 { format!("{n} {many}") } else { String::new() };
        match &c.kind {
            ChatType::Private { user_id } | ChatType::Secret { user_id } if *user_id == self.me => String::new(),
            ChatType::Private { user_id } | ChatType::Secret { user_id } => self.users.get(user_id).map(presence).unwrap_or_default(),
            ChatType::BasicGroup { basic_group_id } => self.basic_groups.get(basic_group_id).map(|g| count(g.member_count, "member", "members")).unwrap_or_default(),
            ChatType::Supergroup { supergroup_id, is_channel } => {
                let n = self.supergroups.get(supergroup_id).map(|g| g.member_count).unwrap_or(0);
                if *is_channel { count(n, "subscriber", "subscribers") } else { count(n, "member", "members") }
            }
            ChatType::Unknown => String::new(),
        }
    }

    /// Main-list chats, pinned first, then by TDLib's order.
    fn ordered(&self) -> Vec<i64> {
        let mut v: Vec<(&Chat, (bool, i64))> = self.chats.values().map(|c| (c, c.main_order())).filter(|(_, o)| o.1 != 0).collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.into_iter().map(|(c, _)| c.id).collect()
    }
}

impl ChatsView {
    pub fn new(win: &adw::ApplicationWindow, client: Arc<Client>, events: async_channel::Receiver<Update>) -> Self {
        // ── sidebar ──
        let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::Single).css_classes(["navigation-sidebar", "avro-chats"]).build();
        let list_scroll = gtk4::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        let side_header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("Avro", "")).build();
        let menu = gtk4::gio::Menu::new();
        menu.append(Some("Preferences"), Some("app.preferences"));
        menu.append(Some("Log out"), Some("app.logout"));
        side_header.pack_end(&gtk4::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).build());
        let new_chat = gtk4::Button::builder().icon_name("list-add-symbolic").tooltip_text("New chat").build();
        side_header.pack_start(&new_chat);
        let side_empty = adw::StatusPage::builder().icon_name("chat-message-new-symbolic").title("No chats").description("Start one with the + button.").build();
        let side_stack = stack(&[("loading", &loading_page("Loading chats…")), ("empty", side_empty.upcast_ref()), ("list", list_scroll.upcast_ref())]);
        let side = adw::ToolbarView::new();
        side.add_top_bar(&side_header);
        side.set_content(Some(&side_stack));
        let sidebar = adw::NavigationPage::builder().title("Chats").child(&side).build();

        // ── thread ──
        let thread = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["boxed-list-separate"]).margin_start(12).margin_end(12).margin_top(8).margin_bottom(8).valign(gtk4::Align::End).build();
        thread.add_css_class("avro-thread");
        let thread_scroll = gtk4::ScrolledWindow::builder().child(&thread).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        let thread_title = adw::WindowTitle::new("", "");
        // Multi-line composer: Enter inserts a newline, Ctrl+Enter sends. The text view grows with
        // its content up to a cap, then scrolls; the buttons sit at the bottom edge either way.
        let entry = gtk4::TextView::builder().wrap_mode(gtk4::WrapMode::WordChar).hexpand(true).accepts_tab(false)
            .top_margin(7).bottom_margin(7).left_margin(10).right_margin(10).css_classes(["avro-entry"]).build();
        let placeholder = gtk4::Label::builder().label("Message").halign(gtk4::Align::Start).valign(gtk4::Align::Start)
            .margin_start(10).margin_top(7).can_target(false).css_classes(["dim-label"]).build();
        let overlay = gtk4::Overlay::builder().child(&entry).build();
        overlay.add_overlay(&placeholder);
        let entry_scroll = gtk4::ScrolledWindow::builder().child(&overlay).hscrollbar_policy(gtk4::PolicyType::Never)
            .propagate_natural_height(true).max_content_height(160).hexpand(true).css_classes(["avro-entry-frame"]).build();
        let pl = placeholder.clone();
        entry.buffer().connect_changed(move |b| pl.set_visible(b.char_count() == 0));
        let emoji_btn = gtk4::Button::builder().icon_name("emoji-people-symbolic").css_classes(["circular"]).tooltip_text("Insert emoji").valign(gtk4::Align::End).build();
        let send = gtk4::Button::builder().icon_name("mail-send-symbolic").css_classes(["suggested-action", "circular"]).valign(gtk4::Align::End).tooltip_text("Send (Ctrl+Enter)").build();
        let attach = gtk4::Button::builder().icon_name("mail-attachment-symbolic").css_classes(["circular"]).tooltip_text("Attach a file").valign(gtk4::Align::End).build();
        let gif_btn = gtk4::Button::builder().label("GIF").css_classes(["circular", "avro-gif-btn"]).tooltip_text("Send a GIF").valign(gtk4::Align::End).build();
        let composer = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).margin_start(12).margin_end(12).margin_top(6).margin_bottom(12).valign(gtk4::Align::End).build();
        composer.append(&attach); composer.append(&gif_btn); composer.append(&emoji_btn); composer.append(&entry_scroll); composer.append(&send);
        composer.set_visible(false);
        let banner = adw::Banner::builder().revealed(false).build();
        let content_empty = adw::StatusPage::builder().icon_name("user-available-symbolic").title("Select a chat").description("Pick a chat from the list to start messaging.").build();
        let content_stack = stack(&[("empty", content_empty.upcast_ref()), ("loading", &loading_page("Loading messages…")), ("thread", thread_scroll.upcast_ref())]);
        let content_box = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        content_box.append(&banner); content_box.append(&content_stack); content_box.append(&composer);
        let content = adw::ToolbarView::new();
        content.add_top_bar(&adw::HeaderBar::builder().title_widget(&thread_title).build());
        content.set_content(Some(&content_box));
        let toast = adw::ToastOverlay::new(); toast.set_child(Some(&content));
        let content_page = adw::NavigationPage::builder().title("Chat").child(&toast).build();

        let widget = adw::NavigationSplitView::builder().sidebar(&sidebar).content(&content_page).min_sidebar_width(260.0).max_sidebar_width(360.0).build();

        let css = gtk4::CssProvider::new();
        css.load_from_string("
            .avro-bubble { padding: 8px 12px; border-radius: 16px; }
            .avro-me { background: var(--accent-bg-color); color: var(--accent-fg-color); }
            .avro-me link, .avro-me a { color: inherit; text-decoration: underline; }
            .avro-them { background: alpha(currentColor, 0.08); }
            .avro-image { border-radius: 16px; }
            .avro-round { border-radius: 999px; }
            .avro-sticker { background: none; }
            .avro-big-emoji { font-size: 64px; }
            .avro-qr { background: white; border-radius: 12px; padding: 0; }
            .avro-entry-frame { border-radius: 18px; border: 1px solid color-mix(in srgb, currentColor var(--border-opacity), transparent); background: alpha(currentColor, 0.05); }
            .avro-entry-frame:focus-within { border-color: var(--accent-bg-color); }
            .avro-entry, .avro-entry text { background: transparent; }
            /* the scrolled window otherwise reserves the scrollbar slider's 40px minimum, so an empty composer would start two lines tall */
            .avro-entry-frame scrollbar, .avro-entry-frame scrollbar slider { min-height: 0; }
            .avro-gif-btn { font-size: 0.7em; font-weight: bold; padding: 0 6px; }
            .avro-gif-tile { padding: 0; border-radius: 8px; }
            .avro-gif-tile picture { border-radius: 8px; }
            .avro-thread row { background: transparent; border: none; box-shadow: none; padding: 0; margin: 2px 0; }
            .avro-meta { font-size: 0.8em; opacity: 0.7; }
            .avro-service { font-size: 0.85em; padding: 2px 10px; border-radius: 999px; background: alpha(currentColor, 0.06); }
            .avro-forward { font-size: 0.85em; font-style: italic; opacity: 0.8; }
            .avro-snippet { opacity: 0.7; }
            .avro-chats row { padding: 10px 10px; }
            .avro-badge { background: var(--accent-bg-color); color: var(--accent-fg-color); border-radius: 999px;
                          min-width: 8px; min-height: 8px; padding: 2px; font-size: 0.65em; font-weight: bold;
                          border: 2px solid var(--window-bg-color); margin: -2px; }
            .avro-badge.muted { background: alpha(currentColor, 0.35); color: var(--window-bg-color); }
            .navigation-sidebar row:selected .avro-badge { border-color: transparent; }
        ");
        gtk4::style_context_add_provider_for_display(&gtk4::gdk::Display::default().unwrap(), &css, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);

        let started = glib::real_time() / 1_000_000;
        Self { widget, win: win.clone(), client, events, on_closed: RefCell::new(None), st: Rc::default(), list, list_scroll, list_dirty: Cell::new(false),
            thread, thread_scroll, scroll_target: Cell::new(ScrollTarget::Free), textures: Rc::default(), animations: Rc::default(), paths: Rc::default(), avatars: Rc::default(),
            thread_title, entry, emoji_btn, send, attach, gif_btn, toast, banner, side_stack, content_stack, composer,
            settings: Rc::new(RefCell::new(crate::settings::Settings::load())), notifier: crate::notify::Notifier::new(), new_chat, started }
    }

    pub fn set_on_closed(&self, f: impl Fn() + 'static) { *self.on_closed.borrow_mut() = Some(Box::new(f)); }

    /// Wire everything up, replay the updates that arrived during login, and start pumping.
    pub fn start(self: &Rc<Self>, early: Vec<Update>) {
        {
            let me = Rc::downgrade(self);
            self.thread_scroll.vadjustment().connect_value_changed(move |_| { if let Some(me) = me.upgrade() { me.on_thread_scrolled(); } });
            let me = Rc::downgrade(self);
            // Row heights only become known after layout, so the range (`upper`) changes some time
            // after rows are appended. Apply the pending scroll target on every such change.
            self.thread_scroll.vadjustment().connect_changed(move |_| { if let Some(me) = me.upgrade() { me.apply_scroll_target(); } });
            // The chat list pages in more chats as you scroll towards its end.
            let me = Rc::downgrade(self);
            self.list_scroll.vadjustment().connect_value_changed(move |a| {
                if a.upper() - a.value() - a.page_size() < 400.0 && let Some(me) = me.upgrade() { me.load_more_chats(); }
            });
        }
        // notification click → focus window and open that chat
        if let Some(n) = &self.notifier {
            let me = Rc::downgrade(self);
            n.set_on_open(move |id, token| { if let Some(me) = me.upgrade() { me.focus_window(token); if let Ok(id) = id.parse() { me.jump_to(id); } } });
        }
        // Coming back to the window reads what arrived while it was in the background.
        let me = Rc::downgrade(self);
        self.win.connect_is_active_notify(move |w| { if w.is_active() && let Some(me) = me.upgrade() { me.mark_current_read(); } });
        let prefs = gtk4::gio::SimpleAction::new("preferences", None);
        let me = self.clone();
        prefs.connect_activate(move |_, _| me.show_preferences());
        if let Some(app) = self.win.application() { app.add_action(&prefs); }
        let me = self.clone();
        self.new_chat.connect_clicked(move |_| me.show_new_chat());
        // right-click menu actions (the popover itself is attached per row in `chat_row`)
        let group = gtk4::gio::SimpleActionGroup::new();
        let delete = gtk4::gio::SimpleAction::new("delete", Some(glib::VariantTy::INT64));
        let me = self.clone();
        delete.connect_activate(move |_, p| { if let Some(id) = p.and_then(|v| v.get::<i64>()) { me.confirm_delete(id); } });
        group.add_action(&delete);
        let read = gtk4::gio::SimpleAction::new("read", Some(glib::VariantTy::INT64));
        let me = self.clone();
        read.connect_activate(move |_, p| { if let Some(id) = p.and_then(|v| v.get::<i64>()) { me.mark_read(id); } });
        group.add_action(&read);
        self.list.insert_action_group("chat", Some(&group));
        // selection → open thread
        let me = self.clone();
        self.list.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let Some(id) = (unsafe { row.data::<i64>("chat-id").map(|p| *p.as_ref()) }) else { return };
            // rebuild_list re-selects the open chat's new row; that is not a new open.
            if me.st.borrow().current == Some(id) { return; }
            me.open(id);
        });
        // composer
        let me = self.clone();
        let keys = gtk4::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, state| {
            let enter = matches!(key, gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::ISO_Enter);
            if enter && state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) { me.send_current(); glib::Propagation::Stop } else { glib::Propagation::Proceed }
        });
        self.entry.add_controller(keys);
        let me = self.clone();
        self.send.connect_clicked(move |_| me.send_current());
        let me = self.clone();
        self.attach.connect_clicked(move |_| me.pick_and_send());
        self.build_gif_picker();
        // emoji picker: GTK's own chooser, anchored to the emoji button, inserting at the cursor
        let chooser = gtk4::EmojiChooser::new();
        chooser.set_parent(&self.emoji_btn);
        let entry = self.entry.clone();
        chooser.connect_emoji_picked(move |_, e| {
            let b = entry.buffer();
            b.delete_selection(true, true);
            b.insert_at_cursor(e);
        });
        let entry = self.entry.clone();
        chooser.connect_hide(move |_| { let entry = entry.clone(); glib::idle_add_local_once(move || { entry.grab_focus(); }); });
        self.emoji_btn.connect_clicked(move |_| chooser.popup());
        // typing indicator: tell the chat (throttled) while the user types
        let me = self.clone();
        let last = Rc::new(Cell::new(Instant::now() - Duration::from_secs(10)));
        self.entry.buffer().connect_changed(move |b| {
            if b.char_count() == 0 || last.get().elapsed() < Duration::from_secs(4) { return; }
            last.set(Instant::now());
            if let Some(id) = me.st.borrow().current { me.client.send_typing(id); }
        });
        // initial state + event pump
        for u in early { self.handle(u); }
        let me = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(u) = me.client.get_me().await { me.st.borrow_mut().me = u.id; me.queue_rebuild(); }
        });
        self.load_more_chats();
        let me = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(u) = me.events.recv().await { me.handle(u); }
        });
    }

    /// Ask TDLib for the next batch of the main chat list; they arrive as updates.
    fn load_more_chats(self: &Rc<Self>) {
        {
            let mut st = self.st.borrow_mut();
            if st.all_chats_loaded || st.loading_chats { return; }
            st.loading_chats = true;
        }
        let me = self.clone();
        glib::spawn_future_local(async move {
            let r = me.client.load_chats(PAGE).await;
            {
                let mut st = me.st.borrow_mut();
                st.loading_chats = false;
                match &r { Ok(more) => st.all_chats_loaded = !more, Err(_) => st.all_chats_loaded = true }
            }
            if let Err(e) = r { me.toast.add_toast(adw::Toast::new(&format!("Could not load chats: {e}"))); }
            me.rebuild_list();
            // A short first page may not fill the sidebar enough to scroll; keep going.
            let me2 = me.clone();
            glib::timeout_add_local_once(Duration::from_millis(100), move || {
                let a = me2.list_scroll.vadjustment();
                if a.upper() <= a.page_size() + 400.0 { me2.load_more_chats(); }
            });
        });
    }

    fn handle(self: &Rc<Self>, u: Update) {
        let current = self.st.borrow().current;
        let is_current = |id: i64| current == Some(id);
        match u {
            Update::NewChat { chat } => { self.st.borrow_mut().chats.insert(chat.id, *chat); self.queue_rebuild(); }
            Update::ChatTitle { chat_id, title } => {
                if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.title = title; }
                if is_current(chat_id) { self.refresh_header(); }
                self.queue_rebuild();
            }
            Update::ChatPhoto { chat_id, photo } => { if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.photo = photo; } self.queue_rebuild(); }
            Update::ChatLastMessage { chat_id, last_message, positions } => {
                if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.last_message = last_message.map(|m| *m); c.positions = positions; }
                self.queue_rebuild();
            }
            Update::ChatDraftMessage { chat_id, positions } => { if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.positions = positions; } self.queue_rebuild(); }
            Update::ChatPosition { chat_id, position } => { if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.set_position(position); } self.queue_rebuild(); }
            Update::ChatReadInbox { chat_id, last_read_inbox_message_id, unread_count } => {
                if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.last_read_inbox_message_id = last_read_inbox_message_id; c.unread_count = unread_count; }
                self.queue_rebuild();
            }
            Update::ChatReadOutbox { chat_id, last_read_outbox_message_id } => {
                if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.last_read_outbox_message_id = last_read_outbox_message_id; }
                if is_current(chat_id) { self.render_thread(self.hold_target()); }
            }
            Update::ChatIsMarkedAsUnread { chat_id, is_marked_as_unread } => { if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.is_marked_as_unread = is_marked_as_unread; } self.queue_rebuild(); }
            Update::ChatUnreadMentionCount { chat_id, unread_mention_count } => { if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.unread_mention_count = unread_mention_count; } }
            Update::ChatNotificationSettings { chat_id, notification_settings } => { if let Some(c) = self.st.borrow_mut().chats.get_mut(&chat_id) { c.notification_settings = notification_settings; } self.queue_rebuild(); }
            Update::ScopeNotificationSettings { scope, notification_settings } => {
                let i = match scope { NotificationScope::Private => 0, NotificationScope::Group => 1, NotificationScope::Channel => 2, NotificationScope::Other => return };
                self.st.borrow_mut().scope_mute[i] = notification_settings.mute_for;
                self.queue_rebuild();
            }
            Update::NewMessage { message } => { self.maybe_notify(&message); self.push_message(*message); }
            Update::MessageSendSucceeded { message, old_message_id } => self.replace_message(old_message_id, *message),
            Update::MessageSendFailed { message, old_message_id, error } => {
                self.toast.add_toast(adw::Toast::new(&format!("Message not sent: {}", error.message)));
                self.replace_message(old_message_id, *message);
            }
            Update::MessageContent { chat_id, message_id, new_content } => {
                if let Some(m) = self.st.borrow_mut().messages.get_mut(&chat_id).and_then(|l| l.iter_mut().find(|m| m.id == message_id)) { m.content = new_content; }
                if is_current(chat_id) { self.render_thread(self.hold_target()); }
            }
            Update::MessageEdited { chat_id, message_id, edit_date } => {
                if let Some(m) = self.st.borrow_mut().messages.get_mut(&chat_id).and_then(|l| l.iter_mut().find(|m| m.id == message_id)) { m.edit_date = edit_date; }
            }
            Update::DeleteMessages { chat_id, message_ids, is_permanent } => {
                if !is_permanent { return; }
                if let Some(l) = self.st.borrow_mut().messages.get_mut(&chat_id) { l.retain(|m| !message_ids.contains(&m.id)); }
                if is_current(chat_id) { self.render_thread(self.hold_target()); }
            }
            Update::User { user } => {
                let id = user.id;
                self.st.borrow_mut().users.insert(id, *user);
                if self.current_user() == Some(id) { self.refresh_header(); }
            }
            Update::UserStatus { user_id, status } => {
                if let Some(u) = self.st.borrow_mut().users.get_mut(&user_id) { u.status = status; }
                if self.current_user() == Some(user_id) { self.refresh_header(); }
            }
            Update::BasicGroup { basic_group } => { self.st.borrow_mut().basic_groups.insert(basic_group.id, basic_group); self.refresh_header(); }
            Update::Supergroup { supergroup } => { self.st.borrow_mut().supergroups.insert(supergroup.id, supergroup); self.refresh_header(); }
            Update::ChatAction { chat_id, sender_id, action } => {
                let who = self.st.borrow().sender_name(&sender_id, true);
                match action_text(&action.t) {
                    Some(what) => { self.st.borrow_mut().actions.insert(chat_id, (who, what, Instant::now())); }
                    None => { self.st.borrow_mut().actions.remove(&chat_id); }
                }
                if is_current(chat_id) {
                    self.refresh_header();
                    // Actions expire after ~6 s without a repeat; redraw then.
                    let me = Rc::downgrade(self);
                    glib::timeout_add_local_once(Duration::from_millis(6100), move || { if let Some(me) = me.upgrade() { me.refresh_header(); } });
                }
            }
            Update::ConnectionState { state } => match state.t.as_str() {
                "connectionStateWaitingForNetwork" => { self.banner.set_title("Waiting for network…"); self.banner.set_revealed(true); }
                "connectionStateConnectingToProxy" => { self.banner.set_title("Connecting to proxy…"); self.banner.set_revealed(true); }
                "connectionStateConnecting" => { self.banner.set_title("Connecting to Telegram…"); self.banner.set_revealed(true); }
                _ => self.banner.set_revealed(false),
            },
            Update::AuthorizationState { authorization_state } => match authorization_state {
                AuthState::LoggingOut => { self.banner.set_title("Logging out…"); self.banner.set_revealed(true); }
                AuthState::Closed => {
                    // Take it: a closed client sends nothing more, and the callback rebuilds the app.
                    let cb = self.on_closed.borrow_mut().take();
                    if let Some(cb) = cb { cb(); }
                }
                _ => {}
            },
            Update::File | Update::Other => {}
        }
    }

    fn current_user(&self) -> Option<i64> {
        let st = self.st.borrow();
        st.current.and_then(|id| st.chats.get(&id)).and_then(|c| c.kind.user_id())
    }

    fn refresh_header(&self) {
        let st = self.st.borrow();
        let Some(c) = st.current.and_then(|id| st.chats.get(&id)) else { return };
        self.thread_title.set_title(&st.chat_title(c));
        self.thread_title.set_subtitle(&st.subtitle(c));
    }

    /// Hold the reader's place across a re-render (or follow the end if they're there).
    fn hold_target(&self) -> ScrollTarget {
        let adj = self.thread_scroll.vadjustment();
        if self.at_bottom() || self.scroll_target.get() == ScrollTarget::Bottom { ScrollTarget::Bottom } else { ScrollTarget::FromBottom(adj.upper() - adj.value()) }
    }

    /// Rebuild the sidebar once this main-loop turn's updates are in.
    fn queue_rebuild(self: &Rc<Self>) {
        if self.list_dirty.replace(true) { return; }
        let me = Rc::downgrade(self);
        glib::idle_add_local_once(move || { if let Some(me) = me.upgrade() { me.rebuild_list(); } });
    }

    fn rebuild_list(self: &Rc<Self>) {
        self.list_dirty.set(false);
        let (selected, order) = { let st = self.st.borrow(); (st.current, st.ordered()) };
        let adj = self.list_scroll.vadjustment();
        let scroll = adj.value();
        while let Some(r) = self.list.row_at_index(0) { self.list.remove(&r); }
        let mut rows = HashMap::new();
        {
            let st = self.st.borrow();
            for id in &order {
                let Some(c) = st.chats.get(id) else { continue };
                let row = self.chat_row(c, &st);
                self.list.append(&row);
                rows.insert(*id, row);
            }
        }
        // Select outside the borrow: row-selected runs its handler synchronously.
        if let Some(row) = selected.and_then(|id| rows.get(&id)) { self.list.select_row(Some(row)); }
        self.st.borrow_mut().rows = rows;
        adj.set_value(scroll);
        let loading = { let st = self.st.borrow(); st.loading_chats && !st.all_chats_loaded };
        self.side_stack.set_visible_child_name(if !order.is_empty() { "list" } else if loading { "loading" } else { "empty" });
    }

    /// The texture for a profile photo, if it's ready. Otherwise starts a download (once) and
    /// calls `on_ready` when it lands, so the caller can paint it in place.
    fn avatar(self: &Rc<Self>, file: &File, on_ready: impl Fn(&gtk4::gdk::Texture) + 'static) -> Option<gtk4::gdk::Texture> {
        if file.id == 0 { return None; }
        if let Some(t) = self.avatars.borrow().get(&file.id) { return t.clone(); }
        if let Some(path) = file.ready_path() && let Ok(t) = gtk4::gdk::Texture::from_filename(path) {
            self.avatars.borrow_mut().insert(file.id, Some(t.clone()));
            return Some(t);
        }
        self.avatars.borrow_mut().insert(file.id, None);
        let (me, id) = (self.clone(), file.id);
        glib::spawn_future_local(async move {
            match me.client.download(id, 1).await {
                Ok(f) => match f.ready_path().map(gtk4::gdk::Texture::from_filename) {
                    Some(Ok(t)) => { me.avatars.borrow_mut().insert(id, Some(t.clone())); on_ready(&t); }
                    Some(Err(e)) => tracing::debug!("avatar {id}: {e}"),
                    None => {}
                },
                Err(e) => tracing::debug!("avatar {id}: {e}"),
            }
        });
        None
    }

    fn avatar_widget(self: &Rc<Self>, size: i32, name: &str, file: Option<&File>) -> adw::Avatar {
        let av = adw::Avatar::new(size, Some(name), true);
        if let Some(f) = file {
            let weak = av.downgrade();
            if let Some(t) = self.avatar(f, move |t| { if let Some(av) = weak.upgrade() { av.set_custom_image(Some(t)); } }) { av.set_custom_image(Some(&t)); }
        }
        av
    }

    /// Drop a chat from the list and close it if it was the one open.
    fn forget_chat(&self, id: i64) {
        let mut st = self.st.borrow_mut();
        st.messages.remove(&id);
        st.exhausted.remove(&id);
        if st.current == Some(id) {
            st.current = None;
            drop(st);
            self.client.close_chat(id);
            while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
            self.thread_title.set_title("");
            self.thread_title.set_subtitle("");
            self.composer.set_visible(false);
            self.content_stack.set_visible_child_name("empty");
            self.widget.set_show_content(false);
        }
    }

    /// Right-click → "Delete chat" / "Leave group": confirm, then ask Telegram.
    fn confirm_delete(self: &Rc<Self>, id: i64) {
        let Some(chat) = self.st.borrow().chats.get(&id).cloned() else { return };
        let name = self.st.borrow().chat_title(&chat);
        let (heading, body, verb) = match chat.kind {
            ChatType::Supergroup { is_channel: true, .. } => ("Leave channel?", format!("You'll stop getting posts from “{name}”."), "Leave"),
            ChatType::Supergroup { .. } | ChatType::BasicGroup { .. } => ("Leave group?", format!("You'll leave “{name}” and it will disappear from your chats."), "Leave"),
            _ => ("Delete chat?", format!("Your history with “{name}” will be deleted for you. They keep their copy."), "Delete"),
        };
        let dlg = adw::AlertDialog::new(Some(heading), Some(&body));
        dlg.add_responses(&[("cancel", "Cancel"), ("delete", verb)]);
        dlg.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dlg.set_default_response(Some("cancel"));
        dlg.set_close_response("cancel");
        let me = self.clone();
        dlg.choose(Some(&self.win), None::<&gtk4::gio::Cancellable>, move |resp| {
            if resp != "delete" { return }
            let me = me.clone();
            let chat = chat.clone();
            glib::spawn_future_local(async move {
                match me.client.delete_chat(&chat).await {
                    // TDLib follows up with updateChatPosition(order 0), which drops the row.
                    Ok(()) => me.forget_chat(chat.id),
                    Err(e) => me.toast.add_toast(adw::Toast::new(&format!("Could not remove chat: {e}"))),
                }
            });
        });
    }

    /// Right-click → "Mark as read".
    fn mark_read(self: &Rc<Self>, id: i64) {
        let Some(chat) = self.st.borrow().chats.get(&id).cloned() else { return };
        let me = self.clone();
        glib::spawn_future_local(async move {
            if let Some(m) = &chat.last_message && let Err(e) = me.client.view_messages(chat.id, &[m.id]).await { tracing::warn!("view messages: {e}"); }
            if chat.is_marked_as_unread && let Err(e) = me.client.set_marked_unread(chat.id, false).await { tracing::warn!("mark read: {e}"); }
        });
    }

    /// Read the open chat up to its newest message, if the window is in front.
    fn mark_current_read(self: &Rc<Self>) {
        let target = {
            let st = self.st.borrow();
            st.current.and_then(|id| st.chats.get(&id)).filter(|c| st.unread(c)).map(|c| c.id)
        };
        if let Some(id) = target && self.win.is_active() { self.mark_read(id); }
    }

    fn open(self: &Rc<Self>, id: i64) {
        if !self.st.borrow().chats.contains_key(&id) { return; }
        let previous = self.st.borrow_mut().current.replace(id);
        if let Some(p) = previous && p != id {
            self.client.close_chat(p);
            self.animations.borrow_mut().clear();
        }
        self.client.open_chat(id);
        self.refresh_header();
        self.widget.set_show_content(true);
        self.composer.set_visible(true);
        self.entry.grab_focus();
        let loaded = self.st.borrow().messages.contains_key(&id);
        if loaded { self.render_thread(ScrollTarget::Bottom); } else {
            while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
            self.content_stack.set_visible_child_name("loading");
            let me = self.clone();
            glib::spawn_future_local(async move {
                match me.client.history(id, 0, PAGE).await {
                    Ok((mut msgs, done)) => {
                        msgs.reverse();
                        {
                            let mut st = me.st.borrow_mut();
                            // Keep anything that arrived live while the page was in flight.
                            let live = st.messages.remove(&id).unwrap_or_default();
                            for m in live { if !msgs.iter().any(|x| x.id == m.id) { msgs.push(m); } }
                            msgs.sort_by_key(|m| m.id);
                            st.messages.insert(id, msgs);
                            if done { st.exhausted.insert(id); }
                        }
                        if me.st.borrow().current == Some(id) { me.render_thread(ScrollTarget::Bottom); }
                    }
                    Err(e) => {
                        if me.st.borrow().current == Some(id) { me.content_stack.set_visible_child_name("thread"); }
                        me.toast.add_toast(adw::Toast::new(&format!("Could not load messages: {e}")));
                    }
                }
            });
        }
        self.mark_current_read();
    }

    fn push_message(self: &Rc<Self>, m: Message) {
        let chat_id = m.chat_id;
        let viewing = self.st.borrow().current == Some(chat_id);
        // Seen as it arrives: read it now (the unread count may not have caught up yet).
        if viewing && !m.is_outgoing && self.win.is_active() {
            let (c, id) = (self.client.clone(), m.id);
            glib::spawn_future_local(async move { if let Err(e) = c.view_messages(chat_id, &[id]).await { tracing::debug!("view {id}: {e}"); } });
        }
        // Decide before touching the list: follow the chat if the user is at its end or just
        // sent something; otherwise hold their place while they read older messages.
        let target = if m.is_outgoing { ScrollTarget::Bottom } else { self.hold_target() };
        {
            let mut st = self.st.borrow_mut();
            // Only chats whose history is loaded keep a message list; others load on open.
            let Some(list) = st.messages.get_mut(&chat_id) else { return };
            match list.binary_search_by_key(&m.id, |x| x.id) {
                Ok(i) => list[i] = m,
                Err(i) => list.insert(i, m),
            }
        }
        if viewing { self.render_thread(target); }
    }

    /// A sent message got its permanent id (or failed): swap it in for the temporary one.
    fn replace_message(self: &Rc<Self>, old_id: i64, m: Message) {
        let chat_id = m.chat_id;
        if let Some(list) = self.st.borrow_mut().messages.get_mut(&chat_id) {
            list.retain(|x| x.id != old_id && x.id != m.id);
            let i = list.binary_search_by_key(&m.id, |x| x.id).unwrap_or_else(|i| i);
            list.insert(i, m);
        }
        if self.st.borrow().current == Some(chat_id) { self.render_thread(self.hold_target()); }
    }

    fn render_thread(self: &Rc<Self>, target: ScrollTarget) {
        self.scroll_target.set(target);
        while let Some(r) = self.thread.row_at_index(0) { self.thread.remove(&r); }
        let (msgs, chat, more) = {
            let st = self.st.borrow();
            let Some(cur) = st.current else { return };
            let Some(msgs) = st.messages.get(&cur) else { return };
            (msgs.clone(), st.chats.get(&cur).cloned().unwrap_or_default(), !st.exhausted.contains(&cur))
        };
        self.content_stack.set_visible_child_name("thread");
        if more {
            let spinner = adw::Spinner::builder().width_request(24).height_request(24).margin_top(8).margin_bottom(8).halign(gtk4::Align::Center).build();
            self.thread.append(&gtk4::ListBoxRow::builder().child(&spinner).activatable(false).selectable(false).build());
        }
        let mut prev: Option<&Message> = None;
        for m in &msgs {
            // Name each run of messages once, as Telegram does.
            let first_of_run = prev.is_none_or(|p| sender_key(&p.sender_id) != sender_key(&m.sender_id) || p.content.is_service() || m.date - p.date > 600);
            self.thread.append(&self.bubble(m, &chat, first_of_run));
            prev = Some(m);
        }
        self.apply_scroll_target();
    }

    /// Move the thread to the pending target. Runs after every range change, so the position
    /// holds while rows are laid out and images swap in; a user scroll releases it.
    fn apply_scroll_target(&self) {
        let adj = self.thread_scroll.vadjustment();
        let value = match self.scroll_target.get() {
            ScrollTarget::Free => return,
            ScrollTarget::Bottom => adj.upper() - adj.page_size(),
            ScrollTarget::FromBottom(d) => adj.upper() - d,
        };
        adj.set_value(value.max(0.0));
    }

    /// True when the thread is scrolled to (or within a few lines of) its end.
    fn at_bottom(&self) -> bool {
        let adj = self.thread_scroll.vadjustment();
        adj.upper() - adj.value() - adj.page_size() < 48.0
    }

    /// A value change that leaves the thread where the target says it should be is one of our
    /// own (or GTK clamping after rows were removed); anything else is the user scrolling away.
    fn on_thread_scrolled(self: &Rc<Self>) {
        let adj = self.thread_scroll.vadjustment();
        let holds = match self.scroll_target.get() {
            ScrollTarget::Free => false,
            ScrollTarget::Bottom => self.at_bottom(),
            ScrollTarget::FromBottom(d) => (adj.upper() - adj.value() - d).abs() < 1.0,
        };
        if !holds { self.scroll_target.set(ScrollTarget::Free); }
        self.maybe_load_older();
    }

    /// Called on every scroll: when the top of the thread comes within a screen of view, fetch
    /// the next page of older messages and prepend them without moving what the user is looking at.
    fn maybe_load_older(self: &Rc<Self>) {
        let adj = self.thread_scroll.vadjustment();
        if adj.value() > adj.page_size() { return; }
        let (id, oldest) = {
            let mut st = self.st.borrow_mut();
            let Some(id) = st.current else { return };
            if st.exhausted.contains(&id) { return; }
            let Some(oldest) = st.messages.get(&id).and_then(|l| l.first()).map(|m| m.id) else { return };
            if !st.loading_older.insert(id) { return; }
            (id, oldest)
        };
        let me = self.clone();
        glib::spawn_future_local(async move {
            let r = me.client.history(id, oldest, PAGE).await;
            me.st.borrow_mut().loading_older.remove(&id);
            match r {
                Ok((page, done)) => {
                    {
                        let mut st = me.st.borrow_mut();
                        if done { st.exhausted.insert(id); }
                        let list = st.messages.entry(id).or_default();
                        for m in page { if let Err(i) = list.binary_search_by_key(&m.id, |x| x.id) { list.insert(i, m); } }
                    }
                    if me.st.borrow().current != Some(id) { return; }
                    // Preserve the visual position: keep the distance from the bottom constant.
                    let adj = me.thread_scroll.vadjustment();
                    me.render_thread(ScrollTarget::FromBottom(adj.upper() - adj.value()));
                    // If the page was short enough that the top is still visible, keep going.
                    if !done {
                        let me2 = me.clone();
                        glib::timeout_add_local_once(Duration::from_millis(100), move || me2.maybe_load_older());
                    }
                }
                Err(e) => me.toast.add_toast(adw::Toast::new(&format!("Could not load older messages: {e}"))),
            }
        });
    }

    fn entry_text(&self) -> String {
        let b = self.entry.buffer();
        b.text(&b.start_iter(), &b.end_iter(), false).to_string()
    }

    fn send_current(self: &Rc<Self>) {
        let text = self.entry_text();
        if text.trim().is_empty() { return; }
        let Some(id) = self.st.borrow().current else { return };
        self.entry.buffer().set_text("");
        let me = self.clone();
        glib::spawn_future_local(async move {
            match me.client.send_text(id, text.trim_end()).await {
                // The pending copy shows at once; updateMessageSendSucceeded swaps in the real one.
                Ok(m) => me.push_message(m),
                Err(e) => me.toast.add_toast(adw::Toast::new(&format!("Send failed: {}", super::login::explain(&e)))),
            }
        });
    }

    /// Send a local file to the open chat, with the composer text as its caption.
    fn send_path(self: &Rc<Self>, path: std::path::PathBuf, kind: FileKind) {
        let Some(id) = self.st.borrow().current else { return };
        let caption = self.entry_text();
        self.entry.buffer().set_text("");
        let me = self.clone();
        glib::spawn_future_local(async move {
            match me.client.send_file(id, &path, kind, caption.trim()).await {
                Ok(m) => me.push_message(m),
                Err(e) => me.toast.add_toast(adw::Toast::new(&format!("Send failed: {}", super::login::explain(&e)))),
            }
        });
    }
}

impl ChatsView {
    /// GIF search popover on the composer's GIF button: a debounced DuckDuckGo search filling a
    /// grid of thumbnails; clicking one downloads the GIF and sends it as a Telegram animation.
    fn build_gif_picker(self: &Rc<Self>) {
        let pop = gtk4::Popover::builder().width_request(372).build();
        pop.set_parent(&self.gif_btn);
        let search = gtk4::SearchEntry::builder().placeholder_text("Search GIFs").build();
        let grid = gtk4::FlowBox::builder().selection_mode(gtk4::SelectionMode::None).min_children_per_line(3).max_children_per_line(3)
            .homogeneous(true).row_spacing(4).column_spacing(4).valign(gtk4::Align::Start).build();
        let scroll = gtk4::ScrolledWindow::builder().child(&grid).hscrollbar_policy(gtk4::PolicyType::Never).height_request(400).build();
        let status = gtk4::Label::builder().label("Type to search").css_classes(["dim-label"]).margin_top(12).margin_bottom(12).build();
        let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).build();
        col.append(&search); col.append(&status); col.append(&scroll);
        pop.set_child(Some(&col));
        let s = search.clone();
        pop.connect_show(move |_| { s.grab_focus(); });
        let p = pop.clone();
        self.gif_btn.connect_clicked(move |_| p.popup());

        // Each keystroke bumps the generation; a query only runs (and its results only land) if it
        // is still the latest one 350ms later.
        let generation = Rc::new(Cell::new(0u64));
        let me = self.clone();
        let grid2 = grid.clone();
        search.connect_search_changed(move |e| {
            let grid = &grid2;
            let query = e.text().to_string();
            generation.set(generation.get() + 1);
            let my_gen = generation.get();
            if query.trim().is_empty() { status.set_label("Type to search"); status.set_visible(true); Self::clear_flow(grid); return; }
            let (generation, status, grid, me) = (generation.clone(), status.clone(), grid.clone(), me.clone());
            glib::timeout_add_local_once(Duration::from_millis(350), move || {
                if generation.get() != my_gen { return; }
                status.set_label("Searching…"); status.set_visible(true);
                let (tx, rx) = async_channel::bounded(1);
                crate::rt::spawn(async move { let _ = tx.send(crate::gif::search(&query, 0).await).await; });
                glib::spawn_future_local(async move {
                    let Ok(res) = rx.recv().await else { return };
                    if generation.get() != my_gen { return; }
                    Self::clear_flow(&grid);
                    match res {
                        Ok(gifs) if gifs.is_empty() => status.set_label("No GIFs found"),
                        Ok(gifs) => { status.set_visible(false); for g in gifs.into_iter().take(45) { grid.append(&me.gif_tile(g)); } }
                        Err(e) => status.set_label(&format!("{e:#}")),
                    }
                });
            });
        });
        let p = pop.clone();
        // Popover clicks route via the tile's stored URL; close the popover once a GIF is chosen.
        let me = self.clone();
        grid.connect_child_activated(move |_, child| {
            let Some(url) = (unsafe { child.child().and_then(|c| c.data::<String>("gif-url")).map(|u| u.as_ref().clone()) }) else { return };
            p.popdown();
            me.send_gif(url);
        });
    }

    fn clear_flow(grid: &gtk4::FlowBox) {
        while let Some(c) = grid.first_child() { grid.remove(&c); }
    }

    /// A grid tile: a fixed-size picture whose thumbnail loads in the background.
    fn gif_tile(self: &Rc<Self>, g: crate::gif::Gif) -> gtk4::Widget {
        let pic = gtk4::Picture::builder().content_fit(gtk4::ContentFit::Cover).can_shrink(true).overflow(gtk4::Overflow::Hidden).build();
        pic.set_size_request(112, 112);
        let btn = gtk4::Button::builder().child(&pic).css_classes(["flat", "avro-gif-tile"]).tooltip_text(&g.url).build();
        unsafe { btn.set_data("gif-url", g.url.clone()); }
        let thumb = g.thumbnail.clone();
        let (tx, rx) = async_channel::bounded(1);
        crate::rt::spawn(async move { let _ = tx.send(crate::gif::thumbnail(&thumb).await).await; });
        let weak = pic.downgrade();
        glib::spawn_future_local(async move {
            let Ok(Ok(bytes)) = rx.recv().await else { return };
            let Some(pic) = weak.upgrade() else { return };
            if let Ok(tex) = gtk4::gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)) { pic.set_paintable(Some(&tex)); }
        });
        // FlowBox activates the child on click; the button itself just needs to not swallow it.
        let btn2 = btn.clone();
        btn.connect_clicked(move |_| { if let Some(child) = btn2.parent().and_downcast::<gtk4::FlowBoxChild>() { child.activate(); } });
        btn.upcast()
    }

    /// Download a GIF by URL into the cache and send it; Telegram converts it to an animation.
    fn send_gif(self: &Rc<Self>, url: String) {
        if self.st.borrow().current.is_none() { return; }
        self.toast.add_toast(adw::Toast::new("Sending GIF…"));
        let (tx, rx) = async_channel::bounded(1);
        crate::rt::spawn(async move {
            let r = async {
                let data = crate::gif::download(&url).await?;
                let dir = directories::ProjectDirs::from("dev", "turbinebmw", "avro").ok_or_else(|| anyhow::anyhow!("no cache dir"))?.cache_dir().join("gifs");
                std::fs::create_dir_all(&dir)?;
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                url.hash(&mut h);
                let path = dir.join(format!("{:016x}.gif", h.finish()));
                std::fs::write(&path, data)?;
                anyhow::Ok(path)
            }.await;
            let _ = tx.send(r).await;
        });
        let me = self.clone();
        glib::spawn_future_local(async move {
            match rx.recv().await {
                Ok(Ok(path)) => me.send_path(path, FileKind::Animation),
                Ok(Err(e)) => me.toast.add_toast(adw::Toast::new(&format!("GIF send failed: {e:#}"))),
                Err(_) => {}
            }
        });
    }

    /// Open a file chooser and send the chosen file to the open chat.
    fn pick_and_send(self: &Rc<Self>) {
        if self.st.borrow().current.is_none() { return; }
        let dialog = gtk4::FileDialog::builder().title("Send a file").build();
        let me = self.clone();
        dialog.open(Some(&self.win), None::<&gtk4::gio::Cancellable>, move |res| {
            let Ok(file) = res else { return };
            let Some(path) = file.path() else { return };
            let head = std::fs::File::open(&path).and_then(|mut f| { use std::io::Read; let mut b = vec![0; 4096]; let n = f.read(&mut b)?; b.truncate(n); Ok(b) }).unwrap_or_default();
            let mime = gtk4::gio::content_type_get_mime_type(&gtk4::gio::content_type_guess(Some(&path), Some(head.as_slice())).0).map(|m| m.to_string()).unwrap_or_default();
            me.send_path(path, FileKind::for_mime(&mime));
        });
    }

    /// Desktop notification for an incoming message, unless it's ours, backlog, muted, or the
    /// chat is already open in a focused window.
    fn maybe_notify(self: &Rc<Self>, m: &Message) {
        if m.is_outgoing || m.date < self.started - 60 { return; }
        let st = self.st.borrow();
        if self.win.is_active() && st.current == Some(m.chat_id) { return; }
        let Some(chat) = st.chats.get(&m.chat_id) else { return };
        if st.muted(chat) { return; }
        let mut title = st.chat_title(chat);
        let text = m.content.summary();
        // In a group, prefix the sender so you know who spoke.
        let body = if chat.kind.is_group() { format!("{}: {text}", st.sender_name(&m.sender_id, true)) } else { text.clone() };
        drop(st);
        let otp = crate::notify::detect_otp(&text);
        if let Some(code) = &otp { title = format!("{code} · {title}"); }
        let Some(n) = &self.notifier else { return };
        n.send(crate::notify::Notice { conversation_id: m.chat_id.to_string(), title, body, otp }, &self.settings.borrow().notification_sound);
    }

    /// Bring the window to the front. On Wayland a compositor only grants focus to a window
    /// holding a fresh xdg-activation token, which the notification daemon supplies with the
    /// click; without one, fall back to asking the compositor directly where we know how.
    fn focus_window(&self, token: Option<String>) {
        match token {
            Some(t) => { self.win.set_startup_id(&t); self.win.present(); }
            None => {
                self.win.present();
                let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
                if desktop.contains("hyprland") {
                    let _ = std::process::Command::new("hyprctl").args(["dispatch", "focuswindow", "class:dev.turbinebmw.Avro"]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
                }
            }
        }
    }

    fn jump_to(self: &Rc<Self>, id: i64) {
        let row = self.st.borrow().rows.get(&id).cloned();
        match row { Some(row) => self.list.select_row(Some(&row)), None => self.open(id) }
    }

    /// A picker over your Telegram contacts, plus rows for a typed phone number or @username.
    fn show_new_chat(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder().title("New chat").content_width(400).content_height(560).build();
        let search = gtk4::SearchEntry::builder().placeholder_text("Name, @username or phone number").margin_start(12).margin_end(12).margin_bottom(6).build();
        let list = gtk4::ListBox::builder().selection_mode(gtk4::SelectionMode::None).css_classes(["navigation-sidebar"]).build();
        let scroll = gtk4::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk4::PolicyType::Never).vexpand(true).build();
        let empty = adw::StatusPage::builder().icon_name("system-search-symbolic").title("No matches").description("Type an @username or a phone number to find someone new.").build();
        let pages = stack(&[("loading", &loading_page("Loading contacts…")), ("list", scroll.upcast_ref()), ("empty", empty.upcast_ref())]);
        let col = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        col.append(&search); col.append(&pages);
        let tv = adw::ToolbarView::new();
        tv.add_top_bar(&adw::HeaderBar::new());
        tv.set_content(Some(&col));
        dialog.set_child(Some(&tv));

        let contacts: Rc<RefCell<Option<Vec<i64>>>> = Rc::default();
        // Rebuild the rows for the current query. Each row carries what to open: "user:ID",
        // "phone:+NUMBER" or "username:NAME".
        let me_w = Rc::downgrade(self);
        let (list2, pages2, contacts2) = (list.clone(), pages.clone(), contacts.clone());
        let fill: Rc<dyn Fn(&str)> = Rc::new(move |query: &str| {
            let Some(me) = me_w.upgrade() else { return };
            let Some(ids) = contacts2.borrow().clone() else { pages2.set_visible_child_name("loading"); return };
            while let Some(r) = list2.row_at_index(0) { list2.remove(&r); }
            let q = query.trim().to_lowercase();
            let mut n = 0;
            let mut special = |title: String, subtitle: &str, icon: &str, target: String| {
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&title)).subtitle(subtitle).activatable(true).build();
                let img = gtk4::Image::from_icon_name(icon); img.set_pixel_size(24);
                row.add_prefix(&img);
                unsafe { row.set_data("target", target); }
                list2.append(&row); n += 1;
            };
            if let Some(num) = normalise_number(query) { special(format!("Message {num}"), "Find this number on Telegram", "phone-symbolic", format!("phone:{num}")); }
            if let Some(u) = as_username(query) { special(format!("Open @{u}"), "Look up this username", "avatar-default-symbolic", format!("username:{u}")); }
            let st = me.st.borrow();
            let mut users: Vec<&User> = ids.iter().filter_map(|id| st.users.get(id)).collect();
            users.sort_by_key(|u| u.full_name().to_lowercase());
            let photos: Vec<(i64, String, Option<File>, String)> = users.iter().filter(|u| {
                q.is_empty() || u.full_name().to_lowercase().contains(&q) || u.username().is_some_and(|n| n.to_lowercase().contains(q.trim_start_matches('@')))
            }).map(|u| (u.id, u.full_name(), u.profile_photo.as_ref().map(|p| p.small.clone()),
                u.username().map(|n| format!("@{n}")).unwrap_or_else(|| presence(u)))).collect();
            drop(st);
            for (id, name, photo, sub) in photos {
                let row = adw::ActionRow::builder().title(glib::markup_escape_text(&name)).subtitle(glib::markup_escape_text(&sub)).activatable(true).build();
                row.add_prefix(&me.avatar_widget(32, &name, photo.as_ref()));
                unsafe { row.set_data("target", format!("user:{id}")); }
                list2.append(&row); n += 1;
            }
            pages2.set_visible_child_name(if n == 0 { "empty" } else { "list" });
        });

        let f = fill.clone();
        search.connect_search_changed(move |e| f(&e.text()));
        let (me, d) = (self.clone(), dialog.clone());
        list.connect_row_activated(move |_, row| {
            let Some(target) = (unsafe { row.data::<String>("target").map(|p| p.as_ref().clone()) }) else { return };
            d.close();
            me.start_chat(target);
        });
        // Enter in the search box takes the first row (the typed number, or the best match).
        let l = list.clone();
        search.connect_activate(move |_| { if let Some(row) = l.row_at_index(0) { l.emit_by_name::<()>("row-activated", &[&row]); } });

        fill("");
        let (me, f, s) = (self.clone(), fill.clone(), search.clone());
        glib::spawn_future_local(async move {
            let ids = match me.client.contacts().await {
                Ok(ids) => ids,
                Err(e) => { me.toast.add_toast(adw::Toast::new(&format!("Could not load contacts: {e}"))); Vec::new() }
            };
            // TDLib announces each contact with updateUser before replying, but be safe.
            for id in &ids {
                if !me.st.borrow().users.contains_key(id) && let Ok(u) = me.client.get_user(*id).await { me.st.borrow_mut().users.insert(u.id, u); }
            }
            *contacts.borrow_mut() = Some(ids);
            f(&s.text());
        });
        dialog.present(Some(&self.win));
        search.grab_focus();
    }

    /// Resolve a picker row to a chat (creating the private chat if needed), then open it.
    fn start_chat(self: &Rc<Self>, target: String) {
        let me = self.clone();
        glib::spawn_future_local(async move {
            let c = &me.client;
            let r = match target.split_once(':') {
                Some(("user", id)) => match id.parse() { Ok(id) => c.create_private_chat(id).await, Err(_) => return },
                Some(("phone", num)) => match c.search_user_by_phone(num).await {
                    Ok(u) => c.create_private_chat(u.id).await,
                    Err(_) => { me.toast.add_toast(adw::Toast::new(&format!("{num} isn't on Telegram, or hides their number"))); return }
                },
                Some(("username", name)) => match c.search_public_chat(name).await {
                    Ok(chat) => Ok(chat),
                    Err(_) => { me.toast.add_toast(adw::Toast::new(&format!("No one is called @{name}"))); return }
                },
                _ => return,
            };
            match r {
                Ok(chat) => {
                    let id = chat.id;
                    me.st.borrow_mut().chats.entry(id).or_insert(chat);
                    me.rebuild_list();
                    me.jump_to(id);
                }
                Err(e) => me.toast.add_toast(adw::Toast::new(&format!("Could not start chat: {e}"))),
            }
        });
    }

    fn show_preferences(self: &Rc<Self>) {
        use crate::settings::Sound;
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::new();
        let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
        let follow = adw::SwitchRow::builder().title("Follow Omarchy theme")
            .subtitle(crate::omarchy::theme_name().map(|name| format!("Match the desktop colors · {name}"))
                .unwrap_or_else(|| "Match the desktop colors".into()))
            .active(self.settings.borrow().follow_omarchy_theme).build();
        let me = self.clone();
        follow.connect_active_notify(move |row| {
            let mut settings = me.settings.borrow_mut();
            settings.follow_omarchy_theme = row.is_active();
            settings.save();
            crate::omarchy::set_follow(row.is_active());
        });
        appearance.add(&follow);
        appearance.set_visible(crate::omarchy::detected());
        page.add(&appearance);
        let group = adw::PreferencesGroup::builder().title("Notifications")
            .description("The sound is requested from your notification daemon, which decides whether to play it — so do-not-disturb rules in your shell still apply. Chats muted in Telegram stay silent.").build();
        let choices = gtk4::StringList::new(&["System default", "Custom file", "None"]);
        let sound_row = adw::ComboRow::builder().title("Sound").model(&choices).build();
        let file_row = adw::ActionRow::builder().title("Sound file").activatable(true).build();
        file_row.add_suffix(&gtk4::Image::from_icon_name("folder-open-symbolic"));
        let current = self.settings.borrow().notification_sound.clone();
        sound_row.set_selected(match &current { Sound::SystemDefault => 0, Sound::File(_) => 1, Sound::None => 2 });
        if let Sound::File(p) = &current { file_row.set_subtitle(&p.to_string_lossy()); }
        file_row.set_visible(matches!(current, Sound::File(_)));
        let (me, fr) = (self.clone(), file_row.clone());
        sound_row.connect_selected_notify(move |r| {
            let mut s = me.settings.borrow_mut();
            s.notification_sound = match r.selected() {
                0 => Sound::SystemDefault,
                1 => match &s.notification_sound { Sound::File(p) => Sound::File(p.clone()), _ => Sound::File(std::path::PathBuf::new()) },
                _ => Sound::None,
            };
            fr.set_visible(r.selected() == 1);
            s.save();
        });
        let (me, win) = (self.clone(), self.win.clone());
        file_row.connect_activated(move |row| {
            let filter = gtk4::FileFilter::new(); filter.set_name(Some("Audio")); filter.add_mime_type("audio/*");
            let filters = gtk4::gio::ListStore::new::<gtk4::FileFilter>(); filters.append(&filter);
            let chooser = gtk4::FileDialog::builder().title("Choose a notification sound").default_filter(&filter).filters(&filters).modal(true).build();
            let (me, row) = (me.clone(), row.clone());
            chooser.open(Some(&win), gtk4::gio::Cancellable::NONE, move |res| {
                if let Ok(f) = res && let Some(p) = f.path() {
                    row.set_subtitle(&p.to_string_lossy());
                    let mut s = me.settings.borrow_mut(); s.notification_sound = Sound::File(p); s.save();
                }
            });
        });
        let test = adw::ButtonRow::builder().title("Send a test notification").build();
        let me = self.clone();
        test.connect_activated(move |_| {
            if let Some(n) = &me.notifier {
                n.send(crate::notify::Notice { conversation_id: String::new(), title: "12345 · Telegram".into(), body: "Login code: 12345. Do not give this code to anyone.".into(), otp: Some("12345".into()) }, &me.settings.borrow().notification_sound);
            }
        });
        group.add(&sound_row); group.add(&file_row); group.add(&test);
        page.add(&group);
        let about = adw::PreferencesGroup::builder().title("About")
            .description("Avro is an unofficial Telegram client built on the Telegram API and TDLib. It is not made or endorsed by Telegram.").build();
        page.add(&about);
        dialog.add(&page);
        dialog.present(Some(&self.win));
    }
}

impl ChatsView {
    /// One chat in the sidebar, laid out Teams-style: the name and a single-line preview
    /// together stand exactly as tall as the avatar, with the time top-right and an unread
    /// badge pinned to the avatar's corner (blank for a chat marked unread, a count otherwise).
    fn chat_row(self: &Rc<Self>, c: &Chat, st: &State) -> gtk4::ListBoxRow {
        const SIZE: i32 = 40;
        let title = st.chat_title(c);
        let saved = c.kind.user_id() == Some(st.me) && st.me != 0;
        let avatar = self.avatar_widget(SIZE, &title, if saved { None } else { c.photo.as_ref().map(|p| &p.small) });
        if saved { avatar.set_icon_name(Some("user-bookmarks-symbolic")); avatar.set_show_initials(false); }
        else if c.kind.is_group() || c.kind.is_channel() { avatar.set_icon_name(Some("system-users-symbolic")); }
        let overlay = gtk4::Overlay::builder().child(&avatar).valign(gtk4::Align::Center).build();
        let unread = st.unread(c);
        if unread {
            let badge = gtk4::Label::builder().css_classes(["avro-badge"]).halign(gtk4::Align::End).valign(gtk4::Align::End).build();
            if c.unread_count > 0 { badge.set_label(&if c.unread_count > 999 { "999+".to_string() } else { c.unread_count.to_string() }); }
            if st.muted(c) { badge.add_css_class("muted"); }
            overlay.add_overlay(&badge);
        }

        let name = gtk4::Label::builder().label(&title).xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End).hexpand(true).valign(gtk4::Align::End).build();
        if unread { name.add_css_class("heading"); }
        let top = gtk4::Box::new(gtk4::Orientation::Horizontal, 6); top.append(&name);
        if st.muted(c) { let i = gtk4::Image::from_icon_name("audio-volume-muted-symbolic"); i.set_pixel_size(12); i.add_css_class("dim-label"); i.set_valign(gtk4::Align::End); top.append(&i); }
        if let Some(m) = &c.last_message { top.append(&gtk4::Label::builder().label(fmt_time(m.date)).css_classes(["avro-meta"]).valign(gtk4::Align::End).build()); }
        else if c.main_order().0 { let i = gtk4::Image::from_icon_name("view-pin-symbolic"); i.set_pixel_size(12); i.add_css_class("dim-label"); top.append(&i); }

        let preview = match &c.last_message {
            None => String::new(),
            Some(m) => {
                let text = m.content.summary();
                if let Some((who, what, at)) = st.actions.get(&c.id) && at.elapsed() < Duration::from_secs(6) {
                    if c.kind.user_id().is_some() || who.is_empty() { format!("{what}…") } else { format!("{who} is {what}…") }
                }
                else if m.content.is_service() { format!("{} {text}", st.sender_name(&m.sender_id, true)) }
                else if m.is_outgoing && !saved && !c.kind.is_channel() { format!("You: {text}") }
                else if c.kind.is_group() { format!("{}: {text}", st.sender_name(&m.sender_id, true)) }
                else { text }
            }
        };
        let snippet = gtk4::Label::builder().label(preview.replace('\n', " ")).xalign(0.0).ellipsize(gtk4::pango::EllipsizeMode::End)
            .single_line_mode(true).valign(gtk4::Align::Start).css_classes(["avro-snippet", "caption"]).build();

        // Two rows sharing the avatar's height: name sits on the midline, preview hangs below it.
        let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).homogeneous(true).hexpand(true).height_request(SIZE).valign(gtk4::Align::Center).build();
        col.append(&top); col.append(&snippet);

        let row_box = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(12).build();
        row_box.append(&overlay); row_box.append(&col);
        let row = gtk4::ListBoxRow::builder().child(&row_box).build();
        unsafe { row.set_data("chat-id", c.id); }

        // Right-click (or long-press) → context menu. Actions live in the "chat" group on the list.
        let menu = gtk4::gio::Menu::new();
        if unread {
            let read = gtk4::gio::MenuItem::new(Some("Mark as read"), None);
            read.set_action_and_target_value(Some("chat.read"), Some(&c.id.to_variant()));
            menu.append_item(&read);
        }
        let label = match c.kind { ChatType::Supergroup { is_channel: true, .. } => "Leave channel", ChatType::Supergroup { .. } | ChatType::BasicGroup { .. } => "Leave group", _ => "Delete chat" };
        let del = gtk4::gio::MenuItem::new(Some(label), None);
        del.set_action_and_target_value(Some("chat.delete"), Some(&c.id.to_variant()));
        menu.append_item(&del);
        let popover = gtk4::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&row);
        popover.set_has_arrow(false);
        popover.set_halign(gtk4::Align::Start);
        popover.connect_closed(|p| p.set_visible(false));
        let pop = popover.clone();
        let show = move |x: f64, y: f64| { pop.set_pointing_to(Some(&gtk4::gdk::Rectangle::new(x as i32, y as i32, 1, 1))); pop.popup(); };
        let click = gtk4::GestureClick::builder().button(gtk4::gdk::BUTTON_SECONDARY).build();
        let s = show.clone();
        click.connect_pressed(move |g, _, x, y| { g.set_state(gtk4::EventSequenceState::Claimed); s(x, y); });
        row.add_controller(click);
        let press = gtk4::GestureLongPress::builder().touch_only(true).build();
        press.connect_pressed(move |g, x, y| { g.set_state(gtk4::EventSequenceState::Claimed); show(x, y); });
        row.add_controller(press);
        // Unparent the popover when the row goes away, or GTK warns about a dangling child.
        row.connect_destroy(move |_| popover.unparent());
        row
    }

    fn bubble(self: &Rc<Self>, m: &Message, chat: &Chat, first_of_run: bool) -> gtk4::ListBoxRow {
        let st = self.st.borrow();
        // Service messages ("Ana joined the group") sit centred, without a bubble.
        if m.content.is_service() {
            let who = st.sender_name(&m.sender_id, false);
            let l = gtk4::Label::builder().label(format!("{who} {}", m.content.summary()).trim()).wrap(true).justify(gtk4::Justification::Center)
                .halign(gtk4::Align::Center).css_classes(["avro-service", "dim-label"]).margin_top(4).margin_bottom(4).build();
            return gtk4::ListBoxRow::builder().child(&l).activatable(false).selectable(false).build();
        }
        let from_me = m.is_outgoing && !chat.kind.is_channel();
        let halign = if from_me { gtk4::Align::End } else { gtk4::Align::Start };
        let col = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(4).halign(halign).build();
        let bubble = gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(6).css_classes(["avro-bubble", if from_me { "avro-me" } else { "avro-them" }]).halign(halign).build();
        // Groups: a small photo and the sender's name in their colour above each run.
        if chat.kind.is_group() && !from_me && first_of_run {
            let name = st.sender_name(&m.sender_id, false);
            let photo = st.sender_photo(&m.sender_id);
            let av = self.avatar_widget(24, &name, photo.as_ref());
            let l = gtk4::Label::builder().xalign(0.0).css_classes(["caption", "heading"]).build();
            l.set_markup(&format!("<span foreground=\"{}\">{}</span>", name_color(sender_key(&m.sender_id)), glib::markup_escape_text(&name)));
            let hdr = gtk4::Box::builder().orientation(gtk4::Orientation::Horizontal).spacing(6).halign(gtk4::Align::Start).build();
            hdr.append(&av); hdr.append(&l);
            col.append(&hdr);
        }
        if let Some(f) = &m.forward_info {
            let from = match &f.origin {
                ForwardOrigin::User { sender_user_id } => st.users.get(sender_user_id).map(|u| u.full_name()).unwrap_or_default(),
                ForwardOrigin::HiddenUser { sender_name } => sender_name.clone(),
                ForwardOrigin::Chat { sender_chat_id: id } | ForwardOrigin::Channel { chat_id: id } => st.chats.get(id).map(|c| c.title.clone()).unwrap_or_default(),
                ForwardOrigin::Unknown => String::new(),
            };
            bubble.append(&gtk4::Label::builder().label(if from.is_empty() { "Forwarded".to_string() } else { format!("Forwarded from {from}") }).xalign(0.0).css_classes(["avro-forward"]).build());
        }
        drop(st);
        // Pictures, stickers and GIFs stand alone (no bubble); files and audio sit inside it.
        match media_of(&m.content) {
            Some(md @ (Media::Image { .. } | Media::Animation { .. } | Media::Video { .. } | Media::Emoji(_))) => { let w = self.media_widget(&md); w.set_halign(halign); col.append(&w); }
            Some(md) => bubble.append(&self.media_widget(&md)),
            None => {}
        }
        let text = match &m.content.data {
            MessageContent::Text { text } => Some(to_markup(text)),
            _ if media_of(&m.content).is_none() => Some(glib::markup_escape_text(&m.content.summary()).to_string()),
            _ => m.content.text().filter(|t| !t.text.trim().is_empty()).map(to_markup),
        };
        if let Some(markup) = text.filter(|t| !t.trim().is_empty()) {
            bubble.append(&gtk4::Label::builder().label(&markup).use_markup(true).wrap(true).wrap_mode(gtk4::pango::WrapMode::WordChar).xalign(0.0).selectable(true).max_width_chars(60).build());
        }
        if bubble.first_child().is_some() { col.append(&bubble); }
        let mut meta = fmt_time(m.date);
        if m.edit_date > 0 { meta.push_str(" · edited"); }
        if from_me {
            meta.push_str(match &m.sending_state {
                Some(SendingState::Pending) => " · sending",
                Some(SendingState::Failed) => " · failed",
                _ if m.id <= chat.last_read_outbox_message_id => " · read",
                _ => " · sent",
            });
        }
        col.append(&gtk4::Label::builder().label(&meta).css_classes(["avro-meta"]).halign(halign).build());
        gtk4::ListBoxRow::builder().child(&col).activatable(false).selectable(false).build()
    }

    /// Display size for media of `w`×`h`, fitting within `max_w`×`max_h` (small ones scale up).
    fn fit(w: i32, h: i32, max_w: f64, max_h: f64) -> (i32, i32) {
        let (w, h) = (w.max(1) as f64, h.max(1) as f64);
        let k = (max_w / w).min(max_h / h);
        ((w * k).round() as i32, (h * k).round() as i32)
    }

    /// A picture of the given size showing the blurry inline preview, if there is one.
    fn placeholder(mini: Option<&Minithumbnail>, size: (i32, i32), round: bool) -> gtk4::Picture {
        let pic = gtk4::Picture::builder().content_fit(gtk4::ContentFit::Cover).can_shrink(true).overflow(gtk4::Overflow::Hidden).css_classes([if round { "avro-round" } else { "avro-image" }]).build();
        pic.set_size_request(size.0, size.1);
        if let Some(m) = mini && let Ok(t) = gtk4::gdk::Texture::from_bytes(&glib::Bytes::from(&m.data)) { pic.set_paintable(Some(&t)); }
        pic
    }

    /// The local path of a file, if it's already downloaded.
    fn local(&self, file: &File) -> Option<String> {
        file.ready_path().map(str::to_string).or_else(|| self.paths.borrow().get(&file.id).cloned())
    }

    /// Download `file` the first time `holder` comes within a screen of the visible part of
    /// the thread, then call `ready` with its path. Already-downloaded files go straight there.
    fn when_visible(self: &Rc<Self>, holder: &gtk4::Widget, file: &File, ready: impl Fn(&str) + 'static) {
        if let Some(p) = self.local(file) { ready(&p); return; }
        let sw = self.thread_scroll.clone();
        let fired = Rc::new(Cell::new(false));
        let ready = Rc::new(ready);
        let (me, h, id) = (Rc::downgrade(self), holder.downgrade(), file.id);
        let check: Rc<dyn Fn()> = Rc::new(move || {
            let (Some(me), Some(h)) = (me.upgrade(), h.upgrade()) else { return };
            if fired.get() || !h.is_mapped() { return; }
            let Some(b) = h.compute_bounds(&sw) else { return };
            let vh = sw.height() as f32;
            if b.y() > vh * 2.0 || b.y() + b.height() < -vh { return; }
            fired.set(true);
            let ready = ready.clone();
            glib::spawn_future_local(async move {
                match me.client.download(id, 16).await {
                    Ok(f) => if let Some(p) = f.ready_path() { me.paths.borrow_mut().insert(id, p.to_string()); ready(p); },
                    Err(e) => tracing::warn!("download {id}: {e}"),
                }
            });
        });
        // Re-check on map, on scroll, and shortly after layout so the first screen fills in.
        let c = check.clone(); holder.connect_map(move |_| c());
        let c = check.clone(); let weak = holder.downgrade();
        let adj = self.thread_scroll.vadjustment();
        let handler = adj.connect_value_changed(move |_| { if weak.upgrade().is_some() { c(); } });
        let handler = Rc::new(RefCell::new(Some(handler)));
        holder.connect_unrealize(move |_| { if let Some(id) = handler.borrow_mut().take() { adj.disconnect(id); } });
        let c = check.clone(); glib::timeout_add_local_once(Duration::from_millis(50), move || c());
    }

    fn texture(&self, id: i32, path: &str) -> Option<gtk4::gdk::Texture> {
        if let Some(t) = self.textures.borrow().get(&id) { return Some(t.clone()); }
        match gtk4::gdk::Texture::from_filename(path) {
            Ok(t) => { self.textures.borrow_mut().insert(id, t.clone()); Some(t) }
            Err(e) => { tracing::warn!("decode {path}: {e}"); None }
        }
    }

    /// A looping, silent animation (GIF-style); kept per chat so re-renders don't restart it.
    fn animation(&self, id: i32, path: &str) -> gtk4::MediaFile {
        self.animations.borrow_mut().entry(id).or_insert_with(|| {
            let mf = gtk4::MediaFile::for_filename(path);
            mf.set_muted(true);
            mf.set_loop(true);
            mf.play();
            mf
        }).clone()
    }

    fn media_widget(self: &Rc<Self>, md: &Media) -> gtk4::Widget {
        match md {
            Media::Emoji(e) => gtk4::Label::builder().label(e).css_classes(["avro-big-emoji"]).build().upcast(),
            Media::Image { file, mini, width, height, sticker } => {
                let size = if *sticker { Self::fit(*width, *height, 180.0, 180.0) } else { Self::fit(*width, *height, 360.0, 480.0) };
                let pic = Self::placeholder(mini.as_ref(), size, false);
                if *sticker { pic.set_content_fit(gtk4::ContentFit::Contain); pic.add_css_class("avro-sticker"); }
                let (me, p, id) = (Rc::downgrade(self), pic.downgrade(), file.id);
                self.when_visible(pic.upcast_ref(), file, move |path| {
                    let (Some(me), Some(p)) = (me.upgrade(), p.upgrade()) else { return };
                    if let Some(t) = me.texture(id, path) { p.set_paintable(Some(&t)); }
                });
                pic.upcast()
            }
            Media::Animation { file, mini, width, height, sticker } => {
                let size = if *sticker { Self::fit(*width, *height, 180.0, 180.0) } else { Self::fit(*width, *height, 360.0, 480.0) };
                let pic = Self::placeholder(mini.as_ref(), size, false);
                if *sticker { pic.set_content_fit(gtk4::ContentFit::Contain); }
                let (me, p, id) = (Rc::downgrade(self), pic.downgrade(), file.id);
                self.when_visible(pic.upcast_ref(), file, move |path| {
                    let (Some(me), Some(p)) = (me.upgrade(), p.upgrade()) else { return };
                    p.set_paintable(Some(&me.animation(id, path)));
                });
                pic.upcast()
            }
            Media::Video { file, mini, thumb, width, height, duration, round } => {
                let size = if *round { (240, 240) } else { Self::fit(*width, *height, 360.0, 480.0) };
                let pic = Self::placeholder(mini.as_ref(), size, *round);
                if let Some(t) = thumb {
                    let (me, p, id) = (Rc::downgrade(self), pic.downgrade(), t.file.id);
                    self.when_visible(pic.upcast_ref(), &t.file, move |path| {
                        let (Some(me), Some(p)) = (me.upgrade(), p.upgrade()) else { return };
                        if let Some(t) = me.texture(id, path) { p.set_paintable(Some(&t)); }
                    });
                }
                let play = gtk4::Button::builder().label(format!("▶ {}", fmt_duration(*duration))).css_classes(["osd", "pill"]).halign(gtk4::Align::Center).valign(gtk4::Align::Center).build();
                let frame = gtk4::Overlay::builder().child(&pic).build();
                frame.add_overlay(&play);
                let (me, f, file, round) = (self.clone(), frame.clone(), file.clone(), *round);
                play.connect_clicked(move |b| {
                    b.set_sensitive(false);
                    b.set_label("⏳");
                    let (me, f, b, file) = (me.clone(), f.clone(), b.clone(), file.clone());
                    glib::spawn_future_local(async move {
                        match me.client.download(file.id, 32).await {
                            Ok(done) if done.ready_path().is_some() => {
                                let v = gtk4::Video::for_filename(done.ready_path());
                                v.set_autoplay(true);
                                v.set_size_request(size.0, size.1);
                                v.set_overflow(gtk4::Overflow::Hidden);
                                v.add_css_class(if round { "avro-round" } else { "avro-image" });
                                f.set_child(Some(&v));
                                b.set_visible(false);
                            }
                            Ok(_) => { b.set_sensitive(true); b.set_label("▶"); }
                            Err(e) => { b.set_sensitive(true); b.set_label("▶"); me.toast.add_toast(adw::Toast::new(&format!("Download failed: {e}"))); }
                        }
                    });
                });
                frame.upcast()
            }
            Media::Audio { file, label } => {
                let btn = gtk4::Button::builder().label(format!("▶ {label}")).css_classes(["flat"]).halign(gtk4::Align::Start).build();
                let player: Rc<RefCell<Option<gtk4::MediaFile>>> = Rc::default();
                let (me, file, label) = (self.clone(), file.clone(), label.clone());
                btn.connect_clicked(move |b| {
                    if let Some(mf) = player.borrow().as_ref() {
                        if mf.is_playing() { mf.pause(); b.set_label(&format!("▶ {label}")); } else { mf.play(); b.set_label(&format!("⏸ {label}")); }
                        return;
                    }
                    b.set_label(&format!("⏳ {label}"));
                    let (me, b, file, label, player) = (me.clone(), b.clone(), file.clone(), label.clone(), player.clone());
                    glib::spawn_future_local(async move {
                        match me.client.download(file.id, 32).await {
                            Ok(done) if done.ready_path().is_some() => {
                                let mf = gtk4::MediaFile::for_filename(done.ready_path().unwrap());
                                let (b2, l2) = (b.downgrade(), label.clone());
                                mf.connect_ended_notify(move |m| { if m.is_ended() && let Some(b) = b2.upgrade() { b.set_label(&format!("▶ {l2}")); m.seek(0); } });
                                mf.play();
                                b.set_label(&format!("⏸ {label}"));
                                *player.borrow_mut() = Some(mf);
                            }
                            Ok(_) => b.set_label(&format!("▶ {label}")),
                            Err(e) => { b.set_label(&format!("▶ {label}")); me.toast.add_toast(adw::Toast::new(&format!("Download failed: {e}"))); }
                        }
                    });
                });
                btn.upcast()
            }
            Media::File { file, name, size } => {
                let label = format!("📎 {name}{}", if *size > 0 { format!(" · {}", fmt_size(*size)) } else { String::new() });
                let btn = gtk4::Button::builder().label(&label).css_classes(["flat"]).halign(gtk4::Align::Start).tooltip_text("Save to Downloads").build();
                let (me, file, name) = (self.clone(), file.clone(), name.clone());
                btn.connect_clicked(move |b| {
                    b.set_sensitive(false);
                    b.set_label(&format!("⏳ {name}"));
                    let (me, b, file, name, label) = (me.clone(), b.clone(), file.clone(), name.clone(), label.clone());
                    glib::spawn_future_local(async move {
                        let r = match me.client.download(file.id, 32).await {
                            Ok(done) => done.ready_path().map(|p| save_download(&name, std::path::Path::new(p))).unwrap_or_else(|| Err(anyhow::anyhow!("download incomplete"))),
                            Err(e) => Err(e.into()),
                        };
                        b.set_sensitive(true);
                        match r {
                            Ok(path) => { b.set_label(&format!("✓ {name}")); me.toast.add_toast(adw::Toast::new(&format!("Saved to {}", path.display()))); }
                            Err(e) => { b.set_label(&label); me.toast.add_toast(adw::Toast::new(&format!("Save failed: {e:#}"))); }
                        }
                    });
                });
                btn.upcast()
            }
        }
    }
}

/// Copy a downloaded file into ~/Downloads under its own name, numbering it if that's taken.
fn save_download(name: &str, from: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    let dir = directories::UserDirs::new().and_then(|d| d.download_dir().map(|p| p.to_path_buf())).unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    // Never let a sender's file name climb out of Downloads.
    let name = std::path::Path::new(name).file_name().and_then(|n| n.to_str()).filter(|n| !n.is_empty()).unwrap_or("telegram-file").to_string();
    let mut path = dir.join(&name);
    let (stem, ext) = match name.rsplit_once('.') { Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")), _ => (name.clone(), String::new()) };
    let mut n = 1;
    while path.exists() { path = dir.join(format!("{stem} ({n}){ext}")); n += 1; }
    std::fs::copy(from, &path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_queries() {
        assert_eq!(normalise_number("+1 (555) 123-4567").as_deref(), Some("+15551234567"));
        assert_eq!(normalise_number("555 1"), None);
        assert_eq!(normalise_number("anna"), None);
        assert_eq!(as_username("@durov").as_deref(), Some("durov"));
        assert_eq!(as_username("https://t.me/telegram").as_deref(), Some("telegram"));
        assert_eq!(as_username("ab"), None);
        assert_eq!(as_username("1abcde"), None);
        assert_eq!(as_username("John Smith"), None);
    }
}
