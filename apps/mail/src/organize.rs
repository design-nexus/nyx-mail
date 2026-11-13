//! Sorting and timing, in the window: the Snooze and More menus of the reading
//! pane, the banners over a conversation (Screener, snoozed, follow-up), and
//! the cards for scheduled mail and for a sender. All of it is kept on this
//! computer by nyx-maild; nothing moves on the server.

use crate::compose::{self, Start};
use crate::data;
use crate::engine;
use crate::reader;
use crate::widgets;
use crate::window;
use chrono::{Datelike, Duration, Local, NaiveTime, TimeZone, Timelike, Weekday};
use gtk::prelude::*;
use nyx_mail_core::db::{MessageRow, Scheduled, SenderRow};
use nyx_mail_core::flags;
use nyx_mail_core::organize::Kind;
use serde_json::json;

fn at(day: chrono::NaiveDate, hour: u32) -> i64 {
    let naive = day.and_time(NaiveTime::from_hms_opt(hour, 0, 0).unwrap_or_default());
    Local.from_local_datetime(&naive).earliest().map(|t| t.timestamp()).unwrap_or(0)
}

/// Snooze choices from now: (label, time).
pub fn snooze_times() -> Vec<(String, i64)> {
    let now = Local::now();
    let today = now.date_naive();
    let mut out = Vec::new();
    let in3 = now + Duration::hours(3);
    out.push((format!("Later today · {}", in3.format("%H:%M")), in3.timestamp()));
    if now.hour() < 17 {
        out.push(("This evening · 18:00".into(), at(today, 18)));
    }
    out.push(("Tomorrow · 08:00".into(), at(today + Duration::days(1), 8)));
    let saturday = today
        + Duration::days(((Weekday::Sat.num_days_from_monday() + 7 - today.weekday().num_days_from_monday()) % 7).max(1) as i64);
    if today.weekday() != Weekday::Sat && today.weekday() != Weekday::Sun {
        out.push(("This weekend · Sat 08:00".into(), at(saturday, 8)));
    }
    let monday = today + Duration::days((7 - today.weekday().num_days_from_monday()) as i64);
    out.push(("Next week · Mon 08:00".into(), at(monday, 8)));
    out
}

/// Follow-up choices: (label, time).
pub fn follow_up_times() -> Vec<(&'static str, i64)> {
    let now = Local::now();
    vec![
        ("If no reply in a day", (now + Duration::days(1)).timestamp()),
        ("If no reply in 3 days", (now + Duration::days(3)).timestamp()),
        ("If no reply in a week", (now + Duration::days(7)).timestamp()),
    ]
}

/// Send-later choices: (label, time).
pub fn send_later_times() -> Vec<(String, i64)> {
    let today = Local::now().date_naive();
    let tomorrow = today + Duration::days(1);
    let monday = today + Duration::days((7 - today.weekday().num_days_from_monday()) as i64);
    vec![
        ("Tomorrow morning · 08:00".into(), at(tomorrow, 8)),
        ("Tomorrow afternoon · 13:00".into(), at(tomorrow, 13)),
        (format!("Monday · {} 08:00", monday.format("%b %-d")), at(monday, 8)),
    ]
}

/// A popover of choices, each a flat row.
fn menu(button: &gtk::MenuButton, items: Vec<(String, Box<dyn Fn()>)>) {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.add_css_class("menu-list");
    let popover = gtk::Popover::new();
    for (label, run) in items {
        if label.is_empty() {
            list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            continue;
        }
        let item = gtk::Button::with_label(&label);
        item.add_css_class("flat");
        item.add_css_class("menu-item");
        if let Some(l) = item.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        let p = popover.clone();
        item.connect_clicked(move |_| {
            p.popdown();
            run();
        });
        list.append(&item);
    }
    popover.set_child(Some(&list));
    button.set_popover(Some(&popover));
}

/// The open conversation's newest message, and its newest from someone else.
fn open_messages() -> Option<(MessageRow, Option<MessageRow>)> {
    let messages = reader::messages();
    let newest = messages.last()?.clone();
    let own = data::own_addresses();
    let theirs = messages.iter().rev().find(|m| !own.iter().any(|o| o.eq_ignore_ascii_case(&m.from_addr))).cloned();
    Some((newest, theirs))
}

