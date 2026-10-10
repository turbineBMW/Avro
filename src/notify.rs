//! Desktop notifications over org.freedesktop.Notifications directly (rather than
//! `gio::Notification`), so we control the hints and actions: the sound request, the
//! desktop-entry for icon/grouping, a "Copy code" action for one-time passcodes, and the
//! xdg-activation token the daemon hands us when the user clicks — the only portable way to
//! take focus on Wayland.
use crate::settings::Sound;
use gtk4::gio;
use gtk4::glib::{self, prelude::*};
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

const BUS: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
const APP_NAME: &str = "Avro";

pub struct Notice {
    pub conversation_id: String,
    pub title: String,
    pub body: String,
    /// A one-time code found in the message, offered as a "Copy" action.
    pub otp: Option<String>,
}

struct Live { conversation_id: String, otp: Option<String> }

pub struct Notifier {
    conn: gio::DBusConnection,
    /// Notification id per conversation, so a new message replaces the previous bubble.
    by_conv: RefCell<HashMap<String, u32>>,
    live: RefCell<HashMap<u32, Live>>,
    /// Activation token the daemon sent just before an ActionInvoked, keyed by notification id.
    tokens: RefCell<HashMap<u32, String>>,
    on_open: RefCell<Option<Box<dyn Fn(&str, Option<String>)>>>,
}

impl Notifier {
    pub fn new() -> Option<Rc<Self>> {
        let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).map_err(|e| tracing::warn!("session bus: {e}")).ok()?;
        let me = Rc::new(Self { conn, by_conv: Default::default(), live: Default::default(), tokens: Default::default(), on_open: Default::default() });
                for sig in ["ActivationToken", "ActionInvoked", "NotificationClosed"] {
            let weak = Rc::downgrade(&me);
            #[allow(deprecated)]
            me.conn.signal_subscribe(Some(BUS), Some(BUS), Some(sig), Some(PATH), None, gio::DBusSignalFlags::NONE, move |_, _, _, _, name, params| {
                if let Some(me) = weak.upgrade() { me.on_signal(name, params); }
            });
        }
        Some(me)
    }

    /// Called with (conversation id, activation token) when a notification is clicked.
    pub fn set_on_open(&self, f: impl Fn(&str, Option<String>) + 'static) { *self.on_open.borrow_mut() = Some(Box::new(f)); }

    fn on_signal(&self, name: &str, params: &glib::Variant) {
        let Some(id) = params.child_value(0).get::<u32>() else { return };
        match name {
            "ActivationToken" => { if let Some(t) = params.child_value(1).get::<String>() { self.tokens.borrow_mut().insert(id, t); } }
            "ActionInvoked" => {
                let action = params.child_value(1).get::<String>().unwrap_or_default();
                let token = self.tokens.borrow_mut().remove(&id);
                let live = self.live.borrow();
                let Some(l) = live.get(&id) else { return };
                match action.as_str() {
                    "copy-otp" => { if let Some(code) = &l.otp { copy_to_clipboard(code); } }
                    _ => { if let Some(f) = self.on_open.borrow().as_ref() { f(&l.conversation_id, token); } }
                }
            }
            "NotificationClosed" => {
                if let Some(l) = self.live.borrow_mut().remove(&id) { self.by_conv.borrow_mut().remove(&l.conversation_id); }
                self.tokens.borrow_mut().remove(&id);
            }
            _ => {}
        }
    }

    pub fn send(self: &Rc<Self>, n: Notice, sound: &Sound) {
        let replaces = self.by_conv.borrow().get(&n.conversation_id).copied().unwrap_or(0);
        let mut actions = vec!["default".to_string(), "Open".to_string()];
        if let Some(code) = &n.otp { actions.push("copy-otp".into()); actions.push(format!("Copy {code}")); }
        let icon = app_icon();
        let mut hints: HashMap<String, glib::Variant> = HashMap::new();
        hints.insert("desktop-entry".into(), "dev.turbinebmw.Avro".to_variant());
        if icon.starts_with('/') { hints.insert("image-path".into(), icon.to_variant()); }
        hints.insert("category".into(), "im.received".to_variant());
        hints.insert("urgency".into(), 1u8.to_variant());
        match sound {
            Sound::SystemDefault => { hints.insert("sound-name".into(), "message-new-instant".to_variant()); }
            Sound::File(p) => { hints.insert("sound-file".into(), p.to_string_lossy().as_ref().to_variant()); }
            Sound::None => { hints.insert("suppress-sound".into(), true.to_variant()); }
        }
        let args = (APP_NAME, replaces, icon.as_str(), n.title.as_str(), n.body.as_str(), actions, hints, -1i32).to_variant();
        let me = self.clone();
        let (conv, otp) = (n.conversation_id, n.otp);
        glib::spawn_future_local(async move {
            match me.conn.call_future(Some(BUS), PATH, BUS, "Notify", Some(&args), None, gio::DBusCallFlags::NONE, 5000).await {
                Ok(r) => {
                    let Some(id) = r.child_value(0).get::<u32>() else { return };
                    if replaces != 0 && replaces != id { me.live.borrow_mut().remove(&replaces); }
                    me.by_conv.borrow_mut().insert(conv.clone(), id);
                    me.live.borrow_mut().insert(id, Live { conversation_id: conv, otp });
                }
                Err(e) => tracing::warn!("Notify: {e}"),
            }
        });
    }
}

