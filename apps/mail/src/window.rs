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
use crate::sorted;
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
    (&["S"], "Set aside or put back"),
    (&["U"], "Mark read or unread"),
    (&["Shift", "U"], "Show only unread"),
    (&["Z"], "Snooze"),
    (&["1"], "Imbox"),
    (&["2"], "Drafts"),
    (&["3"], "Sent"),
    (&["4"], "Archive"),
    (&["Ctrl", "R"], "Check for mail"),
    (&["Ctrl", "F"], "Find messages"),
    (&["Esc"], "Clear the search, or back to the Imbox, Feed or Screener"),
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
    /// The sidebar's foot: whether mail is up to date (click to check).
    foot_icon: gtk::Image,
    foot_label: gtk::Label,
    foot: gtk::Button,
    compose: gtk::Button,
    search: gtk::SearchEntry,
    banners: gtk::Box,
    /// `panes` (the list beside the reader, or the reader alone over a page)
    /// or `page` (the Imbox, The Feed, the Screener).
    main_stack: gtk::Stack,
    page_slot: gtk::Box,
    /// What the page last showed, to redraw it only when that changes.
    page_shows: String,
    list_column: gtk::Box,
    pane_line: gtk::Separator,
    /// Back to the page, over a conversation opened from it.
    back: gtk::Button,
    back_label: gtk::Label,
    /// The reader's labelled buttons, icons only beside the list at 800 px.
    bar_labels: Vec<gtk::Label>,
    /// A conversation opened from a page is being read.
    reading: bool,
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
        let start = nyx_theme::start_page("imbox", &prefs::get().last_section);
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
            open_row(n.saturating_sub(1));
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
        u.reading = false;
    }
    reader::show(None, &[]);
    reload();
    open_first();
}

fn on_page(u: &Ui) -> bool {
    u.view.is_page() && !searching(u)
}

/// Opens a page's conversation (its row in the list) in the reader.
pub(crate) fn open_row(position: u32) {
    let Some(ui) = ui() else { return };
    ui.borrow_mut().reading = true;
    if selected_position() == Some(position) {
        refresh_mode();
    } else {
        select(position);
    }
}

/// From a conversation back to the page it was opened from.
pub(crate) fn back_to_page() {
    let Some(ui) = ui() else { return };
    ui.borrow_mut().reading = false;
    select(gtk::INVALID_LIST_POSITION);
    refresh_mode();
}

/// The Imbox shows what's new and then what was seen, or only what was seen.
pub(crate) fn set_imbox_seen(seen: bool) {
    prefs::update(|p| p.imbox_seen = seen);
    rebuild_page();
}

/// The Imbox as cards (a page) or as the list beside the reader.
pub(crate) fn set_imbox_layout(layout: &str) {
    prefs::update(|p| p.imbox_layout = layout.to_string());
    let view = ui().map(|u| u.borrow().view.clone());
    if view == Some(View::Kind(Kind::Imbox)) {
        show_view(View::Kind(Kind::Imbox));
    }
}

/// The page, or the reader alone over it, or the list beside the reader.
fn refresh_mode() {
    let Some(ui) = ui() else { return };
    let page = on_page(&ui.borrow());
    let reading = page && ui.borrow().reading && selected_position().is_some();
    ui.borrow_mut().reading = reading;
    let u = ui.borrow();
    u.main_stack.set_visible_child_name(if page && !reading { "page" } else { "panes" });
    u.list_column.set_visible(!page);
    u.pane_line.set_visible(!page);
    u.back.set_visible(page);
    u.back_label.set_text(&view_name(&u));
    u.back.set_tooltip_text(Some(&format!("Back to {} (Esc)", view_name(&u))));
    // Beside the list at 800 px the reader is too narrow for its labels.
    let labels = page || !NARROW.with(|n| n.get());
    for l in &u.bar_labels {
        l.set_visible(labels);
    }
    u.list_column.set_size_request(if NARROW.with(|n| n.get()) { 280 } else { 340 }, -1);
}

