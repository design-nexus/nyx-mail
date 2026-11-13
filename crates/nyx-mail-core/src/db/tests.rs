use super::*;
use crate::flags::{FLAGGED, SEEN};
use crate::mime::{Parsed, Person};

fn db() -> (Db, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("nyx-mail-db-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    (Db::open(&dir.join("mail.sqlite")).unwrap(), dir)
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn msg(uid: u32, id: &str, reply_to: Option<&str>, subject: &str, date: i64, body: &str) -> NewMessage {
    NewMessage {
        uid,
        uidvalidity: 7,
        flags: 0,
        parsed: Parsed {
            subject: subject.into(),
            from: Some(Person { name: "Sam Lee".into(), email: "sam@example.com".into() }),
            to: vec![Person { name: String::new(), email: "me@example.com".into() }],
            date: Some(date),
            message_id: Some(id.into()),
            in_reply_to: reply_to.map(|r| vec![r.to_string()]).unwrap_or_default(),
            text: Some(body.into()),
            ..Parsed::default()
        },
        body_text: Some(body.into()),
        raw: Some(format!("raw-{uid}")),
        ..NewMessage::default()
    }
}

#[test]
fn conversations_list_search_and_flags() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "Me").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", Some("/"), Some(Role::Inbox)).unwrap();
    let sent = db.upsert_mailbox("acc", "Sent", Some("/"), Some(Role::Sent)).unwrap();
    db.put_message("acc", inbox, &msg(1, "a@x", None, "Lunch plans", 100, "Shall we get ramen on Friday?")).unwrap();
    let mut reply = msg(2, "b@x", Some("a@x"), "Re: Lunch plans", 200, "Friday works.");
    reply.flags = SEEN;
    db.put_message("acc", sent, &reply).unwrap();
    db.put_message("acc", inbox, &msg(3, "c@x", None, "Invoice", 150, "Your invoice is attached.")).unwrap();
    db.recount(inbox).unwrap();

    // The reply joins the first message's conversation.
    let a = db.message(1).unwrap().unwrap();
    let b = db.message(2).unwrap().unwrap();
    assert_eq!(a.thread, b.thread);

    // Inbox and Sent together show two conversations, the lunch one newest with two messages.
    let both = db.list(&[inbox, sent], 50, 0).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!((both[0].subject.as_str(), both[0].count, both[0].unread), ("Re: Lunch plans", 2, true));
    assert_eq!(db.conversation(&a.thread, &[inbox, sent]).unwrap().len(), 2);
    assert_eq!(db.mailbox(inbox).unwrap().unwrap().unread, 2);

    // Full-text search, by body word prefix and by person.
    assert_eq!(db.search("rame", 10).unwrap().iter().map(|m| m.id).collect::<Vec<_>>(), vec![1]);
    assert_eq!(db.search("sam invoice", 10).unwrap().len(), 1);
    assert!(db.search("\"; DROP TABLE messages", 10).unwrap().is_empty());

    // Local flag changes, then the server's view.
    db.change_flags(1, SEEN | FLAGGED, 0).unwrap();
    assert_eq!(db.message(1).unwrap().unwrap().flags, SEEN | FLAGGED);
    db.set_flags(inbox, 7, 1, SEEN, Some(9)).unwrap();
    assert_eq!(db.uids(inbox, 7).unwrap(), vec![(1, SEEN), (3, 0)]);

    // The server lost message 3.
    assert_eq!(db.remove_uids(inbox, 7, &[3]).unwrap(), 1);
    assert!(db.search("invoice", 10).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn updating_a_message_fetches_its_body_once() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    let mut headers = msg(5, "h@x", None, "Report", 10, "");
    headers.raw = None;
    headers.body_text = None;
    headers.parsed.text = None;
    let id = db.put_message("acc", inbox, &headers).unwrap();
    assert_eq!(db.missing_bodies(inbox, 0, 10).unwrap(), vec![(5, 7)]);
    let full = msg(5, "h@x", None, "Report", 10, "Quarterly numbers inside");
    assert_eq!(db.put_message("acc", inbox, &full).unwrap(), id);
    assert!(db.missing_bodies(inbox, 0, 10).unwrap().is_empty());
    assert_eq!(db.search("quarterly", 5).unwrap().len(), 1);
    assert_eq!(db.message(id).unwrap().unwrap().raw.as_deref(), Some("raw-5"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn uidvalidity_reset_and_account_removal() {
    let (mut db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    db.upsert_account("other", "o@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    let other = db.upsert_mailbox("other", "INBOX", None, Some(Role::Inbox)).unwrap();
    db.put_message("acc", inbox, &msg(1, "a@x", None, "One", 1, "x")).unwrap();
    let mut shared = msg(1, "s@x", None, "Shared", 1, "x");
    shared.raw = Some("raw-shared".into());
    db.put_message("acc", inbox, &NewMessage { uid: 2, ..shared.clone() }).unwrap();
    db.put_message("other", other, &shared).unwrap();

    db.clear_mailbox(inbox).unwrap();
    assert!(db.list(&[inbox], 10, 0).unwrap().is_empty());
    db.put_message("acc", inbox, &NewMessage { uid: 2, ..shared.clone() }).unwrap();
    db.put_message("acc", inbox, &msg(4, "only@x", None, "Mine", 1, "x")).unwrap();

    // Removing an account frees only the raw files nothing else uses.
    let orphans = db.remove_account("acc").unwrap();
    assert_eq!(orphans, vec!["raw-4".to_string()]);
    assert!(db.mailboxes(Some("acc")).unwrap().is_empty());
    assert_eq!(db.list(&[other], 10, 0).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mailboxes_by_role_and_retain() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    db.upsert_mailbox("acc", "[Gmail]/Trash", Some("/"), Role::detect("[Gmail]/Trash", ["\\Trash"])).unwrap();
    db.upsert_mailbox("acc", "INBOX", Some("/"), Role::detect("INBOX", [])).unwrap();
    db.upsert_mailbox("acc", "Projects/Nyx", Some("/"), Role::detect("Projects/Nyx", [])).unwrap();
    let names: Vec<String> = db.mailboxes(Some("acc")).unwrap().iter().map(|m| m.label()).collect();
    assert_eq!(names, vec!["Inbox", "Trash", "Nyx"]);
    assert!(db.mailbox_by_role("acc", Role::Trash).unwrap().is_some());
    assert_eq!(db.retain_mailboxes("acc", &["INBOX".into()]).unwrap(), 2);
    assert_eq!(db.mailboxes(Some("acc")).unwrap().len(), 1);
    assert_eq!(Role::detect("Sent Items", []), Some(Role::Sent));
    assert_eq!(Role::detect("Bulk Mail", []), Some(Role::Junk));
    assert_eq!(Role::detect("Receipts", []), None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn queued_changes_retry_later() {
    let (db, dir) = db();
    let id = db.queue("acc", "flags", &serde_json::json!({"uid": 1}), 100).unwrap();
    assert_eq!(db.due_ops("acc", 100).unwrap().len(), 1);
    db.op_failed(id, "offline", 160).unwrap();
    assert!(db.due_ops("acc", 120).unwrap().is_empty());
    let due = db.due_ops("acc", 160).unwrap();
    assert_eq!((due[0].attempts, due[0].payload["uid"].as_i64()), (1, Some(1)));
    db.op_done(id).unwrap();
    assert_eq!(db.pending_ops().unwrap(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn fts_queries_are_literal() {
    assert_eq!(fts_query("sam lunch"), "\"sam\"* \"lunch\"*");
    assert_eq!(fts_query("a OR \"b\""), "\"a\"* \"OR\"* \"b\"*");
    assert_eq!(fts_query("  "), "");
}

#[test]
fn drafts_save_and_delete() {
    let (db, dir) = db();
    let id = db.save_draft(None, "acc", &serde_json::json!({"subject": "Hi"}), 1).unwrap();
    assert_eq!(db.save_draft(Some(id), "acc", &serde_json::json!({"subject": "Hi there"}), 2).unwrap(), id);
    let all = db.drafts().unwrap();
    assert_eq!((all.len(), all[0].2["subject"].as_str()), (1, Some("Hi there")));
    db.delete_draft(id).unwrap();
    assert!(db.drafts().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn people_to_suggest() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "Me").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    let sent = db.upsert_mailbox("acc", "Sent", None, Some(Role::Sent)).unwrap();
    // Sam wrote twice; I wrote to Samantha once (counts three times).
    db.put_message("acc", inbox, &msg(1, "a@x", None, "Hi", 10, "x")).unwrap();
    db.put_message("acc", inbox, &msg(2, "b@x", None, "Hi again", 20, "x")).unwrap();
    let mut mine = msg(3, "c@x", None, "Plans", 30, "x");
    mine.parsed.from = Some(Person { name: "Me".into(), email: "me@example.com".into() });
    mine.parsed.to = vec![Person { name: "Samantha Ray".into(), email: "sam.ray@example.org".into() }];
    db.put_message("acc", sent, &mine).unwrap();
    let mut robot = msg(4, "d@x", None, "Receipt", 40, "x");
    robot.parsed.from = Some(Person { name: "Shop".into(), email: "noreply@samshop.example".into() });
    db.put_message("acc", inbox, &robot).unwrap();

    let found = db.people("sam", 5).unwrap();
    assert_eq!(found.iter().map(|p| p.email.as_str()).collect::<Vec<_>>(), ["sam.ray@example.org", "sam@example.com"]);
    assert_eq!(found[0].name, "Samantha Ray");
    assert!(db.people("%", 5).unwrap().is_empty(), "wildcards are literal");
    assert!(db.people(" ", 5).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn version_one_databases_are_sorted_on_upgrade() {
    let dir = std::env::temp_dir().join(format!("nyx-mail-migrate-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mail.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO accounts (id, address) VALUES ('a', 'me@example.com');
             INSERT INTO mailboxes (id, account, name, role) VALUES (1, 'a', 'INBOX', 'inbox');
             INSERT INTO messages (account, mailbox, uid, uidvalidity, thread, date, from_addr, subject, list_id)
               VALUES ('a', 1, 1, 1, 't1', 10, 'news@list.example', 'Weekly', '<list.example>'),
                      ('a', 1, 2, 1, 't2', 20, 'noreply@bank.example', 'Statement ready', NULL),
                      ('a', 1, 3, 1, 't3', 30, 'sam@example.com', 'Lunch?', NULL);",
        )
        .unwrap();
    }
    let db = Db::open(&path).unwrap();
    let kinds: Vec<String> = db
        .conn()
        .prepare("SELECT kind FROM messages ORDER BY uid")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|k| k.unwrap())
        .collect();
    assert_eq!(kinds, ["feed", "paper", "imbox"]);
    let version: i64 = db.conn().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, VERSION);
    let _ = std::fs::remove_dir_all(dir);
}

fn from(uid: u32, id: &str, sender: &str, subject: &str, date: i64) -> NewMessage {
    let mut m = msg(uid, id, None, subject, date, "x");
    m.parsed.from = Some(Person { name: String::new(), email: sender.into() });
    m
}

#[test]
fn categories_and_the_screener() {
    use crate::organize::{Filter, Kind, Screen};
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    let sent = db.upsert_mailbox("acc", "Sent", None, Some(Role::Sent)).unwrap();
    let mut news = from(1, "n@x", "news@list.example", "Weekly digest", 100);
    news.parsed.list_id = Some("<list.example>".into());
    db.put_message("acc", inbox, &news).unwrap();
    db.put_message("acc", inbox, &from(2, "r@x", "shop@store.example", "Your receipt", 110)).unwrap();
    db.put_message("acc", inbox, &from(3, "o@x", "old@friend.example", "Hello", 120)).unwrap();
    let kind = |k| {
        db.list_with(&[inbox], &Filter { kind: Some(k), ..Filter::default() }, 10, 0)
            .unwrap()
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(kind(Kind::Feed), ["Weekly digest"]);
    assert_eq!(kind(Kind::Paper), ["Your receipt"]);
    // The user moves the store to the Feed.
    db.set_sender("Shop@Store.example", Some(Some(Kind::Feed)), None, 1).unwrap();
    assert_eq!(kind(Kind::Feed), ["Your receipt", "Weekly digest"]);

    // The Screener starts at 200: earlier senders are in; new ones wait.
    db.set_screener(true, 200).unwrap();
    db.put_message("acc", inbox, &from(4, "s@x", "stranger@new.example", "Quick question", 210)).unwrap();
    let mut mine = from(5, "m@x", "me@example.com", "Plans", 220);
    mine.parsed.to = vec![Person { name: String::new(), email: "Pal@Friend.example".into() }];
    db.put_message("acc", sent, &mine).unwrap();
    db.put_message("acc", inbox, &from(6, "p@x", "pal@friend.example", "Re: Plans", 230)).unwrap();
    let screen = |s| {
        db.list_with(&[inbox], &Filter { screen: s, ..Filter::default() }, 10, 0)
            .unwrap()
            .iter()
            .map(|m| m.subject.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(screen(Screen::Pending), ["Quick question"]);
    assert!(screen(Screen::Cleared).contains(&"Re: Plans".to_string()), "written to: let in");
    assert!(db.screened_out_or_waiting("stranger@new.example").unwrap());
    db.set_sender("stranger@new.example", None, Some(Some("out")), 240).unwrap();
    assert_eq!(screen(Screen::Out), ["Quick question"]);
    assert!(screen(Screen::Pending).is_empty());
    db.set_screener(false, 250).unwrap();
    assert!(!db.screened_out_or_waiting("anyone@new.example").unwrap());

    let senders = db.senders(&[inbox], 10).unwrap();
    let shop = senders.iter().find(|s| s.address == "shop@store.example").unwrap();
    assert_eq!((shop.kind, shop.chosen), (Kind::Feed, true));
    assert_eq!(db.messages_from("SHOP@store.example", &[inbox]).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn snoozes_follow_ups_and_scheduled_mail() {
    use crate::organize::Filter;
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    db.put_message("acc", inbox, &from(1, "a@x", "sam@example.com", "Later", 10)).unwrap();
    let thread = db.message(1).unwrap().unwrap().thread;
    db.snooze(&thread, 500, 20).unwrap();
    assert!(db.list(&[inbox], 10, 0).unwrap().is_empty(), "snoozed: out of the inbox");
    assert_eq!(db.list_with(&[inbox], &Filter { snoozed: true, ..Filter::default() }, 10, 0).unwrap().len(), 1);
    assert_eq!(db.next_due().unwrap(), Some(500));
    assert!(db.wake_snoozes(499).unwrap().is_empty());
    assert_eq!(db.wake_snoozes(500).unwrap(), vec![thread.clone()]);
    assert_eq!(db.list(&[inbox], 10, 0).unwrap().len(), 1);

    // Set aside: only conversations with a flagged message.
    let flagged = Filter { flagged: true, ..Filter::default() };
    assert!(db.list_with(&[inbox], &flagged, 10, 0).unwrap().is_empty());
    db.conn().execute("UPDATE messages SET flags = flags | 2 WHERE uid = 1", []).unwrap();
    assert_eq!(db.list_with(&[inbox], &flagged, 10, 0).unwrap().len(), 1);

    // Follow-ups: one answered by Sam, one only by me.
    let own = vec!["me@example.com".to_string()];
    db.follow_up(&thread, "acc", "Later", 10, 100).unwrap();
    db.follow_up("quiet", "acc", "Anyone?", 10, 100).unwrap();
    db.put_message("acc", inbox, &from(2, "b@x", "me@example.com", "Re: Anyone?", 50)).unwrap();
    db.conn().execute("UPDATE messages SET thread = 'quiet' WHERE uid = 2", []).unwrap();
    db.put_message(
        "acc",
        inbox,
        &NewMessage {
            parsed: crate::mime::Parsed {
                in_reply_to: vec!["a@x".into()],
                ..from(3, "c@x", "sam@example.com", "Re: Later", 60).parsed
            },
            ..from(3, "c@x", "sam@example.com", "Re: Later", 60)
        },
    )
    .unwrap();
    assert!(db.due_follow_ups(99, &own).unwrap().is_empty());
    let due = db.due_follow_ups(100, &own).unwrap();
    assert_eq!(due.iter().map(|f| f.subject.as_str()).collect::<Vec<_>>(), ["Anyone?"]);
    assert!(db.follow_ups().unwrap().is_empty());

    // Mail scheduled for later isn't "waiting"; taking it back removes it.
    let op = db.queue("acc", "send", &serde_json::json!({"raw": "r", "to": ["sam@example.com"], "subject": "Hi"}), 1000).unwrap();
    db.queue("acc", "flags", &serde_json::json!({}), 10).unwrap();
    assert_eq!(db.scheduled(20).unwrap()[0].subject, "Hi");
    assert_eq!(db.waiting_ops(20).unwrap(), 1);
    assert_eq!(db.unschedule(op, 20).unwrap().map(|s| s.at), Some(1000));
    assert!(db.scheduled(20).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stored_mail_is_sorted_again_once() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    let mut m = from(1, "a@x", "hello@shop.example", "Spring sale", 10);
    m.raw = Some("stored".into());
    db.put_message("acc", inbox, &m).unwrap();
    db.conn().execute("UPDATE messages SET kind = 'imbox', unsubscribe = NULL", []).unwrap();
    db.conn().execute("DELETE FROM settings WHERE key = 'sort_version'", []).unwrap();
    let headers = |name: &str| {
        (name == "stored")
            .then(|| crate::mime::Parsed { list_unsubscribe: Some("<https://shop.example/u>".into()), ..Default::default() })
    };
    assert_eq!(db.resort(headers).unwrap(), 1);
    let (kind, unsub): (String, Option<String>) =
        db.conn().query_row("SELECT kind, unsubscribe FROM messages", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!((kind.as_str(), unsub.as_deref()), ("feed", Some("<https://shop.example/u>")));
    assert_eq!(db.resort(headers).unwrap(), 0, "only once per version of the rules");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn engine_settings() {
    let (db, _dir) = db();
    assert!(db.notify_new_mail());
    assert_eq!((db.check_every(), db.body_days()), (5, 30));
    assert!(db.disabled_accounts().is_empty());
    assert!(db.set_engine_setting("notify", "off").unwrap());
    assert!(db.set_engine_setting("check_every", "15").unwrap());
    assert!(db.set_engine_setting("body_days", "90").unwrap());
    assert!(db.set_engine_setting("disabled_accounts", r#"["a"]"#).unwrap());
    assert!(!db.notify_new_mail());
    assert_eq!((db.check_every(), db.body_days()), (15, 90));
    assert_eq!(db.disabled_accounts(), ["a"]);
    // Anything else is refused.
    assert!(!db.set_engine_setting("screener_since", "1").unwrap());
    assert!(!db.set_engine_setting("notify", "maybe").unwrap());
    assert!(!db.set_engine_setting("body_days", "-3").unwrap());
    assert!(!db.notify_new_mail());
}

#[test]
fn unread_only_keeps_the_open_conversation() {
    let (db, _dir) = db();
    db.upsert_account("a", "me@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("a", "INBOX", Some("/"), Some(Role::Inbox)).unwrap();
    let mut read = msg(1, "r@x", None, "Read one", 100, "x");
    read.flags = SEEN;
    db.put_message("a", inbox, &read).unwrap();
    db.put_message("a", inbox, &msg(2, "u@x", None, "Unread one", 200, "x")).unwrap();
    let mut filter = crate::organize::Filter { unread: true, ..Default::default() };
    let subjects = |f: &crate::organize::Filter| -> Vec<String> {
        db.list_with(&[inbox], f, 50, 0).unwrap().into_iter().map(|m| m.subject).collect()
    };
    assert_eq!(subjects(&filter), ["Unread one"]);
    filter.keep = Some(db.list(&[inbox], 50, 0).unwrap().iter().find(|m| m.subject == "Read one").unwrap().thread.clone());
    assert_eq!(subjects(&filter), ["Unread one", "Read one"]);
}
