//! The settings card's groups.

use crate::prefs;
use crate::widgets::{self, Page};
use crate::{data, window};
use gtk::prelude::*;

pub fn build(page: &Page) {
    reading(page);
    writing(page);
    notifications(page);
    checking(page);
    accounts(page);
}

fn reading(page: &Page) {
    let group = page.group("Reading");
    let prefs = prefs::get();
    let current = match (prefs.mark_read, prefs.mark_read_delay) {
        (false, _) => "never",
        (true, 0) => "open",
        (true, _) => "delay",
    };
    let (row, _) = widgets::choice_row(
        "Mark as read",
        "When a conversation counts as read. Mark one yourself any time with U.",
        opts(&[("open", "When opened"), ("delay", "After 3 seconds open"), ("never", "Only when I mark it")]),
        current,
        |id| {
            prefs::update(|p| {
                p.mark_read = id != "never";
                p.mark_read_delay = if id == "delay" { 3 } else { 0 };
            })
        },
    );
    group.add(&row);
    let (row, _) = widgets::switch_row(
        "Open the first conversation",
        "Going to a mailbox shows its newest conversation. It's marked read when you go on to another one.",
        prefs.open_first,
        |on| prefs::update(|p| p.open_first = on),
    );
    group.add(&row);
    let (row, _) = widgets::choice_row(
        "After archiving or deleting",
        "What the reading pane shows once a conversation leaves the list.",
        opts(&[("next", "The next conversation"), ("previous", "The previous one"), ("list", "Nothing (back to the list)")]),
        &prefs.after_moving,
        |id| prefs::update(|p| p.after_moving = id),
    );
    group.add(&row);
    let (row, _) =
        widgets::switch_row("Show previews", "The first lines of each message under its subject.", prefs.show_previews, |on| {
            prefs::update(|p| p.show_previews = on);
            window::reload();
        });
    group.add(&row);
    let (row, _) = widgets::choice_row(
        "Imbox looks like",
        "Cards with what's new first, or the conversation list beside the reader, as in Paper Trail.",
        opts(&[("cards", "Cards"), ("list", "List and reader")]),
        if prefs.imbox_layout == "list" { "list" } else { "cards" },
        |id| window::set_imbox_layout(&id),
    );
    group.add(&row);
    let (row, _) = widgets::switch_row(
        "Screener",
        "Mail from people who haven't written before waits in the Screener until you let them in. People you've written to, and everyone who wrote before, come straight in.",
        crate::data::screener_on(),
        crate::engine::set_screener,
    );
    group.add(&row);
    let (row, _) = widgets::choice_row(
        "Images from the web",
        "Images in messages come from the sender's servers, which then know you opened it.",
        opts(&[("ask", "Ask for each sender"), ("always", "Always show")]),
        if prefs.images_always { "always" } else { "ask" },
        |id| {
            prefs::update(|p| p.images_always = id == "always");
            crate::reader::refresh();
        },
    );
    group.add(&row);
    if prefs.images_always {
        return;
    }
    let count = prefs.remote_senders.len();
    let desc = match count {
        0 => "Images from the web stay hidden in every message until you show them.".to_string(),
        1 => "Images from the web show for 1 sender you chose; hidden for everyone else.".to_string(),
        n => format!("Images from the web show for {n} senders you chose; hidden for everyone else."),
    };
    let clear = widgets::icon_button("edit-clear-all-symbolic", "Hide images from every sender again");
    clear.set_sensitive(count > 0);
    clear.connect_clicked(|b| {
        prefs::update(|p| p.remote_senders.clear());
        b.set_sensitive(false);
        window::toast("Images from the web are hidden for every sender.");
    });
    group.add(&widgets::row("Senders you chose", &desc, Some(clear.upcast_ref())));
}

fn opts(items: &[(&str, &str)]) -> Vec<(String, String)> {
    items.iter().map(|(id, name)| (id.to_string(), name.to_string())).collect()
}

fn notifications(page: &Page) {
    let group = page.group("Notifications");
    let engine = data::engine_settings();
    let (row, _) = widgets::switch_row(
        "New mail",
        "A notification when mail arrives in the Imbox while Mail isn't in front. Never for The Feed, Paper Trail or the Screener.",
        engine.notify,
        |on| crate::engine::set_setting("notify", if on { "on" } else { "off" }),
    );
    group.add(&row);
}

