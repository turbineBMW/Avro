//! One TDLib client: requests out, responses and updates back.
//!
//! Every request carries a numeric `@extra`; TDLib echoes it on the response, which is how
//! a reply finds its caller. Everything without an `@extra` is an update and goes out on the
//! event channel. TDLib's receive call serves all clients and must stay on a single thread,
//! so one process-wide thread drains it and routes each message by `@client_id`.
//!
//! Requests return plain futures (a oneshot underneath) that need no particular executor,
//! so the GTK thread can await them directly.
use super::ffi::{self, Td};
use super::types::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{message} ({code})")]
    Td { code: i32, message: String },
    #[error("TDLib closed the client")]
    Closed,
    #[error("unexpected reply: {0}")]
    Decode(#[from] serde_json::Error),
}

impl Error {
    /// TDLib's error text, e.g. `PHONE_CODE_INVALID`.
    pub fn td_message(&self) -> Option<&str> { if let Self::Td { message, .. } = self { Some(message) } else { None } }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Client {
    id: i32,
    td: &'static Td,
    next_extra: AtomicU64,
    pending: Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>,
    events: async_channel::Sender<Update>,
}

fn registry() -> &'static Mutex<HashMap<i32, Weak<Client>>> {
    static R: OnceLock<Mutex<HashMap<i32, Weak<Client>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

/// Quiet TDLib's own logging down to warnings, written to `~/.cache/avro/tdlib.log`.
fn configure_logging(td: &Td) {
    let dir = directories::ProjectDirs::from("dev", "turbinebmw", "avro").map(|d| d.cache_dir().to_path_buf());
    if let Some(dir) = dir {
        let _ = std::fs::create_dir_all(&dir);
        td.execute(&json!({"@type": "setLogStream", "log_stream": {"@type": "logStreamFile", "path": dir.join("tdlib.log"), "max_file_size": 10 << 20, "redirect_stderr": false}}).to_string());
    }
    td.execute(&json!({"@type": "setLogVerbosityLevel", "new_verbosity_level": 2}).to_string());
}

fn start_receiver(td: &'static Td) {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        configure_logging(td);
        std::thread::Builder::new().name("tdlib-receive".into()).spawn(move || loop {
            let Some(raw) = td.receive(30.0) else { continue };
            let mut v: Value = match serde_json::from_str(&raw) { Ok(v) => v, Err(e) => { tracing::warn!("bad JSON from TDLib: {e}"); continue } };
            let Some(id) = v.as_object_mut().and_then(|o| o.remove("@client_id")).and_then(|i| i.as_i64()) else { continue };
            let client = registry().lock().unwrap().get(&(id as i32)).and_then(Weak::upgrade);
            match client { Some(c) => c.dispatch(v), None => tracing::trace!("message for dropped client {id}") }
        }).expect("spawn TDLib receive thread");
    });
}

impl Client {
    /// Create a TDLib client. TDLib answers with `updateAuthorizationState` straight away; the
    /// caller drives authorisation from there.
    pub fn new() -> std::result::Result<(Arc<Self>, async_channel::Receiver<Update>), String> {
        let td = ffi::td()?;
        start_receiver(td);
        let (tx, rx) = async_channel::unbounded();
        let client = Arc::new(Self { id: td.create_client_id(), td, next_extra: AtomicU64::new(1), pending: Default::default(), events: tx });
        registry().lock().unwrap().insert(client.id, Arc::downgrade(&client));
        // A client only comes alive on its first request.
        client.send(json!({"@type": "getOption", "name": "version"}));
        Ok((client, rx))
    }

    fn dispatch(&self, mut v: Value) {
        if let Some(extra) = v.as_object_mut().and_then(|o| o.remove("@extra")) {
            if let Some(tx) = extra.as_u64().and_then(|e| self.pending.lock().unwrap().remove(&e)) { let _ = tx.send(v); }
            return;
        }
        let kind = v.get("@type").and_then(|t| t.as_str()).unwrap_or_default().to_string();
        match serde_json::from_value::<Update>(v) {
            Ok(Update::Other) => {}
            Ok(u) => {
                if let Update::AuthorizationState { authorization_state: AuthState::Closed } = &u { self.fail_pending(); }
                let _ = self.events.send_blocking(u);
            }
            Err(e) => tracing::warn!("could not parse {kind}: {e}"),
        }
    }

    /// Wake every waiting caller with `Closed` once TDLib is gone.
    fn fail_pending(&self) { self.pending.lock().unwrap().clear(); }

    /// Fire and forget.
    pub fn send(&self, req: Value) { self.td.send(self.id, &req.to_string()); }

