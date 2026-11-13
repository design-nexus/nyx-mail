//! `mail.sqlite`: accounts, mailboxes, messages, attachments, the offline
//! change queue and a full-text index. WAL; `nyx-maild` writes, the window
//! opens it read-only.

use crate::mime::Parsed;
use crate::thread;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

const VERSION: i64 = 3;

const SCHEMA: &str = "
CREATE TABLE accounts (
  id TEXT PRIMARY KEY,            -- the GNOME Online Accounts id
  address TEXT NOT NULL,
  name TEXT NOT NULL DEFAULT '',
  last_sync INTEGER,
  error TEXT
);
CREATE TABLE mailboxes (
  id INTEGER PRIMARY KEY,
  account TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  name TEXT NOT NULL,             -- the IMAP name
  delimiter TEXT,
  role TEXT,                      -- inbox, sent, drafts, trash, junk, archive, all, flagged
  uidvalidity INTEGER,
  uidnext INTEGER,
  highestmodseq INTEGER,
  unread INTEGER NOT NULL DEFAULT 0,
  total INTEGER NOT NULL DEFAULT 0,
  UNIQUE (account, name)
);
CREATE TABLE messages (
  id INTEGER PRIMARY KEY,
  account TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  mailbox INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  uid INTEGER NOT NULL,
  uidvalidity INTEGER NOT NULL,
  modseq INTEGER,
  message_id TEXT,
  gm_msgid INTEGER,
  thread TEXT NOT NULL,
  date INTEGER NOT NULL,
  from_name TEXT NOT NULL DEFAULT '',
  from_addr TEXT NOT NULL DEFAULT '',
  recipients TEXT NOT NULL DEFAULT '[]',   -- JSON: to and cc
  subject TEXT NOT NULL DEFAULT '',
  preview TEXT NOT NULL DEFAULT '',
  flags INTEGER NOT NULL DEFAULT 0,
  labels TEXT NOT NULL DEFAULT '[]',       -- JSON: Gmail labels
  has_attachments INTEGER NOT NULL DEFAULT 0,
  size INTEGER NOT NULL DEFAULT 0,
  raw TEXT,                                -- store name once the body is here
  list_id TEXT,
  UNIQUE (mailbox, uidvalidity, uid)
);
CREATE INDEX messages_list ON messages (mailbox, date DESC);
CREATE INDEX messages_thread ON messages (thread);
CREATE INDEX messages_msgid ON messages (message_id);
CREATE TABLE attachments (
  message INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
  idx INTEGER NOT NULL,
  name TEXT NOT NULL,
  mime TEXT NOT NULL,
  size INTEGER NOT NULL,
  content_id TEXT,
  PRIMARY KEY (message, idx)
);
CREATE TABLE ops (
  id INTEGER PRIMARY KEY,
  account TEXT NOT NULL,
  kind TEXT NOT NULL,             -- flags, move, delete, append, send
  payload TEXT NOT NULL,          -- JSON
  created INTEGER NOT NULL,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_try INTEGER NOT NULL DEFAULT 0,
  error TEXT
);
CREATE TABLE drafts (
  id INTEGER PRIMARY KEY,
  account TEXT NOT NULL,
  data TEXT NOT NULL,             -- JSON
  updated INTEGER NOT NULL
);
CREATE VIRTUAL TABLE messages_fts USING fts5 (
  subject, people, body, content='', contentless_delete=1, tokenize='unicode61 remove_diacritics 2'
);
";

/// Version 2: sorting on this computer (see `organize`).
const MIGRATE_2: &str = "
ALTER TABLE messages ADD COLUMN kind TEXT NOT NULL DEFAULT 'imbox';   -- imbox, feed, paper
ALTER TABLE messages ADD COLUMN unsubscribe TEXT;                     -- List-Unsubscribe
CREATE TABLE senders (
  address TEXT PRIMARY KEY,       -- lowercase
  category TEXT,                  -- the user's choice: imbox, feed, paper
  screened TEXT,                  -- in, out (the Screener)
  updated INTEGER NOT NULL
);
CREATE TABLE snoozes (
  thread TEXT PRIMARY KEY,
  until INTEGER NOT NULL,
  since INTEGER NOT NULL
);
CREATE TABLE followups (
  thread TEXT PRIMARY KEY,
  account TEXT NOT NULL,
  subject TEXT NOT NULL,
  since INTEGER NOT NULL,         -- replies after this count
  due INTEGER NOT NULL
);
CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE INDEX messages_sender ON messages (lower(from_addr), date);
";

