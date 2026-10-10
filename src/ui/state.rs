//! Display helpers over TDLib's objects: times, presence, rich text, and what to draw for a
//! message's media.
use crate::tg::types::*;

pub fn fmt_time(ts_secs: i64) -> String {
    let Ok(dt) = gtk4::glib::DateTime::from_unix_local(ts_secs) else { return String::new() };
    let now = gtk4::glib::DateTime::now_local().unwrap();
    let fmt = if dt.ymd() == now.ymd() { "%H:%M" } else if dt.year() == now.year() { "%-d %b" } else { "%-d %b %Y" };
    dt.format(fmt).map(|s| s.to_string()).unwrap_or_default()
}

/// "online", "last seen 14:02", … for a private chat's subtitle.
pub fn presence(u: &User) -> String {
    if u.is_bot() { return "bot".into(); }
    match &u.status {
        UserStatus::Online => "online".into(),
        UserStatus::Offline { was_online } => format!("last seen {}", fmt_time(*was_online)),
        UserStatus::Recently => "last seen recently".into(),
        UserStatus::LastWeek => "last seen within a week".into(),
        UserStatus::LastMonth => "last seen within a month".into(),
        UserStatus::Empty => String::new(),
    }
}

/// What a chat action (`updateChatAction`) means, in a few words; `None` for "stopped".
pub fn action_text(action: &str) -> Option<&'static str> {
    Some(match action {
        "chatActionCancel" => return None,
        "chatActionRecordingVoiceNote" => "recording a voice message",
        "chatActionRecordingVideo" | "chatActionRecordingVideoNote" => "recording a video",
        "chatActionUploadingPhoto" => "sending a photo",
        "chatActionUploadingVideo" | "chatActionUploadingVideoNote" => "sending a video",
        "chatActionUploadingDocument" | "chatActionUploadingVoiceNote" => "sending a file",
        "chatActionChoosingSticker" => "choosing a sticker",
        _ => "typing",
    })
}

pub fn fmt_size(bytes: i64) -> String {
    match bytes {
        b if b >= 1 << 30 => format!("{:.1} GB", b as f64 / (1u64 << 30) as f64),
        b if b >= 1 << 20 => format!("{:.1} MB", b as f64 / (1u64 << 20) as f64),
        b if b >= 1 << 10 => format!("{} KB", b >> 10),
        b if b > 0 => format!("{b} B"),
        _ => String::new(),
    }
}

/// Telegram's formatted text as Pango markup. Entity offsets count UTF-16 code units, so the
/// text is walked as UTF-16 and each entity becomes a pair of tags around its span.
pub fn to_markup(ft: &FormattedText) -> String {
    let units: Vec<u16> = ft.text.encode_utf16().collect();
    // (position, is_open, span length, tag)
    let mut marks: Vec<(usize, bool, usize, String)> = Vec::new();
    for e in &ft.entities {
        let (open, close) = match &e.kind {
            EntityType::Bold => ("<b>".to_string(), "</b>"),
            EntityType::Italic => ("<i>".into(), "</i>"),
            EntityType::Underline => ("<u>".into(), "</u>"),
            EntityType::Strikethrough => ("<s>".into(), "</s>"),
            EntityType::Code | EntityType::Pre | EntityType::PreCode => ("<tt>".into(), "</tt>"),
            EntityType::Spoiler => ("<span background=\"#888888\" foreground=\"#888888\">".into(), "</span>"),
            EntityType::TextUrl { url } => (format!("<a href=\"{}\">", gtk4::glib::markup_escape_text(url)), "</a>"),
            EntityType::Url => {
                let start = (e.offset as usize).min(units.len());
                let end = ((e.offset + e.length) as usize).min(units.len());
                let raw = String::from_utf16_lossy(&units[start..end]);
                let href = if raw.contains("://") { raw } else { format!("https://{raw}") };
                (format!("<a href=\"{}\">", gtk4::glib::markup_escape_text(&href)), "</a>")
            }
            EntityType::Email => {
                let start = (e.offset as usize).min(units.len());
                let end = ((e.offset + e.length) as usize).min(units.len());
                (format!("<a href=\"mailto:{}\">", gtk4::glib::markup_escape_text(&String::from_utf16_lossy(&units[start..end]))), "</a>")
            }
            EntityType::Other => continue,
        };
        let start = (e.offset.max(0) as usize).min(units.len());
        let end = ((e.offset + e.length).max(0) as usize).min(units.len());
        if end <= start { continue; }
        marks.push((start, true, end - start, open));
        marks.push((end, false, end - start, close.to_string()));
    }
    // At one position: closes before opens, inner spans close first and outer spans open
    // first, so nesting stays well-formed. Pango rejects crossing tags, so if entities
    // overlap we fall back to the plain text below.
    marks.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(if a.1 { b.2.cmp(&a.2) } else { a.2.cmp(&b.2) }));
    let mut out = String::new();
    let mut at = 0;
    for (pos, _, _, tag) in &marks {
        if *pos > at { out.push_str(&gtk4::glib::markup_escape_text(&String::from_utf16_lossy(&units[at..*pos]))); at = *pos; }
        out.push_str(tag);
    }
    out.push_str(&gtk4::glib::markup_escape_text(&String::from_utf16_lossy(&units[at..])));
    // `<a>` is GtkLabel's own tag, unknown to Pango, so check the nesting with spans in its place.
    let probe = out.replace("</a>", "</span>").replace("<a href=", "<span font_desc=");
    if gtk4::pango::parse_markup(&probe, '\0').is_ok() { out } else { gtk4::glib::markup_escape_text(&ft.text).to_string() }
}