    /// Send a request and wait for its reply. A TDLib `error` object becomes `Err`.
    pub async fn call(&self, mut req: Value) -> Result<Value> {
        let extra = self.next_extra.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(extra, tx);
        req["@extra"] = json!(extra);
        let method = req["@type"].as_str().unwrap_or_default().to_string();
        tracing::debug!("→ {method}");
        self.td.send(self.id, &req.to_string());
        let v = rx.await.map_err(|_| Error::Closed)?;
        if v["@type"] == "error" {
            let e: TdError = serde_json::from_value(v)?;
            tracing::debug!("← {method}: error {} {}", e.code, e.message);
            return Err(Error::Td { code: e.code, message: e.message });
        }
        Ok(v)
    }

    pub async fn call_as<T: DeserializeOwned>(&self, req: Value) -> Result<T> { Ok(serde_json::from_value(self.call(req).await?)?) }

    async fn ok(&self, req: Value) -> Result<()> { self.call(req).await.map(|_| ()) }

    // ── authorisation ──

    pub async fn set_parameters(&self, creds: &super::auth::ApiCredentials) -> Result<()> { self.ok(creds.tdlib_parameters()).await }
    pub async fn set_phone_number(&self, phone: &str) -> Result<()> {
        self.ok(json!({"@type": "setAuthenticationPhoneNumber", "phone_number": phone, "settings": null})).await
    }
    pub async fn check_code(&self, code: &str) -> Result<()> { self.ok(json!({"@type": "checkAuthenticationCode", "code": code})).await }
    pub async fn check_password(&self, password: &str) -> Result<()> { self.ok(json!({"@type": "checkAuthenticationPassword", "password": password})).await }
    pub async fn set_email(&self, email: &str) -> Result<()> { self.ok(json!({"@type": "setAuthenticationEmailAddress", "email_address": email})).await }
    pub async fn check_email_code(&self, code: &str) -> Result<()> {
        self.ok(json!({"@type": "checkAuthenticationEmailCode", "code": {"@type": "emailAddressAuthenticationCode", "code": code}})).await
    }
    /// Switch to QR login; the link arrives as `authorizationStateWaitOtherDeviceConfirmation`.
    pub async fn request_qr(&self) -> Result<()> { self.ok(json!({"@type": "requestQrCodeAuthentication", "other_user_ids": []})).await }
    /// Abandon a QR login. TDLib can't switch from QR to a phone number in place, so this
    /// closes the client (`authorizationStateClosed`) and the caller starts a fresh one.
    pub async fn cancel_login(&self) -> Result<()> { self.ok(json!({"@type": "logOut"})).await }
    pub async fn log_out(&self) -> Result<()> { self.ok(json!({"@type": "logOut"})).await }
    pub fn close(&self) { self.send(json!({"@type": "close"})); }

    // ── chats ──

