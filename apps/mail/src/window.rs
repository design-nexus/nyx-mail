//! The Mail window: Music's sidebar (every account's Inbox, Drafts, Sent…
//! together, then each account's folders), the conversation list (340 px) and
//! the reading pane. The sidebar collapses to icons under 980 px or when the
//! user collapses it.
//!
//! Everything shown is read from `mail.sqlite`; every change is a call to
//! nyx-maild, whose `Changed` signal brings the window up to date.

use crate::compose::{self, Start};
use crate::data::{self, View};
use crate::engine::{self, Status};
use crate::organize;
use crate::prefs;
use crate::reader;
use crate::settings_dialog;
use crate::widgets;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use nyx_mail_core::db::{ListItem, Mailbox, Role, Scheduled, SenderRow};
use nyx_mail_core::flags;
use nyx_mail_core::organize::Kind;
use nyx_mail_core::send::Draft;
use nyx_ui::chrome;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;

pub const SHORTCUTS: &[(&[&str], &str)] = &[
    (&["C"], "New message"),
    (&["J"], "Next conversation"),
    (&["K"], "Previous conversation"),
    (&["R"], "Reply"),
    (&["A"], "Reply to all"),
    (&["F"], "Forward"),
    (&["E"], "Archive"),
    (&["#"], "Move to Trash"),
    (&["Delete"], "Move to Trash (in Trash: delete for good)"),
    (&["!"], "Move to Junk"),
    (&["S"], "Star or unstar"),
    (&["U"], "Mark read or unread"),
    (&["Shift", "U"], "Show only unread"),
    (&["Z"], "Snooze"),
    (&["1"], "Unified"),
    (&["2"], "Drafts"),
    (&["3"], "Sent"),
    (&["4"], "Archive"),
    (&["Ctrl", "R"], "Check for mail"),
    (&["Ctrl", "F"], "Find messages"),
    (&["Esc"], "Clear the search"),
    (&["Ctrl", "B"], "Collapse or expand sidebar"),
    (&["Ctrl", ","], "Settings"),
    (&["F1"], "Show keyboard shortcuts"),
    (&["Ctrl", "Q"], "Close"),
];

/// A line in the list.
#[derive(Clone, Debug)]
pub(crate) enum Row {
    Conversation(ListItem),
    /// A draft being written, not sent yet.
    Draft {
        id: i64,
        draft: Box<Draft>,
        updated: i64,
    },
    /// Mail waiting to send at a time.
    Scheduled(Box<Scheduled>),
    /// Someone who writes (the Senders page).
    Sender(Box<SenderRow>),
}

impl Row {
    fn key(&self) -> String {
        match self {
            Self::Conversation(c) => c.thread.clone(),
            Self::Draft { id, .. } => format!("draft:{id}"),
            Self::Scheduled(s) => format!("scheduled:{}", s.op),
            Self::Sender(s) => format!("sender:{}", s.address),
        }
    }
}

/// Archive, Trash and the rest wait for their Undo toast to go before the
/// engine hears about them.
#[derive(Clone)]
struct Pending {
    ids: Vec<i64>,
    target: Option<String>,
    done: Rc<Cell<bool>>,
}

struct Ui {
    window: gtk::ApplicationWindow,
    overlay: gtk::Overlay,
    nav: gtk::Box,
    nav_list: gtk::Box,
    /// The header, which shows the folder you're in after the app's name.
    header: nyx_ui::chrome::AppHeader,
    search: gtk::SearchEntry,
    banners: gtk::Box,
    list_title: gtk::Label,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    list_scroll: gtk::ScrolledWindow,
    /// The view and search the list last showed (a different one starts at the top).
    list_shows: String,
    /// Only conversations with something unread (and the one open).
    unread_only: bool,
    unread_toggle: gtk::ToggleButton,
    list_stack: gtk::Stack,
    empty: gtk::Box,
    reader_bar: gtk::Box,
    /// All, Imbox, The Feed, Paper Trail (inbox views only).
    snooze: gtk::MenuButton,
    view: View,
    mailboxes: Vec<Mailbox>,
    rows: Vec<Row>,
    limit: usize,
    /// The conversation open in the reader.
    open: Option<String>,
    status: Option<Status>,
    engine_up: bool,
    hidden: HashSet<i64>,
    pending: Vec<Pending>,
    /// Restoring the selection after a reload (not the user choosing).
    restoring: bool,
}

thread_local! {
    static UI: RefCell<Option<Rc<RefCell<Ui>>>> = const { RefCell::new(None) };
    static NARROW: Cell<bool> = const { Cell::new(false) };
    static TOAST: RefCell<Option<gtk::Box>> = const { RefCell::new(None) };
    static STATUS: RefCell<Option<gtk::Label>> = const { RefCell::new(None) };
    static RELOAD_QUEUED: Cell<bool> = const { Cell::new(false) };
    /// Selecting without marking read (a right-click picks the row first).
    static QUIET: Cell<bool> = const { Cell::new(false) };
    /// The conversation opened on its own (Open the first conversation) and its
    /// unread messages: read once the user goes on to another one.
    static SEEN_ON_LEAVE: RefCell<Option<(String, Vec<i64>)>> = const { RefCell::new(None) };
    /// The widget showing each list position, for menus opened from the keyboard.
    static ROW_WIDGETS: RefCell<std::collections::HashMap<u32, glib::WeakRef<gtk::Widget>>> = RefCell::new(Default::default());
}

const PAGE: usize = 200;

fn ui() -> Option<Rc<RefCell<Ui>>> {
    UI.with(|u| u.borrow().clone())
}

pub fn window() -> Option<gtk::ApplicationWindow> {
    ui().map(|u| u.borrow().window.clone())
}

pub fn overlay() -> Option<gtk::Overlay> {
    ui().map(|u| u.borrow().overlay.clone())
}

pub fn status() -> Option<Status> {
    ui().and_then(|u| u.borrow().status.clone())
}

pub fn present(app: &gtk::Application, section: Option<&str>) {
    let first = ui().is_none();
    if first {
        build(app);
        let start = nyx_theme::start_page("inbox", &prefs::get().last_section);
        show_view(View::from_id(&start));
        start_engine();
    }
    let Some(ui) = ui() else { return };
    let window = ui.borrow().window.clone();
    let snapshot = std::env::var_os("MAIL_SNAPSHOT");
    if snapshot.is_some() {
        window.set_default_size(env_num("MAIL_SNAPSHOT_W", 1120), env_num("MAIL_SNAPSHOT_H", 800));
    }
    window.present();
    match section {
        Some("settings") => settings_dialog::open(),
        Some(id) => show_view(View::from_id(id)),
        None => {}
    }
    if let Some(out) = snapshot {
        snapshot_and_quit(app, std::path::PathBuf::from(out));
    }
}

fn env_num(key: &str, default: i32) -> i32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Renders the window to a PNG and quits (`MAIL_SNAPSHOT=out.png`; `_PAGE`,
/// `_SELECT` (the Nth conversation), `_COMPOSE`, `_OPAQUE`, `_W`, `_H`, `_DELAY`).
fn snapshot_and_quit(app: &gtk::Application, out: std::path::PathBuf) {
    let Some(ui) = ui() else { return };
    let window = ui.borrow().window.clone();
    let nav = ui.borrow().nav.clone();
    let (w, h) = (env_num("MAIL_SNAPSHOT_W", 1120), env_num("MAIL_SNAPSHOT_H", 800));
    // `_OPAQUE` leaves the window visible for a real screen capture (WebKit
    // draws message bodies itself, out of reach of the widget snapshot).
    if std::env::var_os("MAIL_SNAPSHOT_OPAQUE").is_none() {
        window.set_opacity(0.01);
    }
    window.set_title(Some("Mail snapshot"));
    if let Ok(page) = std::env::var("MAIL_SNAPSHOT_PAGE") {
        show_view(View::from_id(&page));
    }
    window.set_default_size(w, h);
    window.set_size_request(w, h);
    let app = app.clone();
    let delay = env_num("MAIL_SNAPSHOT_DELAY", 4000) as u64;
    if let Ok(query) = std::env::var("MAIL_SNAPSHOT_SEARCH") {
        let search = ui.borrow().search.clone();
        search.set_visible(true);
        search.set_text(&query);
    }
    if std::env::var_os("MAIL_SNAPSHOT_UNREAD").is_some() {
        set_unread_only(true);
    }
    // One row, or several one after another (`1,2,3`, `MAIL_SNAPSHOT_EVERY` ms apart).
    let picks: Vec<u32> =
        std::env::var("MAIL_SNAPSHOT_SELECT").unwrap_or_default().split(',').filter_map(|v| v.trim().parse().ok()).collect();
    let every = env_num("MAIL_SNAPSHOT_EVERY", 1500) as u64;
    for (i, n) in picks.into_iter().enumerate() {
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay / 3 + i as u64 * every), move || {
            select(n.saturating_sub(1));
        });
    }
    let compose_shot = std::env::var_os("MAIL_SNAPSHOT_COMPOSE").is_some();
    if compose_shot {
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay / 3), || compose::open(Start::New));
    }
    glib::timeout_add_local_once(std::time::Duration::from_millis(delay), move || {
        let target: gtk::Window = if compose_shot {
            app.windows().into_iter().find(|w| w.has_css_class("compose-window")).unwrap_or(window.clone().upcast())
        } else {
            window.clone().upcast()
        };
        let (w, h, nav_w, compact) = (target.width(), target.height(), nav.width(), nav.has_css_class("compact"));
        if let Some(child) = target.child() {
            let paintable = gtk::WidgetPaintable::new(Some(&child));
            let snapshot = gtk::Snapshot::new();
            snapshot.append_color(&gdk::RGBA::BLACK, &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32));
            paintable.snapshot(&snapshot, w as f64, h as f64);
            if let (Some(node), Some(renderer)) = (snapshot.to_node(), target.renderer())
                && let Err(e) = renderer.render_texture(node, None).save_to_png(&out)
            {
                eprintln!("snapshot failed: {e}");
            }
        }
        println!("snapshot {w}x{h} nav={nav_w} compact={compact} -> {}", out.display());
        app.quit();
    });
}

