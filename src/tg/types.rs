//! Serde mirrors of the slice of `td_api.tl` Avro uses.
//!
//! TDLib's JSON names every object by `@type` and its fields exactly as the schema does, so
//! these are plain field-for-field copies. Fields Avro doesn't read are left out (serde skips
//! them), everything is `#[serde(default)]` so a schema bump that drops a field degrades to
//! an empty value instead of a parse failure, and unknown variants land in `Other`.
//! `int64` values travel as JSON strings; `int53`/`int32` as numbers.
use serde::{Deserialize, Deserializer};

/// `int64` fields arrive as strings (`"order": "9221294784512000005"`).
fn i64_str<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N { S(String), I(i64) }
    Ok(match N::deserialize(d)? { N::S(s) => s.parse().map_err(serde::de::Error::custom)?, N::I(i) => i })
}

/// `bytes` fields arrive base64-encoded.
fn b64<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
    use base64::Engine;
    let s = String::deserialize(d)?;
    base64::engine::general_purpose::STANDARD.decode(s).map_err(serde::de::Error::custom)
}

/// Just the `@type` of a polymorphic value whose fields we don't need.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Tag {
    #[serde(rename = "@type", default)]
    pub t: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TdError { pub code: i32, pub message: String }

// ── auth ──

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "@type")]
pub enum AuthState {
    #[serde(rename = "authorizationStateWaitTdlibParameters")] WaitTdlibParameters,
    #[serde(rename = "authorizationStateWaitPhoneNumber")] WaitPhoneNumber,
    #[serde(rename = "authorizationStateWaitEmailAddress")] WaitEmailAddress,
    #[serde(rename = "authorizationStateWaitEmailCode")] WaitEmailCode,
    #[serde(rename = "authorizationStateWaitCode")] WaitCode { #[serde(default)] code_info: CodeInfo },
    #[serde(rename = "authorizationStateWaitOtherDeviceConfirmation")] WaitOtherDeviceConfirmation { link: String },
    #[serde(rename = "authorizationStateWaitRegistration")] WaitRegistration,
    #[serde(rename = "authorizationStateWaitPassword")] WaitPassword { #[serde(default)] password_hint: String },
    #[serde(rename = "authorizationStateWaitPremiumPurchase")] WaitPremiumPurchase,
    #[serde(rename = "authorizationStateReady")] Ready,
    #[serde(rename = "authorizationStateLoggingOut")] LoggingOut,
    #[serde(rename = "authorizationStateClosing")] Closing,
    #[serde(rename = "authorizationStateClosed")] Closed,
    #[serde(other)] Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CodeInfo {
    pub phone_number: String,
    #[serde(rename = "type")]
    pub kind: Tag,
}

impl CodeInfo {
    /// Where the code went, for the code page's description.
    pub fn delivery(&self) -> &'static str {
        match self.kind.t.as_str() {
            "authenticationCodeTypeTelegramMessage" => "We sent the code to your other Telegram app.",
            "authenticationCodeTypeSms" | "authenticationCodeTypeSmsWord" | "authenticationCodeTypeSmsPhrase" => "We sent the code by SMS.",
            "authenticationCodeTypeCall" => "Telegram is calling you with the code.",
            "authenticationCodeTypeFragment" => "The code is waiting for you on fragment.com.",
            _ => "Enter the code Telegram sent you.",
        }
    }
}

