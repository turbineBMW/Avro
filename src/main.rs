mod accent;
mod gif;
mod notify;
mod omarchy;
mod rt;
mod settings;
mod tg;
mod ui;

use adw::prelude::*;
use std::collections::HashMap;
use tg::auth::ApiCredentials;
use tg::client::Client;
use tg::types::{AuthState, Chat, Update};

const APP_ID: &str = "dev.turbinebmw.Avro";

fn main() -> anyhow::Result<()> {
    init_logging();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("login") => return rt::block_on(cli_login(args.iter().any(|a| a == "--qr"))),
        Some("probe") => return rt::block_on(cli_probe(args.get(2).and_then(|s| s.parse().ok()))),
        Some("send") => return rt::block_on(cli_send(args.get(2).and_then(|s| s.parse().ok()).ok_or_else(|| anyhow::anyhow!("usage: avro send CHAT_ID text…"))?, args[3..].join(" "))),
        Some("logout") => return rt::block_on(cli_logout()),
        _ => {}
    }
    // Single-instance: TDLib locks its database, so a second launch just raises the first window.
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        if let Some(display) = gtk4::gdk::Display::default() {
            accent::install_fallback(&display);
            omarchy::install(&display);
        }
    });
    app.connect_activate(|app| {
        if let Some(w) = app.active_window() { w.present(); return; }
        gtk4::Window::set_default_icon_name(APP_ID);
        ui::build(app);
    });
    app.run_with_args::<&str>(&[]);
    Ok(())
}

/// stderr at RUST_LOG (default info) + always a debug log at ~/.cache/avro/avro.log.
fn init_logging() {
    use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
    let stderr = tracing_subscriber::fmt::layer().with_writer(std::io::stderr)
        .with_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "avro=info".parse().unwrap()));
    let dir = directories::ProjectDirs::from("dev", "turbinebmw", "avro").map(|d| d.cache_dir().to_path_buf()).unwrap();
    let _ = std::fs::create_dir_all(&dir);
    let file = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("avro.log")).ok();
    let file_layer = file.map(|f| tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f))
        .with_filter(tracing_subscriber::EnvFilter::new("avro=debug")));
    tracing_subscriber::registry().with(stderr).with(file_layer).init();
    tracing::info!("avro {} starting", env!("CARGO_PKG_VERSION"));
}