// ---------- The engine ----------

fn start_engine() {
    engine::on_changed(|what| {
        if what == "status" {
            refresh_status();
        } else {
            queue_reload();
        }
    });
    engine::watch(
        || {
            if let Some(ui) = ui() {
                ui.borrow_mut().engine_up = true;
            }
            if let Some(w) = window() {
                engine::set_active(w.is_active());
            }
            refresh_status();
            queue_reload();
        },
        || {
            if let Some(ui) = ui() {
                ui.borrow_mut().engine_up = false;
            }
            refresh_status();
        },
    );
    if std::env::var_os("MAIL_SNAPSHOT").is_none() {
        engine::ensure_running();
    }
}

fn refresh_status() {
    engine::status(|status| {
        let Some(ui) = ui() else { return };
        ui.borrow_mut().status = status;
        rebuild_banners();
        refresh_readout();
    });
}

/// Reloads once the current burst of changes has passed.
fn queue_reload() {
    if RELOAD_QUEUED.with(|q| q.replace(true)) {
        return;
    }
    glib::timeout_add_local_once(std::time::Duration::from_millis(150), || {
        RELOAD_QUEUED.with(|q| q.set(false));
        reload();
    });
}

/// Re-reads everything shown.
pub fn reload() {
    let Some(ui) = ui() else { return };
    ui.borrow_mut().mailboxes = data::mailboxes();
    rebuild_nav();
    rebuild_list();
    rebuild_banners();
    let (open, mailboxes) = {
        let u = ui.borrow();
        (u.open.clone(), current_mailboxes(&u))
    };
    reader::show(open.as_deref(), &mailboxes);
    refresh_reader_bar();
    refresh_readout();
}

fn current_mailboxes(u: &Ui) -> Vec<i64> {
    if searching(u) { Vec::new() } else { u.view.mailboxes(&u.mailboxes) }
}

fn searching(u: &Ui) -> bool {
    u.search.is_visible() && !u.search.text().trim().is_empty()
}

/// Shows a message's conversation (from Glimpse, Assistant or a notification).
/// False when there's no such message.
pub fn open_message(id: i64) -> bool {
    let Some(m) = data::message(id) else { return false };
    let Some(ui) = ui() else { return false };
    show_view(View::Mailbox(m.mailbox));
    ui.borrow_mut().open = Some(m.thread.clone());
    reload();
    let position = ui.borrow().rows.iter().position(|r| r.key() == m.thread);
    if let Some(p) = position {
        select(p as u32);
    }
    if let Some(w) = window() {
        w.present();
    }
    true
}

pub(crate) fn show_view(view: View) {
    let Some(ui) = ui() else { return };
    // Another mailbox: what opened on its own wasn't necessarily looked at.
    SEEN_ON_LEAVE.with(|s| s.borrow_mut().take());
    prefs::update(|p| p.last_section = view.id());
    {
        let mut u = ui.borrow_mut();
        u.view = view;
        u.limit = PAGE;
        u.open = None;
    }
    reader::show(None, &[]);
    reload();
    open_first();
}

/// Shows the first conversation of the mailbox just opened (Settings → Reading),
/// without marking it read: it wasn't chosen.
fn open_first() {
    let Some(ui) = ui() else { return };
    let first = {
        let u = ui.borrow();
        u.open.is_none() && matches!(u.rows.first(), Some(Row::Conversation(_)))
    };
    if first && prefs::get().open_first {
        select_quietly(0);
        let thread = ui.borrow().open.clone();
        let unread: Vec<i64> = reader::messages().iter().filter(|m| m.flags & flags::SEEN == 0).map(|m| m.id).collect();
        SEEN_ON_LEAVE.with(|s| *s.borrow_mut() = thread.filter(|_| !unread.is_empty()).map(|t| (t, unread)));
    }
}

/// Shows only conversations with something unread, or everything again.
fn set_unread_only(on: bool) {
    let Some(ui) = ui() else { return };
    if ui.borrow().unread_only == on {
        return;
    }
    ui.borrow_mut().unread_only = on;
    if on {
        // The open conversation is read; it would otherwise stay listed.
        ui.borrow_mut().open = None;
        select(gtk::INVALID_LIST_POSITION);
        reader::show(None, &[]);
    }
    let toggle = ui.borrow().unread_toggle.clone();
    if toggle.is_active() != on {
        match toggle.prev_sibling().and_downcast::<gtk::ToggleButton>() {
            Some(all) if !on => all.set_active(true),
            _ => toggle.set_active(on),
        }
    }
    rebuild_list();
}

fn view_title(u: &Ui) -> String {
    if searching(u) {
        return format!("Results for “{}”", u.search.text().trim());
    }
    view_name(u)
}

fn view_name(u: &Ui) -> String {
    match &u.view {
        View::All(role) => role_name(*role).to_string(),
        View::Kind(kind) => kind.name().to_string(),
        View::Screener => "Screener".into(),
        View::Snoozed => "Snoozed".into(),
        View::Scheduled => "Scheduled".into(),
        View::FollowUps => "Follow-ups".into(),
        View::Senders => "Senders".into(),
        View::Mailbox(id) => {
            let Some(mb) = u.mailboxes.iter().find(|m| m.id == *id) else { return "Mailbox".into() };
            let accounts = data::accounts();
            if accounts.len() > 1 {
                let account = accounts.iter().find(|a| a.id == mb.account).map(|a| a.address.clone()).unwrap_or_default();
                format!("{} · {account}", mb.label())
            } else {
                mb.label()
            }
        }
    }
}

fn role_name(role: Role) -> &'static str {
    match role {
        // Every account's inbox together; "Inbox" would read like "Imbox" beside it.
        Role::Inbox => "Unified",
        Role::Sent => "Sent",
        Role::Drafts => "Drafts",
        Role::Trash => "Trash",
        Role::Junk => "Junk",
        Role::Archive => "Archive",
        Role::All => "All Mail",
        Role::Flagged => "Starred",
    }
}

fn role_icon(role: Option<Role>) -> &'static str {
    match role {
        Some(Role::Inbox) => "mail-unread-symbolic",
        Some(Role::Sent) => "mail-send-symbolic",
        Some(Role::Drafts) => "document-edit-symbolic",
        Some(Role::Trash) => "user-trash-symbolic",
        Some(Role::Junk) => "mail-mark-junk-symbolic",
        Some(Role::Archive) => "nyx-mail-archive-symbolic",
        Some(Role::Flagged) => "starred-symbolic",
        _ => "folder-symbolic",
    }
}

// ---------- The list ----------

