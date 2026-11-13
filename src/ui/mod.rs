//! libadwaita UI. All state lives on the GTK thread; TDLib's replies and updates arrive on
//! its own thread and are awaited here through runtime-agnostic channels.
mod chats;
pub mod login;
mod state;

use crate::tg::auth::ApiCredentials;
use crate::tg::client::Client;
use crate::tg::types::{AuthState, Update};
use adw::prelude::*;
use gtk4::glib;
use login::Restart;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

/// Sidebar opacity: `--avro-sidebar-opacity`, a number from 0 to 1, fades the chat list's
/// background but never its text. An Omarchy theme sets it in its `avro.css`. Written with
/// `rgb(from …)` rather than `alpha()`, which drops the whole background when handed a
/// percentage; this form takes one, as the parser in `omarchy::palette` does. Scoped to the
/// split view's own pane, so sidebars nested in dialogs are left alone.
///
/// Below 1, `sync_transparency` also clears the window background so the desktop shows
/// through. Everything else that was drawn on that background is repainted with it: the open
/// chat (the content pane, whose header bar is flat), the split view should it ever collapse
/// into a single page, and the login, connecting and error pages. Dialogs, popovers, toasts
/// and banners paint their own backgrounds.
const CSS: &str = "
:root { --avro-sidebar-opacity: 1; }
navigation-split-view > .sidebar-pane {
    background-color: rgb(from var(--sidebar-bg-color) r g b / calc(alpha * var(--avro-sidebar-opacity)));
}
navigation-split-view > .sidebar-pane:backdrop {
    background-color: rgb(from var(--sidebar-backdrop-color) r g b / calc(alpha * var(--avro-sidebar-opacity)));
}
window.avro-transparent { background-color: transparent; }
window.avro-transparent navigation-split-view > .content-pane,
window.avro-transparent navigation-split-view > navigation-view,
window.avro-transparent stack.avro-pages > :not(navigation-split-view) {
    background-color: var(--window-bg-color);
}
";

pub fn build(app: &adw::Application) {
    let css = gtk4::CssProvider::new();
    css.load_from_string(CSS);
    gtk4::style_context_add_provider_for_display(
        &gtk4::gdk::Display::default().unwrap(),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let win = adw::ApplicationWindow::builder().application(app).title("Avro").default_width(1000).default_height(700).build();
    let stack = gtk4::Stack::builder().transition_type(gtk4::StackTransitionType::Crossfade).css_classes(["avro-pages"]).build();
    win.set_content(Some(&stack));
    let weak = win.downgrade();
    crate::omarchy::connect_sidebar_opacity(move |opacity| {
        if let Some(win) = weak.upgrade() {
            sync_transparency(&win, opacity);
        }
    });
    // Let TDLib close its database before the process exits: hide now, quit on
    // authorizationStateClosed (see `quit_if_closing`), or after a few seconds regardless.
    win.connect_close_request(|w| {
        let Some(c) = (unsafe { w.data::<Arc<Client>>("client").map(|c| c.as_ref().clone()) }) else { return glib::Propagation::Proceed };
        if QUITTING.replace(true) { return glib::Propagation::Proceed; }
        c.close();
        w.set_visible(false);
        let app = w.application();
        glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || { if let Some(a) = app { a.quit(); } });
        glib::Propagation::Stop
    });
    win.present();
    begin(&win, &stack, Restart::Phone);
}

/// The "avro-transparent" class clears the window background, so a translucent sidebar
/// shows the desktop rather than the window behind it.
fn sync_transparency(win: &adw::ApplicationWindow, sidebar_opacity: f32) {
    tracing::debug!("{} is {sidebar_opacity}", crate::omarchy::SIDEBAR_OPACITY);
    if sidebar_opacity < 1.0 {
        win.add_css_class("avro-transparent");
    } else {
        win.remove_css_class("avro-transparent");
    }
}

thread_local! { static QUITTING: Cell<bool> = const { Cell::new(false) }; }

/// Called on authorizationStateClosed: true (and the app quits) if that was the window closing.
fn quit_if_closing(win: &adw::ApplicationWindow) -> bool {
    if !QUITTING.get() { return false; }
    if let Some(a) = win.application() { a.quit(); }
    true
}

/// Drop a page from the stack if it is there (pages are re-created on every login).
fn drop_page(stack: &gtk4::Stack, name: &str) { if let Some(w) = stack.child_by_name(name) { stack.remove(&w); } }

fn begin(win: &adw::ApplicationWindow, stack: &gtk4::Stack, mode: Restart) {
    match ApiCredentials::load() {
        Some(creds) if mode != Restart::NewApi => start(win, stack, creds, mode),
        _ => show_api_page(win, stack),
    }
}

fn show_api_page(win: &adw::ApplicationWindow, stack: &gtk4::Stack) {
    for p in ["api", "login", "connecting", "fatal"] { drop_page(stack, p); }
    let (win2, stack2) = (win.clone(), stack.clone());
    let page = login::ApiPage::new(move |creds| { drop_page(&stack2, "api"); start(&win2, &stack2, creds, Restart::Phone); });
    stack.add_named(&page.widget, Some("api"));
    stack.set_visible_child_name("api");
}

fn connecting(stack: &gtk4::Stack, title: &str) {
    drop_page(stack, "connecting");
    let spinner = adw::StatusPage::builder().title(title).build();
    spinner.set_child(Some(&adw::Spinner::new()));
    stack.add_named(&spinner, Some("connecting"));
    stack.set_visible_child_name("connecting");
}

