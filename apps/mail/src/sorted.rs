//! The sorted pages: the Imbox (people, what's new first), The Feed
//! (newsletters as cards) and the Screener (first-time senders waiting for a
//! yes or a no). They're drawn from the list's rows, so a row's position opens
//! it in the reader and J, K, Archive and the rest work as in any list.

use crate::data::{self, View};
use crate::engine;
use crate::prefs;
use crate::widgets;
use crate::window::{self, Row};
use gtk::glib;
use gtk::prelude::*;
use nyx_mail_core::db::{ListItem, Role};
use nyx_mail_core::flags;
use nyx_mail_core::organize::Kind;
use serde_json::json;

/// The page for `view`. `more`: the list stopped at its limit.
pub fn page(view: &View, rows: &[Row], more: bool) -> gtk::Box {
    let items: Vec<(u32, &ListItem)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            Row::Conversation(c) => Some((i as u32, c)),
            _ => None,
        })
        .collect();
    match view {
        View::Kind(Kind::Feed) => feed(&items, more),
        View::Screener => screener(&items),
        _ => imbox(&items, more),
    }
}

fn who(c: &ListItem) -> String {
    if c.from_name.trim().is_empty() { c.from_addr.clone() } else { c.from_name.trim().to_string() }
}

fn subject(c: &ListItem) -> String {
    if c.subject.trim().is_empty() { "(no subject)".into() } else { c.subject.trim().to_string() }
}

fn line(text: &str, class: &str) -> gtk::Label {
    let l = widgets::label(text, class);
    l.set_xalign(0.0);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_single_line_mode(true);
    l
}

/// Wraps to at most `lines`, then ends in "…"; never widens what holds it.
fn lines(text: &str, class: &str, lines: i32) -> gtk::Label {
    let l = widgets::label(text, class);
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    l.set_lines(lines);
    l.set_ellipsize(gtk::pango::EllipsizeMode::End);
    l.set_width_chars(12);
    l.set_max_width_chars(18);
    l
}

/// The page's head (title and what it holds, an action at the right) over its
/// scrolled body.
fn frame(title: &str, subtitle: &str, end: Option<gtk::Widget>, body: &impl IsA<gtk::Widget>) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("sorted-page");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    head.add_css_class("page-head");
    let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text.set_valign(gtk::Align::Center);
    text.set_hexpand(true);
    text.append(&line(title, "page-title"));
    text.append(&line(subtitle, "page-subtitle"));
    head.append(&text);
    if let Some(end) = end {
        end.set_valign(gtk::Align::Center);
        head.append(&end);
    }
    root.append(&head);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(body)
        .build();
    root.append(&scroll);
    root
}

/// A column no wider than `max`, centred, with the page's padding.
fn column(max: i32) -> (gtk::Box, gtk::Widget) {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let clamp = nyx_ui::clamp::Clamp::new(&column, max);
    clamp.add_css_class("page-body");
    (column, clamp.upcast())
}

fn heading(text: &str, class: &str) -> gtk::Label {
    let l = widgets::label(&text.to_uppercase(), "sorted-heading");
    for c in class.split_whitespace() {
        l.add_css_class(c);
    }
    l
}

fn more_button() -> gtk::Button {
    let b = gtk::Button::with_label("Show more");
    b.add_css_class("flat");
    b.add_css_class("show-more");
    b.set_halign(gtk::Align::Center);
    b.connect_clicked(|_| window::load_more());
    b
}

fn empty(icon: &str, title: &str, desc: &str, action: widgets::Action<'_>) -> gtk::Widget {
    let state = widgets::empty_state(icon, title, desc, action);
    state.set_vexpand(true);
    state.set_margin_top(60);
    state.upcast()
}

// ---------- Imbox ----------

