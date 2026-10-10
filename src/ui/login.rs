//! First run: the API credentials page, then Telegram's login steps as TDLib asks for them —
//! phone number or QR code, the code, the two-step password, and the email steps some
//! accounts need. TDLib owns the state machine; `LoginView::show` just draws its current
//! state, and every submit is a request whose success shows up as the next state.
use crate::tg::auth::ApiCredentials;
use crate::tg::client::{Client, Error};
use crate::tg::types::AuthState;
use adw::prelude::*;
use gtk4::glib;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

pub const NO_ACCOUNT: &str = "There is no Telegram account for this number. Sign up in an official Telegram app first, then log in here — Telegram watches accounts created through unofficial apps.";
pub const NEEDS_PREMIUM: &str = "Telegram asks for a Premium subscription to log this number in to a new app. Log in with an official Telegram app instead.";

/// A TDLib error in words a person can act on.
pub fn explain(e: &Error) -> String {
    let Some(m) = e.td_message() else { return e.to_string() };
    if let Some(secs) = m.strip_prefix("Too Many Requests: retry after ").and_then(|s| s.trim().parse::<u64>().ok()) {
        return format!("Too many attempts. Telegram asks you to wait {} before trying again.", if secs >= 120 { format!("{} minutes", secs.div_ceil(60)) } else { format!("{secs} seconds") });
    }
    match m {
        "PHONE_NUMBER_INVALID" => "That phone number isn't valid. Include the country code, like +1 555 123 4567.".into(),
        "PHONE_NUMBER_BANNED" => "This phone number is banned from Telegram.".into(),
        "PHONE_CODE_INVALID" => "That code isn't right. Check it and try again.".into(),
        "PHONE_CODE_EXPIRED" => "That code expired. Go back and ask for a new one.".into(),
        "PASSWORD_HASH_INVALID" => "Wrong password.".into(),
        "EMAIL_ADDRESS_INVALID" => "That email address isn't valid.".into(),
        "EMAIL_CODE_INVALID" => "That email code isn't right.".into(),
        m if m.starts_with("API_ID") => "Telegram rejected the API ID and hash. Check them at my.telegram.org.".into(),
        m => m.to_string(),
    }
}

fn is_bad_api(e: &Error) -> bool { e.td_message().is_some_and(|m| m.starts_with("API_ID")) }

/// Header + content, as every login page is laid out.
fn page(child: &impl IsA<gtk4::Widget>) -> adw::ToolbarView {
    let tv = adw::ToolbarView::builder().content(child).build();
    tv.add_top_bar(&adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new("Avro", "")).build());
    tv
}

fn form_column() -> gtk4::Box {
    gtk4::Box::builder().orientation(gtk4::Orientation::Vertical).spacing(18).width_request(340).halign(gtk4::Align::Center).build()
}

fn pill(label: &str, suggested: bool) -> gtk4::Button {
    let b = gtk4::Button::builder().label(label).halign(gtk4::Align::Center).css_classes(["pill"]).build();
    if suggested { b.add_css_class("suggested-action"); }
    b
}

fn link_button(label: &str) -> gtk4::Button {
    gtk4::Button::builder().label(label).halign(gtk4::Align::Center).css_classes(["flat"]).build()
}

// ── API credentials ──

pub struct ApiPage { pub widget: adw::ToolbarView }