    /// Ask TDLib to announce (via `updateNewChat`/`updateChatPosition`) up to `limit` more
    /// chats of the main list. `Ok(false)` once the list is complete.
    pub async fn load_chats(&self, limit: i32) -> Result<bool> {
        match self.ok(json!({"@type": "loadChats", "chat_list": {"@type": "chatListMain"}, "limit": limit})).await {
            Ok(()) => Ok(true),
            Err(Error::Td { code: 404, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }
    pub async fn get_chat(&self, chat_id: i64) -> Result<Chat> { self.call_as(json!({"@type": "getChat", "chat_id": chat_id})).await }
    pub async fn get_user(&self, user_id: i64) -> Result<User> { self.call_as(json!({"@type": "getUser", "user_id": user_id})).await }
    pub async fn get_me(&self) -> Result<User> { self.call_as(json!({"@type": "getMe"})).await }
    pub fn open_chat(&self, chat_id: i64) { self.send(json!({"@type": "openChat", "chat_id": chat_id})); }
    pub fn close_chat(&self, chat_id: i64) { self.send(json!({"@type": "closeChat", "chat_id": chat_id})); }

    /// Up to `limit` messages older than `from_message_id` (0 = from the newest), newest first.
    ///
    /// TDLib answers from its local cache first and may return just a few messages even when
    /// more exist, so keep asking until the page is full or the server says there is no more.
    /// The second value is whether history is exhausted.
    pub async fn history(&self, chat_id: i64, mut from_message_id: i64, limit: i32) -> Result<(Vec<Message>, bool)> {
        let mut out: Vec<Message> = Vec::new();
        for _ in 0..6 {
            let r: Messages = self.call_as(json!({"@type": "getChatHistory", "chat_id": chat_id, "from_message_id": from_message_id,
                "offset": 0, "limit": limit - out.len() as i32, "only_local": false})).await?;
            let page: Vec<Message> = r.messages.into_iter().flatten().filter(|m| !out.iter().any(|o| o.id == m.id)).collect();
            if page.is_empty() { return Ok((out, true)); }
            from_message_id = page.iter().map(|m| m.id).min().unwrap_or(from_message_id);
            out.extend(page);
            if out.len() as i32 >= limit { break; }
        }
        Ok((out, false))
    }

    pub async fn send_text(&self, chat_id: i64, text: &str) -> Result<Message> {
        self.send_content(chat_id, json!({"@type": "inputMessageText", "text": {"@type": "formattedText", "text": text}, "link_preview_options": null, "clear_draft": true})).await
    }

    /// Send a local file; TDLib uploads it. Images go as photos, `.gif`/`.mp4` animations as
    /// GIFs, anything else as a document.
    pub async fn send_file(&self, chat_id: i64, path: &std::path::Path, kind: FileKind, caption: &str) -> Result<Message> {
        let input = json!({"@type": "inputFileLocal", "path": path});
        let caption = json!({"@type": "formattedText", "text": caption});
        let content = match kind {
            FileKind::Photo => json!({"@type": "inputMessagePhoto", "photo": {"@type": "inputPhoto", "photo": input}, "caption": caption}),
            FileKind::Animation => json!({"@type": "inputMessageAnimation", "animation": {"@type": "inputAnimation", "animation": input}, "caption": caption}),
            FileKind::Document => json!({"@type": "inputMessageDocument", "document": {"@type": "inputDocument", "document": input}, "caption": caption}),
        };
        self.send_content(chat_id, content).await
    }

    async fn send_content(&self, chat_id: i64, content: Value) -> Result<Message> {
        self.call_as(json!({"@type": "sendMessage", "chat_id": chat_id, "topic_id": null, "reply_to": null, "options": null, "reply_markup": null, "input_message_content": content})).await
    }

    /// Mark messages as read (they must be on screen, or `force_read` must be set).
    pub async fn view_messages(&self, chat_id: i64, ids: &[i64]) -> Result<()> {
        self.ok(json!({"@type": "viewMessages", "chat_id": chat_id, "message_ids": ids, "source": null, "force_read": true})).await
    }
    pub async fn set_marked_unread(&self, chat_id: i64, unread: bool) -> Result<()> {
        self.ok(json!({"@type": "toggleChatIsMarkedAsUnread", "chat_id": chat_id, "is_marked_as_unread": unread})).await
    }
    pub fn send_typing(&self, chat_id: i64) {
        self.send(json!({"@type": "sendChatAction", "chat_id": chat_id, "topic_id": null, "business_connection_id": "", "action": {"@type": "chatActionTyping"}}));
    }

    /// Remove a chat from the list: leave groups and channels, clear private chats for us only.
    pub async fn delete_chat(&self, chat: &Chat) -> Result<()> {
        match chat.kind {
            ChatType::Supergroup { .. } => self.ok(json!({"@type": "leaveChat", "chat_id": chat.id})).await,
            ChatType::BasicGroup { .. } => {
                self.ok(json!({"@type": "leaveChat", "chat_id": chat.id})).await?;
                self.ok(json!({"@type": "deleteChatHistory", "chat_id": chat.id, "remove_from_chat_list": true, "revoke": false})).await
            }
            _ => self.ok(json!({"@type": "deleteChatHistory", "chat_id": chat.id, "remove_from_chat_list": true, "revoke": false})).await,
        }
    }

    // ── people ──

    pub async fn contacts(&self) -> Result<Vec<i64>> { Ok(self.call_as::<Users>(json!({"@type": "getContacts"})).await?.user_ids) }
    pub async fn search_public_chat(&self, username: &str) -> Result<Chat> {
        self.call_as(json!({"@type": "searchPublicChat", "username": username.trim_start_matches('@')})).await
    }
    pub async fn search_user_by_phone(&self, phone: &str) -> Result<User> {
        self.call_as(json!({"@type": "searchUserByPhoneNumber", "phone_number": phone, "only_local": false})).await
    }
    pub async fn create_private_chat(&self, user_id: i64) -> Result<Chat> {
        self.call_as(json!({"@type": "createPrivateChat", "user_id": user_id, "force": false})).await
    }

    // ── files ──

    /// Download a file and wait for it; TDLib keeps it on disk, so later calls return at once.
    pub async fn download(&self, file_id: i32, priority: i32) -> Result<File> {
        self.call_as(json!({"@type": "downloadFile", "file_id": file_id, "priority": priority, "offset": 0, "limit": 0, "synchronous": true})).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FileKind { Photo, Animation, Document }

impl FileKind {
    /// How Telegram should present a file with this MIME type.
    pub fn for_mime(mime: &str) -> Self {
        match mime {
            "image/jpeg" | "image/png" | "image/webp" => Self::Photo,
            "image/gif" => Self::Animation,
            _ => Self::Document,
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) { registry().lock().unwrap().remove(&self.id); }
}
