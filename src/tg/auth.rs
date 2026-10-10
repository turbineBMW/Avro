//! The app's Telegram API credentials and TDLib's on-disk state.
//!
//! Every Telegram client identifies itself with an `api_id`/`api_hash` pair from
//! my.telegram.org. Avro ships none: the pair comes from `$AVRO_API_ID`/`$AVRO_API_HASH`,
//! from the same names at build time, or from `~/.config/avro/api.json` (written by the
//! first-run page or `avro login`). The account session itself lives in TDLib's database
//! under `~/.local/share/avro/tdlib`, which TDLib owns and locks while a client is open.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiCredentials {
    pub api_id: i32,
    pub api_hash: String,
}

fn dirs() -> directories::ProjectDirs {
    directories::ProjectDirs::from("dev", "turbinebmw", "avro").expect("no home directory")
}

pub fn path() -> PathBuf { dirs().config_dir().join("api.json") }

/// TDLib's database (session keys, chat and message cache).
pub fn database_dir() -> PathBuf { dirs().data_dir().join("tdlib") }

/// Downloaded media. Kept in the cache dir: TDLib can always fetch it again.
pub fn files_dir() -> PathBuf { dirs().cache_dir().join("files") }

impl ApiCredentials {
    /// Check the shape of a pair the user typed: a positive integer id and a 32-hex-digit hash.
    pub fn parse(api_id: &str, api_hash: &str) -> Result<Self, &'static str> {
        let api_id: i32 = api_id.trim().parse().ok().filter(|n| *n > 0).ok_or("The API ID is a number, like 1234567.")?;
        let api_hash = api_hash.trim().to_ascii_lowercase();
        if api_hash.len() != 32 || !api_hash.chars().all(|c| c.is_ascii_hexdigit()) { return Err("The API hash is 32 letters and digits (0–9, a–f)."); }
        Ok(Self { api_id, api_hash })
    }

    pub fn load() -> Option<Self> {
        let from = |id: Option<String>, hash: Option<String>| Self::parse(&id?, &hash?).ok();
        from(std::env::var("AVRO_API_ID").ok(), std::env::var("AVRO_API_HASH").ok())
            .or_else(|| from(option_env!("AVRO_API_ID").map(Into::into), option_env!("AVRO_API_HASH").map(Into::into)))
            .or_else(|| std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()))
    }

    /// Write `api.json` readable by the user only: the hash is not a password, but it is what
    /// Telegram holds the app's owner responsible for.
    pub fn save(&self) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let p = path();
        std::fs::create_dir_all(p.parent().unwrap())?;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p)?;
        f.write_all(&serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    pub fn forget() {
        if let Err(e) = std::fs::remove_file(path()) && e.kind() != std::io::ErrorKind::NotFound { tracing::warn!("removing api.json: {e}"); }
    }

    /// The `setTdlibParameters` request TDLib asks for before anything else.
    pub fn tdlib_parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "@type": "setTdlibParameters",
            "use_test_dc": std::env::var_os("AVRO_TEST_DC").is_some(),
            "database_directory": database_dir(),
            "files_directory": files_dir(),
            "database_encryption_key": "",
            "use_file_database": true,
            "use_chat_info_database": true,
            "use_message_database": true,
            "use_secret_chats": false,
            "api_id": self.api_id,
            "api_hash": self.api_hash,
            "system_language_code": system_language(),
            "device_model": "Avro",
            "system_version": system_version(),
            "application_version": env!("CARGO_PKG_VERSION"),
        })
    }
}

fn system_language() -> String {
    let lang = std::env::var("LANG").unwrap_or_default();
    let code: String = lang.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    if code.is_empty() || code == "C" { "en".into() } else { code }
}

/// "Linux 7.2.5" — shown to the user under Settings → Devices in their other apps.
fn system_version() -> String {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    format!("Linux {}", release.trim().split('-').next().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_checked_for_shape() {
        let c = ApiCredentials::parse(" 1234567 ", "0123456789ABCDEF0123456789abcdef").unwrap();
        assert_eq!(c, ApiCredentials { api_id: 1234567, api_hash: "0123456789abcdef0123456789abcdef".into() });
        assert!(ApiCredentials::parse("abc", "0123456789abcdef0123456789abcdef").is_err());
        assert!(ApiCredentials::parse("0", "0123456789abcdef0123456789abcdef").is_err());
        assert!(ApiCredentials::parse("1", "not-a-hash").is_err());
    }
}