fn rebuild_list() {
    let Some(ui) = ui() else { return };
    let (rows, title) = {
        let u = ui.borrow();
        let mut rows: Vec<Row> = Vec::new();
        if searching(&u) {
            rows.extend(data::search(u.search.text().trim(), u.limit).into_iter().map(Row::Conversation));
        } else {
            let mailboxes = u.view.mailboxes(&u.mailboxes);
            match &u.view {
                View::Scheduled => rows.extend(data::scheduled().into_iter().map(|s| Row::Scheduled(Box::new(s)))),
                View::Senders => rows.extend(data::senders(&mailboxes).into_iter().map(|s| Row::Sender(Box::new(s)))),
                View::FollowUps => rows
                    .extend(data::follow_ups().iter().filter_map(|f| data::newest_in_thread(&f.thread)).map(Row::Conversation)),
                view => {
                    if *view == View::All(Role::Drafts) {
                        for (id, _, json, updated) in data::drafts() {
                            if let Ok(draft) = serde_json::from_value::<Draft>(json) {
                                rows.push(Row::Draft { id, draft: Box::new(draft), updated });
                            }
                        }
                    }
                    let mut filter = view.filter(&u.mailboxes, data::screener_on());
                    filter.unread = u.unread_only;
                    filter.keep = u.open.clone();
                    rows.extend(data::list_with(&mailboxes, &filter, u.limit).into_iter().map(Row::Conversation));
                }
            }
        }
        rows.retain(|r| match r {
            Row::Conversation(c) => !u.hidden.contains(&c.id),
            _ => true,
        });
        (rows, view_title(&u))
    };
    ui.borrow().list_title.set_text(&title);
    ui.borrow().header.set_subtitle(&title);
    chrome::set_title(&ui.borrow().window, "Mail", &title);

    let (store, selection, open, scroll) = {
        let u = ui.borrow();
        (u.store.clone(), u.selection.clone(), u.open.clone(), u.list_scroll.clone())
    };
    // Only the rows that changed are replaced (marking one read replaces one):
    // replacing them all would scroll the list back to the top.
    let shows = {
        let u = ui.borrow();
        format!("{}\u{1f}{}", u.view.id(), if searching(&u) { u.search.text().to_string() } else { String::new() })
    };
    let same_list = ui.borrow().list_shows == shows;
    let old: Vec<String> = ui.borrow().rows.iter().map(|r| format!("{r:?}")).collect();
    let new: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    let objects: Vec<glib::Object> =
        rows[prefix..rows.len() - suffix].iter().map(|r| glib::BoxedAnyObject::new(r.clone()).upcast()).collect();
    let at = scroll.vadjustment().value();
    ui.borrow_mut().restoring = true;
    if store.n_items() as usize != old.len() {
        // Out of step (shouldn't happen): start again.
        let all: Vec<glib::Object> = rows.iter().map(|r| glib::BoxedAnyObject::new(r.clone()).upcast()).collect();
        store.splice(0, store.n_items(), &all);
    } else {
        store.splice(prefix as u32, (old.len() - prefix - suffix) as u32, &objects);
    }
    ui.borrow_mut().list_shows = shows;
    if same_list {
        // Same list, same place.
        scroll.vadjustment().set_value(at);
        let s = scroll.clone();
        glib::idle_add_local_once(move || s.vadjustment().set_value(at));
    }
    let position = open.as_ref().and_then(|key| rows.iter().position(|r| r.key() == *key));
    selection.set_selected(position.map(|p| p as u32).unwrap_or(gtk::INVALID_LIST_POSITION));
    ui.borrow_mut().restoring = false;
    let empty = rows.is_empty();
    ui.borrow_mut().rows = rows;
    if open.is_some() && position.is_none() {
        // The open conversation left this view (archived elsewhere, say).
        ui.borrow_mut().open = None;
    }
    let (stack, empty_box) = {
        let u = ui.borrow();
        (u.list_stack.clone(), u.empty.clone())
    };
    if empty {
        fill_empty(&empty_box);
        stack.set_visible_child_name("empty");
    } else {
        stack.set_visible_child_name("list");
    }
}

fn fill_empty(slot: &gtk::Box) {
    while let Some(child) = slot.first_child() {
        slot.remove(&child);
    }
    let Some(ui) = ui() else { return };
    let u = ui.borrow();
    let no_accounts = data::accounts().is_empty() && u.status.as_ref().is_none_or(|s| s.accounts.is_empty());
    let state = if no_accounts && u.engine_up {
        widgets::empty_state(
            "mail-unread-symbolic",
            "No mail accounts",
            "Add an account in Settings → Online Accounts and switch on Mail.",
            Some(("Add an account", Box::new(open_online_accounts))),
        )
    } else if no_accounts {
        widgets::empty_state("mail-send-receive-symbolic", "Starting Mail…", "", None)
    } else if searching(&u) {
        widgets::empty_state("edit-find-symbolic", "Nothing found", "", None)
    } else {
        let text = match (&u.view, u.view.role(&u.mailboxes)) {
            (View::Screener, _) => "Nobody new is waiting",
            (View::Snoozed, _) => "Nothing snoozed",
            (View::Scheduled, _) => "Nothing scheduled to send",
            (View::FollowUps, _) => "No follow-ups set",
            (View::Senders, _) => "Nobody has written yet",
            (View::Kind(Kind::Feed), _) => "Nothing in the Feed",
            (View::Kind(Kind::Paper), _) => "No receipts or notices",
            (_, Some(Role::Inbox)) => "Your inbox is empty",
            (_, Some(Role::Drafts)) => "No drafts",
            (_, Some(Role::Trash)) => "Trash is empty",
            (_, Some(Role::Junk)) => "No junk",
            _ => "No messages",
        };
        widgets::empty_state(role_icon(u.view.role(&u.mailboxes)), text, "", None)
    };
    slot.append(&state);
}

fn list_row(row: &Row) -> gtk::Box {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 3);
    outer.add_css_class("mail-row");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let (who, when, subject, preview, unread, count, attach, flagged, draft) = match row {
        Row::Scheduled(s) => (
            format!("To {}", s.to.join(", ")),
            when_text(s.at, true),
            s.subject.clone(),
            "Sends from this computer at that time.".to_string(),
            false,
            1,
            false,
            false,
            false,
        ),
        Row::Sender(s) => (
            if s.name.trim().is_empty() { s.address.clone() } else { s.name.clone() },
            when_text(s.last, false),
            s.address.clone(),
            {
                let mut facts = vec![s.kind.name().to_string()];
                if s.screened.as_deref() == Some("out") {
                    facts.push("screened out".into());
                }
                if s.unsubscribe.is_some() {
                    facts.push("can unsubscribe".into());
                }
                facts.join(" · ")
            },
            s.unread > 0,
            s.messages,
            false,
            false,
            false,
        ),
        Row::Conversation(c) => (
            if c.from_name.trim().is_empty() { c.from_addr.clone() } else { c.from_name.clone() },
            when_text(c.date, false),
            c.subject.clone(),
            c.preview.clone(),
            c.unread,
            c.count,
            c.has_attachments,
            c.flags & flags::FLAGGED != 0,
            false,
        ),
        Row::Draft { draft, updated, .. } => (
            {
                let to = nyx_mail_core::send::people_text(&draft.to);
                if to.is_empty() { "No recipients".to_string() } else { format!("To {to}") }
            },
            when_text(*updated, false),
            draft.subject.clone(),
            draft.body.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string(),
            false,
            1,
            !draft.attachments.is_empty(),
            false,
            true,
        ),
    };
    if unread {
        outer.add_css_class("unread");
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("unread-dot");
        dot.set_valign(gtk::Align::Center);
        top.append(&dot);
    }
    if draft {
        let tag = widgets::label("Draft", "draft-tag");
        top.append(&tag);
    }
    let name = widgets::label(&who, "mail-from");
    name.set_xalign(0.0);
    name.set_hexpand(true);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    top.append(&name);
    if count > 1 {
        top.append(&widgets::label(&count.to_string(), "thread-count"));
    }
    if attach {
        let i = gtk::Image::from_icon_name("mail-attachment-symbolic");
        i.add_css_class("dim");
        top.append(&i);
    }
    if flagged {
        let i = gtk::Image::from_icon_name("starred-symbolic");
        i.add_css_class("flag");
        top.append(&i);
    }
    top.append(&widgets::label(&when, "mail-date"));
    outer.append(&top);
    let subject = widgets::label(if subject.trim().is_empty() { "(no subject)" } else { subject.trim() }, "mail-subject");
    subject.set_xalign(0.0);
    subject.set_ellipsize(gtk::pango::EllipsizeMode::End);
    outer.append(&subject);
    if !preview.trim().is_empty() && prefs::get().show_previews {
        let p = widgets::label(preview.trim(), "mail-preview");
        p.set_xalign(0.0);
        p.set_single_line_mode(true);
        p.set_ellipsize(gtk::pango::EllipsizeMode::End);
        outer.append(&p);
    }
    outer
}

/// A conversation's row, with Archive, Star, Read and Delete over the end of its
/// preview while the pointer is on it. Each acts on that row without marking it read.
fn with_row_actions(content: &gtk::Widget, item: glib::WeakRef<gtk::ListItem>) -> gtk::Widget {
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(content));
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    actions.add_css_class("row-actions");
    actions.set_halign(gtk::Align::End);
    actions.set_valign(gtk::Align::End);
    actions.set_margin_end(8);
    actions.set_margin_bottom(5);
    actions.set_visible(false);
    let buttons: [(&str, &str, fn()); 4] = [
        ("nyx-mail-archive-symbolic", "Archive (E)", archive),
        ("starred-symbolic", "Star or unstar (S)", toggle_star),
        ("mail-unread-symbolic", "Mark read or unread (U)", toggle_unread),
        ("user-trash-symbolic", "Move to Trash (#)", trash),
    ];
    for (icon, tip, run) in buttons {
        let b = widgets::icon_button(icon, tip);
        b.add_css_class("row-action");
        let item = item.clone();
        b.connect_clicked(move |_| {
            if let Some(item) = item.upgrade()
                && item.position() != gtk::INVALID_LIST_POSITION
            {
                select_quietly(item.position());
                run();
            }
        });
        actions.append(&b);
    }
    overlay.add_overlay(&actions);
    let motion = gtk::EventControllerMotion::new();
    let shown = actions.clone();
    motion.connect_enter(move |_, _, _| shown.set_visible(true));
    motion.connect_leave(move |_| actions.set_visible(false));
    overlay.add_controller(motion);
    overlay.upcast()
}

