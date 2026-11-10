//! The spam filter's side of the chat view: which messages to screen, holding their
//! notifications until Haiku has answered, acting on the verdict, and the review page in
//! Preferences. The screening itself lives in `crate::spam`.
use super::ChatsView;
use crate::settings::SpamFilter;
use crate::spam::{Action, Entry, Judgement, Log, Suspect};
use crate::tg::types::*;
use adw::prelude::*;
use gtk4::glib;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// At most this many `claude` processes at once.
const PARALLEL: usize = 2;

pub(super) struct Screen {
    log: RefCell<Log>,
    queue: RefCell<VecDeque<i64>>,
    /// Chats queued or being checked.
    busy: RefCell<HashSet<i64>>,
    /// Chats that got another message while being checked; checked again afterwards.
    again: RefCell<HashSet<i64>>,
    running: Cell<usize>,
    /// The newest fresh message per chat whose notification waits on the verdict.
    held: RefCell<HashMap<i64, Message>>,
    /// Said once per run that `claude` is missing or failing.
    warned: Cell<bool>,
}

impl Screen {
    pub(super) fn new() -> Self {
        Self { log: RefCell::new(Log::load()), queue: Default::default(), busy: Default::default(), again: Default::default(),
            running: Cell::new(0), held: Default::default(), warned: Cell::new(false) }
    }
}

enum Outcome { Pass, Flag(String), Blocked }

impl ChatsView {
    fn filter(&self) -> SpamFilter { self.settings.borrow().spam_filter }

    /// Cheap check from what's in memory; `spam::suspect` does the rest.
    fn maybe_stranger(&self, chat_id: i64) -> bool {
        let st = self.st.borrow();
        let Some(chat) = st.chats.get(&chat_id) else { return false };
        let ChatType::Private { user_id } = chat.kind else { return false };
        let wrote = chat.last_read_outbox_message_id != 0 || st.messages.get(&chat_id).is_some_and(|ms| ms.iter().any(|m| m.is_outgoing));
        !wrote && !self.spam.log.borrow().trusted.contains(&user_id) && !st.users.get(&user_id).is_some_and(|u| u.is_contact || u.is_bot())
    }

    /// A message just arrived. True if it is being screened, in which case its notification
    /// waits for the verdict.
    pub(super) fn screen_message(self: &Rc<Self>, m: &Message) -> bool {
        if self.filter() == SpamFilter::Off || m.is_outgoing || !self.maybe_stranger(m.chat_id) { return false; }
        if m.date >= self.started - 60 { self.spam.held.borrow_mut().insert(m.chat_id, m.clone()); }
        self.enqueue(m.chat_id);
        true
    }

    /// A chat Telegram shows its "Report spam" bar on, with unread messages nobody has judged:
    /// they came in while Avro was closed.
    pub(super) fn screen_backlog(self: &Rc<Self>, chat_id: i64) {
        if self.filter() == SpamFilter::Off || !self.maybe_stranger(chat_id) { return; }
        let pending = {
            let st = self.st.borrow();
            st.chats.get(&chat_id).is_some_and(|c| c.unread_count > 0 && c.action_bar.as_ref().is_some_and(|b| b.is_report())
                && c.last_message.as_ref().is_some_and(|m| !m.is_outgoing && self.spam.log.borrow().checked.get(&chat_id).is_none_or(|&seen| seen < m.id)))
        };
        if pending { self.enqueue(chat_id); }
    }

    fn enqueue(self: &Rc<Self>, chat_id: i64) {
        if !self.spam.busy.borrow_mut().insert(chat_id) {
            // Already queued: the check reads the latest history anyway. Running: go again after.
            if !self.spam.queue.borrow().contains(&chat_id) { self.spam.again.borrow_mut().insert(chat_id); }
            return;
        }
        self.spam.queue.borrow_mut().push_back(chat_id);
        self.pump();
    }

    fn pump(self: &Rc<Self>) {
        while self.spam.running.get() < PARALLEL {
            let Some(chat_id) = self.spam.queue.borrow_mut().pop_front() else { return };
            self.spam.running.set(self.spam.running.get() + 1);
            let me = self.clone();
            glib::spawn_future_local(async move {
                let outcome = me.check(chat_id).await;
                me.spam.running.set(me.spam.running.get() - 1);
                me.spam.busy.borrow_mut().remove(&chat_id);
                if me.spam.again.borrow_mut().remove(&chat_id) { me.enqueue(chat_id); }
                else {
                    let held = me.spam.held.borrow_mut().remove(&chat_id);
                    match (outcome, held) {
                        (Outcome::Pass, Some(m)) => me.maybe_notify(&m, None),
                        (Outcome::Flag(label), Some(m)) => me.maybe_notify(&m, Some(&label)),
                        _ => {}
                    }
                }
                me.pump();
            });
        }
    }