/// Version 3: Microsoft 365 accounts, synced over Microsoft Graph (see `graph`).
/// A folder's Graph id and delta link, and each message's Graph id by its UID.
const MIGRATE_3: &str = "
CREATE TABLE graph_folders (
  mailbox INTEGER PRIMARY KEY REFERENCES mailboxes(id) ON DELETE CASCADE,
  folder TEXT NOT NULL,           -- the Graph folder id
  delta TEXT                      -- where the next delta sync starts
);
CREATE TABLE graph_items (
  mailbox INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
  uid INTEGER NOT NULL,           -- given here, in arrival order
  item TEXT NOT NULL,             -- the Graph message id
  PRIMARY KEY (mailbox, uid),
  UNIQUE (mailbox, item)
);
";

/// What a mailbox is for, from SPECIAL-USE or its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    All,
    Flagged,
}

impl Role {
    pub fn id(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Junk => "junk",
            Self::Archive => "archive",
            Self::All => "all",
            Self::Flagged => "flagged",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        [Self::Inbox, Self::Sent, Self::Drafts, Self::Trash, Self::Junk, Self::Archive, Self::All, Self::Flagged]
            .into_iter()
            .find(|r| r.id() == id)
    }

    /// From IMAP SPECIAL-USE attributes (`\Sent`, ...) or, failing that, the name.
    pub fn detect<'a>(name: &str, attributes: impl IntoIterator<Item = &'a str>) -> Option<Self> {
        for attr in attributes {
            let found = match attr.to_ascii_lowercase().as_str() {
                "\\sent" => Some(Self::Sent),
                "\\drafts" => Some(Self::Drafts),
                "\\trash" => Some(Self::Trash),
                "\\junk" => Some(Self::Junk),
                "\\archive" => Some(Self::Archive),
                "\\all" => Some(Self::All),
                "\\flagged" => Some(Self::Flagged),
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
        let last = name.rsplit(['/', '.']).next().unwrap_or(name).to_ascii_lowercase();
        match last.as_str() {
            _ if name.eq_ignore_ascii_case("inbox") => Some(Self::Inbox),
            "sent" | "sent items" | "sent mail" | "sent messages" => Some(Self::Sent),
            "drafts" | "draft" => Some(Self::Drafts),
            "trash" | "deleted items" | "deleted messages" | "bin" => Some(Self::Trash),
            "junk" | "spam" | "junk e-mail" | "bulk mail" => Some(Self::Junk),
            "archive" | "archives" => Some(Self::Archive),
            "all mail" => Some(Self::All),
            "starred" => Some(Self::Flagged),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: i64,
    pub account: String,
    pub name: String,
    pub delimiter: Option<String>,
    pub role: Option<Role>,
    pub uidvalidity: Option<u32>,
    pub uidnext: Option<u32>,
    pub highestmodseq: Option<u64>,
    pub unread: i64,
    pub total: i64,
}

impl Mailbox {
    /// The last part of the name, for display.
    pub fn label(&self) -> String {
        let name = match &self.delimiter {
            Some(d) if !d.is_empty() => self.name.rsplit(d.as_str()).next().unwrap_or(&self.name),
            _ => &self.name,
        };
        match self.role {
            Some(Role::Inbox) => "Inbox".into(),
            _ => name.to_string(),
        }
    }
}

/// A message as it arrives from the server.
#[derive(Clone, Debug, Default)]
pub struct NewMessage {
    pub uid: u32,
    pub uidvalidity: u32,
    pub modseq: Option<u64>,
    pub flags: i64,
    pub labels: Vec<String>,
    pub gm_msgid: Option<u64>,
    pub gm_thrid: Option<u64>,
    pub size: i64,
    /// INTERNALDATE, used when the Date header is missing.
    pub internal_date: Option<i64>,
    /// Headers only, or the whole message.
    pub parsed: Parsed,
    /// The store name, when the whole message was fetched.
    pub raw: Option<String>,
    /// The whole message's plain text, for the index.
    pub body_text: Option<String>,
}

/// A row in a message list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    pub id: i64,
    pub account: String,
    pub mailbox: i64,
    pub thread: String,
    pub date: i64,
    pub from_name: String,
    pub from_addr: String,
    pub subject: String,
    pub preview: String,
    pub flags: i64,
    pub has_attachments: bool,
    /// Messages in the conversation within this view.
    pub count: i64,
    /// Any of them unread.
    pub unread: bool,
}

/// One message in full, for the reader.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRow {
    pub id: i64,
    pub account: String,
    pub mailbox: i64,
    pub uid: u32,
    pub uidvalidity: u32,
    pub message_id: Option<String>,
    pub thread: String,
    pub date: i64,
    pub from_name: String,
    pub from_addr: String,
    pub recipients: Vec<(String, String)>,
    pub subject: String,
    pub flags: i64,
    pub raw: Option<String>,
    pub has_attachments: bool,
}

pub struct Db {
    conn: Connection,
}

impl Db {
    /// Opens (and creates or upgrades) the database for writing.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(SCHEMA)?;
        }
        if version < 2 {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(MIGRATE_2)?;
            // Sort what's already here (bulk headers weren't kept; lists were).
            let rows: Vec<(i64, String, String, Option<String>)> = tx
                .prepare("SELECT id, from_addr, subject, list_id FROM messages")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<_>>()?;
            for (id, from, subject, list_id) in rows {
                let kind = crate::organize::classify(&from, &subject, list_id.as_deref(), None, false);
                tx.execute("UPDATE messages SET kind = ?2 WHERE id = ?1", params![id, kind.id()])?;
            }
            tx.commit()?;
        }
        if version < 3 {
            conn.execute_batch(MIGRATE_3)?;
        }
        if version < VERSION {
            conn.pragma_update(None, "user_version", VERSION)?;
        }
        Ok(Self { conn })
    }

    /// Opens an existing database read-only (the window, Glimpse).
    pub fn open_read_only(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ---------- accounts and mailboxes ----------

    pub fn upsert_account(&self, id: &str, address: &str, name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO accounts (id, address, name) VALUES (?1, ?2, ?3)
             ON CONFLICT (id) DO UPDATE SET address = ?2, name = ?3",
            params![id, address, name],
        )?;
        Ok(())
    }

    pub fn accounts(&self) -> rusqlite::Result<Vec<AccountRow>> {
        let mut stmt = self.conn.prepare("SELECT id, address, name, last_sync, error FROM accounts ORDER BY address")?;
        stmt.query_map([], |r| {
            Ok(AccountRow { id: r.get(0)?, address: r.get(1)?, name: r.get(2)?, last_sync: r.get(3)?, error: r.get(4)? })
        })?
        .collect()
    }

    pub fn set_account_state(&self, id: &str, last_sync: Option<i64>, error: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE accounts SET last_sync = COALESCE(?2, last_sync), error = ?3 WHERE id = ?1",
            params![id, last_sync, error],
        )?;
        Ok(())
    }

    /// Removes an account and everything kept for it; returns store names that
    /// nothing else uses any more (to delete from the store).
    pub fn remove_account(&mut self, id: &str) -> rusqlite::Result<Vec<String>> {
        let tx = self.conn.transaction()?;
        let ids: Vec<i64> = tx
            .prepare("SELECT id FROM messages WHERE account = ?1")?
            .query_map(params![id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for msg in &ids {
            tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![msg])?;
        }
        let raws: Vec<String> = tx
            .prepare("SELECT DISTINCT raw FROM messages WHERE account = ?1 AND raw IS NOT NULL")?
            .query_map(params![id], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        tx.execute("DELETE FROM accounts WHERE id = ?1", params![id])?;
        tx.execute("DELETE FROM ops WHERE account = ?1", params![id])?;
        tx.execute("DELETE FROM drafts WHERE account = ?1", params![id])?;
        let mut orphans = Vec::new();
        for raw in raws {
            let used: i64 = tx.query_row("SELECT count(*) FROM messages WHERE raw = ?1", params![raw], |r| r.get(0))?;
            if used == 0 {
                orphans.push(raw);
            }
        }
        tx.commit()?;
        Ok(orphans)
    }

    pub fn upsert_mailbox(
        &self,
        account: &str,
        name: &str,
        delimiter: Option<&str>,
        role: Option<Role>,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO mailboxes (account, name, delimiter, role) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account, name) DO UPDATE SET delimiter = ?3, role = ?4",
            params![account, name, delimiter, role.map(Role::id)],
        )?;
        self.conn.query_row("SELECT id FROM mailboxes WHERE account = ?1 AND name = ?2", params![account, name], |r| r.get(0))
    }

    /// Drops mailboxes of an account that the server no longer lists.
    pub fn retain_mailboxes(&self, account: &str, names: &[String]) -> rusqlite::Result<usize> {
        let existing = self.mailboxes(Some(account))?;
        let mut removed = 0;
        for mb in existing.iter().filter(|m| !names.contains(&m.name)) {
            self.clear_mailbox(mb.id)?;
            self.conn.execute("DELETE FROM mailboxes WHERE id = ?1", params![mb.id])?;
            removed += 1;
        }
        Ok(removed)
    }

    pub fn mailboxes(&self, account: Option<&str>) -> rusqlite::Result<Vec<Mailbox>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, name, delimiter, role, uidvalidity, uidnext, highestmodseq, unread, total
             FROM mailboxes WHERE ?1 IS NULL OR account = ?1 ORDER BY account, CASE role
               WHEN 'inbox' THEN 0 WHEN 'flagged' THEN 1 WHEN 'drafts' THEN 2 WHEN 'sent' THEN 3 WHEN 'archive' THEN 4
               WHEN 'all' THEN 5 WHEN 'junk' THEN 6 WHEN 'trash' THEN 7 ELSE 8 END, name COLLATE NOCASE",
        )?;
        stmt.query_map(params![account], mailbox_row)?.collect()
    }

    pub fn mailbox(&self, id: i64) -> rusqlite::Result<Option<Mailbox>> {
        self.conn
            .query_row(
                "SELECT id, account, name, delimiter, role, uidvalidity, uidnext, highestmodseq, unread, total FROM mailboxes WHERE id = ?1",
                params![id],
                mailbox_row,
            )
            .optional()
    }

    pub fn mailbox_by_role(&self, account: &str, role: Role) -> rusqlite::Result<Option<Mailbox>> {
        Ok(self.mailboxes(Some(account))?.into_iter().find(|m| m.role == Some(role)))
    }

    pub fn set_mailbox_state(
        &self,
        id: i64,
        uidvalidity: Option<u32>,
        uidnext: Option<u32>,
        highestmodseq: Option<u64>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE mailboxes SET uidvalidity = ?2, uidnext = ?3, highestmodseq = ?4 WHERE id = ?1",
            params![id, uidvalidity, uidnext, highestmodseq.map(|m| m as i64)],
        )?;
        Ok(())
    }

    /// Forgets every message of a mailbox (after UIDVALIDITY changed).
    pub fn clear_mailbox(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM messages_fts WHERE rowid IN (SELECT id FROM messages WHERE mailbox = ?1)", params![id])?;
        self.conn.execute("DELETE FROM messages WHERE mailbox = ?1", params![id])?;
        self.recount(id)
    }

    pub fn recount(&self, mailbox: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE mailboxes SET
               total = (SELECT count(*) FROM messages WHERE mailbox = ?1 AND flags & 16 = 0),
               unread = (SELECT count(*) FROM messages WHERE mailbox = ?1 AND flags & 1 = 0 AND flags & 16 = 0)
             WHERE id = ?1",
            params![mailbox],
        )?;
        Ok(())
    }

    // ---------- messages ----------

    /// The conversation a message with this id belongs to, if stored.
    /// The conversation a message being sent will join (for its follow-up).
    pub fn thread_for(&self, account: &str, message_id: &str, in_reply_to: &[String], references: &[String]) -> String {
        thread::key(Some(message_id), in_reply_to, references, None, |id| self.thread_of(account, id))
    }

    /// Where a message's sender is sorted: the user's choice, else its kind.
    pub fn effective_kind(&self, id: i64) -> rusqlite::Result<Option<crate::organize::Kind>> {
        let kind: Option<String> = self
            .conn
            .query_row(
                "SELECT COALESCE(s.category, m.kind) FROM messages m LEFT JOIN senders s ON s.address = lower(m.from_addr) WHERE m.id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(kind.as_deref().and_then(crate::organize::Kind::from_id))
    }

    fn thread_of(&self, account: &str, message_id: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT thread FROM messages WHERE account = ?1 AND message_id = ?2 LIMIT 1",
                params![account, message_id],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// Stores (or updates) a message; returns its row id.
    pub fn put_message(&self, account: &str, mailbox: i64, m: &NewMessage) -> rusqlite::Result<i64> {
        let p = &m.parsed;
        let existing: Option<(i64, Option<String>)> = self
            .conn
            .query_row(
                "SELECT id, raw FROM messages WHERE mailbox = ?1 AND uidvalidity = ?2 AND uid = ?3",
                params![mailbox, m.uidvalidity, m.uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let thread =
            thread::key(p.message_id.as_deref(), &p.in_reply_to, &p.references, m.gm_thrid, |id| self.thread_of(account, id));
        let recipients: Vec<(String, String)> = p.to.iter().chain(&p.cc).map(|x| (x.name.clone(), x.email.clone())).collect();
        let from = p.from.clone().unwrap_or_default();
        let date = p.date.or(m.internal_date).unwrap_or(0);
        let id = match existing {
            Some((id, old_raw)) => {
                self.conn.execute(
                    "UPDATE messages SET modseq = ?2, flags = ?3, labels = ?4, raw = COALESCE(?5, raw),
                       preview = CASE WHEN ?6 <> '' THEN ?6 ELSE preview END, has_attachments = ?7 WHERE id = ?1",
                    params![
                        id,
                        m.modseq.map(|v| v as i64),
                        m.flags,
                        serde_json::to_string(&m.labels).unwrap_or_default(),
                        m.raw,
                        p.preview(),
                        p.has_attachments() as i64
                    ],
                )?;
                if m.raw.is_some() && old_raw.is_none() {
                    self.index(id, p, m.body_text.as_deref())?;
                    self.put_attachments(id, p)?;
                }
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO messages (account, mailbox, uid, uidvalidity, modseq, message_id, gm_msgid, thread, date,
                       from_name, from_addr, recipients, subject, preview, flags, labels, has_attachments, size, raw, list_id,
                       kind, unsubscribe)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)",
                    params![
                        account,
                        mailbox,
                        m.uid,
                        m.uidvalidity,
                        m.modseq.map(|v| v as i64),
                        p.message_id,
                        m.gm_msgid.map(|v| v as i64),
                        thread,
                        date,
                        from.name,
                        from.email,
                        serde_json::to_string(&recipients).unwrap_or_default(),
                        p.subject,
                        p.preview(),
                        m.flags,
                        serde_json::to_string(&m.labels).unwrap_or_default(),
                        p.has_attachments() as i64,
                        m.size,
                        m.raw,
                        p.list_id,
                        crate::organize::classify(&from.email, &p.subject, p.list_id.as_deref(), p.list_unsubscribe.as_deref(), p.bulk).id(),
                        p.list_unsubscribe,
                    ],
                )?;
                let id = self.conn.last_insert_rowid();
                self.know_recipients(mailbox, &recipients)?;
                self.index(id, p, m.body_text.as_deref())?;
                if m.raw.is_some() {
                    self.put_attachments(id, p)?;
                }
                id
            }
        };
        Ok(id)
    }

    fn index(&self, id: i64, p: &Parsed, body: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![id])?;
        let body = body.map(str::to_string).unwrap_or_else(|| p.preview());
        self.conn.execute(
            "INSERT INTO messages_fts (rowid, subject, people, body) VALUES (?1, ?2, ?3, ?4)",
            params![id, p.subject, p.people_text(), body],
        )?;
        Ok(())
    }

    fn put_attachments(&self, id: i64, p: &Parsed) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM attachments WHERE message = ?1", params![id])?;
        for a in &p.attachments {
            self.conn.execute(
                "INSERT INTO attachments (message, idx, name, mime, size, content_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, a.index as i64, a.name, a.mime, a.size as i64, a.content_id],
            )?;
        }
        Ok(())
    }

    /// UIDs (and current flags) of a mailbox for one UIDVALIDITY.
    pub fn uids(&self, mailbox: i64, uidvalidity: u32) -> rusqlite::Result<Vec<(u32, i64)>> {
        let mut stmt =
            self.conn.prepare("SELECT uid, flags FROM messages WHERE mailbox = ?1 AND uidvalidity = ?2 ORDER BY uid")?;
        stmt.query_map(params![mailbox, uidvalidity], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
    }

    pub fn set_flags(&self, mailbox: i64, uidvalidity: u32, uid: u32, flags: i64, modseq: Option<u64>) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE messages SET flags = ?4, modseq = COALESCE(?5, modseq) WHERE mailbox = ?1 AND uidvalidity = ?2 AND uid = ?3",
            params![mailbox, uidvalidity, uid, flags, modseq.map(|v| v as i64)],
        )?;
        Ok(())
    }

    /// Changes flags locally by message id (before the server hears about it).
    pub fn change_flags(&self, id: i64, add: i64, remove: i64) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE messages SET flags = (flags | ?2) & ~?3 WHERE id = ?1", params![id, add, remove])?;
        Ok(())
    }

    /// Forgets messages the server no longer has.
    /// Unread messages in these mailboxes (for Mark all as read).
    pub fn unread_ids(&self, mailboxes: &[i64]) -> rusqlite::Result<Vec<i64>> {
        let mut out = Vec::new();
        let mut stmt = self.conn.prepare("SELECT id FROM messages WHERE mailbox = ?1 AND flags & 1 = 0 AND flags & 16 = 0")?;
        for mailbox in mailboxes {
            out.extend(stmt.query_map(params![mailbox], |r| r.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?);
        }
        Ok(out)
    }

    pub fn remove_uids(&self, mailbox: i64, uidvalidity: u32, uids: &[u32]) -> rusqlite::Result<usize> {
        let mut n = 0;
        for uid in uids {
            let id: Option<i64> = self
                .conn
                .query_row(
                    "SELECT id FROM messages WHERE mailbox = ?1 AND uidvalidity = ?2 AND uid = ?3",
                    params![mailbox, uidvalidity, uid],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(id) = id {
                self.remove_message(id)?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn remove_message(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![id])?;
        self.conn.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn message(&self, id: i64) -> rusqlite::Result<Option<MessageRow>> {
        self.conn
            .query_row(
                "SELECT id, account, mailbox, uid, uidvalidity, message_id, thread, date, from_name, from_addr, recipients,
                        subject, flags, raw, has_attachments FROM messages WHERE id = ?1",
                params![id],
                message_row,
            )
            .optional()
    }

    /// Every message of a conversation in the given mailboxes, oldest first.
    pub fn conversation(&self, thread: &str, mailboxes: &[i64]) -> rusqlite::Result<Vec<MessageRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, account, mailbox, uid, uidvalidity, message_id, thread, date, from_name, from_addr, recipients,
                    subject, flags, raw, has_attachments FROM messages WHERE thread = ?1 AND flags & 16 = 0 ORDER BY date",
        )?;
        let all: Vec<MessageRow> = stmt.query_map(params![thread], message_row)?.collect::<Result<_, _>>()?;
        // One copy per message id (Gmail keeps a message in several labels).
        let mut seen = std::collections::HashSet::new();
        Ok(all
            .into_iter()
            .filter(|m| mailboxes.is_empty() || mailboxes.contains(&m.mailbox) || m.message_id.is_some())
            .filter(|m| m.message_id.as_ref().is_none_or(|id| seen.insert(id.clone())))
            .collect())
    }

    /// Conversations in some mailboxes, newest first: one row per conversation.
    /// Conversations in some mailboxes, newest first, leaving out snoozed ones.
    pub fn list(&self, mailboxes: &[i64], limit: usize, offset: usize) -> rusqlite::Result<Vec<ListItem>> {
        self.list_with(mailboxes, &crate::organize::Filter::default(), limit, offset)
    }

    /// Conversations in some mailboxes, newest first: one row per conversation,
    /// sorted by its newest message's kind and sender (`organize`).
    pub fn list_with(
        &self,
        mailboxes: &[i64],
        filter: &crate::organize::Filter,
        limit: usize,
        offset: usize,
    ) -> rusqlite::Result<Vec<ListItem>> {
        use crate::organize::Screen;
        if mailboxes.is_empty() {
            return Ok(Vec::new());
        }
        let ids = mailboxes.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(",");
        let snoozed = if filter.snoozed { "IN" } else { "NOT IN" };
        let since = self.screener_since()?.unwrap_or(i64::MAX);
        // Let in: chosen, written to, or heard from before the Screener began.
        // (COALESCE: a sender with no row has a NULL choice, and NOT NULL is NULL.)
        let cleared = format!("(COALESCE(m.screened, '') = 'in' OR (m.screened IS NULL AND m.first_seen < {since}))");
        let mut wanted = Vec::new();
        if let Some(kind) = filter.kind {
            wanted.push(format!("m.cat = '{}'", kind.id()));
        }
        match filter.screen {
            Screen::Any => {}
            Screen::Cleared => wanted.push(cleared),
            Screen::Pending => wanted.push(format!("m.screened IS NULL AND NOT {cleared}")),
            Screen::Out => wanted.push("COALESCE(m.screened, '') = 'out'".into()),
        }
        if filter.flagged {
            wanted.push("l.flagged = 1".into());
        }
        if filter.unread {
            wanted.push("(l.unread = 1 OR m.thread = ?3)".into());
        }
        let wanted = if wanted.is_empty() { String::new() } else { format!("WHERE {}", wanted.join(" AND ")) };
        let sql = format!(
            "WITH shown AS (
                    SELECT x.*, COALESCE(s.category, x.kind) AS cat, s.screened AS screened,
                           (SELECT min(date) FROM messages f WHERE lower(f.from_addr) = lower(x.from_addr)) AS first_seen
                    FROM messages x LEFT JOIN senders s ON s.address = lower(x.from_addr)
                    WHERE x.mailbox IN ({ids}) AND x.flags & 16 = 0 AND x.thread {snoozed} (SELECT thread FROM snoozes)),
                  latest AS (SELECT thread, max(date) AS date, count(*) AS n, max(flags & 1 = 0) AS unread,
                                 max(flags & 2 <> 0) AS flagged FROM shown GROUP BY thread)
             SELECT m.id, m.account, m.mailbox, m.thread, m.date, m.from_name, m.from_addr, m.subject, m.preview, m.flags,
                    m.has_attachments, l.n, l.unread
             FROM latest l JOIN shown m ON m.thread = l.thread AND m.date = l.date
             {wanted}
             GROUP BY m.thread ORDER BY m.date DESC LIMIT ?1 OFFSET ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let keep = filter.keep.clone().unwrap_or_default();
        if filter.unread {
            stmt.query_map(params![limit as i64, offset as i64, keep], list_row)?.collect()
        } else {
            stmt.query_map(params![limit as i64, offset as i64], list_row)?.collect()
        }
    }

    /// People to suggest as recipients: whose name or address contains `query`,
    /// the ones the user wrote to (weighed more) and heard from, most often
    /// first. Mailing lists and no-reply senders are left out.
    pub fn people(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<crate::mime::Person>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let pattern = format!("%{}%", q.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = self.conn.prepare(
            "WITH people AS (
               SELECT from_name AS name, from_addr AS email, 1 AS weight, date FROM messages WHERE list_id IS NULL
               UNION ALL
               SELECT json_extract(j.value, '$[0]'), json_extract(j.value, '$[1]'), 3, m.date
                 FROM messages m JOIN mailboxes b ON b.id = m.mailbox AND b.role = 'sent', json_each(m.recipients) j
             )
             SELECT max(name), max(email) FROM people
             WHERE email <> '' AND (email LIKE ?1 ESCAPE '\\' OR name LIKE ?1 ESCAPE '\\')
               AND email NOT LIKE '%noreply%' AND email NOT LIKE '%no-reply%' AND email NOT LIKE '%donotreply%'
               AND email NOT LIKE '%notifications@%' AND email NOT LIKE 'mailer-daemon@%'
             GROUP BY lower(email) ORDER BY sum(weight) DESC, max(date) DESC LIMIT ?2",
        )?;
        stmt.query_map(params![pattern, limit as i64], |r| Ok(crate::mime::Person { name: r.get(0)?, email: r.get(1)? }))?
            .collect()
    }

    /// Full-text search over every account; newest first.
    pub fn search(&self, query: &str, limit: usize) -> rusqlite::Result<Vec<ListItem>> {
        let fts = fts_query(query);
        if fts.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.account, m.mailbox, m.thread, m.date, m.from_name, m.from_addr, m.subject, m.preview, m.flags,
                    m.has_attachments, 1, m.flags & 1 = 0
             FROM messages_fts f JOIN messages m ON m.id = f.rowid
             WHERE messages_fts MATCH ?1 AND m.flags & 16 = 0 ORDER BY m.date DESC LIMIT ?2",
        )?;
        stmt.query_map(params![fts, limit as i64], list_row)?.collect()
    }

    /// Messages whose bodies aren't here yet, newest first (for offline reading).
    pub fn missing_bodies(&self, mailbox: i64, since: i64, limit: usize) -> rusqlite::Result<Vec<(u32, u32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT uid, uidvalidity FROM messages WHERE mailbox = ?1 AND raw IS NULL AND date >= ?2 AND size < 26214400
             ORDER BY date DESC LIMIT ?3",
        )?;
        stmt.query_map(params![mailbox, since, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
    }

    pub fn attachments(&self, message: i64) -> rusqlite::Result<Vec<crate::mime::Attachment>> {
        let mut stmt =
            self.conn.prepare("SELECT idx, name, mime, size, content_id FROM attachments WHERE message = ?1 ORDER BY idx")?;
        stmt.query_map(params![message], |r| {
            let size: i64 = r.get(3)?;
            Ok(crate::mime::Attachment {
                index: r.get::<_, i64>(0)? as usize,
                name: r.get(1)?,
                mime: r.get(2)?,
                size: size as usize,
                content_id: r.get(4)?,
                inline: false,
            })
        })?
        .collect()
    }

    // ---------- drafts (kept here while being written) ----------

    /// Saves a draft (new when `id` is `None`); returns its id.
    pub fn save_draft(&self, id: Option<i64>, account: &str, data: &serde_json::Value, now: i64) -> rusqlite::Result<i64> {
        match id {
            Some(id) => {
                let n = self.conn.execute(
                    "UPDATE drafts SET account = ?2, data = ?3, updated = ?4 WHERE id = ?1",
                    params![id, account, data.to_string(), now],
                )?;
                if n == 1 {
                    return Ok(id);
                }
                self.conn.execute(
                    "INSERT INTO drafts (id, account, data, updated) VALUES (?1, ?2, ?3, ?4)",
                    params![id, account, data.to_string(), now],
                )?;
                Ok(id)
            }
            None => {
                self.conn.execute(
                    "INSERT INTO drafts (account, data, updated) VALUES (?1, ?2, ?3)",
                    params![account, data.to_string(), now],
                )?;
                Ok(self.conn.last_insert_rowid())
            }
        }
    }

    pub fn drafts(&self) -> rusqlite::Result<Vec<(i64, String, serde_json::Value, i64)>> {
        let mut stmt = self.conn.prepare("SELECT id, account, data, updated FROM drafts ORDER BY updated DESC")?;
        stmt.query_map([], |r| {
            let data: String = r.get(2)?;
            Ok((r.get(0)?, r.get(1)?, serde_json::from_str(&data).unwrap_or_default(), r.get(3)?))
        })?
        .collect()
    }

    pub fn delete_draft(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM drafts WHERE id = ?1", params![id])?;
        Ok(())
    }

    // ---------- the offline change queue ----------

    pub fn queue(&self, account: &str, kind: &str, payload: &serde_json::Value, now: i64) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO ops (account, kind, payload, created, next_try) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![account, kind, payload.to_string(), now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Queued changes for an account that are due, oldest first.
    pub fn due_ops(&self, account: &str, now: i64) -> rusqlite::Result<Vec<Op>> {
        let mut stmt =
            self.conn.prepare("SELECT id, kind, payload, attempts FROM ops WHERE account = ?1 AND next_try <= ?2 ORDER BY id")?;
        stmt.query_map(params![account, now], |r| {
            let payload: String = r.get(2)?;
            Ok(Op {
                id: r.get(0)?,
                kind: r.get(1)?,
                payload: serde_json::from_str(&payload).unwrap_or_default(),
                attempts: r.get(3)?,
            })
        })?
        .collect()
    }

    pub fn op_done(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM ops WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Records a failed attempt; tries again after `retry_at`.
    pub fn op_failed(&self, id: i64, error: &str, retry_at: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE ops SET attempts = attempts + 1, error = ?2, next_try = ?3 WHERE id = ?1",
            params![id, error, retry_at],
        )?;
        Ok(())
    }

    pub fn pending_ops(&self) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT count(*) FROM ops", [], |r| r.get(0))
    }
}

/// An account as Mail keeps it (the account itself is GOA's).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRow {
    pub id: String,
    pub address: String,
    pub name: String,
    pub last_sync: Option<i64>,
    pub error: Option<String>,
}

/// A queued change.
#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    pub id: i64,
    pub kind: String,
    pub payload: serde_json::Value,
    pub attempts: i64,
}

