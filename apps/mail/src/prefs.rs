//! This app's own preferences (`~/.config/nixe-mail/settings.toml`).

pub use nixe_theme::ThemeMode;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub mode: ThemeMode,
    pub theme: String,
    pub reduce_motion: bool,
    pub sidebar_collapsed: bool,
    pub glow: bool,
    /// The mailbox shown last: `inbox`, `sent`… (every account) or `mailbox:<id>`.
    pub last_section: String,
    /// Mark a conversation read when it's opened.
    pub mark_read: bool,
    /// Senders whose images and other web content load without asking.
    pub remote_senders: Vec<String>,
    /// Added under new messages, after "-- ".
    pub signature: String,
    /// The account new messages are from, when there's a choice.
    pub default_account: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            mode: ThemeMode::Omarchy,
            theme: "tokyo-night".into(),
            reduce_motion: false,
            sidebar_collapsed: false,
            glow: true,
            last_section: "inbox".into(),
            mark_read: true,
            remote_senders: Vec::new(),
            signature: String::new(),
            default_account: String::new(),
        }
    }
}

/// Times as "2:05 PM" where the locale uses a 12-hour clock, else "14:05".
pub fn twelve_hour() -> bool {
    let locale =
        ["LC_ALL", "LC_TIME", "LANG"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty())).unwrap_or_default();
    let region = locale.split(['.', '@']).next().unwrap_or("").rsplit('_').next().unwrap_or("").to_string();
    ["US", "CA", "AU", "NZ", "IN", "PH", "PK", "EG", "SA"].contains(&region.as_str())
}

/// Whether web content from this sender loads without asking.
pub fn trusts(sender: &str) -> bool {
    get().remote_senders.iter().any(|s| s.eq_ignore_ascii_case(sender))
}

nixe_core::prefs_store!(Prefs);