/// The app icon as an absolute path when it's installed (any daemon can show that), else the
/// theme name for the daemon to resolve itself.
fn app_icon() -> String {
    let candidates = [
        directories::BaseDirs::new().map(|b| b.data_dir().join("icons/hicolor/128x128/apps/dev.turbinebmw.Avro.png")),
        Some(std::path::PathBuf::from("/usr/share/icons/hicolor/128x128/apps/dev.turbinebmw.Avro.png")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "dev.turbinebmw.Avro".into())
}

/// Wayland won't let an unfocused window own the clipboard, so prefer wl-copy (which uses the
/// data-control protocol) and fall back to GTK's clipboard when it's missing.
fn copy_to_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let ok = Command::new("wl-copy").stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        .and_then(|mut c| { c.stdin.take().unwrap().write_all(text.as_bytes())?; c.wait() }).map(|s| s.success()).unwrap_or(false);
    if !ok { if let Some(d) = gtk4::gdk::Display::default() { d.clipboard().set_text(text); } }
}

/// Guess a one-time passcode: a 4–8 digit run (optionally split once, like `123-456`) in a
/// message that also talks about a code. Long runs (phone numbers), and amounts, don't count.
pub fn detect_otp(text: &str) -> Option<String> {
    const KEYWORDS: [&str; 10] = ["code", "otp", "verif", "passcode", "pin", "one-time", "one time", "single-use", "2fa", "password"];
    let lower = text.to_lowercase();
    if !KEYWORDS.iter().any(|k| lower.contains(k)) { return None; }
    let b = text.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if !b[i].is_ascii_digit() { i += 1; continue; }
        let start = i;
        let mut digits = String::new();
        let mut split = false;
        while i < b.len() {
            if b[i].is_ascii_digit() { digits.push(b[i] as char); i += 1; }
            else if !split && (b[i] == b'-' || b[i] == b' ') && i + 1 < b.len() && b[i + 1].is_ascii_digit() && digits.len() >= 3 { split = true; i += 1; }
            else { break }
        }
        // A further digit group right after (`519 222 0428`) means a phone number, not a code.
        let more_digits_follow = i + 1 < b.len() && (b[i] == b' ' || b[i] == b'-') && b[i + 1].is_ascii_digit();
        let digits_precede = start >= 2 && (b[start - 1] == b' ' || b[start - 1] == b'-') && b[start - 2].is_ascii_digit();
        let prev = start.checked_sub(1).map(|p| b[p]);
        let money = matches!(prev, Some(b'$' | b'+' | b'#' | b'.' | b','));
        let glued = matches!(prev, Some(c) if c.is_ascii_alphanumeric()) || (i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == b'.' && i + 1 < b.len() && b[i + 1].is_ascii_digit()));
        if (4..=8).contains(&digits.len()) && !money && !glued && !more_digits_follow && !digits_precede { return Some(digits); }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::detect_otp;
    #[test]
    fn otps() {
        assert_eq!(detect_otp("Your FieldHook verification code is: 660347").as_deref(), Some("660347"));
        assert_eq!(detect_otp("Your single-use code is 606842 for MySchoolBucks").as_deref(), Some("606842"));
        assert_eq!(detect_otp("G-123456 is your Google verification code").as_deref(), Some("123456"));
        assert_eq!(detect_otp("Use code 123-456 to log in").as_deref(), Some("123456"));
        assert_eq!(detect_otp("17702293719 Deposited a new message: \"Please call\""), None);
        assert_eq!(detect_otp("I only need 1 more gift from 30224 to hit my daily"), None);
        assert_eq!(detect_otp("Your code: call +1 519 222 0428"), None);
        assert_eq!(detect_otp("Your PIN is $1234"), None);
    }
}