fn selected_row() -> Option<Row> {
    let ui = ui()?;
    let u = ui.borrow();
    let pos = u.selection.selected();
    u.rows.get(pos as usize).cloned()
}

fn on_selected() {
    let Some(ui) = ui() else { return };
    if ui.borrow().restoring {
        return;
    }
    match selected_row() {
        Some(Row::Conversation(c)) => {
            // Leaving the conversation that opened on its own: it was seen.
            if let Some((thread, ids)) = SEEN_ON_LEAVE.with(|s| s.borrow_mut().take()) {
                if thread != c.thread && prefs::get().mark_read {
                    engine::set_flags(&ids, flags::SEEN, 0);
                } else if thread == c.thread {
                    SEEN_ON_LEAVE.with(|s| *s.borrow_mut() = Some((thread, ids)));
                }
            }
            ui.borrow_mut().open = Some(c.thread.clone());
            let mailboxes = current_mailboxes(&ui.borrow());
            reader::show(Some(&c.thread), &mailboxes);
            let p = prefs::get();
            if p.mark_read && !QUIET.with(|q| q.get()) {
                let mark = || {
                    let unread: Vec<i64> =
                        reader::messages().iter().filter(|m| m.flags & flags::SEEN == 0).map(|m| m.id).collect();
                    engine::set_flags(&unread, flags::SEEN, 0);
                };
                if p.mark_read_delay == 0 {
                    mark();
                } else {
                    // Still open after the delay: read.
                    let thread = c.thread.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_secs(p.mark_read_delay as u64), move || {
                        if self::ui().is_some_and(|u| u.borrow().open.as_deref() == Some(thread.as_str())) {
                            mark();
                        }
                    });
                }
            }
        }
        Some(Row::Draft { id, .. }) => {
            ui.borrow_mut().open = None;
            reader::show(None, &[]);
            compose::open(Start::Draft(id));
        }
        Some(Row::Scheduled(s)) => {
            ui.borrow_mut().open = None;
            reader::show_card(&organize::scheduled_card(&s));
        }
        Some(Row::Sender(s)) => {
            ui.borrow_mut().open = None;
            reader::show_card(&organize::sender_card(&s));
        }
        None => {
            ui.borrow_mut().open = None;
            reader::show(None, &[]);
        }
    }
    refresh_reader_bar();
}

/// Selects a row (never while the UI state is borrowed: the selection's
/// handler borrows it).
pub(crate) fn select(position: u32) {
    let Some((selection, view)) = ui().map(|u| (u.borrow().selection.clone(), u.borrow().list_view.clone())) else { return };
    selection.set_selected(position);
    // Keep the selected row in view (arrowing down past the bottom, say).
    if position != gtk::INVALID_LIST_POSITION && position < selection.n_items() {
        view.scroll_to(position, gtk::ListScrollFlags::NONE, None);
    }
}

/// Selects a row without marking it read (a right-click on it).
pub(crate) fn select_quietly(position: u32) {
    QUIET.with(|q| q.set(true));
    select(position);
    QUIET.with(|q| q.set(false));
}

pub(crate) fn row_at(position: u32) -> Option<Row> {
    ui()?.borrow().rows.get(position as usize).cloned()
}

pub(crate) fn selected_position() -> Option<u32> {
    let p = ui()?.borrow().selection.selected();
    (p != gtk::INVALID_LIST_POSITION).then_some(p)
}

/// The widget showing a list position, if it's on screen.
pub(crate) fn row_widget(position: u32) -> Option<gtk::Widget> {
    ROW_WIDGETS.with(|w| w.borrow().get(&position).and_then(|w| w.upgrade()))
}

/// The role of the mailbox being looked at (Trash, Junk…), if it has one.
pub(crate) fn current_role() -> Option<Role> {
    let ui = ui()?;
    let u = ui.borrow();
    u.view.role(&u.mailboxes)
}

pub(crate) fn mailboxes() -> Vec<Mailbox> {
    ui().map(|u| u.borrow().mailboxes.clone()).unwrap_or_default()
}

fn move_selection(step: i32) {
    let Some(ui) = ui() else { return };
    let (selection, n) = {
        let u = ui.borrow();
        (u.selection.clone(), u.rows.len() as i32)
    };
    if n == 0 {
        return;
    }
    let current = selection.selected();
    let next = if current == gtk::INVALID_LIST_POSITION { 0 } else { (current as i32 + step).clamp(0, n - 1) };
    select(next as u32);
}

fn load_more() {
    let Some(ui) = ui() else { return };
    let more = {
        let u = ui.borrow();
        u.rows.len() >= u.limit
    };
    if more {
        ui.borrow_mut().limit += PAGE;
        rebuild_list();
    }
}

// ---------- Actions on the open conversation ----------

/// The open conversation's messages in the mailboxes being looked at (not the
/// user's replies sitting in Sent).
fn targets() -> Vec<i64> {
    let Some(ui) = ui() else { return Vec::new() };
    let mailboxes = current_mailboxes(&ui.borrow());
    reader::messages().into_iter().filter(|m| mailboxes.is_empty() || mailboxes.contains(&m.mailbox)).map(|m| m.id).collect()
}

/// Archive, Junk, Trash, Inbox or delete for good, with Undo.
fn act(target: Option<&str>, done_text: &str) {
    let ids = targets();
    let Some(ui) = ui() else { return };
    if ids.is_empty() {
        return;
    }
    let done = Rc::new(Cell::new(false));
    {
        let mut u = ui.borrow_mut();
        u.hidden.extend(ids.iter().copied());
        u.pending.push(Pending { ids: ids.clone(), target: target.map(str::to_string), done: done.clone() });
    }
    // Select the next conversation, as the list closes up.
    let pos = ui.borrow().selection.selected();
    ui.borrow_mut().open = None;
    rebuild_list();
    let n = ui.borrow().rows.len() as u32;
    let then = prefs::get().after_moving;
    if n > 0 && pos != gtk::INVALID_LIST_POSITION && then != "list" {
        let at = if then == "previous" { pos.saturating_sub(1) } else { pos };
        select(at.min(n - 1));
    } else {
        select(gtk::INVALID_LIST_POSITION);
        reader::show(None, &[]);
    }
    let undo_done = done.clone();
    let undo_ids = ids.clone();
    toast_undo(done_text, move || {
        if undo_done.replace(true) {
            return;
        }
        if let Some(ui) = self::ui() {
            for id in &undo_ids {
                ui.borrow_mut().hidden.remove(id);
            }
        }
        rebuild_list();
    });
    let target = target.map(str::to_string);
    glib::timeout_add_local_once(std::time::Duration::from_millis(6200), move || commit(&done, &ids, Some(target)));
}

/// Moves the open conversation to a target the engine knows (`archive`,
/// `mailbox:<id>`…), with Undo.
pub(crate) fn move_to(target: &str, done_text: &str) {
    act(Some(target), done_text);
}

fn commit(done: &Rc<Cell<bool>>, ids: &[i64], target: Option<Option<String>>) {
    if done.replace(true) {
        return;
    }
    match target {
        Some(Some(t)) => engine::move_to(ids, &t),
        Some(None) => engine::delete(ids),
        None => {}
    }
    if let Some(ui) = ui() {
        ui.borrow_mut().pending.retain(|p| p.ids != ids);
    }
}

/// Sends every change still waiting for its toast (the window is closing).
fn flush_pending() {
    let Some(ui) = ui() else { return };
    let pending = ui.borrow().pending.clone();
    for p in pending {
        commit(&p.done, &p.ids, Some(p.target));
    }
}

pub(crate) fn archive() {
    act(Some("archive"), "Archived.");
}

pub(crate) fn trash() {
    let role = ui().and_then(|u| {
        let u = u.borrow();
        u.view.role(&u.mailboxes)
    });
    if matches!(role, Some(Role::Trash)) {
        act(None, "Deleted.");
    } else {
        act(Some("trash"), "Moved to Trash.");
    }
}

pub(crate) fn junk() {
    let role = ui().and_then(|u| {
        let u = u.borrow();
        u.view.role(&u.mailboxes)
    });
    if matches!(role, Some(Role::Junk)) {
        act(Some("inbox"), "Moved to Inbox.");
    } else {
        act(Some("junk"), "Moved to Junk.");
    }
}

pub(crate) fn toggle_star() {
    let messages = reader::messages();
    let Some(last) = messages.last() else { return };
    let starred = messages.iter().any(|m| m.flags & flags::FLAGGED != 0);
    if starred {
        let ids: Vec<i64> = messages.iter().filter(|m| m.flags & flags::FLAGGED != 0).map(|m| m.id).collect();
        engine::set_flags(&ids, 0, flags::FLAGGED);
    } else {
        engine::set_flags(&[last.id], flags::FLAGGED, 0);
    }
}