/// Redraws the page from the list's rows, when what it shows changed.
fn rebuild_page() {
    let Some(ui) = ui() else { return };
    let (view, rows, more, slot) = {
        let u = ui.borrow();
        (u.view.clone(), u.rows.clone(), u.rows.len() >= u.limit, u.page_slot.clone())
    };
    let shows = format!("{}\u{1f}{rows:?}\u{1f}{more}\u{1f}{}\u{1f}{}", view.id(), prefs::get().imbox_seen, data::screener_on());
    if ui.borrow().page_shows == shows {
        return;
    }
    let same_view = ui.borrow().page_shows.split('\u{1f}').next() == Some(view.id().as_str());
    ui.borrow_mut().page_shows = shows;
    let at = slot
        .first_child()
        .and_then(|p| p.last_child())
        .and_downcast::<gtk::ScrolledWindow>()
        .map(|s| s.vadjustment().value())
        .filter(|_| same_view);
    while let Some(child) = slot.first_child() {
        slot.remove(&child);
    }
    let page = sorted::page(&view, &rows, more);
    slot.append(&page);
    if let Some(at) = at
        && let Some(scroll) = page.last_child().and_downcast::<gtk::ScrolledWindow>()
    {
        // Once it's laid out: the adjustment has no range until then.
        glib::idle_add_local_once(move || scroll.vadjustment().set_value(at));
    }
}

/// Shows the first conversation of the mailbox just opened (Settings → Reading),
/// without marking it read: it wasn't chosen.
fn open_first() {
    let Some(ui) = ui() else { return };
    let first = {
        let u = ui.borrow();
        u.open.is_none() && !on_page(&u) && matches!(u.rows.first(), Some(Row::Conversation(_)))
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
        View::SetAside => "Set aside".into(),
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
        Role::Inbox => "Inbox",
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
                    filter.unread = u.unread_only && !view.is_page();
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
    if on_page(&ui.borrow()) {
        rebuild_page();
    }
    refresh_mode();
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
            (View::SetAside, _) => "Nothing set aside",
            (View::Scheduled, _) => "Nothing scheduled to send",
            (View::FollowUps, _) => "No follow-ups set",
            (View::Senders, _) => "Nobody has written yet",
            (View::Kind(Kind::Imbox), _) => "Nothing in the Imbox",
            (View::Kind(Kind::Feed), _) => "Nothing in the Feed",
            (View::Kind(Kind::Paper), _) => "No receipts or notices",
            (_, Some(Role::Inbox)) => "Your inbox is empty",
            (_, Some(Role::Drafts)) => "No drafts",
            (_, Some(Role::Trash)) => "Trash is empty",
            (_, Some(Role::Junk)) => "No junk",
            _ => "No messages",
        };
        let icon = match u.view {
            View::SetAside => "nyx-mail-flag-symbolic",
            View::Snoozed => "alarm-symbolic",
            _ => role_icon(u.view.role(&u.mailboxes)),
        };
        widgets::empty_state(icon, text, "", None)
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
        let i = gtk::Image::from_icon_name("nyx-mail-flag-symbolic");
        i.add_css_class("flag");
        i.set_tooltip_text(Some("Set aside"));
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
        ("nyx-mail-flag-symbolic", "Set aside or put back (S)", toggle_star),
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
    refresh_mode();
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
    // On a page, J and K open conversations in the reader.
    ui.borrow_mut().reading = true;
    select(next as u32);
}