/// Open a TDLib client and follow its authorisation states until it is ready (→ chats) or
/// closed (→ start over the way the login view asked for).
fn start(win: &adw::ApplicationWindow, stack: &gtk4::Stack, creds: ApiCredentials, mode: Restart) {
    let (client, events) = match Client::new() { Ok(x) => x, Err(e) => { fatal(stack, &e); return; } };
    unsafe { win.set_data("client", client.clone()); }
    connecting(stack, "Connecting to Telegram…");
    let next = Rc::new(Cell::new(None::<Restart>));
    let login: RefCell<Option<Rc<login::LoginView>>> = RefCell::new(None);
    let (win, stack) = (win.clone(), stack.clone());
    glib::spawn_future_local(async move {
        let mut early = Vec::new();
        let mut asked_qr = false;
        while let Ok(u) = events.recv().await {
            let Update::AuthorizationState { authorization_state: s } = u else { early.push(u); continue };
            tracing::info!("authorization state: {s:?}");
            match s {
                AuthState::WaitTdlibParameters => {
                    if let Err(e) = client.set_parameters(&creds).await {
                        let msg = e.to_string();
                        if msg.contains("lock") { fatal(&stack, "Avro is already running. TDLib's database can only be opened by one copy at a time — close the other one (or the `avro` command in a terminal) and try again."); }
                        else { fatal(&stack, &format!("TDLib refused its parameters: {msg}")); }
                        return;
                    }
                }
                AuthState::WaitPhoneNumber if mode == Restart::Qr && !asked_qr => {
                    asked_qr = true;
                    if let Err(e) = client.request_qr().await { tracing::warn!("QR login: {e}"); }
                }
                AuthState::Ready => {
                    for p in ["login", "connecting"] { drop_page(&stack, p); }
                    show_chats(&win, &stack, client, events, early);
                    return;
                }
                AuthState::Closed => {
                    if quit_if_closing(&win) { return; }
                    for p in ["login", "connecting"] { drop_page(&stack, p); }
                    begin(&win, &stack, next.take().unwrap_or(Restart::Phone));
                    return;
                }
                AuthState::LoggingOut | AuthState::Closing | AuthState::Other => {}
                s => {
                    let view = login.borrow_mut().get_or_insert_with(|| {
                        let (client, next) = (client.clone(), next.clone());
                        let v = login::LoginView::new(client.clone(), move |r| {
                            next.set(Some(r));
                            if r == Restart::NewApi { ApiCredentials::forget(); client.close(); }
                            else { let c = client.clone(); glib::spawn_future_local(async move { if let Err(e) = c.cancel_login().await { tracing::warn!("cancel login: {e}"); c.close(); } }); }
                        });
                        stack.add_named(&v.widget, Some("login"));
                        v
                    }).clone();
                    view.show(&s);
                    stack.set_visible_child_name("login");
                    drop_page(&stack, "connecting");
                }
            }
        }
    });
}

fn show_chats(win: &adw::ApplicationWindow, stack: &gtk4::Stack, client: Arc<Client>, events: async_channel::Receiver<Update>, early: Vec<Update>) {
    let view = Rc::new(chats::ChatsView::new(win, client.clone(), events));
    stack.add_named(&view.widget, Some("chats"));
    stack.set_visible_child_name("chats");
    {
        // Logged out — here, from another device, or the session was revoked: back to login.
        let (win, stack) = (win.clone(), stack.clone());
        view.set_on_closed(move || { if quit_if_closing(&win) { return; } drop_page(&stack, "chats"); begin(&win, &stack, Restart::Phone); });
    }
    let logout = gtk4::gio::SimpleAction::new("logout", None);
    {
        let win = win.clone();
        logout.connect_activate(move |a, _| {
            let dlg = adw::AlertDialog::new(Some("Log out?"), Some("Avro forgets this session and its downloaded messages and media. Your chats stay on Telegram."));
            dlg.add_responses(&[("cancel", "Cancel"), ("logout", "Log out")]);
            dlg.set_response_appearance("logout", adw::ResponseAppearance::Destructive);
            dlg.set_default_response(Some("cancel"));
            dlg.set_close_response("cancel");
            let (a, client) = (a.clone(), client.clone());
            dlg.choose(Some(&win), None::<&gtk4::gio::Cancellable>, move |resp| {
                if resp != "logout" { return; }
                a.set_enabled(false);
                let (a, client) = (a.clone(), client.clone());
                // Success shows up as authorizationStateClosed, which the view handles.
                glib::spawn_future_local(async move { if let Err(e) = client.log_out().await { tracing::warn!("log out: {e}"); a.set_enabled(true); } });
            });
        });
    }
    win.application().unwrap().add_action(&logout);
    view.start(early);
}

fn fatal(stack: &gtk4::Stack, msg: &str) -> adw::StatusPage {
    drop_page(stack, "fatal");
    let p = adw::StatusPage::builder().title("Something went wrong").description(glib::markup_escape_text(msg)).icon_name("dialog-error-symbolic").build();
    stack.add_named(&p, Some("fatal"));
    stack.set_visible_child_name("fatal");
    p
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_stylesheet_reads_the_token_the_theme_sets() {
        let token = crate::omarchy::SIDEBAR_OPACITY;
        assert!(super::CSS.contains(&format!(":root {{ {token}: 1; }}")));
        assert_eq!(super::CSS.matches(&format!("var({token})")).count(), 2);
    }
}