fn checking(page: &Page) {
    let group = page.group("Checking mail");
    let engine = data::engine_settings();
    let every = engine.check_every.to_string();
    let (row, _) = widgets::choice_row(
        "Check for new mail",
        "Gmail and other IMAP accounts also hear about new mail at once while Mail is running.",
        opts(&[
            ("2", "Every 2 minutes"),
            ("5", "Every 5 minutes"),
            ("15", "Every 15 minutes"),
            ("30", "Every 30 minutes"),
            ("0", "Only when I check (Ctrl+R)"),
        ]),
        &every,
        |id| crate::engine::set_setting("check_every", &id),
    );
    group.add(&row);
    let days = engine.body_days.to_string();
    let (row, _) = widgets::choice_row(
        "Keep for reading offline",
        "Whole messages this recent are downloaded; older ones download when you open them.",
        opts(&[("7", "The last week"), ("30", "The last month"), ("90", "The last 3 months"), ("365", "The last year")]),
        &days,
        |id| crate::engine::set_setting("body_days", &id),
    );
    group.add(&row);
}

fn writing(page: &Page) {
    let group = page.group("Writing");
    let prefs = prefs::get();
    let accounts: Vec<(String, String)> = data::accounts().into_iter().map(|a| (a.id, a.address)).collect();
    if accounts.len() > 1 {
        let current = if accounts.iter().any(|(id, _)| *id == prefs.default_account) {
            prefs.default_account.clone()
        } else {
            accounts[0].0.clone()
        };
        let (row, _) = widgets::choice_row(
            "Send new messages from",
            "Replies come from the account the message arrived at.",
            accounts,
            &current,
            |id| prefs::update(|p| p.default_account = id),
        );
        group.add(&row);
    }
    let text = gtk::TextView::new();
    text.add_css_class("signature-text");
    text.set_wrap_mode(gtk::WrapMode::WordChar);
    text.set_top_margin(8);
    text.set_bottom_margin(8);
    text.set_left_margin(10);
    text.set_right_margin(10);
    text.set_size_request(-1, 90);
    text.buffer().set_text(&prefs.signature);
    text.buffer().connect_changed(|b| {
        let sig = b.text(&b.start_iter(), &b.end_iter(), false).to_string();
        prefs::update(|p| p.signature = sig);
    });
    let row = widgets::vbox(8);
    row.add_css_class("settings-option");
    row.add_css_class("stacked");
    let title = widgets::label("Signature", "option-title");
    title.set_xalign(0.0);
    row.append(&title);
    let desc = widgets::label("Added under new messages, replies and forwards. Leave it empty for none.", "option-desc");
    desc.set_xalign(0.0);
    desc.set_wrap(true);
    row.append(&desc);
    let frame = gtk::Frame::new(None);
    frame.add_css_class("signature-box");
    frame.set_child(Some(&text));
    row.append(&frame);
    group.add(&row);
    group.add(&aliases_row());
}

