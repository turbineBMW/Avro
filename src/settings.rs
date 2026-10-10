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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub notification_sound: Sound,
    pub follow_omarchy_theme: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { notification_sound: Sound::default(), follow_omarchy_theme: true }
    }
}

pub fn path() -> PathBuf {
    directories::ProjectDirs::from("dev", "turbinebmw", "avro").map(|d| d.config_dir().join("settings.json")).expect("no config dir")
}

impl Settings {
    pub fn load() -> Self {
        std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
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