pub(crate) fn toggle_unread() {
    let ids = targets();
    let messages = reader::messages();
    let any_unread = messages.iter().any(|m| m.flags & flags::SEEN == 0);
    if any_unread {
        engine::set_flags(&ids, flags::SEEN, 0);
    } else if let Some(last) = messages.iter().rev().find(|m| ids.contains(&m.id)) {
        engine::set_flags(&[last.id], 0, flags::SEEN);
        // Leave it unread: close it so it isn't marked read again.
        if let Some(ui) = ui() {
            ui.borrow_mut().open = None;
            select(gtk::INVALID_LIST_POSITION);
        }
        reader::show(None, &[]);
    }
}

/// The message a reply answers: the newest not from the user, else the newest.
fn reply_target() -> Option<i64> {
    let messages = reader::messages();
    let me = data::own_addresses();
    messages.iter().rev().find(|m| !me.iter().any(|a| a.eq_ignore_ascii_case(&m.from_addr))).or(messages.last()).map(|m| m.id)
}

pub(crate) fn reply(all: bool) {
    if let Some(id) = reply_target() {
        compose::open(Start::Reply { message: id, all });
    }
}

pub(crate) fn forward() {
    if let Some(m) = reader::messages().last() {
        compose::open(Start::Forward(m.id));
    }
}

pub(crate) fn check_mail() {
    engine::sync("");
    toast("Checking for mail…");
}

fn refresh_reader_bar() {
    let Some(ui) = ui() else { return };
    let open = !reader::messages().is_empty();
    ui.borrow().reader_bar.set_sensitive(open);
}

// ---------- Status and banners ----------

fn refresh_readout() {
    let Some(ui) = ui() else { return };
    let u = ui.borrow();
    let mut parts = Vec::new();
    if u.status.is_some() || !u.rows.is_empty() {
        let n = u.rows.len();
        let (one, many) = match u.view {
            View::Senders => ("sender", "senders"),
            View::Scheduled => ("scheduled message", "scheduled messages"),
            _ => ("conversation", "conversations"),
        };
        parts.push(match n {
            0 => format!("No {many}"),
            1 => format!("1 {one}"),
            n if n >= u.limit => format!("{n}+ {many}"),
            n => format!("{n} {many}"),
        });
    }
    match &u.status {
        Some(s) => {
            if s.accounts.iter().any(|a| a.syncing) {
                parts.push("Checking for mail…".into());
            } else if !s.accounts.is_empty() && s.accounts.iter().all(|a| a.offline) {
                parts.push("Offline".into());
            } else if let Some(last) = s.accounts.iter().filter_map(|a| a.last_sync).max() {
                parts.push(format!("Updated {}", when_text(last, false)));
            }
            if s.pending > 0 {
                parts.push(if s.pending == 1 { "1 change waiting".into() } else { format!("{} changes waiting", s.pending) });
            }
        }
        None if !u.engine_up => parts.push("Mail's engine isn't running".into()),
        None => {}
    }
    STATUS.with(|s| {
        if let Some(label) = s.borrow().as_ref() {
            label.set_text(&parts.join(" · "));
        }
    });
}

fn rebuild_banners() {
    let Some(ui) = ui() else { return };
    let (banners, status) = {
        let u = ui.borrow();
        (u.banners.clone(), u.status.clone())
    };
    while let Some(child) = banners.first_child() {
        banners.remove(&child);
    }
    if let Some(status) = &status {
        let dismissed: HashSet<String> = prefs::get().dismissed_status_banners.into_iter().collect();
        let mut active = HashSet::new();
        let mut notice = |text: String, warning: bool, action: Option<gtk::Button>| {
            active.insert(text.clone());
            if dismissed.contains(&text) {
                return;
            }
            let banner = widgets::banner(&text, warning);
            if let Some(action) = action {
                banner.append(&action);
            }
            let close = widgets::icon_button("window-close-symbolic", "Dismiss message");
            close.set_valign(gtk::Align::Center);
            close.connect_clicked(move |_| {
                prefs::update(|p| {
                    if !p.dismissed_status_banners.contains(&text) {
                        p.dismissed_status_banners.push(text.clone());
                    }
                });
                rebuild_banners();
            });
            banner.append(&close);
            banners.append(&banner);
        };
        for a in &status.accounts {
            let Some(error) = &a.error else { continue };
            let text = format!("<b>{}</b>: {}", glib::markup_escape_text(&a.address), glib::markup_escape_text(error));
            let action = if error.contains("Sign in again") {
                let button = gtk::Button::with_label("Sign in");
                button.set_valign(gtk::Align::Center);
                button.connect_clicked(|_| open_online_accounts());
                Some(button)
            } else {
                None
            };
            notice(text, true, action);
        }
        for s in &status.skipped {
            notice(format!(
                "<b>{}</b> can't be used for mail: {}",
                glib::markup_escape_text(&s.address),
                glib::markup_escape_text(&s.reason)
            ), false, None);
        }
        if status.accounts.iter().any(|a| a.offline) && !status.accounts.iter().all(|a| a.offline) {
            let names: Vec<&str> = status.accounts.iter().filter(|a| a.offline).map(|a| a.address.as_str()).collect();
            notice(format!("Can't reach {}. Showing saved mail.", glib::markup_escape_text(&names.join(", "))), false, None);
        } else if !status.accounts.is_empty() && status.accounts.iter().all(|a| a.offline) {
            notice("Offline. Showing saved mail; changes and sent mail go when you're back online.".into(), false, None);
        }
        // A profile awaiting sign-in is still configured, not a removed account.
        for a in data::accounts().into_iter().filter(|a| {
            !status.accounts.iter().any(|s| s.id == a.id)
                && !status.skipped.iter().any(|s| s.address.eq_ignore_ascii_case(&a.address))
        }) {
            let text = format!(
                "<b>{}</b> was removed from Online Accounts. Its mail is still on this computer.",
                glib::markup_escape_text(&a.address)
            );
            let button = gtk::Button::with_label("Forget");
            button.set_tooltip_text(Some("Remove its mail from this computer (the server keeps it)"));
            button.set_valign(gtk::Align::Center);
            let (id, address) = (a.id.clone(), a.address.clone());
            button.connect_clicked(move |_| forget_account(&id, &address));
            notice(text, false, Some(button));
        }
        if dismissed.iter().any(|key| !active.contains(key)) {
            prefs::update(|p| p.dismissed_status_banners.retain(|key| active.contains(key)));
        }
    }
    banners.set_visible(banners.first_child().is_some());
}

fn forget_account(id: &str, address: &str) {
    let done = Rc::new(Cell::new(false));
    let d = done.clone();
    toast_undo(&format!("Forgetting {address}'s mail on this computer."), move || d.set(true));
    let id = id.to_string();
    glib::timeout_add_local_once(std::time::Duration::from_millis(6200), move || {
        if !done.replace(true) {
            engine::forget(&id);
        }
    });
}

pub fn open_online_accounts() {
    let settings = nyx_accounts::find_bin("nyx-settings");
    let started =
        settings.is_some_and(|bin| std::process::Command::new(bin).args(["--section", "online-accounts"]).spawn().is_ok());
    if !started && nyx_accounts::open_management().is_err() {
        toast("Install Nyx Settings or gnome-online-accounts-gtk to add accounts.");
    }
}

// ---------- Sidebar ----------

fn nav_button(icon: &str, title: &str, tooltip: &str, count: i64, active: bool) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("nav-item");
    if active {
        button.add_css_class("active");
    }
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    content.append(&gtk::Image::from_icon_name(icon));
    let label = widgets::label(title, "nav-label");
    label.set_hexpand(true);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.add_css_class("compact-hide");
    content.append(&label);
    if count > 0 {
        let badge = widgets::label(&count.to_string(), "nav-readout");
        badge.add_css_class("compact-hide");
        content.append(&badge);
    }
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(tooltip));
    button
}

fn group_label(text: &str, first: bool, add: bool) -> gtk::Widget {
    let label = widgets::label(&text.to_uppercase(), "nav-group");
    label.add_css_class("compact-hide");
    if first {
        label.add_css_class("first");
    }
    if !add {
        return label.upcast();
    }
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("compact-hide");
    label.set_hexpand(true);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&label);
    let plus = widgets::icon_button("list-add-symbolic", "Add an account");
    plus.add_css_class("flat");
    plus.add_css_class("nav-group-add");
    plus.set_valign(gtk::Align::End);
    plus.connect_clicked(|_| open_online_accounts());
    row.append(&plus);
    row.upcast()
}

/// An account's heading: click (or the chevron) folds its folders away;
/// right-click for the account's menu.
fn account_header(account: &nyx_mail_core::AccountRow, first_account: bool, folded: bool) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("compact-hide");
    row.add_css_class("account-header");
    let label = widgets::label(&account.address.to_uppercase(), "nav-group");
    label.set_hexpand(true);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&label);
    if first_account {
        let plus = widgets::icon_button("list-add-symbolic", "Add an account");
        plus.add_css_class("flat");
        plus.add_css_class("nav-group-add");
        plus.set_valign(gtk::Align::End);
        plus.connect_clicked(|_| open_online_accounts());
        row.append(&plus);
    }
    let (icon, tip) = if folded { ("pan-end-symbolic", "Show its folders") } else { ("pan-down-symbolic", "Hide its folders") };
    let fold = widgets::icon_button(icon, tip);
    fold.add_css_class("flat");
    fold.add_css_class("nav-group-add");
    fold.set_valign(gtk::Align::End);
    let id = account.id.clone();
    fold.connect_clicked(move |_| toggle_account_folded(&id));
    row.append(&fold);
    let click = gtk::GestureClick::new();
    let id = account.id.clone();
    click.connect_released(move |_, _, _, _| toggle_account_folded(&id));
    label.add_controller(click);
    crate::menus::attach_account(&row, account, folded);
    row.upcast()
}