impl ApiPage {
    pub fn new(on_saved: impl Fn(ApiCredentials) + 'static) -> Self {
        let status = adw::StatusPage::builder().icon_name("dialog-password-symbolic").title("Connect Avro to Telegram")
            .description("Telegram gives every app its own API ID. Create one at <a href=\"https://my.telegram.org/apps\">my.telegram.org</a> → API development tools (any app title and short name will do), then paste the two values here.\n\nAvro keeps them in ~/.config/avro/api.json.")
            .build();
        let id = adw::EntryRow::builder().title("API ID").input_purpose(gtk4::InputPurpose::Digits).build();
        let hash = adw::PasswordEntryRow::builder().title("API hash").build();
        let group = adw::PreferencesGroup::new();
        group.add(&id); group.add(&hash);
        let error = gtk4::Label::builder().css_classes(["error"]).wrap(true).visible(false).build();
        let save = pill("Continue", true);
        let col = form_column();
        col.append(&group); col.append(&error); col.append(&save);
        status.set_child(Some(&col));
        let on_saved = Rc::new(on_saved);
        let submit = {
            let (id, hash, error) = (id.clone(), hash.clone(), error.clone());
            move || match ApiCredentials::parse(&id.text(), &hash.text()) {
                Ok(c) => match c.save() {
                    Ok(()) => on_saved(c),
                    Err(e) => { error.set_label(&format!("Could not save: {e:#}")); error.set_visible(true); }
                },
                Err(msg) => { error.set_label(msg); error.set_visible(true); }
            }
        };
        let submit = Rc::new(submit);
        let s = submit.clone(); save.connect_clicked(move |_| s());
        let s = submit.clone(); hash.connect_entry_activated(move |_| s());
        let h = hash.clone(); id.connect_entry_activated(move |_| { h.grab_focus(); });
        Self { widget: page(&status) }
    }
}

// ── login steps ──

/// What the app should do once this client has closed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Restart { Phone, Qr, NewApi }

type Submit = Rc<dyn Fn(Arc<Client>, String) -> Pin<Box<dyn Future<Output = crate::tg::client::Result<()>>>>>;

pub struct LoginView {
    pub widget: adw::ToastOverlay,
    client: Arc<Client>,
    stack: gtk4::Stack,
    qr: gtk4::Picture,
    code_page: adw::StatusPage,
    code: adw::EntryRow,
    password_page: adw::StatusPage,
    password: adw::PasswordEntryRow,
    info: adw::StatusPage,
    /// Set by the app: close this client and start over in the given mode.
    on_restart: Rc<dyn Fn(Restart)>,
}

impl LoginView {
    pub fn new(client: Arc<Client>, on_restart: impl Fn(Restart) + 'static) -> Rc<Self> {
        let stack = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::SlideLeftRight).build();
        let widget = adw::ToastOverlay::new();
        widget.set_child(Some(&page(&stack)));

        let waiting = adw::StatusPage::builder().title("Connecting to Telegram…").build();
        waiting.set_child(Some(&adw::Spinner::new()));
        stack.add_named(&waiting, Some("waiting"));

        let (phone_page, phone, phone_next, use_qr) = Self::entry_page("Log in to Telegram", "Enter your phone number with the country code.", "Phone number", "Next", Some("Log in with a QR code instead"));
        phone.set_input_purpose(gtk4::InputPurpose::Phone);
        stack.add_named(&phone_page, Some("phone"));

        let qr = gtk4::Picture::builder().width_request(240).height_request(240).can_shrink(false).halign(gtk4::Align::Center).css_classes(["avro-qr"]).build();
        let use_phone = link_button("Log in with a phone number instead");
        let qr_col = form_column();
        qr_col.append(&qr); qr_col.append(&use_phone);
        let qr_page = adw::StatusPage::builder().title("Scan from your phone")
            .description("Open Telegram on your phone, go to Settings → Devices → Link Desktop Device, and point the camera here.").build();
        qr_page.set_child(Some(&qr_col));
        stack.add_named(&qr_page, Some("qr"));

        let (code_page, code, code_next, other_number) = Self::entry_page("Enter the code", "", "Code", "Next", Some("Use a different number"));
        code.set_input_purpose(gtk4::InputPurpose::Digits);
        stack.add_named(&code_page, Some("code"));

        let password = adw::PasswordEntryRow::builder().title("Password").build();
        let password_next = pill("Log in", true);
        let (password_page, _) = Self::form_page("Two-step verification", "Your account is protected with an extra password.", password.upcast_ref(), &password_next, None);
        stack.add_named(&password_page, Some("password"));