    async fn check(self: &Rc<Self>, chat_id: i64) -> Outcome {
        let Some(chat) = self.st.borrow().chats.get(&chat_id).cloned() else { return Outcome::Pass };
        let s = match crate::spam::suspect(&self.client, &chat).await {
            Ok(Some(s)) => s,
            Ok(None) => return Outcome::Pass,
            Err(e) => { tracing::warn!("spam filter: reading chat {chat_id}: {e}"); return Outcome::Pass }
        };
        let Some(claude) = crate::spam::find_claude() else { self.warn("Spam filter: the `claude` command isn't installed, so messages aren't being checked."); return Outcome::Pass };
        let context = self.settings.borrow().spam_context.clone();
        let (s, verdict) = crate::spam::judge(claude, context, s).await;
        let v = match verdict {
            Ok(v) => v,
            Err(e) => { tracing::warn!("spam filter: {e:#}"); self.warn("Spam filter: Claude couldn't check a message (see ~/.cache/avro/avro.log)."); return Outcome::Pass }
        };
        tracing::info!("spam filter: {} → {:?}: {}", s.name, v.judgement, v.reason);
        let block = v.judgement == Judgement::Spam && self.filter() == SpamFilter::Block;
        let action = match v.judgement { Judgement::Legit => Action::Passed, _ if block => Action::Blocked, _ => Action::Flagged };
        let entry = Entry { chat_id, user_id: s.user_id, name: s.name.clone(), excerpt: s.excerpt(), judgement: v.judgement, reason: v.reason.clone(), date: glib::real_time() / 1_000_000, action };
        self.spam.log.borrow_mut().record(entry, s.last_message_id);
        match action {
            Action::Blocked => {
                if let Err(e) = self.block(&s).await { tracing::warn!("spam filter: blocking {}: {e}", s.name); return Outcome::Flag("⚠ Likely spam".into()) }
                let toast = adw::Toast::builder().title(format!("Blocked {} as spam", s.name)).button_label("Undo").timeout(8).build();
                let (me, user_id, chat_id) = (self.clone(), s.user_id, chat_id);
                toast.connect_button_clicked(move |_| me.allow(chat_id, user_id));
                self.toast.add_toast(toast);
                Outcome::Blocked
            }
            Action::Flagged => Outcome::Flag(if v.judgement == Judgement::Spam { "⚠ Likely spam".into() } else { "⚠ Possible spam".into() }),
            _ => Outcome::Pass,
        }
    }

    fn warn(&self, text: &str) {
        if self.spam.warned.replace(true) { return; }
        self.toast.add_toast(adw::Toast::builder().title(text).timeout(0).build());
    }

    /// Block, report and archive, and read what they sent so it doesn't sit there unread.
    async fn block(&self, s: &Suspect) -> crate::tg::client::Result<()> {
        self.client.set_blocked(s.user_id, true).await?;
        if let Err(e) = self.client.report_spam(s.chat_id).await { tracing::debug!("report spam: {e}"); }
        if let Err(e) = self.client.set_archived(s.chat_id, true).await { tracing::warn!("archive: {e}"); }
        if let Err(e) = self.client.view_messages(s.chat_id, &[s.last_message_id]).await { tracing::debug!("read: {e}"); }
        Ok(())
    }

    /// Undo: unblock, bring the chat back, and never screen this person again.
    fn allow(self: &Rc<Self>, chat_id: i64, user_id: i64) {
        self.spam.log.borrow_mut().allow(user_id);
        let me = self.clone();
        glib::spawn_future_local(async move {
            let r = async { me.client.set_blocked(user_id, false).await?; me.client.set_archived(chat_id, false).await }.await;
            if let Err(e) = r { me.toast.add_toast(adw::Toast::new(&format!("Could not unblock: {e}"))); }
        });
    }