/// Words the user typed as an FTS5 query: each word a prefix match, all
/// required; quotes and operators taken literally.
pub fn fts_query(input: &str) -> String {
    input
        .split_whitespace()
        .map(|w| {
            w.chars().filter(|c| c.is_alphanumeric() || *c == '@' || *c == '.' || *c == '-' || *c == '_').collect::<String>()
        })
        .filter(|w| !w.is_empty())
        .map(|w| format!("\"{}\"*", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn mailbox_row(r: &rusqlite::Row) -> rusqlite::Result<Mailbox> {
    let role: Option<String> = r.get(4)?;
    let modseq: Option<i64> = r.get(7)?;
    Ok(Mailbox {
        id: r.get(0)?,
        account: r.get(1)?,
        name: r.get(2)?,
        delimiter: r.get(3)?,
        role: role.as_deref().and_then(Role::from_id),
        uidvalidity: r.get(5)?,
        uidnext: r.get(6)?,
        highestmodseq: modseq.map(|m| m as u64),
        unread: r.get(8)?,
        total: r.get(9)?,
    })
}

fn message_row(r: &rusqlite::Row) -> rusqlite::Result<MessageRow> {
    let recipients: String = r.get(10)?;
    Ok(MessageRow {
        id: r.get(0)?,
        account: r.get(1)?,
        mailbox: r.get(2)?,
        uid: r.get(3)?,
        uidvalidity: r.get(4)?,
        message_id: r.get(5)?,
        thread: r.get(6)?,
        date: r.get(7)?,
        from_name: r.get(8)?,
        from_addr: r.get(9)?,
        recipients: serde_json::from_str(&recipients).unwrap_or_default(),
        subject: r.get(11)?,
        flags: r.get(12)?,
        raw: r.get(13)?,
        has_attachments: r.get(14)?,
    })
}

fn list_row(r: &rusqlite::Row) -> rusqlite::Result<ListItem> {
    Ok(ListItem {
        id: r.get(0)?,
        account: r.get(1)?,
        mailbox: r.get(2)?,
        thread: r.get(3)?,
        date: r.get(4)?,
        from_name: r.get(5)?,
        from_addr: r.get(6)?,
        subject: r.get(7)?,
        preview: r.get(8)?,
        flags: r.get(9)?,
        has_attachments: r.get(10)?,
        count: r.get(11)?,
        unread: r.get(12)?,
    })
}

mod graph;
mod organize;
pub use organize::{BODY_DAYS, CHECK_EVERY, DISABLED_ACCOUNTS, FollowUp, NOTIFY, Scheduled, SenderRow};

#[cfg(test)]
mod tests;
