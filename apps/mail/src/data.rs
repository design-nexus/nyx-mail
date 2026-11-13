//! What the window reads: `mail.sqlite` (read-only; nyx-maild writes it) and
//! the message store.

use nyx_mail_core::db::{AccountRow, Db, FollowUp, ListItem, Mailbox, MessageRow, Role, Scheduled, SenderRow};
use nyx_mail_core::mime::{self, Parsed};
use nyx_mail_core::organize::{Filter, Kind, Screen};
use nyx_mail_core::store::Store;
use nyx_mail_core::{Paths, rusqlite};
use std::cell::RefCell;

thread_local! {
    static DB: RefCell<Option<Db>> = const { RefCell::new(None) };
}

pub fn paths() -> Paths {
    Paths::new(nyx_core::paths::data_dir())
}

/// Runs `f` with the database, opening it on first use (it appears once the
/// engine has run).
fn with<T: Default>(f: impl FnOnce(&Db) -> rusqlite::Result<T>) -> T {
    DB.with(|slot| {
        if slot.borrow().is_none() {
            let path = paths().db();
            if path.exists() {
                *slot.borrow_mut() = Db::open_read_only(&path).ok();
            }
        }
        let borrowed = slot.borrow();
        let Some(db) = borrowed.as_ref() else { return T::default() };
        f(db).unwrap_or_else(|e| {
            eprintln!("nyx-mail: reading mail: {e}");
            T::default()
        })
    })
}

/// What the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// A role across every account (Inbox, Sent…).
    All(Role),
    Mailbox(i64),
    /// Every inbox, one kind of mail.
    Kind(Kind),
    /// Inbox mail from senders not let in yet.
    Screener,
    Snoozed,
    /// Conversations with a flagged message, wherever they are.
    SetAside,
    Scheduled,
    FollowUps,
    Senders,
}

impl View {
    pub fn id(&self) -> String {
        match self {
            Self::All(role) => role.id().to_string(),
            Self::Mailbox(id) => format!("mailbox:{id}"),
            Self::Kind(kind) => kind.id().to_string(),
            Self::Screener => "screener".into(),
            Self::Snoozed => "snoozed".into(),
            Self::SetAside => "set-aside".into(),
            Self::Scheduled => "scheduled".into(),
            Self::FollowUps => "follow-ups".into(),
            Self::Senders => "senders".into(),
        }
    }

    pub fn from_id(id: &str) -> Self {
        if let Some(mb) = id.strip_prefix("mailbox:").and_then(|i| i.parse().ok()) {
            return Self::Mailbox(mb);
        }
        match id {
            "screener" => Self::Screener,
            "snoozed" => Self::Snoozed,
            "set-aside" => Self::SetAside,
            "scheduled" => Self::Scheduled,
            "follow-ups" => Self::FollowUps,
            "senders" => Self::Senders,
            _ => Kind::from_id(id).map(Self::Kind).unwrap_or_else(|| Self::All(Role::from_id(id).unwrap_or(Role::Inbox))),
        }
    }

    /// The mailboxes it covers.
    pub fn mailboxes(&self, all: &[Mailbox]) -> Vec<i64> {
        let role = |r: Role| all.iter().filter(|m| m.role == Some(r)).map(|m| m.id).collect();
        match self {
            Self::All(r) => role(*r),
            Self::Mailbox(id) => vec![*id],
            Self::Kind(_) | Self::Screener | Self::Senders => role(Role::Inbox),
            // Wherever the conversation is (not Trash, Junk or Sent).
            Self::Snoozed | Self::SetAside | Self::FollowUps => all
                .iter()
                .filter(|m| {
                    !matches!(m.role, Some(Role::Trash | Role::Junk | Role::Sent | Role::Drafts | Role::All | Role::Flagged))
                })
                .map(|m| m.id)
                .collect(),
            Self::Scheduled => Vec::new(),
        }
    }

    /// The role of what it shows (a folder has none).
    pub fn role(&self, all: &[Mailbox]) -> Option<Role> {
        match self {
            Self::All(role) => Some(*role),
            Self::Mailbox(id) => all.iter().find(|m| m.id == *id).and_then(|m| m.role),
            Self::Kind(_) | Self::Screener => Some(Role::Inbox),
            _ => None,
        }
    }

    /// What the list leaves out or keeps, beyond its mailboxes.
    pub fn filter(&self, all: &[Mailbox], screener: bool) -> Filter {
        let inbox = self.role(all) == Some(Role::Inbox);
        let screen = if !screener || !inbox {
            Screen::Any
        } else if *self == Self::Screener {
            Screen::Pending
        } else {
            Screen::Cleared
        };
        match self {
            Self::Kind(kind) => Filter { kind: Some(*kind), screen, ..Filter::default() },
            Self::Snoozed => Filter { snoozed: true, ..Filter::default() },
            Self::SetAside => Filter { flagged: true, ..Filter::default() },
            _ => Filter { screen, ..Filter::default() },
        }
    }

    /// Shown as a page of its own (people, cards, senders) rather than a list
    /// beside the reader.
    pub fn is_page(&self) -> bool {
        match self {
            Self::Kind(Kind::Imbox) => crate::prefs::get().imbox_layout != "list",
            Self::Kind(Kind::Feed) | Self::Screener => true,
            _ => false,
        }
    }
}