fn imbox(items: &[(u32, &ListItem)], more: bool) -> gtk::Box {
    let seen_only = prefs::get().imbox_seen;
    let options = widgets::opts(&[("new", "New for you"), ("seen", "Previously seen")]);
    let switch = widgets::segmented(&options, if seen_only { "seen" } else { "new" }, |id| {
        glib::idle_add_local_once(move || window::set_imbox_seen(id == "seen"));
    });
    switch.add_css_class("page-switch");
    // Without the Screener nobody was asked for a yes: it's who writes to you.
    let subtitle = if data::screener_on() { "People and things you said yes to" } else { "People writing to you" };
    if items.is_empty() {
        let body = empty(
            "starred-symbolic",
            "Nothing in the Imbox",
            "Mail from people lands here. Newsletters go to The Feed, receipts and notices to Paper Trail.",
            None,
        );
        return frame("Imbox", subtitle, Some(switch.upcast()), &body);
    }
    let (list, body) = column(760);
    let new: Vec<&(u32, &ListItem)> = items.iter().filter(|(_, c)| c.unread).collect();
    let seen: Vec<&(u32, &ListItem)> = items.iter().filter(|(_, c)| !c.unread).collect();
    if !seen_only {
        list.append(&heading(&format!("New for you · {}", new.len()), ""));
        if new.is_empty() {
            list.append(&line("Nothing new. You've seen everything here.", "sorted-none"));
        }
        for (pos, c) in &new {
            list.append(&person_card(*pos, c));
        }
    }
    if !seen.is_empty() {
        list.append(&heading("Previously seen", if seen_only { "seen first" } else { "seen" }));
        for (pos, c) in &seen {
            list.append(&person_row(*pos, c));
        }
    }
    if more {
        list.append(&more_button());
    }
    frame("Imbox", subtitle, Some(switch.upcast()), &body)
}

fn when_and_extras(c: &ListItem, row: &gtk::Box) {
    if c.count > 1 {
        let n = widgets::label(&c.count.to_string(), "thread-count");
        n.set_tooltip_text(Some(&format!("{} messages", c.count)));
        row.append(&n);
    }
    if c.has_attachments {
        let i = gtk::Image::from_icon_name("mail-attachment-symbolic");
        i.add_css_class("dim");
        row.append(&i);
    }
    if c.flags & flags::FLAGGED != 0 {
        let i = gtk::Image::from_icon_name("nyx-mail-flag-symbolic");
        i.add_css_class("flag");
        i.set_tooltip_text(Some("Set aside"));
        row.append(&i);
    }
    row.append(&widgets::label(&window::when_text(c.date, false), "person-time"));
}

/// Someone new: their card, with the subject and the start of what they wrote.
fn person_card(pos: u32, c: &ListItem) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("person-card");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    row.append(&widgets::avatar(&who(c), &c.from_addr, 40));
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let name = line(&who(c), "person-name");
    name.set_hexpand(true);
    top.append(&name);
    when_and_extras(c, &top);
    text.append(&top);
    text.append(&line(&subject(c), "person-subject"));
    if prefs::get().show_previews && !c.preview.trim().is_empty() {
        text.append(&line(c.preview.trim(), "person-preview"));
    }
    row.append(&text);
    button.set_child(Some(&row));
    button.update_property(&[gtk::accessible::Property::Label(&format!("{}: {}", who(c), subject(c)))]);
    button.connect_clicked(move |_| window::open_row(pos));
    button
}

/// Someone seen before: a quieter line.
fn person_row(pos: u32, c: &ListItem) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("person-row");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.append(&widgets::avatar(&who(c), &c.from_addr, 32));
    let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text.set_hexpand(true);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let name = line(&who(c), "person-name");
    name.set_hexpand(true);
    top.append(&name);
    when_and_extras(c, &top);
    text.append(&top);
    text.append(&line(&subject(c), "person-subject"));
    row.append(&text);
    button.set_child(Some(&row));
    button.update_property(&[gtk::accessible::Property::Label(&format!("{}: {}", who(c), subject(c)))]);
    button.connect_clicked(move |_| window::open_row(pos));
    button
}

// ---------- The Feed ----------

fn feed(items: &[(u32, &ListItem)], more: bool) -> gtk::Box {
    let unread: Vec<String> = items.iter().filter(|(_, c)| c.unread).map(|(_, c)| c.thread.clone()).collect();
    let mark = widgets::labeled_button("object-select-symbolic", "Mark all read");
    mark.add_css_class("flat");
    mark.set_sensitive(!unread.is_empty());
    mark.set_tooltip_text(Some("Mark everything in The Feed read"));
    mark.connect_clicked(move |_| mark_read(&unread));
    let subtitle = "Newsletters and updates, to read when you like";
    if items.is_empty() {
        let body = empty(
            "application-rss+xml-symbolic",
            "Nothing in the Feed",
            "Newsletters, mailing lists and updates land here, out of the way of mail from people.",
            None,
        );
        return frame("The Feed", subtitle, Some(mark.upcast()), &body);
    }
    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.add_css_class("page-body");
    let grid = gtk::FlowBox::new();
    grid.add_css_class("feed-grid");
    grid.set_selection_mode(gtk::SelectionMode::None);
    grid.set_homogeneous(true);
    grid.set_min_children_per_line(1);
    grid.set_max_children_per_line(3);
    grid.set_row_spacing(14);
    grid.set_column_spacing(14);
    grid.set_valign(gtk::Align::Start);
    for (pos, c) in items {
        grid.insert(&feed_card(*pos, c), -1);
    }
    let mut child = grid.first_child();
    while let Some(c) = child {
        c.set_focusable(false);
        child = c.next_sibling();
    }
    body.append(&grid);
    if more {
        body.append(&more_button());
    }
    frame("The Feed", subtitle, Some(mark.upcast()), &body)
}