pub fn snooze(until: i64) {
    let Some((m, _)) = open_messages() else { return };
    engine::snooze(m.id, until);
    let id = m.id;
    window::toast_undo(&format!("Snoozed until {}.", window::when_text(until, true)), move || engine::snooze(id, 0));
}

/// The reading pane's Snooze button.
pub fn snooze_button() -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_icon_name("alarm-symbolic");
    button.set_has_frame(false);
    button.set_tooltip_text(Some("Snooze (Z)"));
    button.update_property(&[gtk::accessible::Property::Label("Snooze")]);
    button.add_css_class("flat");
    button.add_css_class("icon-button");
    // Built each time it opens: the times move on.
    button.set_create_popup_func(fill_snooze);
    button
}

fn fill_snooze(button: &gtk::MenuButton) {
    let mut items: Vec<(String, Box<dyn Fn()>)> =
        snooze_times().into_iter().map(|(label, t)| (label, Box::new(move || snooze(t)) as Box<dyn Fn()>)).collect();
    let snoozed = reader::messages().last().and_then(|m| data::snoozed_until(&m.thread));
    if snoozed.is_some() {
        items.push((String::new(), Box::new(|| {})));
        items.push((
            "Bring it back now".into(),
            Box::new(|| {
                if let Some((m, _)) = open_messages() {
                    engine::snooze(m.id, 0);
                }
            }),
        ));
    }
    menu(button, items);
}

/// The reading pane's More button: setting aside, read, junk, follow-ups and
/// the sender.
pub fn more_button() -> gtk::MenuButton {
    let button = gtk::MenuButton::new();
    button.set_icon_name("view-more-symbolic");
    button.set_has_frame(false);
    button.set_tooltip_text(Some("Set aside, mark unread, junk, follow up, sender"));
    button.update_property(&[gtk::accessible::Property::Label("More")]);
    button.add_css_class("flat");
    button.add_css_class("icon-button");
    button.set_create_popup_func(fill_more);
    button
}

fn fill_more(button: &gtk::MenuButton) {
    let mut items: Vec<(String, Box<dyn Fn()>)> = Vec::new();
    let Some((newest, theirs)) = open_messages() else {
        menu(button, items);
        return;
    };
    let messages = reader::messages();
    let aside = messages.iter().any(|m| m.flags & flags::FLAGGED != 0);
    let unread = messages.iter().any(|m| m.flags & flags::SEEN == 0);
    let junk = window::current_role() == Some(nyx_mail_core::db::Role::Junk);
    items.push((if aside { "Put back (S)" } else { "Set aside (S)" }.into(), Box::new(window::toggle_star)));
    items.push((if unread { "Mark as read (U)" } else { "Mark as unread (U)" }.into(), Box::new(window::toggle_unread)));
    items.push((if junk { "Not junk (!)" } else { "Move to Junk (!)" }.into(), Box::new(window::junk)));
    items.push((String::new(), Box::new(|| {})));
    for (label, due) in follow_up_times() {
        let id = newest.id;
        items.push((
            format!("Remind me {}", label.to_lowercase()),
            Box::new(move || {
                engine::follow_up(id, due);
                window::toast(&format!("You'll hear about it {} if nobody answers.", window::when_text(due, true)));
            }),
        ));
    }
    if data::follow_ups().iter().any(|f| f.thread == newest.thread) {
        let id = newest.id;
        items.push(("Don't remind me".into(), Box::new(move || engine::follow_up(id, 0))));
    }
    if let Some(sender) = theirs {
        items.push((String::new(), Box::new(|| {})));
        let address = sender.from_addr.clone();
        let who = if sender.from_name.is_empty() { sender.from_addr.clone() } else { sender.from_name.clone() };
        for kind in [Kind::Imbox, Kind::Feed, Kind::Paper] {
            let a = address.clone();
            items.push((
                format!("Put {who}'s mail in {}", kind.name()),
                Box::new(move || {
                    engine::set_sender(&a, json!({"kind": kind.id()}));
                    window::toast(&format!("Their mail goes to {} now.", kind.name()));
                }),
            ));
        }
        if data::screener_on() {
            let a = address.clone();
            items.push(("Screen them out".into(), Box::new(move || engine::set_sender(&a, json!({"screened": "out"})))));
        }
        let inboxes = data::View::All(nyx_mail_core::db::Role::Inbox).mailboxes(&data::mailboxes());
        let unsubscribe = data::sender(&address, &inboxes).and_then(|s| s.unsubscribe);
        if let Some(link) = unsubscribe.or_else(|| list_unsubscribe(&sender)) {
            items.push(("Unsubscribe…".into(), Box::new(move || reader::open_link(&link))));
        }
    }
    menu(button, items);
}