/// Other From addresses: the list, and a line to add one.
fn aliases_row() -> gtk::Widget {
    let row = widgets::vbox(8);
    row.add_css_class("settings-option");
    row.add_css_class("stacked");
    let title = widgets::label("Send from other addresses", "option-title");
    title.set_xalign(0.0);
    row.append(&title);
    let desc = widgets::label(
        "Aliases appear under their account in From, and replies to them come from them. The account's provider must let it send as the address (Gmail: “Send mail as”).",
        "option-desc",
    );
    desc.set_xalign(0.0);
    desc.set_wrap(true);
    row.append(&desc);
    let list = widgets::vbox(4);
    row.append(&list);
    fill_aliases(&list);

    let accounts: Vec<(String, String)> = data::accounts().into_iter().map(|a| (a.id, a.address)).collect();
    if accounts.is_empty() {
        return row.upcast();
    }
    let add = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let account = widgets::dropdown(&accounts, &accounts[0].0);
    account.update_property(&[gtk::accessible::Property::Label("Account")]);
    let name = gtk::Entry::new();
    name.set_placeholder_text(Some("Name"));
    name.set_width_chars(8);
    name.update_property(&[gtk::accessible::Property::Label("Name to send as")]);
    let email = gtk::Entry::new();
    email.set_placeholder_text(Some("me@example.com"));
    email.set_hexpand(true);
    email.update_property(&[gtk::accessible::Property::Label("Address to send from")]);
    let button = widgets::icon_button("list-add-symbolic", "Add this address");
    // Two lines, so the card fits a half-screen window.
    row.append(&account);
    add.append(&name);
    add.append(&email);
    add.append(&button);
    row.append(&add);
    let save = {
        let (account, name, email, list) = (account.clone(), name.clone(), email.clone(), list.clone());
        move || {
            let address = email.text().trim().to_string();
            if !address.contains('@') || address.contains(char::is_whitespace) {
                window::toast("Enter an address like me@example.com.");
                return;
            }
            let Some((id, _)) = accounts.get(account.selected() as usize).cloned() else { return };
            prefs::update(|p| {
                p.aliases.retain(|a| !(a.account == id && a.email.eq_ignore_ascii_case(&address)));
                p.aliases.push(prefs::Alias { account: id.clone(), name: name.text().trim().to_string(), email: address.clone() });
            });
            name.set_text("");
            email.set_text("");
            fill_aliases(&list);
        }
    };
    let s = save.clone();
    button.connect_clicked(move |_| s());
    email.connect_activate(move |_| save());
    row.upcast()
}

fn fill_aliases(list: &gtk::Box) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
    let accounts = data::accounts();
    for alias in prefs::get().aliases {
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let shown = if alias.name.is_empty() { alias.email.clone() } else { format!("{} <{}>", alias.name, alias.email) };
        let text = widgets::label(&shown, "");
        text.set_xalign(0.0);
        text.set_hexpand(true);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&text);
        let owner = accounts.iter().find(|a| a.id == alias.account).map(|a| a.address.clone()).unwrap_or_else(|| "an account that's off".into());
        let via = widgets::label(&format!("via {owner}"), "option-desc");
        via.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&via);
        let remove = widgets::icon_button("list-remove-symbolic", &format!("Stop sending from {}", alias.email));
        let l = list.clone();
        remove.connect_clicked(move |_| {
            prefs::update(|p| p.aliases.retain(|a| *a != alias));
            fill_aliases(&l);
        });
        line.append(&remove);
        list.append(&line);
    }
}

fn accounts(page: &Page) {
    let group = page.group("Accounts");
    group.note("Mail accounts come from Settings → Online Accounts (with Mail switched on). Passwords and sign-ins stay there.");
    if let Some(status) = window::status() {
        let off = data::disabled_accounts();
        let mut shown: Vec<String> = Vec::new();
        for a in &status.accounts {
            let desc = match (&a.error, a.offline, a.last_sync) {
                (Some(e), _, _) => e.clone(),
                (None, true, _) => "Offline".into(),
                (None, false, Some(t)) => format!("Updated {}", window::when_text(t, true)),
                _ => "Not checked yet".into(),
            };
            shown.push(a.id.clone());
            group.add(&account_switch(&a.id, &a.address, &desc, true));
        }
        for a in data::all_accounts().into_iter().filter(|a| off.contains(&a.id) && !shown.contains(&a.id)) {
            group.add(&account_switch(&a.id, &a.address, "Off: not checked or shown. Its mail stays on this computer.", false));
        }
        for s in &status.skipped {
            group.add(&widgets::row(&s.address, &s.reason, None));
        }
    }
    let open = widgets::icon_button("go-next-symbolic", "Open Settings → Online Accounts");
    open.connect_clicked(|_| window::open_online_accounts());
    group.add(&widgets::row(
        "Online accounts",
        "Add, sign in to or remove Google, Fastmail, iCloud and other mail accounts.",
        Some(open.upcast_ref()),
    ));
}

fn account_switch(id: &str, address: &str, desc: &str, on: bool) -> gtk::Widget {
    let id = id.to_string();
    let (row, _) = widgets::switch_row(address, desc, on, move |on| crate::engine::set_account_enabled(&id, on));
    row.upcast()
}