        let (email_page, email, email_next, _) = Self::entry_page("Add a login email", "Telegram asks this account for an email address to send login codes to.", "Email", "Next", None);
        email.set_input_purpose(gtk4::InputPurpose::Email);
        stack.add_named(&email_page, Some("email"));
        let (email_code_page, email_code, email_code_next, _) = Self::entry_page("Check your email", "Enter the code Telegram emailed you.", "Code", "Next", None);
        stack.add_named(&email_code_page, Some("email-code"));

        let info = adw::StatusPage::builder().icon_name("dialog-information-symbolic").build();
        let info_back = pill("Use a different number", false);
        info.set_child(Some(&info_back));
        stack.add_named(&info, Some("info"));

        let me = Rc::new(Self { widget, client, stack, qr, code_page, code: code.clone(), password_page, password: password.clone(), info, on_restart: Rc::new(on_restart) });

        me.wire(phone.upcast_ref(), &phone_next, Rc::new(|c, s| Box::pin(async move { c.set_phone_number(&s).await })));
        me.wire(code.upcast_ref(), &code_next, Rc::new(|c, s| Box::pin(async move { c.check_code(&s).await })));
        me.wire(password.upcast_ref(), &password_next, Rc::new(|c, s| Box::pin(async move { c.check_password(&s).await })));
        me.wire(email.upcast_ref(), &email_next, Rc::new(|c, s| Box::pin(async move { c.set_email(&s).await })));
        me.wire(email_code.upcast_ref(), &email_code_next, Rc::new(|c, s| Box::pin(async move { c.check_email_code(&s).await })));