    /// Block someone the filter let through or only flagged.
    fn block_now(self: &Rc<Self>, chat_id: i64, user_id: i64) {
        self.spam.log.borrow_mut().set_action(user_id, Action::Blocked);
        let me = self.clone();
        glib::spawn_future_local(async move {
            let r = async {
                me.client.set_blocked(user_id, true).await?;
                if let Err(e) = me.client.report_spam(chat_id).await { tracing::debug!("report spam: {e}"); }
                me.client.set_archived(chat_id, true).await
            }.await;
            if let Err(e) = r { me.toast.add_toast(adw::Toast::new(&format!("Could not block: {e}"))); }
        });
    }

    /// The "Spam filter" group for Preferences.
    pub(super) fn spam_preferences(self: &Rc<Self>, dialog: &adw::PreferencesDialog) -> adw::PreferencesGroup {
        let found = crate::spam::find_claude().is_some();
        let group = adw::PreferencesGroup::builder().title("Spam filter")
            .description(if found {
                "Messages from people who aren't your contacts, in chats you've never written in, are checked by Claude Haiku through the claude command. Their name, bio, profile photo and messages are sent to Anthropic."
            } else {
                "Needs the claude command (Claude Code), which wasn't found on PATH or in ~/.local/bin."
            }).build();
        let choices = gtk4::StringList::new(&["Off", "Flag likely spam", "Block likely spam"]);
        let mode = adw::ComboRow::builder().title("Messages from strangers").model(&choices)
            .subtitle("Blocking also reports the chat to Telegram and archives it").build();
        mode.set_selected(match self.filter() { SpamFilter::Off => 0, SpamFilter::Flag => 1, SpamFilter::Block => 2 });
        let me = self.clone();
        mode.connect_selected_notify(move |r| {
            let mut s = me.settings.borrow_mut();
            s.spam_filter = match r.selected() { 1 => SpamFilter::Flag, 2 => SpamFilter::Block, _ => SpamFilter::Off };
            s.save();
        });
        let context = adw::EntryRow::builder().title("Who might message you out of the blue").show_apply_button(true)
            .text(self.settings.borrow().spam_context.as_str()).build();
        let me = self.clone();
        context.connect_apply(move |e| { let mut s = me.settings.borrow_mut(); s.spam_context = e.text().to_string(); s.save(); });
        let review = adw::ActionRow::builder().title("Review decisions").activatable(true).build();
        review.add_suffix(&gtk4::Image::from_icon_name("go-next-symbolic"));
        let (me, dlg) = (self.clone(), dialog.clone());
        review.connect_activated(move |_| dlg.push_subpage(&me.spam_review()));
        group.add(&mode); group.add(&context); group.add(&review);
        group
    }

    /// Everything the filter decided, newest first, each with a way to undo it.
    fn spam_review(self: &Rc<Self>) -> adw::NavigationPage {
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::new();
        let entries: Vec<Entry> = self.spam.log.borrow().entries.iter().rev().cloned().collect();
        if entries.is_empty() {
            group.set_description(Some("Nothing has been checked yet."));
        }
        for e in entries {
            let verdict = match e.judgement { Judgement::Spam => "Spam", Judgement::Unsure => "Unsure", Judgement::Legit => "Legit" };
            let when = super::fmt_time(e.date);
            let row = adw::ActionRow::builder().title(&e.name).use_markup(false).subtitle_lines(4)
                .subtitle(format!("{verdict} · {when} — {}\n“{}”", e.reason, e.excerpt)).build();
            let button = gtk4::Button::builder().valign(gtk4::Align::Center).build();
            let set = |b: &gtk4::Button, action: Action| match action {
                Action::Blocked => { b.set_label("Unblock"); b.remove_css_class("destructive-action"); }
                Action::Allowed => { b.set_label("Allowed"); b.set_sensitive(false); }
                _ => { b.set_label("Block"); b.add_css_class("destructive-action"); }
            };
            set(&button, e.action);
            let me = self.clone();
            let action = Rc::new(Cell::new(e.action));
            button.connect_clicked(move |b| {
                if action.get() == Action::Blocked { me.allow(e.chat_id, e.user_id); action.set(Action::Allowed); }
                else { me.block_now(e.chat_id, e.user_id); action.set(Action::Blocked); }
                set(b, action.get());
            });
            row.add_suffix(&button);
            group.add(&row);
        }
        page.add(&group);
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&page));
        adw::NavigationPage::builder().title("Spam filter decisions").child(&view).build()
    }
}
