//! This app's own preferences (`~/.config/nyx-mail/settings.toml`).

pub use nyx_theme::ThemeMode;

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
    /// Seconds a conversation stays open before it's marked read (with `mark_read`).
    pub mark_read_delay: u32,
    /// Images and other web content load in every message, without asking.
    pub images_always: bool,
    /// The first lines of each message under its subject in the list.
    pub show_previews: bool,
    /// After Archive, Trash or Junk: `next`, `previous` or `list`.
    pub after_moving: String,
    /// Accounts whose folders show in the sidebar (they're folded away under
    /// their heading otherwise); `all` for the every-account mailboxes.
    pub open_accounts: Vec<String>,
    /// The Imbox shows only what was seen before (not what's new first).
    pub imbox_seen: bool,
    /// How the Imbox is shown: `cards` (a page of its own; a conversation opens
    /// over it) or `list` (the list beside the reader, like Paper Trail).
    pub imbox_layout: String,
    /// Folders in the sidebar's Pinned group, as `account` + `\u{1f}` + IMAP name.
    pub pinned: Vec<String>,
    /// Show a mailbox's first conversation in the reading pane on going to it.
    pub open_first: bool,
    /// Other addresses an account sends from (its provider must allow them).
    pub aliases: Vec<Alias>,
    /// Status notices dismissed until their condition clears or changes.
    pub dismissed_status_banners: Vec<String>,
}

/// An address an account can send from besides its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alias {
    pub account: String,
    pub name: String,
    pub email: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            mode: ThemeMode::Omarchy,
            theme: "tokyo-night".into(),
            reduce_motion: false,
            sidebar_collapsed: false,
            glow: true,
            last_section: "imbox".into(),
            mark_read: true,
            remote_senders: Vec::new(),
            signature: String::new(),
            default_account: String::new(),
            mark_read_delay: 0,
            images_always: false,
            show_previews: true,
            after_moving: "next".into(),
            open_accounts: Vec::new(),
            imbox_seen: false,
            imbox_layout: "cards".into(),
            pinned: Vec::new(),
            open_first: true,
            aliases: Vec::new(),
            dismissed_status_banners: Vec::new(),
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
    let p = get();
    p.images_always || p.remote_senders.iter().any(|s| s.eq_ignore_ascii_case(sender))
}

/// How a pinned folder is remembered (ids can change; account and name don't).
pub fn pin_key(account: &str, name: &str) -> String {
    format!("{account}\u{1f}{name}")
}

nyx_core::prefs_store!(Prefs);
