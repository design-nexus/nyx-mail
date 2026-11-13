//! Right-click menus: conversations and the list's other rows, folders, the
//! all-accounts mailboxes and account headings. Each item does what its
//! button or key does; the Menu key (or Shift+F10) opens the selected row's.

use crate::compose::{self, Start};
use crate::data::{self, View};
use crate::widgets::{self, MenuEntry, danger, disabled, item, keyed, separator, sub};
use crate::window::{self, Row};
use crate::{engine, organize, prefs, reader, reminders};
use gtk::prelude::*;
use nyx_mail_core::AccountRow;
use nyx_mail_core::db::{Mailbox, Role};
use nyx_mail_core::flags;
use nyx_mail_core::organize::Kind;
use serde_json::json;

/// A list row's menu, at a point in its widget. The row is selected first,
/// without marking it read.
pub fn list(position: u32, anchor: &gtk::Widget, x: f64, y: f64) {
    if window::selected_position() != Some(position) {
        window::select_quietly(position);
    }
    let Some(row) = window::row_at(position) else { return };
    widgets::popup_menu(anchor, x, y, entries(&row));
}

/// The selected row's menu (the Menu key).
pub fn for_selected() {
    let Some(position) = window::selected_position() else { return };
    let (Some(row), Some(anchor)) = (window::row_at(position), window::row_widget(position)) else { return };
    let y = anchor.height() as f64 / 2.0;
    widgets::popup_menu(&anchor, 24.0, y, entries(&row));
}

fn entries(row: &Row) -> Vec<MenuEntry> {
    match row {
        Row::Conversation(_) => conversation(),
        Row::Draft { id, .. } => {
            let id = *id;
            vec![
                item("Open", move || compose::open(Start::Draft(id))),
                separator(),
                danger("Delete draft", None, move || {
                    engine::delete_draft(id);
                    window::toast("Draft deleted.");
                }),
            ]
        }
        Row::Scheduled(s) => {
            let op = s.op;
            vec![
                item("Back to Drafts to change it", move || organize::edit_scheduled(op)),
                separator(),
                danger("Don't send it", None, move || organize::discard_scheduled(op)),
            ]
        }
        Row::Sender(s) => sender(&s.address, s.unsubscribe.clone(), s.screened.as_deref() == Some("out")),
    }
}

/// The open conversation (the right-clicked row was just opened).
fn conversation() -> Vec<MenuEntry> {
    let messages = reader::messages();
    let Some(newest) = messages.last().cloned() else { return Vec::new() };
    let unread = messages.iter().any(|m| m.flags & flags::SEEN == 0);
    let starred = messages.iter().any(|m| m.flags & flags::FLAGGED != 0);
    let role = window::current_role();
    let mut out = vec![
        keyed("Reply", "R", || window::reply(false)),
        keyed("Reply to all", "A", || window::reply(true)),
        keyed("Forward", "F", window::forward),
        separator(),
        keyed(if unread { "Mark as read" } else { "Mark as unread" }, "U", window::toggle_unread),
        keyed(if starred { "Put back" } else { "Set aside" }, "S", window::toggle_star),
    ];

    let mut snooze: Vec<MenuEntry> =
        organize::snooze_times().into_iter().map(|(label, t)| item(&label, move || organize::snooze(t))).collect();
    if data::snoozed_until(&newest.thread).is_some() {
        let id = newest.id;
        snooze.push(separator());
        snooze.push(item("Bring it back now", move || engine::snooze(id, 0)));
    }
    out.push(sub("Snooze", snooze));
    let mut remind: Vec<MenuEntry> = organize::follow_up_times()
        .into_iter()
        .map(|(label, due)| {
            let id = newest.id;
            item(label, move || {
                engine::follow_up(id, due);
                window::toast(&format!("You'll hear about it {} if nobody answers.", window::when_text(due, true)));
            })
        })
        .collect();
    if data::follow_ups().iter().any(|f| f.thread == newest.thread) {
        let id = newest.id;
        remind.push(separator());
        remind.push(item("Don't remind me", move || engine::follow_up(id, 0)));
    }
    out.push(sub("Remind me if nobody answers", remind));
    let (subject, from) = (newest.subject.clone(), if newest.from_name.is_empty() { newest.from_addr.clone() } else { newest.from_name.clone() });
    out.push(sub(
        "Add to Reminders",
        reminders::times()
            .into_iter()
            .map(|(label, due)| {
                let (id, title, notes) = (newest.id, reminders::title(&subject, &from), format!("From {from}"));
                item(&label, move || reminders::add(title.clone(), notes.clone(), due.clone(), id))
            })
            .collect(),
    ));

    out.push(separator());
    if role != Some(Role::Archive) {
        out.push(keyed("Archive", "E", window::archive));
    }
    let folders: Vec<MenuEntry> = window::mailboxes()
        .into_iter()
        .filter(|m| m.account == newest.account && m.id != newest.mailbox)
        .filter(|m| !matches!(m.role, Some(Role::All | Role::Flagged | Role::Drafts)))
        .map(|m| {
            let (target, label) = (format!("mailbox:{}", m.id), m.label());
            item(&label.clone(), move || window::move_to(&target, &format!("Moved to {label}.")))
        })
        .collect();
    out.push(if folders.is_empty() { disabled("Move to") } else { sub("Move to", folders) });
    out.push(keyed(if role == Some(Role::Junk) { "Not junk" } else { "Move to Junk" }, "!", window::junk));
    out.push(if role == Some(Role::Trash) {
        danger("Delete for good", Some("#"), window::trash)
    } else {
        keyed("Move to Trash", "#", window::trash)
    });

    // The person writing.
    let own = data::own_addresses();
    if let Some(them) = messages.iter().rev().find(|m| !own.iter().any(|o| o.eq_ignore_ascii_case(&m.from_addr))) {
        out.push(separator());
        let address = them.from_addr.clone();
        out.push(sub(
            "Put their mail in",
            [Kind::Imbox, Kind::Feed, Kind::Paper]
                .into_iter()
                .map(|kind| {
                    let a = address.clone();
                    item(kind.name(), move || {
                        engine::set_sender(&a, json!({"kind": kind.id()}));
                        window::toast(&format!("Their mail goes to {} now.", kind.name()));
                    })
                })
                .collect(),
        ));
        if !prefs::get().images_always {
            let trusted = prefs::trusts(&address);
            let a = address.clone();
            out.push(item(if trusted { "Stop showing their images" } else { "Always show their images" }, move || {
                prefs::update(|p| {
                    p.remote_senders.retain(|s| !s.eq_ignore_ascii_case(&a));
                    if !trusted {
                        p.remote_senders.push(a.clone());
                    }
                });
                reader::refresh();
            }));
        }
        if let Some(link) = organize::unsubscribe_link() {
            out.push(item("Unsubscribe…", move || reader::open_link(&link)));
        }
    }
    out
}