        // Phone → QR works in place; QR → phone needs a fresh client (TDLib can't go back).
        if let Some(b) = use_qr {
            let me2 = Rc::downgrade(&me);
            b.connect_clicked(move |b| {
                let Some(me) = me2.upgrade() else { return };
                b.set_sensitive(false);
                let b = b.clone();
                glib::spawn_future_local(async move {
                    if let Err(e) = me.client.request_qr().await { me.fail(&e); }
                    b.set_sensitive(true);
                });
            });
        }
        let me2 = Rc::downgrade(&me);
        use_phone.connect_clicked(move |_| { if let Some(me) = me2.upgrade() { me.stack.set_visible_child_name("waiting"); (me.on_restart)(Restart::Phone); } });
        // A different number: TDLib accepts a new one from the code/password states directly.
        let s = me.stack.clone();
        let back = move |_: &gtk4::Button| s.set_visible_child_name("phone");
        if let Some(b) = other_number { b.connect_clicked(back.clone()); }
        info_back.connect_clicked(back);
        me
    }

    /// A status page holding one entry row, a primary button and optionally a secondary link.
    fn entry_page(title: &str, desc: &str, label: &str, action: &str, alt: Option<&str>) -> (adw::StatusPage, adw::EntryRow, gtk4::Button, Option<gtk4::Button>) {
        let entry = adw::EntryRow::builder().title(label).build();
        let next = pill(action, true);
        let (p, alt) = Self::form_page(title, desc, entry.upcast_ref(), &next, alt);
        (p, entry, next, alt)
    }

    fn form_page(title: &str, desc: &str, row: &gtk4::Widget, next: &gtk4::Button, alt: Option<&str>) -> (adw::StatusPage, Option<gtk4::Button>) {
        let group = adw::PreferencesGroup::new();
        group.add(row);
        let col = form_column();
        col.append(&group); col.append(next);
        let alt = alt.map(|a| { let b = link_button(a); col.append(&b); b });
        let p = adw::StatusPage::builder().title(title).build();
        if !desc.is_empty() { p.set_description(Some(desc)); }
        p.set_child(Some(&col));
        (p, alt)
    }

    /// Enter or the button submits the row's text; errors come back as a toast.
    fn wire(self: &Rc<Self>, row: &adw::EntryRow, button: &gtk4::Button, submit: Submit) {
        let me = Rc::downgrade(self);
        let (row2, button2) = (row.clone(), button.clone());
        let go = Rc::new(move || {
            let Some(me) = me.upgrade() else { return };
            let text = row2.text().trim().to_string();
            if text.is_empty() || !button2.is_sensitive() { return; }
            button2.set_sensitive(false);
            let (submit, button) = (submit.clone(), button2.clone());
            glib::spawn_future_local(async move {
                let r = submit(me.client.clone(), text).await;
                button.set_sensitive(true);
                if let Err(e) = r { me.fail(&e); }
            });
        });
        let g = go.clone(); button.connect_clicked(move |_| g());
        row.connect_entry_activated(move |_| go());
    }

    fn fail(&self, e: &Error) {
        if is_bad_api(e) {
            self.info.set_title("Telegram rejected the API ID");
            self.info.set_description(Some("The API ID and hash don't match an app on my.telegram.org. Enter them again."));
            let b = pill("Enter them again", true);
            let restart = self.on_restart.clone();
            b.connect_clicked(move |_| restart(Restart::NewApi));
            self.info.set_child(Some(&b));
            self.stack.set_visible_child_name("info");
            return;
        }
        self.widget.add_toast(adw::Toast::builder().title(glib::markup_escape_text(&explain(e))).timeout(6).build());
    }

    /// Draw the step TDLib is waiting for.
    pub fn show(&self, state: &AuthState) {
        let name = match state {
            AuthState::WaitPhoneNumber => "phone",
            AuthState::WaitOtherDeviceConfirmation { link } => { self.qr.set_paintable(Some(&qr_texture(link))); "qr" }
            AuthState::WaitCode { code_info } => {
                self.code_page.set_description(Some(&format!("{}\n{}", code_info.delivery(), code_info.phone_number)));
                self.code.set_text("");
                "code"
            }
            AuthState::WaitPassword { password_hint } => {
                self.password_page.set_description(Some(&if password_hint.is_empty() { "Your account is protected with an extra password.".to_string() } else { format!("Your account is protected with an extra password.\nHint: {password_hint}") }));
                self.password.set_text("");
                "password"
            }
            AuthState::WaitEmailAddress => "email",
            AuthState::WaitEmailCode => "email-code",
            AuthState::WaitRegistration => { self.info.set_title("No account"); self.info.set_description(Some(NO_ACCOUNT)); "info" }
            AuthState::WaitPremiumPurchase => { self.info.set_title("Premium required"); self.info.set_description(Some(NEEDS_PREMIUM)); "info" }
            _ => "waiting",
        };
        self.stack.set_visible_child_name(name);
        // Focus the field once the page is up, so you can type straight away.
        let stack = self.stack.clone();
        glib::idle_add_local_once(move || { if let Some(w) = stack.visible_child() { w.child_focus(gtk4::DirectionType::TabForward); } });
    }
}

/// The login link as a QR code: black modules on white with a quiet zone, whatever the theme,
/// so phone cameras read it.
fn qr_texture(link: &str) -> gtk4::gdk::MemoryTexture {
    const SCALE: usize = 6;
    const QUIET: usize = 4;
    let code = qrcode::QrCode::new(link.as_bytes()).expect("login link fits in a QR code");
    let n = code.width();
    let colors = code.to_colors();
    let side = (n + 2 * QUIET) * SCALE;
    let mut px = vec![255u8; side * side * 3];
    for (i, c) in colors.iter().enumerate() {
        if *c != qrcode::Color::Dark { continue; }
        let (mx, my) = ((i % n + QUIET) * SCALE, (i / n + QUIET) * SCALE);
        for y in my..my + SCALE { for x in mx..mx + SCALE { px[(y * side + x) * 3..][..3].fill(0); } }
    }
    gtk4::gdk::MemoryTexture::new(side as i32, side as i32, gtk4::gdk::MemoryFormat::R8g8b8, &glib::Bytes::from_owned(px), side * 3)
}