/// What to draw for a message's attachment.
#[derive(Clone, Debug)]
pub enum Media {
    /// A still image (photos, static stickers): placeholder, then the full file.
    Image { file: File, mini: Option<Minithumbnail>, width: i32, height: i32, sticker: bool },
    /// Loops silently, like a GIF (Telegram sends GIFs as MP4; video stickers are WebM).
    Animation { file: File, mini: Option<Minithumbnail>, width: i32, height: i32, sticker: bool },
    /// Click to play.
    Video { file: File, mini: Option<Minithumbnail>, thumb: Option<Thumbnail>, width: i32, height: i32, duration: i32, round: bool },
    /// Voice notes and music: a play button.
    Audio { file: File, label: String },
    /// Anything else: click to save to Downloads.
    File { file: File, name: String, size: i64 },
    /// Animated (Lottie) stickers Avro can't render: shown as their emoji.
    Emoji(String),
}

/// The photo size to show: the largest that is at most `max` pixels on its long side.
fn pick_size(sizes: &[PhotoSize], max: i32) -> Option<&PhotoSize> {
    sizes.iter().filter(|s| s.width.max(s.height) <= max).max_by_key(|s| s.width * s.height)
        .or_else(|| sizes.iter().min_by_key(|s| s.width * s.height))
}

pub fn media_of(c: &Content) -> Option<Media> {
    use MessageContent as M;
    Some(match &c.data {
        M::Photo { photo, .. } => {
            let s = pick_size(&photo.sizes, 1280)?;
            Media::Image { file: s.photo.clone(), mini: photo.minithumbnail.clone(), width: s.width, height: s.height, sticker: false }
        }
        M::Sticker { sticker } => match sticker.format.t.as_str() {
            "stickerFormatWebp" => Media::Image { file: sticker.sticker.clone(), mini: None, width: sticker.width, height: sticker.height, sticker: true },
            "stickerFormatWebm" => Media::Animation { file: sticker.sticker.clone(), mini: None, width: sticker.width, height: sticker.height, sticker: true },
            // Lottie: draw the still thumbnail when it's an image, else the emoji.
            _ => match &sticker.thumbnail {
                Some(t) if t.format.t == "thumbnailFormatWebp" || t.format.t == "thumbnailFormatPng" =>
                    Media::Image { file: t.file.clone(), mini: None, width: t.width, height: t.height, sticker: true },
                _ => Media::Emoji(sticker.emoji.clone()),
            },
        },
        M::Animation { animation: a, .. } => Media::Animation { file: a.animation.clone(), mini: a.minithumbnail.clone(), width: a.width, height: a.height, sticker: false },
        M::Video { video: v, .. } => Media::Video { file: v.video.clone(), mini: v.minithumbnail.clone(), thumb: v.thumbnail.clone(), width: v.width, height: v.height, duration: v.duration, round: false },
        M::VideoNote { video_note: v } => Media::Video { file: v.video.clone(), mini: v.minithumbnail.clone(), thumb: v.thumbnail.clone(), width: v.length, height: v.length, duration: v.duration, round: true },
        M::VoiceNote { voice_note: v, .. } => Media::Audio { file: v.voice.clone(), label: format!("Voice message · {}", fmt_duration(v.duration)) },
        M::Audio { audio: a, .. } => {
            let title = if a.title.is_empty() { a.file_name.clone() } else if a.performer.is_empty() { a.title.clone() } else { format!("{} — {}", a.performer, a.title) };
            Media::Audio { file: a.audio.clone(), label: format!("{title} · {}", fmt_duration(a.duration)) }
        }
        M::Document { document: d, .. } => Media::File { file: d.document.clone(), name: if d.file_name.is_empty() { "file".into() } else { d.file_name.clone() }, size: d.document.size() },
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ft(text: &str, entities: Vec<(i32, i32, EntityType)>) -> FormattedText {
        FormattedText { text: text.into(), entities: entities.into_iter().map(|(offset, length, kind)| TextEntity { offset, length, kind }).collect() }
    }

    #[test]
    fn markup_counts_utf16_and_escapes() {
        // "😀" is two UTF-16 units, so "bold" starts at 3, not 2.
        assert_eq!(to_markup(&ft("😀 bold <x>", vec![(3, 4, EntityType::Bold)])), "😀 <b>bold</b> &lt;x&gt;");
        assert_eq!(to_markup(&ft("see example.com", vec![(4, 11, EntityType::Url)])), "see <a href=\"https://example.com\">example.com</a>");
        assert_eq!(to_markup(&ft("nested", vec![(0, 6, EntityType::Bold), (0, 3, EntityType::Italic)])), "<b><i>nes</i>ted</b>");
    }

    #[test]
    fn crossing_entities_fall_back_to_plain_text() {
        assert_eq!(to_markup(&ft("abcdef", vec![(0, 4, EntityType::Bold), (2, 4, EntityType::Italic)])), "abcdef");
    }
}