/// How many conversations a view lists (up to 999), for the sidebar.
pub fn count(all: &[Mailbox], view: &View, unread: bool) -> usize {
    let mut filter = view.filter(all, screener_on());
    filter.unread = unread;
    list_with(&view.mailboxes(all), &filter, 999).len()
}

/// Accounts shown in Mail (not the ones turned off in Settings).
pub fn accounts() -> Vec<AccountRow> {
    let off = disabled_accounts();
    all_accounts().into_iter().filter(|a| !off.contains(&a.id)).collect()
}

/// Every account Mail knows, turned off or not.
pub fn all_accounts() -> Vec<AccountRow> {
    with(|db| db.accounts())
}

/// The mailboxes of the accounts shown.
pub fn mailboxes() -> Vec<Mailbox> {
    let off = disabled_accounts();
    with(|db| db.mailboxes(None)).into_iter().filter(|m| !off.contains(&m.account)).collect()
}

pub fn disabled_accounts() -> Vec<String> {
    with(|db| Ok(db.disabled_accounts()))
}

pub fn unread_ids(mailboxes: &[i64]) -> Vec<i64> {
    with(|db| db.unread_ids(mailboxes))
}

/// The engine's settings, as Settings shows them.
pub struct EngineSettings {
    pub notify: bool,
    pub check_every: u64,
    pub body_days: i64,
}

pub fn engine_settings() -> EngineSettings {
    with(|db| Ok(EngineSettings { notify: db.notify_new_mail(), check_every: db.check_every(), body_days: db.body_days() }))
}

pub fn list(mailboxes: &[i64], limit: usize) -> Vec<ListItem> {
    with(|db| db.list(mailboxes, limit, 0))
}

pub fn list_with(mailboxes: &[i64], filter: &Filter, limit: usize) -> Vec<ListItem> {
    with(|db| db.list_with(mailboxes, filter, limit, 0))
}

pub fn screener_on() -> bool {
    with(|db| db.screener_since().map(|s| s.is_some()))
}

/// Conversations waiting in the Screener.
pub fn screener_count(all: &[Mailbox]) -> usize {
    if !screener_on() {
        return 0;
    }
    list_with(&View::Screener.mailboxes(all), &View::Screener.filter(all, true), 500).len()
}

pub fn senders(inboxes: &[i64]) -> Vec<SenderRow> {
    with(|db| db.senders(inboxes, 500))
}

pub fn messages_from(address: &str, mailboxes: &[i64]) -> Vec<i64> {
    with(|db| db.messages_from(address, mailboxes))
}

pub fn scheduled() -> Vec<Scheduled> {
    with(|db| db.scheduled(crate::window::now()))
}

pub fn follow_ups() -> Vec<FollowUp> {
    with(|db| db.follow_ups())
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self { notify: true, check_every: 5, body_days: 30 }
    }
}

pub fn snoozed_until(thread: &str) -> Option<i64> {
    with(|db| db.snoozed_until(thread))
}

pub fn newest_in_thread(thread: &str) -> Option<ListItem> {
    with(|db| db.newest_in_thread(thread))
}

/// Whether a sender's mail is waiting in the Screener (or was screened out).
pub fn waiting(address: &str) -> bool {
    with(|db| db.screened_out_or_waiting(address))
}

/// The user's choice and guess for a sender, from the sender list.
pub fn sender(address: &str, inboxes: &[i64]) -> Option<SenderRow> {
    senders(inboxes).into_iter().find(|s| s.address.eq_ignore_ascii_case(address))
}

pub fn search(query: &str, limit: usize) -> Vec<ListItem> {
    with(|db| db.search(query, limit))
}

pub fn message(id: i64) -> Option<MessageRow> {
    with(|db| db.message(id))
}

/// A conversation's messages, oldest first. Within a mailbox view, the user's
/// own replies (in Sent) show too.
pub fn conversation(thread: &str, mailboxes: &[i64]) -> Vec<MessageRow> {
    with(|db| db.conversation(thread, mailboxes))
}

/// Local drafts (being written; not yet on the server).
pub fn drafts() -> Vec<(i64, String, serde_json::Value, i64)> {
    with(|db| db.drafts())
}

/// People from the user's mail to suggest as recipients.
pub fn people(query: &str, limit: usize) -> Vec<nyx_mail_core::mime::Person> {
    with(|db| db.people(query, limit))
}

pub fn store() -> Store {
    Store::new(paths().store())
}

pub fn raw(name: &str) -> Option<Vec<u8>> {
    store().get(name).ok()
}

/// A message read from the store, if its body is here.
pub fn parsed(row: &MessageRow) -> Option<(Vec<u8>, Parsed)> {
    let raw = raw(row.raw.as_deref()?)?;
    let parsed = mime::parse(&raw)?;
    Some((raw, parsed))
}

/// Every address the user sends from.
pub fn own_addresses() -> Vec<String> {
    let mut own: Vec<String> = accounts().into_iter().map(|a| a.address).collect();
    own.extend(crate::prefs::get().aliases.into_iter().map(|a| a.email));
    own
}
