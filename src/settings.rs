//! User preferences, stored as JSON next to auth.json.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Which sound the notification daemon is asked to play. Avro never plays audio itself: the
/// choice travels as a hint on the notification so the shell (and its do-not-disturb logic)
/// stays in charge of whether anything is actually heard.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", content = "path", rename_all = "kebab-case")]
pub enum Sound {
    /// `sound-name = message-new-instant`, resolved from the system sound theme.
    #[default]
    SystemDefault,
    /// `sound-file = <path>`.
    File(PathBuf),
    /// `suppress-sound = true`.
    None,
}

/// What the spam filter does with messages from strangers (see `crate::spam`).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SpamFilter {
    /// Nothing is sent to Claude.
    #[default]
    Off,
    /// Likely spam still notifies, labelled as such.
    Flag,
    /// Likely spam is blocked, reported and archived without a notification.
    Block,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub notification_sound: Sound,
    pub follow_omarchy_theme: bool,
    pub spam_filter: SpamFilter,
    /// Who might legitimately message out of the blue, told to the model so it lets them through.
    pub spam_context: String,
    /// Fade the chat list's background so the desktop shows through (see `sidebar`).
    pub transparent_sidebar: bool,
    /// How opaque that background is, from 0 (clear) to 1 (solid).
    pub sidebar_opacity: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { notification_sound: Sound::default(), follow_omarchy_theme: true, spam_filter: SpamFilter::Off, spam_context: String::new(),
            transparent_sidebar: false, sidebar_opacity: 0.8 }
    }
}

pub fn path() -> PathBuf {
    directories::ProjectDirs::from("dev", "turbinebmw", "avro").map(|d| d.config_dir().join("settings.json")).expect("no config dir")
}

impl Settings {
    pub fn load() -> Self {
        let raw = std::fs::read(path()).ok();
        let mut settings: Self = raw.as_deref().and_then(|b| serde_json::from_slice(b).ok()).unwrap_or_default();
        if settings.adopt_theme_opacity(raw.as_deref(), crate::omarchy::legacy_sidebar_opacity) {
            settings.save();
        }
        settings
    }

    /// The sidebar's opacity used to come from the Omarchy theme's `avro.css`. The first time
    /// settings without `transparent_sidebar` load, a see-through sidebar the theme asked for is
    /// carried over, so nothing changes on screen; after that the theme has no say. Returns
    /// whether the settings need saving.
    fn adopt_theme_opacity(&mut self, raw: Option<&[u8]>, theme_opacity: impl FnOnce() -> Option<f32>) -> bool {
        let migrated = raw
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
            .is_some_and(|v| v.get("transparent_sidebar").is_some());
        if migrated { return false; }
        // Only a followed theme was ever applied.
        if self.follow_omarchy_theme {
            if let Some(opacity) = theme_opacity().filter(|o| *o < 1.0) {
                self.transparent_sidebar = true;
                self.sidebar_opacity = opacity;
            }
        }
        true
    }
    pub fn save(&self) {
        let p = path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Err(e) = serde_json::to_vec_pretty(self).map_err(anyhow::Error::from).and_then(|b| std::fs::write(&p, b).map_err(Into::into)) {
            tracing::warn!("saving settings: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_theme_opacity_is_adopted_once() {
        let old = br#"{"spam_filter":"off"}"#;
        let mut s: Settings = serde_json::from_slice(old).unwrap();
        assert!(!s.transparent_sidebar);
        assert_eq!(s.sidebar_opacity, 0.8);
        assert!(s.adopt_theme_opacity(Some(old), || Some(0.6)));
        assert!(s.transparent_sidebar && s.sidebar_opacity == 0.6);

        // Saved, the theme no longer matters, even if it changes.
        let saved = serde_json::to_vec(&s).unwrap();
        let mut again: Settings = serde_json::from_slice(&saved).unwrap();
        assert!(!again.adopt_theme_opacity(Some(&saved), || panic!("theme read after migration")));
        assert!(again.transparent_sidebar && again.sidebar_opacity == 0.6);

        // An opaque or absent theme value, or an unfollowed theme, leaves it off.
        for (theme, follow) in [(Some(1.0), true), (None, true), (Some(0.5), false)] {
            let mut s = Settings { follow_omarchy_theme: follow, ..Settings::default() };
            assert!(s.adopt_theme_opacity(None, || theme));
            assert!(!s.transparent_sidebar, "{theme:?} {follow}");
        }
    }

    #[test]
    fn existing_preferences_follow_omarchy_and_preserve_sound() {
        let settings: Settings = serde_json::from_str(r#"{"notification_sound":{"kind":"none"}}"#).unwrap();
        assert!(settings.follow_omarchy_theme);
        assert_eq!(settings.notification_sound, Sound::None);
        let opted_out = Settings { follow_omarchy_theme: false, ..settings };
        let saved = serde_json::to_string(&opted_out).unwrap();
        let loaded: Settings = serde_json::from_str(&saved).unwrap();
        assert!(!loaded.follow_omarchy_theme);
        assert_eq!(loaded.notification_sound, Sound::None);
    }
}
