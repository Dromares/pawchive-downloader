//! User settings. Only the cookie *path* is remembered; cookie contents are never persisted.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The site's stated limits: at most one request per second and five active downloads.
pub const MIN_DELAY_MS: u64 = 1000;
pub const MAX_CONCURRENCY: usize = 5;
pub const DEFAULT_CONCURRENCY: usize = 2;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub concurrency: usize,
    /// Minimum gap between request starts, across all hosts.
    pub delay_ms: u64,
    /// Optional way for the site operator to reach you; appended to the user agent.
    pub contact: String,
    /// Path of a Netscape cookies.txt. Contents are read into memory only.
    pub cookie_file: Option<PathBuf>,
    pub output_dir: Option<PathBuf>,
    pub custom_extensions: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            concurrency: DEFAULT_CONCURRENCY,
            delay_ms: MIN_DELAY_MS,
            contact: String::new(),
            cookie_file: None,
            output_dir: None,
            custom_extensions: String::new(),
        }
    }
}

impl Settings {
    /// Clamp to the site's rules: concurrency 1–5, delay never below one second.
    pub fn normalized(mut self) -> Self {
        self.concurrency = self.concurrency.clamp(1, MAX_CONCURRENCY);
        self.delay_ms = self.delay_ms.max(MIN_DELAY_MS);
        self.contact = self.contact.trim().to_string();
        self
    }

    pub fn delay(&self) -> Duration {
        Duration::from_millis(self.delay_ms.max(MIN_DELAY_MS))
    }

    /// `pawchive-downloader/<version>`, plus ` (<contact>)` when set. Control characters in the
    /// contact string are dropped so it is always a valid header value.
    pub fn user_agent(&self) -> String {
        let base = format!("pawchive-downloader/{}", crate::VERSION);
        let contact: String = self.contact.chars().filter(|c| !c.is_control()).collect();
        let contact = contact.trim();
        if contact.is_empty() {
            base
        } else {
            format!("{base} ({contact})")
        }
    }

    pub fn config_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
        Some(base.join("pawchive-downloader").join("settings.json"))
    }

    pub fn load() -> Self {
        Self::config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Settings>(&t).ok())
            .unwrap_or_default()
            .normalized()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Self::config_path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_to_site_limits() {
        let s = Settings { concurrency: 99, delay_ms: 10, ..Default::default() }.normalized();
        assert_eq!((s.concurrency, s.delay_ms), (5, 1000));
        let s = Settings { concurrency: 0, ..Default::default() }.normalized();
        assert_eq!(s.concurrency, 1);
    }

    #[test]
    fn user_agent_has_version_and_optional_contact() {
        let mut s = Settings::default();
        assert_eq!(s.user_agent(), format!("pawchive-downloader/{}", crate::VERSION));
        s.contact = " me@example.org\r\n".into();
        assert_eq!(s.user_agent(), format!("pawchive-downloader/{} (me@example.org)", crate::VERSION));
    }

    #[test]
    fn partial_json_fills_defaults() {
        let s: Settings = serde_json::from_str(r#"{"concurrency":3}"#).unwrap();
        assert_eq!((s.concurrency, s.delay_ms), (3, 1000));
    }
}