/// The unsubscribe link of a message (read from the store).
fn list_unsubscribe(m: &MessageRow) -> Option<String> {
    let (_, parsed) = data::parsed(m)?;
    nyx_mail_core::organize::unsubscribe_link(parsed.list_unsubscribe.as_deref()?)
}

// ---------- Banners over a conversation ----------

/// What to say over the open conversation: it waits in the Screener, it's
/// snoozed, or a follow-up is set.
pub fn banner(messages: &[MessageRow]) -> Option<gtk::Box> {
    let newest = messages.last()?;
    let own = data::own_addresses();
    let theirs = messages.iter().rev().find(|m| !own.iter().any(|o| o.eq_ignore_ascii_case(&m.from_addr)));
    if let Some(m) = theirs
        && data::screener_on()
        && data::waiting(&m.from_addr)
    {
        let who = if m.from_name.is_empty() {
            m.from_addr.clone()
        } else {
            format!("{} &lt;{}&gt;", gtk::glib::markup_escape_text(&m.from_name), m.from_addr)
        };
        let b = widgets::banner(&format!("<b>{who}</b> hasn't written to you before. Let their mail into your inbox?"), false);
        let address = m.from_addr.clone();
        let yes = widgets::icon_button("object-select-symbolic", "Let them in");
        let a = address.clone();
        yes.connect_clicked(move |_| {
            engine::set_sender(&a, json!({"screened": "in"}));
            window::toast("Let in. Their mail comes to your inbox.");
        });
        let no = widgets::icon_button("action-unavailable-symbolic", "Screen them out (their mail stays out of the inbox)");
        no.connect_clicked(move |_| {
            engine::set_sender(&address, json!({"screened": "out"}));
            window::toast("Screened out. Find them in Senders to let them in later.");
        });
        b.append(&yes);
        b.append(&no);
        return Some(b);
    }
    if let Some(until) = data::snoozed_until(&newest.thread) {
        let b = widgets::banner(&format!("Snoozed until {}.", window::when_text(until, true)), false);
        let back = widgets::icon_button("edit-undo-symbolic", "Bring it back now");
        let id = newest.id;
        back.connect_clicked(move |_| engine::snooze(id, 0));
        b.append(&back);
        return Some(b);
    }
    if let Some(f) = data::follow_ups().into_iter().find(|f| f.thread == newest.thread) {
        let b = widgets::banner(&format!("You'll hear about this {} if nobody answers.", window::when_text(f.due, true)), false);
        let cancel = widgets::icon_button("window-close-symbolic", "Don't remind me");
        let id = newest.id;
        cancel.connect_clicked(move |_| engine::follow_up(id, 0));
        b.append(&cancel);
        return Some(b);
    }
    None
}

// ---------- Cards ----------

fn card_head(title: &str, subtitle: &str) -> gtk::Box {
    let head = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let t = widgets::label(title, "conversation-subject");
    t.set_xalign(0.0);
    t.set_wrap(true);
    head.append(&t);
    if !subtitle.is_empty() {
        let s = widgets::label(subtitle, "dim");
        s.set_xalign(0.0);
        s.set_wrap(true);
        head.append(&s);
    }
    head
}

/// Mail waiting to send: when, to whom, and taking it back.
pub fn scheduled_card(s: &Scheduled) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 14);
    card.append(&card_head(
        if s.subject.trim().is_empty() { "(no subject)" } else { s.subject.trim() },
        &format!("Sends {} · to {}", window::full_date(s.at), s.to.join(", ")),
    ));
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let edit = widgets::icon_button("document-edit-symbolic", "Don't send it yet: back to Drafts to change it");
    let op = s.op;
    edit.connect_clicked(move |_| edit_scheduled(op));
    actions.append(&edit);
    let discard = widgets::icon_button("user-trash-symbolic", "Don't send it (it's discarded)");
    discard.connect_clicked(move |_| discard_scheduled(op));
    actions.append(&discard);
    card.append(&actions);
    card.upcast()
}