pub(crate) fn toggle_account_folded(id: &str) {
    prefs::update(|p| {
        if p.collapsed_accounts.iter().any(|a| a == id) {
            p.collapsed_accounts.retain(|a| a != id);
        } else {
            p.collapsed_accounts.push(id.to_string());
        }
    });
    rebuild_nav();
}

pub(crate) fn toggle_pinned(m: &Mailbox) {
    let key = prefs::pin_key(&m.account, &m.name);
    let pinned = prefs::get().pinned.contains(&key);
    prefs::update(|p| {
        p.pinned.retain(|k| *k != key);
        if !pinned {
            p.pinned.push(key.clone());
        }
    });
    rebuild_nav();
    toast(&if pinned { format!("Unpinned {}.", m.label()) } else { format!("Pinned {}.", m.label()) });
}

pub(crate) fn rebuild_nav() {
    let Some(ui) = ui() else { return };
    let list_box = ui.borrow().nav_list.clone();
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    let (view, mailboxes, search_on) = {
        let u = ui.borrow();
        (u.view.clone(), u.mailboxes.clone(), searching(&u))
    };
    let active = |v: &View| !search_on && view == *v;
    let accounts = data::accounts();
    let several = accounts.len() > 1;
    let heading = group_label(if several { "All accounts" } else { "Mailboxes" }, true, false);
    nyx_ui::sidebar::set_section(&heading, nyx_ui::sidebar::ACCENT_SECTION);
    list_box.append(&heading);
    let drafts = data::drafts().len() as i64;
    for (role, key) in
        [(Role::Inbox, "1"), (Role::Drafts, "2"), (Role::Sent, "3"), (Role::Archive, "4"), (Role::Junk, ""), (Role::Trash, "")]
    {
        let has = mailboxes.iter().any(|m| m.role == Some(role));
        if !has && !matches!(role, Role::Inbox | Role::Drafts) {
            continue;
        }
        let count = match role {
            Role::Inbox => mailboxes.iter().filter(|m| m.role == Some(Role::Inbox)).map(|m| m.unread).sum(),
            Role::Drafts => drafts,
            Role::Junk => mailboxes.iter().filter(|m| m.role == Some(Role::Junk)).map(|m| m.unread).sum(),
            _ => 0,
        };
        let name = role_name(role);
        let tooltip = if key.is_empty() { name.to_string() } else { format!("{name} ({key})") };
        let v = View::All(role);
        let button = nav_button(role_icon(Some(role)), name, &tooltip, count, active(&v));
        button.connect_clicked(move |_| show_view(v.clone()));
        crate::menus::attach_role(&button, role);
        list_box.append(&button);
        // The three kinds of inbox mail, sorted on this computer, under Unified.
        if role == Role::Inbox {
            for (kind, icon) in [
                (Kind::Imbox, "avatar-default-symbolic"),
                (Kind::Feed, "application-rss+xml-symbolic"),
                (Kind::Paper, "x-office-document-symbolic"),
            ] {
                let v = View::Kind(kind);
                let count = data::kind_unread(&mailboxes, kind) as i64;
                let button = nav_button(icon, kind.name(), kind.name(), count, active(&v));
                button.connect_clicked(move |_| show_view(v.clone()));
                list_box.append(&button);
            }
        }
    }

    // Sorting and timing (kept on this computer).
    let status = ui.borrow().status.clone().unwrap_or_default();
    let mut extra: Vec<(View, &str, &str, i64)> = Vec::new();
    if status.screener || data::screener_on() {
        extra.push((View::Screener, "view-reveal-symbolic", "Screener", data::screener_count(&mailboxes) as i64));
    }
    extra.push((View::Snoozed, "alarm-symbolic", "Snoozed", 0));
    extra.push((View::Scheduled, "x-office-calendar-symbolic", "Scheduled", status.scheduled));
    extra.push((View::FollowUps, "document-open-recent-symbolic", "Follow-ups", status.follow_ups));
    extra.push((View::Senders, "x-office-address-book-symbolic", "Senders", 0));
    for (v, icon, name, count) in extra {
        let button = nav_button(icon, name, name, count, active(&v));
        button.connect_clicked(move |_| show_view(v.clone()));
        list_box.append(&button);
    }

    // Pinned folders, in the order they were pinned.
    let pins: Vec<&Mailbox> =
        prefs::get().pinned.iter().filter_map(|k| mailboxes.iter().find(|m| prefs::pin_key(&m.account, &m.name) == *k)).collect();
    if !pins.is_empty() {
        let heading = group_label("Pinned", false, false);
        nyx_ui::sidebar::set_section(&heading, "pinned");
        list_box.append(&heading);
        for m in pins {
            let address = accounts.iter().find(|a| a.id == m.account).map(|a| a.address.as_str()).unwrap_or("");
            let tooltip = if several { format!("{} · {address}", m.name) } else { m.name.clone() };
            let v = View::Mailbox(m.id);
            let button = nav_button(role_icon(m.role), &m.label(), &tooltip, m.unread, active(&v));
            button.connect_clicked(move |_| show_view(v.clone()));
            crate::menus::attach_mailbox(&button, m);
            list_box.append(&button);
        }
    }

    let collapsed = prefs::get().collapsed_accounts;
    for (i, account) in accounts.iter().enumerate() {
        let folded = collapsed.contains(&account.id);
        // Each account in the next swatch, in the accounts' order.
        let heading = account_header(account, i == 0, folded);
        nyx_ui::sidebar::set_section(&heading, &nyx_ui::sidebar::rotating_section(i));
        list_box.append(&heading);
        if folded {
            continue;
        }
        let mine: Vec<&Mailbox> = mailboxes
            .iter()
            .filter(|m| m.account == account.id)
            .filter(|m| !matches!(m.role, Some(Role::All) | Some(Role::Flagged)))
            .filter(|m| several && m.role == Some(Role::Inbox) || m.role.is_none())
            .collect();
        for m in mine {
            // Indented under parents that are shown (not Gmail's "[Gmail]").
            let depth = m
                .delimiter
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(|d| {
                    let parts: Vec<&str> = m.name.split(d).collect();
                    (1..parts.len())
                        .filter(|i| mailboxes.iter().any(|p| p.account == m.account && p.name == parts[..*i].join(d)))
                        .count()
                })
                .unwrap_or(0);
            let v = View::Mailbox(m.id);
            let button = nav_button(role_icon(m.role), &m.label(), &m.name, m.unread, active(&v));
            if let Some(content) = button.child() {
                content.set_margin_start((depth.min(4) * 14) as i32);
            }
            button.connect_clicked(move |_| show_view(v.clone()));
            crate::menus::attach_mailbox(&button, m);
            list_box.append(&button);
        }
    }
    if accounts.is_empty() {
        list_box.append(&group_label("Accounts", false, true));
    }

    if let Some(foot) = ui.borrow().nav.last_child().and_downcast::<gtk::Box>() {
        while let Some(child) = foot.first_child() {
            foot.remove(&child);
        }
        if let Some(address) = compose::send_from() {
            foot.append(&send_from_row(&address));
        }
    }

    let nav = ui.borrow().nav.clone();
    apply_compact(&nav, NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
}

// ---------- Building ----------

fn install_icons() {
    let dir = nyx_core::paths::cache_dir().join("icons");
    let name = "nyx-mail-archive-symbolic.svg";
    let svg = include_str!("../data/nyx-mail-archive-symbolic.svg");
    let path = dir.join(name);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(svg) {
        let _ = nyx_core::cmd::atomic_write(&path, svg);
    }
    if let Some(display) = gdk::Display::default() {
        gtk::IconTheme::for_display(&display).add_search_path(dir);
    }
}

fn build(app: &gtk::Application) {
    install_icons();
    let window = gtk::ApplicationWindow::builder().application(app).title("Mail").default_width(1120).default_height(800).build();
    window.add_css_class("mail-window");
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.set_icon_name(Some("io.github.design_nexus.nyx.Mail"));
    nyx_ui::keys::install(
        &window,
        nyx_ui::keys::Common {
            settings: Some(settings_dialog::open),
            shortcuts: Some(show_shortcuts),
            close: true,
            ..Default::default()
        },
    );

    let nav = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav.add_css_class("settings-navigation");
    nav.set_hexpand(false);
    let nav_list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav_list.set_hexpand(false);
    let nav_scroll = gtk::ScrolledWindow::builder()
        .hexpand(false)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&nav_list)
        .build();
    // The sidebar's own toggle, at its top right (centred on the narrow rail).
    let nav_top = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    nav_top.add_css_class("nav-top");
    let collapse = nyx_ui::widgets::bar_button("sidebar-show-symbolic", "Collapse the sidebar (Ctrl+B)");
    collapse.add_css_class("nav-toggle");
    collapse.set_hexpand(true);
    collapse.set_halign(gtk::Align::End);
    collapse.connect_clicked(|_| toggle_sidebar());
    nav_top.append(&collapse);
    nav.append(&nav_top);
    nav.append(&nav_scroll);
    let nav_foot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav_foot.add_css_class("nav-foot");
    nav.append(&nav_foot);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search mail  (Ctrl+F)"));
    search.update_property(&[gtk::accessible::Property::Label("Search mail")]);
    search.add_css_class("bar-search");
    search.set_width_chars(40);
    search.connect_search_changed(|_| {
        if let Some(ui) = ui() {
            ui.borrow_mut().limit = PAGE;
        }
        rebuild_list();
        rebuild_nav();
        refresh_readout();
    });
    search.connect_stop_search(leave_search);

    let banners = gtk::Box::new(gtk::Orientation::Vertical, 8);
    banners.add_css_class("banners");
    banners.set_visible(false);

    // The list column.
    let list_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    list_bar.add_css_class("toolbar");
    let list_title = widgets::label("", "toolbar-title");
    list_title.set_xalign(0.0);
    list_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    // A long title ("Results for …") shortens; it never widens the list.
    list_title.set_max_width_chars(8);
    list_bar.append(&list_title);
    let readout = widgets::label("", "status-readout");
    readout.set_ellipsize(gtk::pango::EllipsizeMode::End);
    readout.set_xalign(0.0);
    readout.set_hexpand(true);
    readout.set_max_width_chars(1);
    STATUS.with(|s| *s.borrow_mut() = Some(readout.clone()));
    list_bar.append(&readout);
    // All or only unread, like a view switch (Shift+U).
    let show = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    show.add_css_class("segmented");
    show.set_valign(gtk::Align::Center);
    let all_toggle = gtk::ToggleButton::with_label("All");
    all_toggle.add_css_class("segment");
    all_toggle.set_tooltip_text(Some("Show all mail (Shift+U)"));
    all_toggle.set_active(true);
    let unread_toggle = gtk::ToggleButton::with_label("Unread");
    unread_toggle.add_css_class("segment");
    unread_toggle.set_tooltip_text(Some("Show only unread (Shift+U)"));
    unread_toggle.set_group(Some(&all_toggle));
    unread_toggle.connect_toggled(|t| set_unread_only(t.is_active()));
    show.append(&all_toggle);
    show.append(&unread_toggle);
    list_bar.append(&show);

    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::new(Some(store.clone()));
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    selection.connect_selected_notify(|_| on_selected());
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
        let Some(obj) = item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let row = obj.borrow::<Row>().clone();
        let child: gtk::Widget = if matches!(row, Row::Conversation(_)) {
            with_row_actions(list_row(&row).upcast_ref(), item.downgrade())
        } else {
            list_row(&row).upcast()
        };
        let position = item.position();
        ROW_WIDGETS.with(|w| w.borrow_mut().insert(position, child.downgrade()));
        let weak = item.downgrade();
        widgets::on_right_click(&child, move |w, x, y| {
            if let Some(item) = weak.upgrade()
                && item.position() != gtk::INVALID_LIST_POSITION
            {
                crate::menus::list(item.position(), w, x, y);
            }
        });
        item.set_child(Some(&child));
    });
    let list_view = gtk::ListView::new(Some(selection.clone()), Some(factory));
    list_view.add_css_class("mail-list");
    list_view.set_single_click_activate(false);
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&list_view)
        .build();
    list_scroll.connect_edge_reached(|_, edge| {
        if edge == gtk::PositionType::Bottom {
            load_more();
        }
    });
    let empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
    empty.set_vexpand(true);
    let list_stack = gtk::Stack::new();
    list_stack.add_named(&list_scroll, Some("list"));
    list_stack.add_named(&empty, Some("empty"));
    let list_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list_column.add_css_class("list-column");
    list_column.set_size_request(340, -1);
    list_column.set_hexpand(false);
    list_column.append(&list_bar);
    list_column.append(&list_stack);

    // The reading column.
    let reader_bar = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    reader_bar.add_css_class("toolbar");
    reader_bar.add_css_class("reader-bar");
    let buttons: [(&str, &str, fn()); 8] = [
        ("mail-reply-sender-symbolic", "Reply (R)", || reply(false)),
        ("mail-reply-all-symbolic", "Reply to all (A)", || reply(true)),
        ("mail-forward-symbolic", "Forward (F)", forward),
        ("nyx-mail-archive-symbolic", "Archive (E)", archive),
        ("mail-mark-junk-symbolic", "Junk or not junk (!)", junk),
        ("user-trash-symbolic", "Move to Trash (#)", trash),
        ("mail-unread-symbolic", "Mark read or unread (U)", toggle_unread),
        ("starred-symbolic", "Star or unstar (S)", toggle_star),
    ];
    let gap = || {
        let gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        gap.add_css_class("bar-gap");
        gap
    };
    for (i, (icon, tip, run)) in buttons.into_iter().enumerate() {
        if i == 3 || i == 6 {
            reader_bar.append(&gap());
        }
        let b = widgets::icon_button(icon, tip);
        b.connect_clicked(move |_| run());
        reader_bar.append(&b);
    }
    reader_bar.append(&gap());
    let snooze = organize::snooze_button();
    reader_bar.append(&snooze);
    reader_bar.append(&organize::more_button());
    let reader_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reader_column.set_hexpand(true);
    reader_column.append(&reader_bar);
    reader_column.append(&reader::build());

    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    panes.set_vexpand(true);
    panes.append(&list_column);
    panes.append(&nyx_ui::frame::pane_line(gtk::Orientation::Vertical));
    panes.append(&reader_column);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("settings-content");
    content.set_hexpand(true);
    content.append(&banners);
    content.append(&panes);

    // The header: the app and the folder you're in, the sidebar toggle, search,
    // and checking and writing mail.
    let header = nyx_ui::chrome::app_header(&window, "mail-unread-symbolic", "Mail");
    header.center.append(&search);
    let check = nyx_ui::widgets::bar_button("view-refresh-symbolic", "Check for mail (Ctrl+R)");
    check.connect_clicked(|_| check_mail());
    header.end.append(&check);
    let new = nyx_ui::widgets::bar_button("mail-message-new-symbolic", "New message (C)");
    new.add_css_class("accent-action");
    new.connect_clicked(|_| compose::open(Start::New));
    header.end.append(&new);
    header.end.append(&nyx_ui::chrome::settings_button(settings_dialog::open));

    let frame = nyx_ui::frame::AppFrame::new(&header.root);
    frame.set_sidebar(&nav);
    frame.set_page(&content);
    let overlay = frame.install(&window);
    install_keys(&window, &search);
    {
        let list_view = list_view.clone();
        window.connect_map(move |_| {
            let list_view = list_view.clone();
            glib::idle_add_local_once(move || {
                list_view.grab_focus();
            });
        });
    }
    window.connect_is_active_notify(|w| engine::set_active(w.is_active()));
    window.connect_close_request(|_| {
        flush_pending();
        engine::set_active(false);
        glib::Propagation::Proceed
    });

    let ui = Ui {
        window: window.clone(),
        overlay: overlay.clone(),
        nav: nav.clone(),
        nav_list,
        header,
        search,
        banners,
        list_title,
        store,
        selection,
        list_view: list_view.clone(),
        list_scroll: list_scroll.clone(),
        list_shows: String::new(),
        unread_only: false,
        unread_toggle: unread_toggle.clone(),
        list_stack,
        empty,
        reader_bar,
        snooze,
        view: View::All(Role::Inbox),
        mailboxes: Vec::new(),
        rows: Vec::new(),
        limit: PAGE,
        open: None,
        status: None,
        engine_up: false,
        hidden: HashSet::new(),
        pending: Vec::new(),
        restoring: false,
    };
    UI.with(|slot| *slot.borrow_mut() = Some(Rc::new(RefCell::new(ui))));

    let apply_width = {
        let nav = nav.clone();
        let window = window.clone();
        move || {
            let width = if window.width() > 0 { window.width() } else { window.default_width() };
            let narrow = width > 0 && width < 980;
            settings_dialog::fit(&window);
            reader::remeasure();
            let prev = NARROW.with(|n| n.get());
            let sized = nav.has_css_class("sized");
            if narrow == prev && sized {
                return;
            }
            nav.add_css_class("sized");
            NARROW.with(|n| n.set(narrow));
            apply_compact(&nav, narrow || prefs::get().sidebar_collapsed);
        }
    };
    let aw = apply_width.clone();
    window.connect_realize(move |_| aw());
    let probe = gtk::DrawingArea::new();
    probe.set_can_target(false);
    probe.set_can_focus(false);
    overlay.add_overlay(&probe);
    overlay.set_measure_overlay(&probe, false);
    probe.connect_resize(move |_, _, _| {
        let aw = apply_width.clone();
        glib::idle_add_local_once(aw);
    });
    // Dates move on ("14:05" becomes "Tue").
    glib::timeout_add_seconds_local(60, || {
        refresh_readout();
        glib::ControlFlow::Continue
    });
}