/// A newsletter: a band in its sender's colours, who it's from, the subject
/// and how it starts.
fn feed_card(pos: u32, c: &ListItem) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("feed-card");
    if c.unread {
        button.add_css_class("unread");
    }
    button.set_overflow(gtk::Overflow::Hidden);
    button.set_size_request(200, -1);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let art = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    art.add_css_class("feed-art");
    art.set_size_request(-1, 96);
    let key = c.from_addr.to_lowercase();
    let paint = {
        let art = art.downgrade();
        move || {
            let Some(art) = art.upgrade() else { return };
            let swatches: Vec<String> =
                nyx_theme::swatches().into_iter().filter(|(n, _)| n != "graphite").map(|(_, c)| c).collect();
            if swatches.len() < 2 {
                return;
            }
            let hash = key.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619)) as usize;
            let (a, b) = (&swatches[hash % swatches.len()], &swatches[(hash / 7 + 1) % swatches.len()]);
            widgets::paint(&art, &format!("linear-gradient(125deg, alpha({a}, 0.82), alpha({b}, 0.82))"));
        }
    };
    paint();
    nyx_theme::subscribe(&art, paint);
    card.append(&art);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 3);
    text.add_css_class("feed-body");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    if c.unread {
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("unread-dot");
        dot.set_valign(gtk::Align::Center);
        top.append(&dot);
    }
    let from = line(&who(c), "feed-from");
    from.set_max_width_chars(14);
    from.set_hexpand(true);
    top.append(&from);
    top.append(&widgets::label(&window::when_text(c.date, false), "person-time"));
    text.append(&top);
    text.append(&lines(&subject(c), "feed-subject", 2));
    if prefs::get().show_previews && !c.preview.trim().is_empty() {
        text.append(&lines(c.preview.trim(), "feed-preview", 2));
    }
    card.append(&text);
    button.set_child(Some(&card));
    button.update_property(&[gtk::accessible::Property::Label(&format!("{}: {}", who(c), subject(c)))]);
    button.connect_clicked(move |_| window::open_row(pos));
    button
}

/// Marks every unread message in these Feed conversations read.
fn mark_read(threads: &[String]) {
    let inboxes = View::Kind(Kind::Feed).mailboxes(&data::mailboxes());
    let ids: Vec<i64> = threads
        .iter()
        .flat_map(|t| data::conversation(t, &inboxes))
        .filter(|m| m.flags & flags::SEEN == 0)
        .map(|m| m.id)
        .collect();
    if ids.is_empty() {
        return;
    }
    engine::set_flags(&ids, flags::SEEN, 0);
    window::toast(&if threads.len() == 1 {
        "Marked 1 conversation read.".to_string()
    } else {
        format!("Marked {} conversations read.", threads.len())
    });
}

// ---------- Screener ----------