// ── files ──

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct File {
    pub id: i32,
    pub size: i64,
    pub expected_size: i64,
    pub local: LocalFile,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct LocalFile {
    pub path: String,
    pub is_downloading_completed: bool,
    pub can_be_downloaded: bool,
}

impl File {
    /// The local copy, if TDLib has finished downloading it.
    pub fn ready_path(&self) -> Option<&str> {
        (self.local.is_downloading_completed && !self.local.path.is_empty()).then_some(self.local.path.as_str())
    }
    pub fn size(&self) -> i64 { if self.size > 0 { self.size } else { self.expected_size } }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Minithumbnail {
    pub width: i32,
    pub height: i32,
    /// A tiny JPEG, inline.
    #[serde(deserialize_with = "b64")]
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Thumbnail { pub format: Tag, pub width: i32, pub height: i32, pub file: File }

// ── users and chats ──

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct User {
    pub id: i64,
    pub first_name: String,
    pub last_name: String,
    pub usernames: Option<Usernames>,
    pub phone_number: String,
    pub status: UserStatus,
    pub profile_photo: Option<ProfilePhoto>,
    pub is_contact: bool,
    #[serde(rename = "type")]
    pub kind: Tag,
}

impl User {
    pub fn full_name(&self) -> String {
        let n = format!("{} {}", self.first_name, self.last_name).trim().to_string();
        if !n.is_empty() { n } else if self.kind.t == "userTypeDeleted" { "Deleted Account".into() } else { self.phone_number.clone() }
    }
    pub fn short_name(&self) -> String { if self.first_name.is_empty() { self.full_name() } else { self.first_name.clone() } }
    pub fn username(&self) -> Option<&str> { self.usernames.as_ref()?.active_usernames.first().map(String::as_str) }
    pub fn is_bot(&self) -> bool { self.kind.t == "userTypeBot" }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Usernames { pub active_usernames: Vec<String> }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "@type")]
pub enum UserStatus {
    #[serde(rename = "userStatusOnline")] Online,
    #[serde(rename = "userStatusOffline")] Offline { #[serde(default)] was_online: i64 },
    #[serde(rename = "userStatusRecently")] Recently,
    #[serde(rename = "userStatusLastWeek")] LastWeek,
    #[serde(rename = "userStatusLastMonth")] LastMonth,
    #[default]
    #[serde(other)] Empty,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ProfilePhoto { pub small: File, pub big: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct BasicGroup { pub id: i64, pub member_count: i32 }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Supergroup { pub id: i64, pub member_count: i32, pub is_channel: bool }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type")]
    pub kind: ChatType,
    pub title: String,
    pub photo: Option<ChatPhotoInfo>,
    pub last_message: Option<Message>,
    pub positions: Vec<ChatPosition>,
    pub unread_count: i32,
    pub unread_mention_count: i32,
    pub last_read_inbox_message_id: i64,
    pub last_read_outbox_message_id: i64,
    pub is_marked_as_unread: bool,
    pub notification_settings: ChatNotificationSettings,
}

impl Chat {
    /// Sort key in the main chat list; 0 means "not in it".
    pub fn main_order(&self) -> (bool, i64) {
        self.positions.iter().find(|p| matches!(p.list, ChatList::Main))
            .map(|p| (p.is_pinned, p.order)).unwrap_or((false, 0))
    }
    /// Apply one position change, as `updateChatPosition` describes: order 0 removes it.
    pub fn set_position(&mut self, pos: ChatPosition) {
        self.positions.retain(|p| p.list != pos.list);
        if pos.order != 0 { self.positions.push(pos); }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "@type")]
pub enum ChatType {
    #[serde(rename = "chatTypePrivate")] Private { user_id: i64 },
    #[serde(rename = "chatTypeBasicGroup")] BasicGroup { basic_group_id: i64 },
    #[serde(rename = "chatTypeSupergroup")] Supergroup { supergroup_id: i64, #[serde(default)] is_channel: bool },
    #[serde(rename = "chatTypeSecret")] Secret { user_id: i64 },
    #[default]
    #[serde(other)] Unknown,
}

impl ChatType {
    pub fn is_group(&self) -> bool { matches!(self, Self::BasicGroup { .. } | Self::Supergroup { is_channel: false, .. }) }
    pub fn is_channel(&self) -> bool { matches!(self, Self::Supergroup { is_channel: true, .. }) }
    pub fn user_id(&self) -> Option<i64> { match self { Self::Private { user_id } | Self::Secret { user_id } => Some(*user_id), _ => None } }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChatPhotoInfo { pub small: File, pub minithumbnail: Option<Minithumbnail> }

#[derive(Clone, Debug, Deserialize)]
pub struct ChatPosition {
    pub list: ChatList,
    #[serde(deserialize_with = "i64_str")]
    pub order: i64,
    #[serde(default)]
    pub is_pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(tag = "@type")]
pub enum ChatList {
    #[serde(rename = "chatListMain")] Main,
    #[serde(rename = "chatListArchive")] Archive,
    #[serde(rename = "chatListFolder")] Folder { chat_folder_id: i32 },
    #[serde(other)] Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ChatNotificationSettings { pub use_default_mute_for: bool, pub mute_for: i32 }

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "@type")]
pub enum NotificationScope {
    #[serde(rename = "notificationSettingsScopePrivateChats")] Private,
    #[serde(rename = "notificationSettingsScopeGroupChats")] Group,
    #[serde(rename = "notificationSettingsScopeChannelChats")] Channel,
    #[serde(other)] Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ScopeNotificationSettings { pub mute_for: i32 }

// ── messages ──

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Message {
    pub id: i64,
    pub sender_id: MessageSender,
    pub chat_id: i64,
    pub sending_state: Option<SendingState>,
    pub is_outgoing: bool,
    /// Unix seconds.
    pub date: i64,
    pub edit_date: i64,
    pub forward_info: Option<ForwardInfo>,
    pub content: Content,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "@type")]
pub enum MessageSender {
    #[serde(rename = "messageSenderUser")] User { user_id: i64 },
    #[serde(rename = "messageSenderChat")] Chat { chat_id: i64 },
    #[default]
    #[serde(other)] Unknown,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "@type")]
pub enum SendingState {
    #[serde(rename = "messageSendingStatePending")] Pending,
    #[serde(rename = "messageSendingStateFailed")] Failed,
    #[serde(other)] Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct ForwardInfo { pub origin: ForwardOrigin }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "@type")]
pub enum ForwardOrigin {
    #[serde(rename = "messageOriginUser")] User { sender_user_id: i64 },
    #[serde(rename = "messageOriginHiddenUser")] HiddenUser { sender_name: String },
    #[serde(rename = "messageOriginChat")] Chat { sender_chat_id: i64 },
    #[serde(rename = "messageOriginChannel")] Channel { chat_id: i64 },
    #[default]
    #[serde(other)] Unknown,
}

/// A message's content plus its raw `@type`, so service messages Avro has no struct for can
/// still be named.
#[derive(Clone, Debug, Default)]
pub struct Content { pub kind: String, pub data: MessageContent }

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = serde_json::Value::deserialize(d)?;
        let kind = v.get("@type").and_then(|t| t.as_str()).unwrap_or_default().to_string();
        let data = serde_json::from_value(v).unwrap_or_else(|e| {
            tracing::debug!("unparsed {kind}: {e}");
            MessageContent::Other
        });
        Ok(Self { kind, data })
    }
}

impl Content {
    /// The text part: the message itself, or a media caption.
    pub fn text(&self) -> Option<&FormattedText> {
        use MessageContent::*;
        match &self.data {
            Text { text } => Some(text),
            Photo { caption, .. } | Document { caption, .. } | Animation { caption, .. } | Video { caption, .. }
            | VoiceNote { caption, .. } | Audio { caption, .. } => Some(caption),
            _ => None,
        }
    }

    /// Whether this is a service message ("X joined the group") rather than something sent.
    pub fn is_service(&self) -> bool {
        use MessageContent::*;
        matches!(self.data, ChatChangeTitle { .. } | ChatAddMembers | ChatDeleteMember | BasicGroupChatCreate { .. }
            | SupergroupChatCreate { .. } | PinMessage | CustomServiceAction { .. })
            || (matches!(self.data, Other) && SERVICE_KINDS.contains(&self.kind.as_str()))
    }

    /// One line for the chat list, a notification, or the terminal: the text if there is
    /// any, otherwise a label for the media ("📷 Photo"), with the caption after it.
    pub fn summary(&self) -> String {
        use MessageContent::*;
        let label = match &self.data {
            Text { text } => return text.text.clone(),
            Photo { .. } => "📷 Photo".to_string(),
            Document { document, .. } => format!("📎 {}", if document.file_name.is_empty() { "File" } else { &document.file_name }),
            Animation { .. } => "GIF".into(),
            Video { .. } => "📹 Video".into(),
            Sticker { sticker } => format!("{} Sticker", sticker.emoji).trim().to_string(),
            VoiceNote { voice_note, .. } => format!("🎤 Voice message ({})", fmt_duration(voice_note.duration)),
            Audio { audio, .. } => format!("🎵 {}", if audio.title.is_empty() { &audio.file_name } else { &audio.title }),
            VideoNote { .. } => "📹 Video message".into(),
            AnimatedEmoji { emoji } => return emoji.clone(),
            Call { is_video, duration } => format!("{} {}call{}", if *is_video { "📹" } else { "📞" }, if *is_video { "Video " } else { "" },
                if *duration > 0 { format!(" ({})", fmt_duration(*duration)) } else { String::new() }),
            Contact { contact } => format!("👤 {} {}", contact.first_name, contact.last_name).trim_end().to_string(),
            Location | Venue => "📍 Location".into(),
            Poll { poll } => format!("📊 {}", poll.question.text),
            ChatChangeTitle { title } => format!("changed the name to “{title}”"),
            ChatAddMembers => "added members".into(),
            ChatDeleteMember => "left the group".into(),
            BasicGroupChatCreate { title } | SupergroupChatCreate { title } => format!("created “{title}”"),
            PinMessage => "pinned a message".into(),
            CustomServiceAction { text } => text.clone(),
            Other => match self.kind.as_str() {
                "messageChatJoinByLink" | "messageChatJoinByRequest" => "joined the group".into(),
                "messageChatChangePhoto" => "changed the group photo".into(),
                "messageChatDeletePhoto" => "removed the group photo".into(),
                "messageContactRegistered" => "joined Telegram".into(),
                "messageScreenshotTaken" => "took a screenshot".into(),
                "messageDice" => "🎲 Dice".into(),
                "messageGame" => "🎮 Game".into(),
                "messageStory" => "Story".into(),
                "messageExpiredPhoto" | "messageExpiredVideo" => "Media expired".into(),
                _ => "Unsupported message".into(),
            },
        };
        match self.text().map(|t| t.text.trim()).filter(|t| !t.is_empty()) {
            Some(caption) => format!("{label} · {caption}"),
            None => label,
        }
    }
}

const SERVICE_KINDS: &[&str] = &["messageChatJoinByLink", "messageChatJoinByRequest", "messageChatChangePhoto", "messageChatDeletePhoto",
    "messageContactRegistered", "messageScreenshotTaken", "messageChatUpgradeTo", "messageChatUpgradeFrom", "messageChatSetMessageAutoDeleteTime"];

pub fn fmt_duration(secs: i32) -> String { format!("{}:{:02}", secs / 60, secs % 60) }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "@type")]
pub enum MessageContent {
    #[serde(rename = "messageText")] Text { text: FormattedText },
    #[serde(rename = "messagePhoto")] Photo { photo: Photo, #[serde(default)] caption: FormattedText },
    #[serde(rename = "messageDocument")] Document { document: Document, #[serde(default)] caption: FormattedText },
    #[serde(rename = "messageAnimation")] Animation { animation: Animation, #[serde(default)] caption: FormattedText },
    #[serde(rename = "messageVideo")] Video { video: Video, #[serde(default)] caption: FormattedText },
    #[serde(rename = "messageSticker")] Sticker { sticker: Sticker },
    #[serde(rename = "messageVoiceNote")] VoiceNote { voice_note: VoiceNote, #[serde(default)] caption: FormattedText },
    #[serde(rename = "messageAudio")] Audio { audio: Audio, #[serde(default)] caption: FormattedText },
    #[serde(rename = "messageVideoNote")] VideoNote { video_note: VideoNote },
    #[serde(rename = "messageAnimatedEmoji")] AnimatedEmoji { emoji: String },
    #[serde(rename = "messageCall")] Call { #[serde(default)] is_video: bool, #[serde(default)] duration: i32 },
    #[serde(rename = "messageContact")] Contact { contact: Contact },
    #[serde(rename = "messageLocation")] Location,
    #[serde(rename = "messageVenue")] Venue,
    #[serde(rename = "messagePoll")] Poll { poll: Poll },
    #[serde(rename = "messageChatChangeTitle")] ChatChangeTitle { title: String },
    #[serde(rename = "messageChatAddMembers")] ChatAddMembers,
    #[serde(rename = "messageChatDeleteMember")] ChatDeleteMember,
    #[serde(rename = "messageBasicGroupChatCreate")] BasicGroupChatCreate { title: String },
    #[serde(rename = "messageSupergroupChatCreate")] SupergroupChatCreate { title: String },
    #[serde(rename = "messagePinMessage")] PinMessage,
    #[serde(rename = "messageCustomServiceAction")] CustomServiceAction { text: String },
    #[default]
    #[serde(other)] Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct FormattedText { pub text: String, pub entities: Vec<TextEntity> }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TextEntity {
    /// Offsets and lengths count UTF-16 code units.
    pub offset: i32,
    pub length: i32,
    #[serde(rename = "type")]
    pub kind: EntityType,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(tag = "@type")]