/// Mail scheduled for later goes back to Drafts and opens to be changed.
pub fn edit_scheduled(op: i64) {
    engine::unschedule(op, |r| match r {
        Ok(draft) => compose::open(Start::Draft(draft)),
        Err(e) => window::toast(&e),
    })
}

/// Mail scheduled for later won't be sent.
pub fn discard_scheduled(op: i64) {
    engine::unschedule(op, |r| match r {
        Ok(draft) => {
            engine::delete_draft(draft);
            window::toast("It won't be sent.");
        }
        Err(e) => window::toast(&e),
    })
}

/// Archives everything of a sender's in the inbox.
pub fn archive_all_from(address: &str) {
    let inboxes = data::View::All(nyx_mail_core::db::Role::Inbox).mailboxes(&data::mailboxes());
    let ids = data::messages_from(address, &inboxes);
    if ids.is_empty() {
        window::toast("None of their mail is in the inbox.");
        return;
    }
    engine::move_to(&ids, "archive");
    window::toast(&format!(
        "Archived {} from {address}.",
        if ids.len() == 1 { "1 message".to_string() } else { format!("{} messages", ids.len()) }
    ));
}

/// The open conversation's unsubscribe link, if its sender offers one.
pub fn unsubscribe_link() -> Option<String> {
    let (_, theirs) = open_messages()?;
    let sender = theirs?;
    let inboxes = data::View::All(nyx_mail_core::db::Role::Inbox).mailboxes(&data::mailboxes());
    data::sender(&sender.from_addr, &inboxes).and_then(|s| s.unsubscribe).or_else(|| list_unsubscribe(&sender))
}

/// A sender: where their mail goes, the Screener, unsubscribing, archiving.
pub fn sender_card(s: &SenderRow) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 14);
    let title = if s.name.trim().is_empty() { s.address.clone() } else { s.name.clone() };
    let mut facts =
        vec![s.address.clone(), if s.messages == 1 { "1 message".into() } else { format!("{} messages", s.messages) }];
    if s.unread > 0 {
        facts.push(format!("{} unread", s.unread));
    }
    facts.push(format!("last {}", window::when_text(s.last, false)));
    card.append(&card_head(&title, &facts.join(" · ")));

    let group = gtk::Box::new(gtk::Orientation::Vertical, 0);
    group.add_css_class("group-list");
    let options: Vec<(String, String)> =
        [Kind::Imbox, Kind::Feed, Kind::Paper].iter().map(|k| (k.id().to_string(), k.name().to_string())).collect();
    let address = s.address.clone();
    let segmented = widgets::segmented(&options, s.kind.id(), move |id| engine::set_sender(&address, json!({"kind": id})));
    let desc = if s.chosen { "Your choice." } else { "Guessed from their mail; choose to keep it." };
    group.append(&widgets::row("Their mail goes to", desc, Some(segmented.upcast_ref())));
    if data::screener_on() || s.screened.is_some() {
        let address = s.address.clone();
        let (row, _) = widgets::switch_row(
            "Let into the inbox",
            "Off: their mail stays out of the inbox (the Screener).",
            s.screened.as_deref() != Some("out"),
            move |on| engine::set_sender(&address, json!({"screened": if on { "in" } else { "out" }})),
        );
        group.append(&row);
    }
    if let Some(link) = s.unsubscribe.clone() {
        let open = widgets::icon_button("action-unavailable-symbolic", "Unsubscribe (opens their page, or a message to send)");
        open.connect_clicked(move |_| reader::open_link(&link));
        group.append(&widgets::row("Unsubscribe", "They offer a way to stop their mail.", Some(open.upcast_ref())));
    }
    let archive = widgets::icon_button("nyx-mail-archive-symbolic", "Archive all their mail in the inbox");
    let address = s.address.clone();
    archive.connect_clicked(move |_| archive_all_from(&address));
    group.append(&widgets::row("Archive all", "Everything of theirs in the inbox.", Some(archive.upcast_ref())));
    card.append(&group);
    card.upcast()
}