fn screener(items: &[(u32, &ListItem)]) -> gtk::Box {
    let senders = widgets::labeled_button("x-office-address-book-symbolic", "Senders");
    senders.add_css_class("flat");
    senders.set_tooltip_text(Some("Everyone who writes, where their mail goes, and who's screened out"));
    senders.connect_clicked(|_| window::show_view(View::Senders));
    let subtitle = "First-time senders wait here until you decide";
    if !data::screener_on() {
        let body = empty(
            "nyx-mail-filter-symbolic",
            "The Screener is off",
            "Turn it on and mail from people who haven't written before waits here until you say where it goes. \
             People you've written to, and everyone who wrote before, come straight in.",
            Some(("Turn on the Screener", Box::new(|| engine::set_screener(true)))),
        );
        return frame("Screener", subtitle, Some(senders.upcast()), &body);
    }
    // One card per sender: their newest conversation, and how many wait.
    let mut people: Vec<(u32, &ListItem, usize)> = Vec::new();
    for (pos, c) in items {
        match people.iter_mut().find(|(_, p, _)| p.from_addr.eq_ignore_ascii_case(&c.from_addr)) {
            Some((_, _, n)) => *n += 1,
            None => people.push((*pos, c, 1)),
        }
    }
    if people.is_empty() {
        let body = empty(
            "nyx-mail-filter-symbolic",
            "Nobody new is waiting",
            "Mail from people who haven't written before waits here until you say where it goes.",
            None,
        );
        return frame("Screener", subtitle, Some(senders.upcast()), &body);
    }
    let (list, body) = column(720);
    for (pos, c, n) in people {
        list.append(&screen_card(pos, c, n));
    }
    let note = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    note.add_css_class("screen-note");
    let icon = gtk::Image::from_icon_name("dialog-information-symbolic");
    icon.set_valign(gtk::Align::Start);
    note.append(&icon);
    let text = gtk::Label::new(None);
    text.set_markup("Say no and you won't see mail from them again. You can change your mind in <a href=\"senders\">Senders</a>.");
    text.set_xalign(0.0);
    text.set_wrap(true);
    text.set_hexpand(true);
    text.connect_activate_link(|_, _| {
        window::show_view(View::Senders);
        glib::Propagation::Stop
    });
    note.append(&text);
    list.append(&note);
    frame("Screener", subtitle, Some(senders.upcast()), &body)
}

/// A first-time sender: who, what they wrote about, and where their mail goes.
fn screen_card(pos: u32, c: &ListItem, waiting: usize) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    card.add_css_class("screen-card");
    let avatar = widgets::avatar(&who(c), &c.from_addr, 40);
    avatar.set_valign(gtk::Align::Center);
    card.append(&avatar);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    text.append(&line(&who(c), "person-name"));
    text.append(&line(&c.from_addr, "screen-address"));
    let about = if waiting > 1 {
        format!("“{}” and {} more", subject(c), waiting - 1)
    } else {
        format!("“{}”", subject(c))
    };
    text.append(&line(&about, "screen-subject"));
    text.set_tooltip_text(Some("Read it first"));
    text.set_cursor_from_name(Some("pointer"));
    let click = gtk::GestureClick::new();
    click.connect_released(move |_, _, _, _| window::open_row(pos));
    text.add_controller(click);
    card.append(&text);

    let choices = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    choices.set_valign(gtk::Align::Center);
    let name = who(c);
    let yes = widgets::labeled_button("object-select-symbolic", "Yes, to Imbox");
    yes.add_css_class("suggested-action");
    yes.set_tooltip_text(Some("Let them in; their mail goes to the Imbox"));
    let feed = gtk::Button::with_label("Feed");
    feed.set_tooltip_text(Some("Let them in; their mail goes to The Feed"));
    let paper = gtk::Button::with_label("Paper Trail");
    paper.set_tooltip_text(Some("Let them in; their mail goes to Paper Trail"));
    let no = widgets::labeled_button("window-close-symbolic", "No");
    no.add_css_class("screen-no");
    no.set_tooltip_text(Some("Screen them out: their mail stays out of the inbox"));
    for (button, kind) in [(&yes, Some(Kind::Imbox)), (&feed, Some(Kind::Feed)), (&paper, Some(Kind::Paper)), (&no, None)] {
        let (address, name) = (c.from_addr.clone(), name.clone());
        button.connect_clicked(move |_| decide(&address, &name, kind));
        choices.append(button);
    }
    card.append(&choices);
    card
}

/// Lets a sender in (their mail to `kind`) or screens them out, with Undo.
fn decide(address: &str, name: &str, kind: Option<Kind>) {
    let inboxes = View::All(Role::Inbox).mailboxes(&data::mailboxes());
    let chosen = data::sender(address, &inboxes).filter(|s| s.chosen).map(|s| json!(s.kind.id()));
    let (choice, undo, text) = match kind {
        Some(k) => (
            json!({"screened": "in", "kind": k.id()}),
            json!({"screened": null, "kind": chosen.unwrap_or(serde_json::Value::Null)}),
            format!("{name}'s mail goes to {} now.", if k == Kind::Imbox { "the Imbox" } else { k.name() }),
        ),
        None => (json!({"screened": "out"}), json!({"screened": null}), format!("Screened out {name}.")),
    };
    engine::set_sender(address, choice);
    let address = address.to_string();
    window::toast_undo(&text, move || engine::set_sender(&address, undo.clone()));
}