/// A sender on the Senders page.
fn sender(address: &str, unsubscribe: Option<String>, screened_out: bool) -> Vec<MenuEntry> {
    let mut out: Vec<MenuEntry> = [Kind::Imbox, Kind::Feed, Kind::Paper]
        .into_iter()
        .map(|kind| {
            let a = address.to_string();
            item(&format!("Put their mail in {}", kind.name()), move || {
                engine::set_sender(&a, json!({"kind": kind.id()}));
            })
        })
        .collect();
    if data::screener_on() || screened_out {
        let a = address.to_string();
        out.push(separator());
        out.push(item(if screened_out { "Let them into the inbox" } else { "Screen them out" }, move || {
            engine::set_sender(&a, json!({"screened": if screened_out { "in" } else { "out" }}));
        }));
    }
    out.push(separator());
    let a = address.to_string();
    out.push(item("Archive all their mail", move || organize::archive_all_from(&a)));
    if let Some(link) = unsubscribe {
        out.push(item("Unsubscribe…", move || reader::open_link(&link)));
    }
    out
}

fn mark_all_read(mailboxes: &[i64]) {
    let ids = data::unread_ids(mailboxes);
    if ids.is_empty() {
        window::toast("Nothing unread here.");
        return;
    }
    engine::set_flags(&ids, flags::SEEN, 0);
    window::toast(&if ids.len() == 1 {
        "Marked 1 message read.".to_string()
    } else {
        format!("Marked {} messages read.", ids.len())
    });
}

/// Inbox, Sent… for every account.
pub fn attach_role(button: &gtk::Button, role: Role) {
    widgets::on_right_click(button, move |w, x, y| {
        let mailboxes = View::All(role).mailboxes(&window::mailboxes());
        widgets::popup_menu(
            w,
            x,
            y,
            vec![
                item("Open", move || window::show_view(View::All(role))),
                item("Mark all as read", move || mark_all_read(&mailboxes)),
                separator(),
                keyed("Check for mail", "Ctrl+R", window::check_mail),
            ],
        );
    });
}

/// One account's folder (in its group, or Pinned).
pub fn attach_mailbox(button: &gtk::Button, m: &Mailbox) {
    let m = m.clone();
    widgets::on_right_click(button, move |w, x, y| {
        let pinned = prefs::get().pinned.contains(&prefs::pin_key(&m.account, &m.name));
        let (open, mark, pin, account) = (m.clone(), m.id, m.clone(), m.account.clone());
        widgets::popup_menu(
            w,
            x,
            y,
            vec![
                item("Open", move || window::show_view(View::Mailbox(open.id))),
                item("Mark all as read", move || mark_all_read(&[mark])),
                item(if pinned { "Unpin" } else { "Pin to the top" }, move || window::toggle_pinned(&pin)),
                separator(),
                item("Check for mail", move || {
                    engine::sync(&account);
                    window::toast("Checking for mail…");
                }),
            ],
        );
    });
}

/// An account's heading.
pub fn attach_account(header: &gtk::Box, account: &AccountRow, folded: bool) {
    let account = account.clone();
    widgets::on_right_click(header, move |w, x, y| {
        let (fold, sync, mark, off) = (account.id.clone(), account.id.clone(), account.id.clone(), account.clone());
        widgets::popup_menu(
            w,
            x,
            y,
            vec![
                item(if folded { "Show its folders" } else { "Hide its folders" }, move || window::toggle_account_folded(&fold)),
                item("Check for mail", move || {
                    engine::sync(&sync);
                    window::toast("Checking for mail…");
                }),
                item("Mark its inbox as read", move || {
                    let inboxes: Vec<i64> = window::mailboxes()
                        .iter()
                        .filter(|m| m.account == mark && m.role == Some(Role::Inbox))
                        .map(|m| m.id)
                        .collect();
                    mark_all_read(&inboxes);
                }),
                separator(),
                item("Turn off in Mail", move || {
                    engine::set_account_enabled(&off.id, false);
                    let id = off.id.clone();
                    window::toast_undo(
                        &format!("{} is off in Mail. Turn it back on in Settings → Accounts.", off.address),
                        move || engine::set_account_enabled(&id, true),
                    );
                }),
                item("Add or manage accounts…", window::open_online_accounts),
            ],
        );
    });
}