fn prompt(label: &str) -> anyhow::Result<String> {
    use std::io::Write;
    print!("{label}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line)? == 0 { anyhow::bail!("cancelled"); }
    Ok(line.trim().to_string())
}

/// Like `prompt`, without echoing what is typed.
fn prompt_secret(label: &str) -> anyhow::Result<String> {
    let stty = |arg: &str| std::process::Command::new("stty").arg(arg).stdin(std::process::Stdio::inherit()).status().map(|s| s.success()).unwrap_or(false);
    let hidden = stty("-echo");
    let r = prompt(label);
    if hidden { stty("echo"); println!(); }
    r
}

/// Re-ask until TDLib accepts the answer (a mistyped code leaves the state where it was).
async fn ask_until_ok<F: std::future::Future<Output = tg::client::Result<()>>>(ask: impl Fn() -> anyhow::Result<String>, mut submit: impl FnMut(String) -> F) -> anyhow::Result<()> {
    loop {
        match submit(ask()?).await {
            Ok(()) => return Ok(()),
            Err(e) => eprintln!("  {}", ui::login::explain(&e)),
        }
    }
}

fn credentials_or_prompt() -> anyhow::Result<ApiCredentials> {
    if let Some(c) = ApiCredentials::load() { return Ok(c); }
    println!("Avro needs a Telegram API ID and hash. Create them at https://my.telegram.org → API development tools.");
    loop {
        match ApiCredentials::parse(&prompt("API ID: ")?, &prompt("API hash: ")?) {
            Ok(c) => { c.save()?; println!("saved {}", tg::auth::path().display()); return Ok(c); }
            Err(e) => eprintln!("  {e}"),
        }
    }
}

/// `avro login [--qr]` — headless login: phone number, code and 2FA password at the
/// terminal, or (with `--qr`) a QR code to scan from Telegram on the phone.
async fn cli_login(qr: bool) -> anyhow::Result<()> {
    let creds = credentials_or_prompt()?;
    let (client, events) = Client::new().map_err(anyhow::Error::msg)?;
    loop {
        let Update::AuthorizationState { authorization_state } = events.recv().await? else { continue };
        match authorization_state {
            AuthState::WaitTdlibParameters => client.set_parameters(&creds).await?,
            AuthState::WaitPhoneNumber if qr => client.request_qr().await?,
            AuthState::WaitPhoneNumber => ask_until_ok(|| prompt("Phone number (international, e.g. +15551234567): "), |p| { let c = client.clone(); async move { c.set_phone_number(&p).await } }).await?,
            AuthState::WaitOtherDeviceConfirmation { link } => {
                let code = qrcode::QrCode::new(link.as_bytes())?;
                use qrcode::render::unicode::Dense1x2;
                println!("{}", code.render::<Dense1x2>().dark_color(Dense1x2::Light).light_color(Dense1x2::Dark).quiet_zone(true).build());
                println!("\nOn the phone: Telegram → Settings → Devices → Link Desktop Device, then scan. Waiting…");
            }
            AuthState::WaitCode { code_info } => {
                println!("{}", code_info.delivery());
                ask_until_ok(|| prompt("Code: "), |c| { let cl = client.clone(); async move { cl.check_code(&c).await } }).await?
            }
            AuthState::WaitPassword { password_hint } => {
                let label = if password_hint.is_empty() { "Two-step verification password: ".to_string() } else { format!("Two-step verification password (hint: {password_hint}): ") };
                ask_until_ok(|| prompt_secret(&label), |p| { let c = client.clone(); async move { c.check_password(&p).await } }).await?
            }
            AuthState::WaitEmailAddress => ask_until_ok(|| prompt("Login email address: "), |e| { let c = client.clone(); async move { c.set_email(&e).await } }).await?,
            AuthState::WaitEmailCode => ask_until_ok(|| prompt("Code sent to your email: "), |e| { let c = client.clone(); async move { c.check_email_code(&e).await } }).await?,
            AuthState::Ready => {
                let me = client.get_me().await?;
                println!("logged in as {} (+{}); session in {}", me.full_name(), me.phone_number, tg::auth::database_dir().display());
                return shutdown(&client, &events).await;
            }
            AuthState::WaitRegistration => { shutdown(&client, &events).await?; anyhow::bail!("{}", ui::login::NO_ACCOUNT) }
            AuthState::WaitPremiumPurchase => { shutdown(&client, &events).await?; anyhow::bail!("{}", ui::login::NEEDS_PREMIUM) }
            AuthState::Closed => anyhow::bail!("TDLib closed the session"),
            _ => {}
        }
    }
}

/// Close TDLib cleanly, so its database is flushed before the process exits.
async fn shutdown(client: &Client, events: &async_channel::Receiver<Update>) -> anyhow::Result<()> {
    client.close();
    while let Ok(u) = events.recv().await {
        if matches!(u, Update::AuthorizationState { authorization_state: AuthState::Closed }) { break; }
    }
    Ok(())
}

/// A logged-in client, or an error telling the user to log in first.
async fn connected() -> anyhow::Result<(std::sync::Arc<Client>, async_channel::Receiver<Update>)> {
    let creds = ApiCredentials::load().ok_or_else(|| anyhow::anyhow!("no API credentials; run `avro login`"))?;
    let (client, events) = Client::new().map_err(anyhow::Error::msg)?;
    loop {
        let Update::AuthorizationState { authorization_state } = tokio::time::timeout(std::time::Duration::from_secs(30), events.recv()).await?? else { continue };
        match authorization_state {
            AuthState::WaitTdlibParameters => client.set_parameters(&creds).await?,
            AuthState::Ready => return Ok((client, events)),
            AuthState::Closed => anyhow::bail!("TDLib closed the session"),
            _ => { shutdown(&client, &events).await?; anyhow::bail!("not logged in; run `avro login`") }
        }
    }
}

/// `avro probe [CHAT_ID]` — list chats, or dump recent messages of one, then tail updates.
async fn cli_probe(chat: Option<i64>) -> anyhow::Result<()> {
    let (client, events) = connected().await?;
    let mut chats: HashMap<i64, Chat> = HashMap::new();
    let mut users = HashMap::new();
    let absorb = |u: &Update, chats: &mut HashMap<i64, Chat>, users: &mut HashMap<i64, String>| match u {
        Update::NewChat { chat } => { chats.insert(chat.id, (**chat).clone()); }
        Update::ChatPosition { chat_id, position } => { if let Some(c) = chats.get_mut(chat_id) { c.set_position(position.clone()); } }
        Update::ChatLastMessage { chat_id, last_message, positions } => {
            if let Some(c) = chats.get_mut(chat_id) { c.last_message = last_message.as_deref().cloned(); for p in positions { c.set_position(p.clone()); } }
        }
        Update::User { user } => { users.insert(user.id, user.full_name()); }
        _ => {}
    };
    match chat {
        None => {
            client.load_chats(30).await?;
            while let Ok(u) = events.try_recv() { absorb(&u, &mut chats, &mut users); }
            let mut list: Vec<&Chat> = chats.values().filter(|c| c.main_order().1 != 0).collect();
            list.sort_by_key(|c| std::cmp::Reverse(c.main_order()));
            for c in list {
                let lm = c.last_message.as_ref().map(|m| m.content.summary()).unwrap_or_default().replace('\n', " ");
                let unread = if c.unread_count > 0 { format!("({}) ", c.unread_count) } else { String::new() };
                println!("{:>15}  {:<28}  {unread}{}", c.id, c.title.chars().take(28).collect::<String>(), lm.chars().take(60).collect::<String>());
            }
        }
        Some(id) => {
            let (msgs, _) = client.history(id, 0, 30).await?;
            while let Ok(u) = events.try_recv() { absorb(&u, &mut chats, &mut users); }
            for m in msgs.iter().rev() {
                let who = if m.is_outgoing { "me".to_string() } else { match &m.sender_id {
                    tg::types::MessageSender::User { user_id } => users.get(user_id).cloned().unwrap_or_else(|| user_id.to_string()),
                    tg::types::MessageSender::Chat { chat_id } => chats.get(chat_id).map(|c| c.title.clone()).unwrap_or_default(),
                    _ => String::new(),
                } };
                println!("{}  {:<16} {}", m.date, who.chars().take(16).collect::<String>(), m.content.summary());
            }
        }
    }
    println!("--- tailing updates (Ctrl-C to stop) ---");
    while let Ok(u) = events.recv().await {
        match &u {
            Update::NewMessage { message } => println!("msg {} {}: {}", message.chat_id, if message.is_outgoing { "→" } else { "←" }, message.content.summary()),
            Update::User { .. } | Update::File | Update::UserStatus { .. } => {}
            u => println!("{u:?}"),
        }
    }
    Ok(())
}

async fn cli_send(chat: i64, text: String) -> anyhow::Result<()> {
    if text.trim().is_empty() { anyhow::bail!("nothing to send"); }
    let (client, events) = connected().await?;
    // TDLib needs the chat in memory before it will send to it.
    client.get_chat(chat).await?;
    let pending = client.send_text(chat, &text).await?;
    let result = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(30), events.recv()).await {
            Ok(Ok(Update::MessageSendSucceeded { old_message_id, message })) if old_message_id == pending.id => break Ok(message.id),
            Ok(Ok(Update::MessageSendFailed { old_message_id, error, .. })) if old_message_id == pending.id => break Err(anyhow::anyhow!("send failed: {}", error.message)),
            Ok(Ok(_)) => {}
            Ok(Err(_)) => break Err(anyhow::anyhow!("TDLib went away")),
            Err(_) => break Err(anyhow::anyhow!("no confirmation from Telegram after 30 s")),
        }
    };
    shutdown(&client, &events).await?;
    println!("sent (message {})", result?);
    Ok(())
}

async fn cli_logout() -> anyhow::Result<()> {
    let (client, events) = connected().await?;
    client.log_out().await?;
    while let Ok(u) = events.recv().await {
        if matches!(u, Update::AuthorizationState { authorization_state: AuthState::Closed }) { break; }
    }
    println!("logged out; the local database is gone");
    Ok(())
}