/// The sidebar's foot: who new mail is from. Settings → Writing changes it.
fn send_from_row(address: &str) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("send-from");
    row.set_tooltip_text(Some("New mail is sent from this address"));
    let initial = address.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
    let avatar = widgets::label(&initial, "send-from-avatar");
    avatar.set_xalign(0.5);
    avatar.set_valign(gtk::Align::Center);
    row.append(&avatar);
    let label = widgets::label(address, "send-from-address");
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.add_css_class("compact-hide");
    row.append(&label);
    row
}

/// Clear the search; the field stays in the header.
fn leave_search(search: &gtk::SearchEntry) {
    search.set_text("");
    if let Some(window) = search.root().and_downcast::<gtk::Window>() {
        gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
    }
}

fn apply_compact(nav: &gtk::Box, compact: bool) {
    if compact {
        nav.add_css_class("compact");
    } else {
        nav.remove_css_class("compact");
    }
    fn walk(widget: &gtk::Widget, compact: bool) {
        if widget.has_css_class("nav-toggle") {
            widget.set_halign(if compact { gtk::Align::Center } else { gtk::Align::End });
        }
        if widget.has_css_class("compact-hide") {
            widget.set_visible(!compact);
        }
        if widget.has_css_class("nav-item")
            && let Some(content) = widget.downcast_ref::<gtk::Button>().and_then(|b| b.child())
        {
            content.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Fill });
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            walk(&next, compact);
            child = next.next_sibling();
        }
    }
    walk(nav.upcast_ref(), compact);
}