pub enum EntityType {
    #[serde(rename = "textEntityTypeBold")] Bold,
    #[serde(rename = "textEntityTypeItalic")] Italic,
    #[serde(rename = "textEntityTypeUnderline")] Underline,
    #[serde(rename = "textEntityTypeStrikethrough")] Strikethrough,
    #[serde(rename = "textEntityTypeCode")] Code,
    #[serde(rename = "textEntityTypePre")] Pre,
    #[serde(rename = "textEntityTypePreCode")] PreCode,
    #[serde(rename = "textEntityTypeUrl")] Url,
    #[serde(rename = "textEntityTypeEmailAddress")] Email,
    #[serde(rename = "textEntityTypeTextUrl")] TextUrl { url: String },
    #[serde(rename = "textEntityTypeSpoiler")] Spoiler,
    #[default]
    #[serde(other)] Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Photo { pub minithumbnail: Option<Minithumbnail>, pub sizes: Vec<PhotoSize> }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PhotoSize {
    #[serde(rename = "type")]
    pub kind: String,
    pub photo: File,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Document { pub file_name: String, pub mime_type: String, pub minithumbnail: Option<Minithumbnail>, pub thumbnail: Option<Thumbnail>, pub document: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Animation { pub duration: i32, pub width: i32, pub height: i32, pub file_name: String, pub mime_type: String, pub minithumbnail: Option<Minithumbnail>, pub thumbnail: Option<Thumbnail>, pub animation: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Video { pub duration: i32, pub width: i32, pub height: i32, pub file_name: String, pub mime_type: String, pub minithumbnail: Option<Minithumbnail>, pub thumbnail: Option<Thumbnail>, pub video: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Sticker { pub width: i32, pub height: i32, pub emoji: String, pub format: Tag, pub thumbnail: Option<Thumbnail>, pub sticker: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct VoiceNote { pub duration: i32, pub mime_type: String, pub voice: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Audio { pub duration: i32, pub title: String, pub performer: String, pub file_name: String, pub mime_type: String, pub audio: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct VideoNote { pub duration: i32, pub length: i32, pub minithumbnail: Option<Minithumbnail>, pub thumbnail: Option<Thumbnail>, pub video: File }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Contact { pub phone_number: String, pub first_name: String, pub last_name: String }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Poll { pub question: FormattedText }

// ── updates ──

/// Everything TDLib pushes that Avro reacts to. Responses to requests never land here; the
/// client routes those back to their caller by `@extra`.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "@type")]
pub enum Update {
    #[serde(rename = "updateAuthorizationState")] AuthorizationState { authorization_state: AuthState },
    #[serde(rename = "updateNewChat")] NewChat { chat: Box<Chat> },
    #[serde(rename = "updateChatTitle")] ChatTitle { chat_id: i64, title: String },
    #[serde(rename = "updateChatPhoto")] ChatPhoto { chat_id: i64, #[serde(default)] photo: Option<ChatPhotoInfo> },
    #[serde(rename = "updateChatLastMessage")] ChatLastMessage { chat_id: i64, #[serde(default)] last_message: Option<Box<Message>>, #[serde(default)] positions: Vec<ChatPosition> },
    #[serde(rename = "updateChatDraftMessage")] ChatDraftMessage { chat_id: i64, #[serde(default)] positions: Vec<ChatPosition> },
    #[serde(rename = "updateChatPosition")] ChatPosition { chat_id: i64, position: ChatPosition },
    #[serde(rename = "updateChatReadInbox")] ChatReadInbox { chat_id: i64, last_read_inbox_message_id: i64, unread_count: i32 },
    #[serde(rename = "updateChatReadOutbox")] ChatReadOutbox { chat_id: i64, last_read_outbox_message_id: i64 },
    #[serde(rename = "updateChatIsMarkedAsUnread")] ChatIsMarkedAsUnread { chat_id: i64, is_marked_as_unread: bool },
    #[serde(rename = "updateChatUnreadMentionCount")] ChatUnreadMentionCount { chat_id: i64, unread_mention_count: i32 },
    #[serde(rename = "updateChatNotificationSettings")] ChatNotificationSettings { chat_id: i64, notification_settings: ChatNotificationSettings },
    #[serde(rename = "updateScopeNotificationSettings")] ScopeNotificationSettings { scope: NotificationScope, notification_settings: ScopeNotificationSettings },
    #[serde(rename = "updateNewMessage")] NewMessage { message: Box<Message> },
    #[serde(rename = "updateMessageSendSucceeded")] MessageSendSucceeded { message: Box<Message>, old_message_id: i64 },
    #[serde(rename = "updateMessageSendFailed")] MessageSendFailed { message: Box<Message>, old_message_id: i64, #[serde(default)] error: TdError },
    #[serde(rename = "updateMessageContent")] MessageContent { chat_id: i64, message_id: i64, new_content: Content },
    #[serde(rename = "updateMessageEdited")] MessageEdited { chat_id: i64, message_id: i64, #[serde(default)] edit_date: i64 },
    #[serde(rename = "updateDeleteMessages")] DeleteMessages { chat_id: i64, message_ids: Vec<i64>, #[serde(default)] is_permanent: bool },
    #[serde(rename = "updateUser")] User { user: Box<User> },
    #[serde(rename = "updateUserStatus")] UserStatus { user_id: i64, status: UserStatus },
    #[serde(rename = "updateBasicGroup")] BasicGroup { basic_group: BasicGroup },
    #[serde(rename = "updateSupergroup")] Supergroup { supergroup: Supergroup },
    #[serde(rename = "updateFile")] File,
    #[serde(rename = "updateChatAction")] ChatAction { chat_id: i64, sender_id: MessageSender, action: Tag },
    #[serde(rename = "updateConnectionState")] ConnectionState { state: Tag },
    #[serde(other)] Other,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Messages { pub total_count: i32, pub messages: Vec<Option<Message>> }

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Users { pub user_ids: Vec<i64> }


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_positions_parse_string_orders() {
        let u: Update = serde_json::from_str(r#"{"@type":"updateChatPosition","chat_id":42,"position":{"@type":"chatPosition","list":{"@type":"chatListMain"},"order":"9221294784512000005","is_pinned":true,"source":null}}"#).unwrap();
        let Update::ChatPosition { chat_id, position } = u else { panic!("{u:?}") };
        assert_eq!(chat_id, 42);
        assert_eq!(position.order, 9221294784512000005);
        assert_eq!(position.list, ChatList::Main);
        let mut chat = Chat::default();
        chat.set_position(position.clone());
        assert_eq!(chat.main_order(), (true, 9221294784512000005));
        chat.set_position(ChatPosition { order: 0, ..position });
        assert_eq!(chat.main_order(), (false, 0));
    }

    #[test]
    fn unknown_updates_and_contents_degrade() {
        let u: Update = serde_json::from_str(r#"{"@type":"updateOption","name":"version","value":{"@type":"optionValueString","value":"1.8.67"}}"#).unwrap();
        assert!(matches!(u, Update::Other));
        let m: Message = serde_json::from_str(r#"{"@type":"message","id":7,"chat_id":1,"is_outgoing":false,"date":1,"sender_id":{"@type":"messageSenderUser","user_id":5},"content":{"@type":"messageDice","value":3}}"#).unwrap();
        assert_eq!(m.content.kind, "messageDice");
        assert!(matches!(m.content.data, MessageContent::Other));
        assert!(matches!(m.sender_id, MessageSender::User { user_id: 5 }));
    }

    #[test]
    fn auth_states_and_minithumbnails() {
        let u: Update = serde_json::from_str(r#"{"@type":"updateAuthorizationState","authorization_state":{"@type":"authorizationStateWaitOtherDeviceConfirmation","link":"tg://login?token=abc"}}"#).unwrap();
        assert!(matches!(u, Update::AuthorizationState { authorization_state: AuthState::WaitOtherDeviceConfirmation { ref link } } if link == "tg://login?token=abc"));
        let m: Minithumbnail = serde_json::from_str(r#"{"@type":"minithumbnail","width":40,"height":30,"data":"/9j/"}"#).unwrap();
        assert_eq!(m.data, vec![0xff, 0xd8, 0xff]);
    }
}