pub(crate) fn load_more() {
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
    // From a page's menu (nothing open in the reader): stay on the page.
    let reading = !on_page(&ui.borrow()) || ui.borrow().reading;
    ui.borrow_mut().open = None;
    rebuild_list();
    let n = ui.borrow().rows.len() as u32;
    let then = prefs::get().after_moving;
    if n > 0 && pos != gtk::INVALID_LIST_POSITION && then != "list" && reading {
        let at = if then == "previous" { pos.saturating_sub(1) } else { pos };
        ui.borrow_mut().reading = true;
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
        toast("Put back: it's no longer set aside.");
    } else {
        engine::set_flags(&[last.id], flags::FLAGGED, 0);
        toast("Set aside. Find it under Later › Set aside.");
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
    STATUS.with(|s| {
        if let Some(label) = s.borrow().as_ref() {
            label.set_text(&parts.join(" · "));
        }
    });

    // The sidebar's foot: how the mail is, at a glance.
    let last = u.status.as_ref().and_then(|s| s.accounts.iter().filter_map(|a| a.last_sync).max());
    let (icon, class, mut text) = match &u.status {
        Some(s) if s.accounts.iter().any(|a| a.syncing) => ("view-refresh-symbolic", "", "Checking for mail…".to_string()),
        Some(s) if !s.accounts.is_empty() && s.accounts.iter().all(|a| a.offline) => {
            ("network-offline-symbolic", "warn", "Offline".to_string())
        }
        Some(s) if s.accounts.iter().any(|a| a.error.is_some()) => {
            ("dialog-warning-symbolic", "warn", "An account needs attention".to_string())
        }
        Some(_) => match last {
            Some(t) if now() - t < 15 * 60 => ("object-select-symbolic", "ok", "Up to date".to_string()),
            Some(t) => ("object-select-symbolic", "", format!("Updated {}", when_text(t, false))),
            None => ("view-refresh-symbolic", "", "Not checked yet".to_string()),
        },
        None if !u.engine_up => ("dialog-warning-symbolic", "warn", "Mail's engine isn't running".to_string()),
        None => ("view-refresh-symbolic", "", "Starting…".to_string()),
    };
    if let Some(s) = &u.status
        && s.pending > 0
    {
        text.push_str(&if s.pending == 1 { " · 1 change waiting".to_string() } else { format!(" · {} changes waiting", s.pending) });
    }
    u.foot_icon.set_icon_name(Some(icon));
    for c in ["ok", "warn"] {
        u.foot_icon.remove_css_class(c);
    }
    if !class.is_empty() {
        u.foot_icon.add_css_class(class);
    }
    u.foot_label.set_text(&text);
    let updated = last.map(|t| format!("Updated {}. ", when_text(t, true))).unwrap_or_default();
    u.foot.set_tooltip_text(Some(&format!("{text}. {updated}Check for mail (Ctrl+R)")));
    let from = compose::send_from().map(|a| format!(" from {a}")).unwrap_or_default();
    u.compose.set_tooltip_text(Some(&format!("New message{from} (C or Ctrl+N)")));
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

/// A sidebar row for a view: its count in the dim readout, or as a badge.
fn nav(view: View, icon: &str, name: &str, tooltip: &str, count: usize, badge: bool, active: bool) -> gtk::Button {
    let item = nyx_ui::sidebar::nav_item(icon, name, tooltip);
    if badge {
        item.set_badge(Some(count as u64));
        if let Some(readout) = item.button.child().and_then(|c| c.last_child()) {
            readout.set_valign(gtk::Align::Center);
        }
    } else {
        item.set_count(Some(count as u64));
    }
    item.set_active(active);
    item.button.connect_clicked(move |_| show_view(view.clone()));
    item.button
}

/// Where a group starts in the icon rail: a short bar in its colour.
fn rail_divider(section: &str) -> gtk::Separator {
    let bar = gtk::Separator::new(gtk::Orientation::Horizontal);
    bar.add_css_class("nav-divider");
    bar.add_css_class("compact-show");
    bar.add_css_class(&format!("section-{section}"));
    bar.set_visible(false);
    bar
}

/// A folder under an account's heading, and what its right-click menu is.
struct Folder {
    view: View,
    icon: &'static str,
    name: String,
    tooltip: String,
    count: i64,
    depth: usize,
    menu: FolderMenu,
}

enum FolderMenu {
    Role(Role),
    Mailbox(Mailbox),
}

/// "Inbox, Sent, Junk and Trash", "Inbox and 3 folders".
fn fold_note(folders: &[Folder]) -> String {
    let mut parts: Vec<String> = folders.iter().filter(|f| f.icon != "folder-symbolic").map(|f| f.name.clone()).collect();
    let others = folders.iter().filter(|f| f.icon == "folder-symbolic").count();
    match others {
        0 => {}
        1 => parts.push("1 folder".into()),
        n => parts.push(format!("{n} folders")),
    }
    match parts.as_slice() {
        [] => "No folders".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A heading that folds its folders away (`key` in `open_accounts` when shown),
/// then its folders, or a line saying what's folded.
fn folder_group(
    list_box: &gtk::Box,
    key: &str,
    title: &str,
    section: &str,
    folders: &[Folder],
    account: Option<&nyx_mail_core::AccountRow>,
    active: &dyn Fn(&View) -> bool,
) {
    let open = prefs::get().open_accounts.iter().any(|k| k == key) || folders.iter().any(|f| active(&f.view));
    list_box.append(&rail_divider(section));
    let heading = nyx_ui::sidebar::group_label(title, false);
    heading.add_css_class("account-header");
    let (icon, tip) = if open { ("pan-down-symbolic", "Hide its folders") } else { ("pan-end-symbolic", "Show its folders") };
    let fold = widgets::icon_button(icon, tip);
    fold.add_css_class("nav-group-add");
    fold.set_valign(gtk::Align::End);
    let k = key.to_string();
    fold.connect_clicked(move |_| toggle_account_folded(&k));
    heading.append(&fold);
    if let Some(label) = heading.first_child() {
        let click = gtk::GestureClick::new();
        let k = key.to_string();
        click.connect_released(move |_, _, _, _| toggle_account_folded(&k));
        label.add_controller(click);
    }
    if let Some(account) = account {
        crate::menus::attach_account(&heading, account, !open);
    }
    nyx_ui::sidebar::set_section(&heading, section);
    list_box.append(&heading);
    if !open {
        let note = fold_note(folders);
        let line = widgets::label(&note, "fold-note");
        line.add_css_class("compact-hide");
        line.set_wrap(true);
        line.set_tooltip_text(Some("Show these folders"));
        let click = gtk::GestureClick::new();
        let k = key.to_string();
        click.connect_released(move |_, _, _, _| toggle_account_folded(&k));
        line.add_controller(click);
        list_box.append(&line);
        // The icon rail has no heading to click: one button stands for them.
        let rail = nyx_ui::sidebar::nav_item("folder-symbolic", title, &format!("{title}: {note}"));
        rail.button.add_css_class("compact-show");
        rail.button.set_visible(false);
        let k = key.to_string();
        rail.button.connect_clicked(move |_| toggle_account_folded(&k));
        list_box.append(&rail.button);
        return;
    }
    for f in folders {
        let button = nav(f.view.clone(), f.icon, &f.name, &f.tooltip, f.count.max(0) as usize, false, active(&f.view));
        if let Some(content) = button.child() {
            content.set_margin_start((f.depth.min(4) * 14) as i32);
        }
        match &f.menu {
            FolderMenu::Role(role) => crate::menus::attach_role(&button, *role),
            FolderMenu::Mailbox(m) => crate::menus::attach_mailbox(&button, m),
        }
        list_box.append(&button);
    }
}

/// The mailboxes every account has, in the order they're listed (Drafts is
/// under Later).
const ROLES: [Role; 5] = [Role::Inbox, Role::Sent, Role::Archive, Role::Junk, Role::Trash];

fn role_count(role: Role, unread: i64) -> i64 {
    if matches!(role, Role::Inbox | Role::Junk) { unread } else { 0 }
}

pub(crate) fn toggle_account_folded(key: &str) {
    prefs::update(|p| {
        if p.open_accounts.iter().any(|a| a == key) {
            p.open_accounts.retain(|a| a != key);
        } else {
            p.open_accounts.push(key.to_string());
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
    let (view, mailboxes, search_on, status) = {
        let u = ui.borrow();
        (u.view.clone(), u.mailboxes.clone(), searching(&u), u.status.clone().unwrap_or_default())
    };
    // The Senders page belongs to the Screener.
    let active = |v: &View| !search_on && (view == *v || (*v == View::Screener && view == View::Senders));
    let accounts = data::accounts();
    let several = accounts.len() > 1;

    // Mail sorted for you: the app's home, without a heading.
    let screener = data::screener_on();
    let kinds = [
        (Kind::Imbox, "starred-symbolic", "Imbox (1)", true),
        (Kind::Feed, "application-rss+xml-symbolic", "The Feed", false),
        (Kind::Paper, "x-office-document-symbolic", "Paper Trail", false),
    ];
    for (kind, icon, tooltip, badge) in kinds {
        let v = View::Kind(kind);
        let count = data::count(&mailboxes, &v, true);
        list_box.append(&nav(v.clone(), icon, kind.name(), tooltip, count, badge, active(&v)));
    }
    let waiting = if screener { data::screener_count(&mailboxes) } else { 0 };
    let tooltip = if screener { "Screener: first-time senders" } else { "Screener (off)" };
    list_box.append(&nav(View::Screener, "nyx-mail-filter-symbolic", "Screener", tooltip, waiting, true, active(&View::Screener)));

    // Later: what's waiting on time or on you.
    list_box.append(&rail_divider(nyx_ui::sidebar::ACCENT_SECTION));
    let later = nyx_ui::sidebar::group_label("Later", false);
    nyx_ui::sidebar::set_section(&later, nyx_ui::sidebar::ACCENT_SECTION);
    list_box.append(&later);
    let snoozed = data::count(&mailboxes, &View::Snoozed, false);
    list_box.append(&nav(View::Snoozed, "alarm-symbolic", "Snoozed", "Snoozed", snoozed, false, active(&View::Snoozed)));
    let aside = data::count(&mailboxes, &View::SetAside, false);
    list_box.append(&nav(
        View::SetAside,
        "nyx-mail-flag-symbolic",
        "Set aside",
        "Set aside (S on a conversation)",
        aside,
        false,
        active(&View::SetAside),
    ));
    let drafts_view = View::All(Role::Drafts);
    let drafts = nav(
        drafts_view.clone(),
        role_icon(Some(Role::Drafts)),
        "Drafts",
        "Drafts (2)",
        data::drafts().len(),
        false,
        active(&drafts_view),
    );
    crate::menus::attach_role(&drafts, Role::Drafts);
    list_box.append(&drafts);
    // Only while there's something to show (or it's open).
    if status.scheduled > 0 || active(&View::Scheduled) {
        list_box.append(&nav(
            View::Scheduled,
            "x-office-calendar-symbolic",
            "Scheduled",
            "Mail waiting to send",
            status.scheduled.max(0) as usize,
            false,
            active(&View::Scheduled),
        ));
    }
    if status.follow_ups > 0 || active(&View::FollowUps) {
        list_box.append(&nav(
            View::FollowUps,
            "document-open-recent-symbolic",
            "Follow-ups",
            "Conversations you'll hear about if nobody answers",
            status.follow_ups.max(0) as usize,
            false,
            active(&View::FollowUps),
        ));
    }

    // Pinned folders, in the order they were pinned.
    let pins: Vec<&Mailbox> =
        prefs::get().pinned.iter().filter_map(|k| mailboxes.iter().find(|m| prefs::pin_key(&m.account, &m.name) == *k)).collect();
    if !pins.is_empty() {
        list_box.append(&rail_divider("pinned"));
        let heading = nyx_ui::sidebar::group_label("Pinned", false);
        nyx_ui::sidebar::set_section(&heading, "pinned");
        list_box.append(&heading);
        for m in pins {
            let address = accounts.iter().find(|a| a.id == m.account).map(|a| a.address.as_str()).unwrap_or("");
            let tooltip = if several { format!("{} · {address}", m.name) } else { m.name.clone() };
            let v = View::Mailbox(m.id);
            let button = nav(v.clone(), role_icon(m.role), &m.label(), &tooltip, m.unread.max(0) as usize, false, active(&v));
            crate::menus::attach_mailbox(&button, m);
            list_box.append(&button);
        }
    }

    // The classic mailboxes, folded under each account (and, with several,
    // every account's together first).
    let keyed = |role: Role| match role {
        Role::Sent => " (3)",
        Role::Archive => " (4)",
        _ => "",
    };
    let mut swatch = 0;
    if several {
        let folders: Vec<Folder> = ROLES
            .iter()
            .filter(|r| **r == Role::Inbox || mailboxes.iter().any(|m| m.role == Some(**r)))
            .map(|&role| Folder {
                view: View::All(role),
                icon: role_icon(Some(role)),
                name: role_name(role).into(),
                tooltip: format!("{} in every account{}", role_name(role), keyed(role)),
                count: role_count(role, mailboxes.iter().filter(|m| m.role == Some(role)).map(|m| m.unread).sum()),
                depth: 0,
                menu: FolderMenu::Role(role),
            })
            .collect();
        folder_group(&list_box, "all", "All accounts", &nyx_ui::sidebar::rotating_section(swatch), &folders, None, &active);
        swatch += 1;
    }
    for account in &accounts {
        let mine: Vec<&Mailbox> = mailboxes.iter().filter(|m| m.account == account.id).collect();
        let mut folders: Vec<Folder> = Vec::new();
        for role in ROLES {
            let Some(m) = mine.iter().find(|m| m.role == Some(role)) else { continue };
            // With one account its mailboxes are every account's (the keys open them).
            let (view, menu) =
                if several { (View::Mailbox(m.id), FolderMenu::Mailbox((*m).clone())) } else { (View::All(role), FolderMenu::Role(role)) };
            folders.push(Folder {
                view,
                icon: role_icon(Some(role)),
                name: role_name(role).into(),
                tooltip: format!("{}{}", role_name(role), if several { "" } else { keyed(role) }),
                count: role_count(role, m.unread),
                depth: 0,
                menu,
            });
        }
        for m in mine.iter().filter(|m| m.role.is_none()) {
            // Indented under parents that are shown (not Gmail's "[Gmail]").
            let depth = m
                .delimiter
                .as_deref()
                .filter(|d| !d.is_empty())
                .map(|d| {
                    let parts: Vec<&str> = m.name.split(d).collect();
                    (1..parts.len()).filter(|i| mine.iter().any(|p| p.name == parts[..*i].join(d))).count()
                })
                .unwrap_or(0);
            folders.push(Folder {
                view: View::Mailbox(m.id),
                icon: "folder-symbolic",
                name: m.label(),
                tooltip: m.name.clone(),
                count: m.unread,
                depth,
                menu: FolderMenu::Mailbox((*m).clone()),
            });
        }
        let section = nyx_ui::sidebar::rotating_section(swatch);
        swatch += 1;
        folder_group(&list_box, &account.id, &account.address, &section, &folders, Some(account), &active);
    }
    if accounts.is_empty() {
        list_box.append(&nyx_ui::sidebar::group_label_with_add("Accounts", false, "Add an account", open_online_accounts));
    }
    list_box.append(&ui.borrow().foot);

    let nav = ui.borrow().nav.clone();
    apply_compact(&nav, NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
}

// ---------- Building ----------

fn install_icons() {
    let dir = nyx_core::paths::cache_dir().join("icons");
    let icons = [
        ("nyx-mail-archive-symbolic.svg", include_str!("../data/nyx-mail-archive-symbolic.svg")),
        ("nyx-mail-filter-symbolic.svg", include_str!("../data/nyx-mail-filter-symbolic.svg")),
        ("nyx-mail-flag-symbolic.svg", include_str!("../data/nyx-mail-flag-symbolic.svg")),
    ];
    for (name, svg) in icons {
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg) {
            let _ = nyx_core::cmd::atomic_write(&path, svg);
        }
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
    let nav_list = gtk::Box::new(gtk::Orientation::Vertical, 1);
    nav_list.set_hexpand(false);
    let nav_scroll = gtk::ScrolledWindow::builder()
        .hexpand(false)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .vexpand(true)
        .child(&nav_list)
        .build();
    nav.append(&nav_scroll);
    // How the mail is, under the folders; a click checks for more.
    let foot = gtk::Button::new();
    foot.add_css_class("nav-item");
    foot.add_css_class("nav-status");
    let foot_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let foot_icon = gtk::Image::from_icon_name("view-refresh-symbolic");
    foot_icon.add_css_class("status-icon");
    foot_row.append(&foot_icon);
    let foot_label = widgets::label("", "status-text");
    foot_label.add_css_class("compact-hide");
    foot_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    foot_label.set_hexpand(true);
    foot_row.append(&foot_label);
    foot.set_child(Some(&foot_row));
    foot.update_property(&[gtk::accessible::Property::Label("Check for mail")]);
    foot.connect_clicked(|_| check_mail());

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Search mail"));
    search.update_property(&[gtk::accessible::Property::Label("Search mail")]);
    search.add_css_class("bar-search");
    search.set_width_chars(30);
    search.set_max_width_chars(40);
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

    // The reading column: what to do with it, labelled, then filing it away.
    let reader_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    reader_bar.add_css_class("toolbar");
    reader_bar.add_css_class("reader-bar");
    let mut bar_labels = Vec::new();
    let labelled = |icon: &str, text: &str, tip: &str, labels: &mut Vec<gtk::Label>| {
        let b = gtk::Button::new();
        b.add_css_class("bar-labelled");
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        row.append(&gtk::Image::from_icon_name(icon));
        let label = gtk::Label::new(Some(text));
        row.append(&label);
        labels.push(label);
        b.set_child(Some(&row));
        b.set_tooltip_text(Some(tip));
        b.update_property(&[gtk::accessible::Property::Label(text)]);
        b
    };
    let back = gtk::Button::new();
    back.add_css_class("flat");
    back.add_css_class("reader-back");
    let back_row = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    back_row.append(&gtk::Image::from_icon_name("go-previous-symbolic"));
    let back_label = gtk::Label::new(None);
    back_row.append(&back_label);
    back.set_child(Some(&back_row));
    back.set_visible(false);
    back.connect_clicked(|_| back_to_page());
    reader_bar.append(&back);
    let reply_button = labelled("mail-reply-sender-symbolic", "Reply", "Reply (R)", &mut bar_labels);
    reply_button.add_css_class("suggested-action");
    reply_button.connect_clicked(|_| reply(false));
    reader_bar.append(&reply_button);
    let reply_all = labelled("mail-reply-all-symbolic", "Reply all", "Reply to all (A)", &mut bar_labels);
    reply_all.connect_clicked(|_| reply(true));
    reader_bar.append(&reply_all);
    let forward_button = labelled("mail-forward-symbolic", "Forward", "Forward (F)", &mut bar_labels);
    forward_button.connect_clicked(|_| forward());
    reader_bar.append(&forward_button);
    let gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    gap.set_hexpand(true);
    reader_bar.append(&gap);
    let archive_button = widgets::icon_button("nyx-mail-archive-symbolic", "Archive (E)");
    archive_button.connect_clicked(|_| archive());
    reader_bar.append(&archive_button);
    let snooze = organize::snooze_button();
    reader_bar.append(&snooze);
    let trash_button = widgets::icon_button("user-trash-symbolic", "Move to Trash (#)");
    trash_button.connect_clicked(|_| trash());
    reader_bar.append(&trash_button);
    reader_bar.append(&organize::more_button());
    let reader_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reader_column.set_hexpand(true);
    reader_column.append(&reader_bar);
    reader_column.append(&reader::build());

    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    panes.set_vexpand(true);
    panes.append(&list_column);
    let pane_line = nyx_ui::frame::pane_line(gtk::Orientation::Vertical);
    panes.append(&pane_line);
    panes.append(&reader_column);
    let page_slot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page_slot.set_vexpand(true);
    let main_stack = gtk::Stack::new();
    main_stack.set_vexpand(true);
    main_stack.add_named(&panes, Some("panes"));
    main_stack.add_named(&page_slot, Some("page"));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("settings-content");
    content.set_hexpand(true);
    content.append(&banners);
    content.append(&main_stack);

    // The header: the app and the sidebar's toggle, search, writing mail and Settings.
    let header = nyx_ui::chrome::app_header(&window, "mail-unread-symbolic", "Mail");
    let collapse = nyx_ui::widgets::bar_button("sidebar-show-symbolic", "Collapse the sidebar (Ctrl+B)");
    collapse.connect_clicked(|_| toggle_sidebar());
    header.start.append(&collapse);
    let search_box = gtk::Overlay::new();
    search_box.set_child(Some(&search));
    let hint = widgets::label("Ctrl F", "key-hint");
    hint.set_halign(gtk::Align::End);
    hint.set_valign(gtk::Align::Center);
    hint.set_margin_end(10);
    hint.set_can_target(false);
    search_box.add_overlay(&hint);
    search.connect_changed(move |s| hint.set_visible(s.text().is_empty()));
    header.center.append(&search_box);
    let compose_button = gtk::Button::new();
    compose_button.add_css_class("new-button");
    let face = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    face.append(&gtk::Image::from_icon_name("mail-message-new-symbolic"));
    face.append(&gtk::Label::new(Some("Compose")));
    face.append(&widgets::label("Ctrl N", "key-hint"));
    compose_button.set_child(Some(&face));
    compose_button.set_valign(gtk::Align::Center);
    compose_button.set_tooltip_text(Some("New message (C or Ctrl+N)"));
    compose_button.update_property(&[gtk::accessible::Property::Label("Compose")]);
    compose_button.connect_clicked(|_| compose::open(Start::New));
    header.end.append(&compose_button);
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
        foot_icon,
        foot_label,
        foot,
        compose: compose_button,
        search,
        banners,
        main_stack,
        page_slot,
        page_shows: String::new(),
        list_column,
        pane_line,
        back,
        back_label,
        bar_labels,
        reading: false,
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
            refresh_mode();
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

/// Clear the search; the field stays in the header.
fn leave_search(search: &gtk::SearchEntry) {
    search.set_text("");
    if let Some(window) = search.root().and_downcast::<gtk::Window>() {
        gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
    }
}

/// The icon rail (or the full sidebar): nyx_ui's compact mode, and the rail's
/// own marks (`compact-show`) where groups start.
fn apply_compact(nav: &gtk::Box, compact: bool) {
    nyx_ui::sidebar::set_compact(nav, compact);
    fn walk(widget: &gtk::Widget, compact: bool) {
        if widget.has_css_class("compact-show") {
            widget.set_visible(compact);
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
            if ui().is_some_and(|u| on_page(&u.borrow()) && u.borrow().reading) {
                back_to_page();
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
            gdk::Key::_1 => show_view(View::Kind(Kind::Imbox)),
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