pub fn toggle_sidebar() {
    prefs::update(|p| p.sidebar_collapsed = !p.sidebar_collapsed);
    let Some(ui) = ui() else { return };
    let nav = ui.borrow().nav.clone();
    apply_compact(&nav, NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
}

fn typing(window: &gtk::Window) -> bool {
    let Some(focus) = gtk::prelude::GtkWindowExt::focus(window) else { return false };
    focus.is::<gtk::Text>() || focus.is::<gtk::TextView>() || focus.ancestor(gtk::Entry::static_type()).is_some()
}

fn install_keys(window: &gtk::ApplicationWindow, search: &gtk::SearchEntry) {
    let keys = gtk::EventControllerKey::new();
    let (search, win) = (search.clone(), window.clone());
    keys.connect_key_pressed(move |_, key, _, mods| {
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        if ctrl {
            match key {
                gdk::Key::f => {
                    search.grab_focus();
                }
                gdk::Key::b => toggle_sidebar(),
                gdk::Key::n => compose::open(Start::New),
                gdk::Key::r => check_mail(),
                _ => return glib::Propagation::Proceed,
            }
            return glib::Propagation::Stop;
        }
        if key == gdk::Key::Escape {
            if !search.text().is_empty() || search.has_focus() {
                leave_search(&search);
                rebuild_list();
                rebuild_nav();
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        if typing(win.upcast_ref()) {
            return glib::Propagation::Proceed;
        }
        if widgets::is_menu_key(key, mods) {
            crate::menus::for_selected();
            return glib::Propagation::Stop;
        }
        match key {
            gdk::Key::c | gdk::Key::n => compose::open(Start::New),
            gdk::Key::j | gdk::Key::Down => move_selection(1),
            gdk::Key::k | gdk::Key::Up => move_selection(-1),
            gdk::Key::r => reply(false),
            gdk::Key::a => reply(true),
            gdk::Key::f => forward(),
            gdk::Key::e | gdk::Key::y => archive(),
            gdk::Key::numbersign | gdk::Key::Delete => trash(),
            gdk::Key::exclam => junk(),
            gdk::Key::s => toggle_star(),
            gdk::Key::u => toggle_unread(),
            gdk::Key::U => {
                let on = ui().is_some_and(|u| u.borrow().unread_only);
                set_unread_only(!on);
            }
            gdk::Key::z => {
                if let Some(ui) = ui()
                    && !reader::messages().is_empty()
                {
                    let snooze = ui.borrow().snooze.clone();
                    snooze.popup();
                }
            }
            gdk::Key::_1 => show_view(View::All(Role::Inbox)),
            gdk::Key::_2 => show_view(View::All(Role::Drafts)),
            gdk::Key::_3 => show_view(View::All(Role::Sent)),
            gdk::Key::_4 => show_view(View::All(Role::Archive)),
            gdk::Key::slash => {
                search.set_visible(true);
                search.grab_focus();
            }
            gdk::Key::F1 | gdk::Key::question => show_shortcuts(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
}

pub fn show_shortcuts() {
    let (dialog, card) = widgets::dialog("Keyboard shortcuts", 520);
    let list = widgets::vbox(0);
    list.add_css_class("group-list");
    for (keys, what) in SHORTCUTS {
        list.append(&widgets::row(what, "", Some(widgets::key_caps(keys).upcast_ref())));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .propagate_natural_height(true)
        .max_content_height(560)
        .child(&list)
        .build();
    card.append(&scroll);
    let close = gtk::Button::with_label("Close");
    close.set_halign(gtk::Align::End);
    let dialog2 = dialog.clone();
    close.connect_clicked(move |_| dialog2.close());
    card.append(&close);
    dialog.present();
}

// ---------- Dates ----------

pub fn now() -> i64 {
    chrono::Local::now().timestamp()
}

fn local(ts: i64) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::DateTime::from_timestamp(ts, 0).map(|d| d.with_timezone(&chrono::Local))
}

fn clock(t: &chrono::DateTime<chrono::Local>) -> String {
    if prefs::twelve_hour() { t.format("%-I:%M %p").to_string() } else { t.format("%H:%M").to_string() }
}

/// "14:05" today, "Tue" this week, "Oct 3" this year, else "Oct 3, 2024"; with
/// `long`, times are added ("Yesterday 09:12", "Tue, Oct 3, 14:05").
pub fn when_text(ts: i64, long: bool) -> String {
    use chrono::Datelike;
    let (Some(t), Some(today)) = (local(ts), local(now())) else { return String::new() };
    let days = (today.date_naive() - t.date_naive()).num_days();
    match (days, long) {
        (0, false) => clock(&t),
        (0, true) => format!("Today {}", clock(&t)),
        (1, true) => format!("Yesterday {}", clock(&t)),
        (1..=6, false) => t.format("%a").to_string(),
        (1..=6, true) => format!("{} {}", t.format("%a"), clock(&t)),
        _ if t.year() == today.year() && !long => t.format("%b %-d").to_string(),
        _ if t.year() == today.year() => format!("{}, {}", t.format("%a, %b %-d"), clock(&t)),
        _ if !long => t.format("%b %-d, %Y").to_string(),
        _ => format!("{}, {}", t.format("%b %-d, %Y"), clock(&t)),
    }
}

/// "Tuesday, October 21, 2026 at 14:05", for tooltips and reply headers.
pub fn full_date(ts: i64) -> String {
    let Some(t) = local(ts) else { return String::new() };
    format!("{} at {}", t.format("%A, %B %-d, %Y"), clock(&t))
}

// ---------- Toasts ----------

pub fn toast(message: &str) {
    show_toast(message, None);
}

/// A toast with Undo; it stays 6 s.
pub fn toast_undo(message: &str, undo: impl Fn() + 'static) {
    show_toast(message, Some(Box::new(undo)));
}

fn show_toast(message: &str, undo: Option<Box<dyn Fn()>>) {
    let Some(ui) = ui() else {
        eprintln!("nyx-mail: {message}");
        return;
    };
    let overlay = ui.borrow().overlay.clone();
    if let Some(old) = TOAST.with(|t| t.borrow_mut().take())
        && old.parent().is_some()
    {
        overlay.remove_overlay(&old);
    }
    let label = gtk::Label::new(Some(message));
    label.set_wrap(true);
    label.set_max_width_chars(70);
    let bx = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    bx.add_css_class("toast");
    bx.set_halign(gtk::Align::Center);
    bx.set_valign(gtk::Align::End);
    bx.set_margin_bottom(48);
    bx.append(&label);
    let lasts = if undo.is_some() { 6000 } else { 3500 };
    if let Some(undo) = undo {
        let button = gtk::Button::with_label("Undo");
        let (o, b) = (overlay.clone(), bx.clone());
        button.connect_clicked(move |_| {
            undo();
            if b.parent().is_some() {
                o.remove_overlay(&b);
            }
        });
        bx.append(&button);
    }
    overlay.add_overlay(&bx);
    TOAST.with(|t| *t.borrow_mut() = Some(bx.clone()));
    glib::timeout_add_local_once(std::time::Duration::from_millis(lasts), move || {
        if bx.parent().is_some() {
            overlay.remove_overlay(&bx);
        }
    });
}

