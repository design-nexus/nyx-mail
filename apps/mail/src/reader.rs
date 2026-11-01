//! The reading pane: a conversation, oldest first, with native headers.
//!
//! Plain-text bodies are labels. HTML bodies, already sanitized (no script,
//! forms, frames or event handlers), each get a WebKit view that can't run
//! page script (only this app's own measuring, in an isolated world), uses an
//! ephemeral network session, and loads nothing from the network: a strict
//! Content-Security-Policy allows only `cid:` parts (served from the store by
//! the `nyx-mail-msg:` scheme) and `data:` until the user allows the
//! sender's images. Links open in the browser; `mailto:` opens the composer.

use crate::compose;
use crate::data;
use crate::engine;
use crate::prefs;
use crate::widgets;
use crate::window;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use nyx_mail_core::db::MessageRow;
use nyx_mail_core::flags;
use nyx_mail_core::mime::{self, Attachment, Parsed};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use webkit6::prelude::*;

const SCHEME: &str = "nyx-mail-msg";

struct Reader {
    /// Over the conversation: the Screener's question, a snooze, a follow-up.
    banner: gtk::Box,
    body: gtk::Box,
    scroll: gtk::ScrolledWindow,
    /// The conversation shown, and the mailboxes it was opened from.
    thread: Option<String>,
    mailboxes: Vec<i64>,
    messages: Vec<MessageRow>,
    /// Messages opened out of their collapsed line, and ones whose web
    /// content the user allowed this time.
    expanded: HashSet<i64>,
    remote_once: HashSet<i64>,
    /// What's drawn, to skip redrawing when nothing changed.
    drawn: Vec<(i64, i64, bool)>,
    views: Vec<webkit6::WebView>,
}

thread_local! {
    static READER: RefCell<Option<Reader>> = const { RefCell::new(None) };
    static WEB: RefCell<Option<(webkit6::WebContext, webkit6::NetworkSession)>> = const { RefCell::new(None) };
    /// A hidden view started with the window: its web process is up before the
    /// first message opens, and every message view shares it (`related_view`).
    static WARM: RefCell<Option<webkit6::WebView>> = const { RefCell::new(None) };
    /// Message views from the last conversation, loaded again for the next one
    /// (making a view, and closing one, costs WebKit a round trip each).
    static POOL: RefCell<Vec<webkit6::WebView>> = const { RefCell::new(Vec::new()) };
    /// Which body each (reused) view shows, so a late timer from an earlier one
    /// doesn't reveal the next early.
    static LOADS: Cell<u64> = const { Cell::new(0) };
}

fn with<T>(f: impl FnOnce(&mut Reader) -> T) -> Option<T> {
    READER.with(|r| r.borrow_mut().as_mut().map(f))
}

pub fn build() -> gtk::Box {
    prewarm();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("reader");
    root.set_hexpand(true);
    let body = gtk::Box::new(gtk::Orientation::Vertical, 14);
    body.add_css_class("conversation");
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&body)
        .build();
    let banner = gtk::Box::new(gtk::Orientation::Vertical, 0);
    banner.add_css_class("reader-banner");
    banner.set_visible(false);
    root.append(&banner);
    root.append(&scroll);
    READER.with(|r| {
        *r.borrow_mut() = Some(Reader {
            banner,
            body,
            scroll,
            thread: None,
            mailboxes: Vec::new(),
            messages: Vec::new(),
            expanded: HashSet::new(),
            remote_once: HashSet::new(),
            drawn: Vec::new(),
            views: Vec::new(),
        })
    });
    show(None, &[]);
    root
}

/// Shows something other than a conversation (a scheduled message, a sender).
pub fn show_card(card: &gtk::Widget) {
    let Some(body) = with(|r| {
        r.thread = None;
        r.messages.clear();
        r.drawn.clear();
        r.views.clear();
        r.banner.set_visible(false);
        r.body.clone()
    }) else {
        return;
    };
    while let Some(child) = body.first_child() {
        body.remove(&child);
    }
    body.append(card);
}

/// The conversation's messages as shown.
pub fn messages() -> Vec<MessageRow> {
    with(|r| r.messages.clone()).unwrap_or_default()
}

/// Shows a conversation (or nothing). Called again when mail changes; it only
/// redraws what changed.
pub fn show(thread: Option<&str>, mailboxes: &[i64]) {
    let same = with(|r| r.thread.as_deref() == thread).unwrap_or(false);
    let messages = thread.map(|t| data::conversation(t, mailboxes)).unwrap_or_default();
    let changed = with(|r| {
        if !same {
            r.expanded.clear();
            r.remote_once.clear();
        }
        r.thread = thread.map(str::to_string);
        r.mailboxes = mailboxes.to_vec();
        r.messages = messages.clone();
        // Marking read changes nothing shown: redrawing for it would reload
        // every body a moment after opening (the open message is marked at once).
        let drawn: Vec<(i64, i64, bool)> = messages.iter().map(|m| (m.id, m.flags & !flags::SEEN, m.raw.is_some())).collect();
        let changed = !same || drawn != r.drawn;
        r.drawn = drawn;
        changed
    })
    .unwrap_or(false);
    if changed {
        redraw(!same);
    }
    // The banner reflects choices made elsewhere too, so it's redone each time.
    if let Some(banner) = with(|r| r.banner.clone()) {
        while let Some(child) = banner.first_child() {
            banner.remove(&child);
        }
        let b = crate::organize::banner(&messages);
        banner.set_visible(b.is_some());
        if let Some(b) = b {
            banner.append(&b);
        }
    }
}

/// Redraws after a choice that changes what shows (expanding, images).
pub fn refresh() {
    with(|r| r.drawn.clear());
    let (thread, mailboxes) = with(|r| (r.thread.clone(), r.mailboxes.clone())).unwrap_or_default();
    show(thread.as_deref(), &mailboxes);
}

fn redraw(scroll_to_end: bool) {
    let Some((body, messages, expanded, scroll)) =
        with(|r| (r.body.clone(), r.messages.clone(), r.expanded.clone(), r.scroll.clone()))
    else {
        return;
    };
    // Keep the old views for the new conversation's bodies.
    let old = with(|r| std::mem::take(&mut r.views)).unwrap_or_default();
    for view in old {
        if let Some(parent) = view.parent().and_downcast::<gtk::Box>() {
            parent.remove(&view);
        }
        POOL.with(|p| {
            let mut p = p.borrow_mut();
            if p.len() < 4 {
                p.push(view);
            }
        });
    }
    while let Some(child) = body.first_child() {
        body.remove(&child);
    }
    if messages.is_empty() {
        body.append(&widgets::empty_state("mail-read-symbolic", "No conversation selected", "", None));
        return;
    }
    let subject = messages.last().map(|m| m.subject.clone()).unwrap_or_default();
    let title = widgets::label(if subject.trim().is_empty() { "(no subject)" } else { subject.trim() }, "conversation-subject");
    title.set_xalign(0.0);
    title.set_wrap(true);
    title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    title.set_selectable(true);
    body.append(&title);
    let last = messages.len() - 1;
    let mut focus: Option<gtk::Widget> = None;
    for (i, m) in messages.iter().enumerate() {
        let open = i == last || m.flags & flags::SEEN == 0 || expanded.contains(&m.id);
        if open {
            // Stays open when it's read and the conversation is drawn again.
            with(|r| r.expanded.insert(m.id));
        }
        let card = if open { message_card(m) } else { collapsed_card(m) };
        if open && focus.is_none() && m.flags & flags::SEEN == 0 {
            focus = Some(card.clone().upcast());
        }
        body.append(&card);
    }
    if scroll_to_end && messages.len() > 1 {
        // Start at the first unread message, else the newest.
        let target = focus.or_else(|| body.last_child());
        glib::timeout_add_local_once(std::time::Duration::from_millis(80), move || {
            if let Some(t) = target
                && let Some(point) = t.compute_point(&scroll.child().unwrap_or(t.clone()), &gtk::graphene::Point::new(0.0, 0.0))
            {
                scroll.vadjustment().set_value((point.y() as f64 - 12.0).max(0.0));
            }
        });
    } else {
        scroll.vadjustment().set_value(0.0);
    }
}

fn sender(m: &MessageRow) -> String {
    if m.from_name.trim().is_empty() { m.from_addr.clone() } else { m.from_name.clone() }
}

fn avatar(name: &str) -> gtk::Label {
    let initial: String =
        name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
    let a = widgets::label(&initial, "avatar");
    a.set_valign(gtk::Align::Start);
    a.set_xalign(0.5);
    a.set_justify(gtk::Justification::Center);
    a
}

fn collapsed_card(m: &MessageRow) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    card.add_css_class("message-card");
    card.add_css_class("collapsed");
    card.append(&avatar(&sender(m)));
    let name = widgets::label(&sender(m), "message-from");
    name.set_xalign(0.0);
    card.append(&name);
    let preview = data::parsed(m).map(|(_, p)| p.preview()).unwrap_or_default();
    let snippet = widgets::label(&preview, "dim");
    snippet.set_hexpand(true);
    snippet.set_xalign(0.0);
    snippet.set_ellipsize(gtk::pango::EllipsizeMode::End);
    card.append(&snippet);
    card.append(&widgets::label(&window::when_text(m.date, true), "message-date"));
    card.set_tooltip_text(Some("Show this message"));
    let click = gtk::GestureClick::new();
    let id = m.id;
    click.connect_released(move |_, _, _, _| {
        with(|r| r.expanded.insert(id));
        refresh();
    });
    card.add_controller(click);
    card
}

fn message_card(m: &MessageRow) -> gtk::Box {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 10);
    card.add_css_class("message-card");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    head.append(&avatar(&sender(m)));
    let who = gtk::Box::new(gtk::Orientation::Vertical, 2);
    who.set_hexpand(true);
    let from = gtk::Label::new(None);
    from.set_markup(&format!(
        "<b>{}</b>  <span alpha=\"60%\">{}</span>",
        glib::markup_escape_text(&sender(m)),
        glib::markup_escape_text(&m.from_addr)
    ));
    from.add_css_class("message-from");
    from.set_xalign(0.0);
    from.set_ellipsize(gtk::pango::EllipsizeMode::End);
    from.set_selectable(true);
    who.append(&from);
    let to: Vec<String> = m.recipients.iter().map(|(n, e)| if n.is_empty() { e.clone() } else { n.clone() }).collect();
    if !to.is_empty() {
        let to_label = widgets::label(&format!("To {}", to.join(", ")), "dim");
        to_label.set_xalign(0.0);
        to_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        to_label.set_tooltip_text(Some(&m.recipients.iter().map(|(_, e)| e.as_str()).collect::<Vec<_>>().join(", ")));
        who.append(&to_label);
    }
    head.append(&who);
    if m.flags & flags::FLAGGED != 0 {
        let star = gtk::Image::from_icon_name("starred-symbolic");
        star.add_css_class("flag");
        star.set_valign(gtk::Align::Start);
        star.set_tooltip_text(Some("Starred"));
        head.append(&star);
    }
    let date = widgets::label(&window::when_text(m.date, true), "message-date");
    date.set_valign(gtk::Align::Start);
    date.set_tooltip_text(Some(&window::full_date(m.date)));
    head.append(&date);
    card.append(&head);

    match data::parsed(m) {
        Some((raw, parsed)) => {
            body_for(&card, m, &raw, &parsed);
            attachments_for(&card, m, &parsed.attachments);
        }
        None => {
            let waiting = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            waiting.add_css_class("message-body");
            let spinner = gtk::Spinner::new();
            spinner.start();
            waiting.append(&spinner);
            let note = widgets::label("Downloading this message…", "dim");
            waiting.append(&note);
            card.append(&waiting);
            let id = m.id;
            engine::fetch_body(id, move |r| {
                spinner.stop();
                if let Err(e) = r {
                    note.set_text(&e);
                }
                // The engine's Changed signal redraws with the body.
            });
        }
    }
    card
}

fn body_for(card: &gtk::Box, m: &MessageRow, raw: &[u8], parsed: &Parsed) {
    let Some(html) = parsed.html.as_deref() else {
        card.append(&plain_body(&parsed.plain_text()));
        return;
    };
    let clean = mime::sanitize(html);
    let allowed = prefs::trusts(&m.from_addr) || with(|r| r.remote_once.contains(&m.id)).unwrap_or(false);
    if clean.remote && !allowed {
        card.append(&remote_banner(m));
    }
    let _ = raw;
    let view = web_view();
    view.add_css_class("message-html");
    view.set_size_request(-1, 120);
    // Hidden until it's measured, so it never shows cut off at the first height.
    view.set_opacity(0.0);
    view.load_html(&document(&clean.html, m.id, allowed), Some(&format!("{SCHEME}://message/{}", m.id)));
    let load = LOADS.with(|l| l.get() + 1);
    LOADS.with(|l| l.set(load));
    view.set_widget_name(&format!("body-{load}"));
    fit_height(&view, load);
    with(|r| r.views.push(view.clone()));
    card.append(&view);
}

/// Plain text with links and dimmed quotes.
fn plain_body(text: &str) -> gtk::Label {
    let mut markup = String::new();
    for line in text.trim_end().lines() {
        let linked = linkify(line);
        if line.trim_start().starts_with('>') {
            markup.push_str(&format!("<span alpha=\"62%\">{linked}</span>\n"));
        } else {
            markup.push_str(&linked);
            markup.push('\n');
        }
    }
    let label = gtk::Label::new(None);
    label.set_markup(markup.trim_end());
    label.add_css_class("message-body");
    label.add_css_class("plain");
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_selectable(true);
    label.connect_activate_link(|_, uri| {
        open_link(uri);
        glib::Propagation::Stop
    });
    label
}

/// Escapes a line for Pango markup and turns URLs and addresses into links.
pub fn linkify(line: &str) -> String {
    let mut out = String::new();
    for (i, word) in line.split(' ').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let trimmed = word.trim_end_matches(['.', ',', ')', ';', ':', '!', '?', '>', '"', '\'']);
        let tail = &word[trimmed.len()..];
        let start = trimmed.trim_start_matches(['(', '<', '"', '\'']);
        let head = &trimmed[..trimmed.len() - start.len()];
        let href = if start.starts_with("https://") || start.starts_with("http://") {
            Some(start.to_string())
        } else if nyx_mail_core::send::valid_address(start) && start.contains('.') {
            Some(format!("mailto:{start}"))
        } else {
            None
        };
        match href {
            Some(h) => out.push_str(&format!(
                "{}<a href=\"{}\">{}</a>{}",
                glib::markup_escape_text(head),
                glib::markup_escape_text(&h),
                glib::markup_escape_text(start),
                glib::markup_escape_text(tail)
            )),
            None => out.push_str(&glib::markup_escape_text(word)),
        }
    }
    out
}

pub fn open_link(uri: &str) {
    if uri.to_ascii_lowercase().starts_with("mailto:") {
        compose::open(compose::Start::Mailto(uri.to_string()));
        return;
    }
    if !(uri.starts_with("https://") || uri.starts_with("http://")) {
        return;
    }
    let launcher = gtk::UriLauncher::new(uri);
    launcher.launch(window::window().as_ref(), gio::Cancellable::NONE, |r| {
        if let Err(e) = r {
            window::toast(&format!("Couldn't open the link: {}", e.message()));
        }
    });
}

fn remote_banner(m: &MessageRow) -> gtk::Box {
    let banner =
        widgets::banner("Images and other content from the web are hidden, so the sender can't tell you've read this.", false);
    banner.add_css_class("remote-banner");
    let once = widgets::icon_button("image-x-generic-symbolic", "Show images in this message");
    let id = m.id;
    once.connect_clicked(move |_| {
        with(|r| r.remote_once.insert(id));
        refresh();
    });
    banner.append(&once);
    let always = widgets::icon_button("object-select-symbolic", &format!("Always show images from {}", m.from_addr));
    let sender = m.from_addr.clone();
    always.connect_clicked(move |_| {
        prefs::update(|p| {
            if !p.remote_senders.iter().any(|s| s.eq_ignore_ascii_case(&sender)) {
                p.remote_senders.push(sender.clone());
            }
        });
        refresh();
    });
    banner.append(&always);
    banner
}

fn attachments_for(card: &gtk::Box, m: &MessageRow, attachments: &[Attachment]) {
    let shown: Vec<&Attachment> = attachments.iter().filter(|a| !(a.inline && a.content_id.is_some())).collect();
    if shown.is_empty() {
        return;
    }
    let flow = gtk::FlowBox::new();
    flow.add_css_class("attachments");
    flow.set_selection_mode(gtk::SelectionMode::None);
    flow.set_max_children_per_line(4);
    flow.set_column_spacing(8);
    flow.set_row_spacing(8);
    for a in shown {
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chip.add_css_class("attachment");
        chip.append(&gtk::Image::from_icon_name(
            &gio::content_type_get_generic_icon_name(&a.mime)
                .map(|s| format!("{s}-symbolic"))
                .unwrap_or_else(|| "mail-attachment-symbolic".into()),
        ));
        let name = widgets::label(&a.name, "attachment-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        name.set_max_width_chars(24);
        chip.append(&name);
        chip.append(&widgets::label(&size_text(a.size), "dim"));
        let save = widgets::icon_button("document-save-symbolic", &format!("Save “{}”", a.name));
        let (id, index, file) = (m.id, a.index, a.name.clone());
        save.connect_clicked(move |_| save_attachment(id, index, &file));
        chip.append(&save);
        if !executable(a) {
            let open = widgets::icon_button("document-open-symbolic", &format!("Open “{}”", a.name));
            let (id, index, file) = (m.id, a.index, a.name.clone());
            open.connect_clicked(move |_| open_attachment(id, index, &file));
            chip.append(&open);
            let share = widgets::icon_button("send-to-symbolic", &format!("Share “{}”", a.name));
            let (id, index, file, mime) = (m.id, a.index, a.name.clone(), a.mime.clone());
            share.connect_clicked(move |_| share_attachment(id, index, &file, &mime));
            chip.append(&share);
        }
        flow.insert(&chip, -1);
    }
    card.append(&flow);
}

pub fn size_text(bytes: usize) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} KB", bytes as f64 / 1e3),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

/// Programs and scripts: saved, never opened from here.
fn executable(a: &Attachment) -> bool {
    let name = a.name.to_ascii_lowercase();
    let mime = a.mime.to_ascii_lowercase();
    [
        ".exe",
        ".msi",
        ".bat",
        ".cmd",
        ".com",
        ".scr",
        ".sh",
        ".bash",
        ".run",
        ".bin",
        ".appimage",
        ".desktop",
        ".jar",
        ".py",
        ".pl",
        ".js",
        ".vbs",
        ".ps1",
        ".deb",
        ".rpm",
        ".pkg.tar.zst",
        ".flatpakref",
        ".lnk",
    ]
    .iter()
    .any(|ext| name.ends_with(ext))
        || [
            "executable",
            "x-sharedlib",
            "x-shellscript",
            "x-msdownload",
            "x-msi",
            "java-archive",
            "x-desktop",
            "x-python",
            "javascript",
        ]
        .iter()
        .any(|k| mime.contains(k))
}

fn attachment_bytes(id: i64, index: usize) -> Option<Vec<u8>> {
    let row = data::message(id)?;
    let raw = data::raw(row.raw.as_deref()?)?;
    mime::attachment_bytes(&raw, index)
}

/// A file name that can't leave the folder it's saved in.
fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim().trim_start_matches('.');
    if base.is_empty() { "attachment".into() } else { base.chars().map(|c| if c.is_control() { '_' } else { c }).collect() }
}

fn save_attachment(id: i64, index: usize, name: &str) {
    let Some(bytes) = attachment_bytes(id, index) else {
        window::toast("That attachment isn't on this computer.");
        return;
    };
    let dialog = gtk::FileDialog::builder().title("Save attachment").initial_name(safe_name(name)).modal(true).build();
    dialog.save(window::window().as_ref(), gio::Cancellable::NONE, move |r| {
        let Ok(file) = r else { return };
        let Some(path) = file.path() else { return };
        match std::fs::write(&path, &bytes) {
            Ok(()) => window::toast(&format!("Saved {}.", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default())),
            Err(e) => window::toast(&format!("Couldn't save it: {e}")),
        }
    });
}

/// Nyx Share's Share Sheet for an attachment, sent from memory through a
/// read-only descriptor; nothing is saved.
fn share_attachment(id: i64, index: usize, name: &str, mime: &str) {
    let Some(bytes) = attachment_bytes(id, index) else {
        window::toast("That attachment isn't on this computer.");
        return;
    };
    let (name, mime) = (safe_name(name), mime.to_string());
    nyx_core::cmd::background(
        move || {
            use nyx_share_client::{Client, Content, ContentKind, ShareOptions};
            let kind = if mime.starts_with("image/") { ContentKind::Image } else { ContentKind::File };
            let mut content = Content::new();
            content.bytes(kind, &name, Some(&mime), &bytes).map_err(|e| e.to_string())?;
            let options = ShareOptions { app_name: Some("Mail".into()), ..Default::default() };
            Client::connect().and_then(|c| c.share(&content, &options)).map(|_| ()).map_err(|e| e.to_string())
        },
        |result: Result<(), String>| {
            if let Err(e) = result {
                window::toast(&format!("Couldn't share it: {e}"));
            }
        },
    );
}

/// Opens a copy (readable by this user only) with the default app.
fn open_attachment(id: i64, index: usize, name: &str) {
    let Some(bytes) = attachment_bytes(id, index) else {
        window::toast("That attachment isn't on this computer.");
        return;
    };
    // The runtime directory is this user's alone and goes at logout.
    let dir = glib::user_runtime_dir().join("nyx-mail/open").join(format!("{id}-{index}"));
    let path = dir.join(safe_name(name));
    let written = (|| -> std::io::Result<()> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        std::io::Write::write_all(&mut f, &bytes)
    })();
    if let Err(e) = written {
        window::toast(&format!("Couldn't open it: {e}"));
        return;
    }
    gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(window::window().as_ref(), gio::Cancellable::NONE, |r| {
        if let Err(e) = r {
            window::toast(&format!("Couldn't open it: {}", e.message()));
        }
    });
}

// ---------- HTML ----------

/// The page for one message: the sanitized HTML with `cid:` parts pointing at
/// the store and a policy that blocks the network unless allowed.
fn document(clean: &str, id: i64, remote: bool) -> String {
    let web = if remote { " https: http:" } else { "" };
    let csp = format!(
        "default-src 'none'; img-src {SCHEME}: data:{web}; style-src 'unsafe-inline'{web}; font-src {SCHEME}: data:{web}; \
         media-src 'none'; frame-src 'none'; form-action 'none'; base-uri 'none'"
    );
    let body = clean.replace("src=\"cid:", &format!("src=\"{SCHEME}://cid/{id}/"));
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"{csp}\">\
         <meta name=\"color-scheme\" content=\"light\">\
         <style>html{{overflow-y:hidden;overflow-x:auto}}body{{margin:0;padding:18px 20px;background:#fff;color:#1f2328;\
         font:15px/1.5 system-ui,sans-serif;overflow-wrap:anywhere}}img{{max-width:100%;height:auto}}\
         pre{{white-space:pre-wrap}}blockquote{{margin:0 0 0 .6em;padding-left:.8em;border-left:3px solid #d0d7de;color:#57606a}}</style>\
         </head><body>{body}</body></html>"
    )
}

fn context() -> (webkit6::WebContext, webkit6::NetworkSession) {
    WEB.with(|w| {
        if let Some(pair) = w.borrow().as_ref() {
            return pair.clone();
        }
        let ctx = webkit6::WebContext::new();
        if let Some(security) = ctx.security_manager() {
            security.register_uri_scheme_as_secure(SCHEME);
            security.register_uri_scheme_as_cors_enabled(SCHEME);
        }
        ctx.register_uri_scheme(SCHEME, serve);
        let session = webkit6::NetworkSession::new_ephemeral();
        *w.borrow_mut() = Some((ctx.clone(), session.clone()));
        (ctx, session)
    })
}

/// `nyx-mail-msg://cid/<message>/<content id>`: an inline part from the store.
fn serve(request: &webkit6::URISchemeRequest) {
    let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
    let found = uri.strip_prefix(&format!("{SCHEME}://cid/")).and_then(|rest| rest.split_once('/')).and_then(|(id, cid)| {
        let row = data::message(id.parse().ok()?)?;
        let raw = data::raw(row.raw.as_deref()?)?;
        mime::cid_bytes(
            &raw,
            &glib::Uri::unescape_string(cid, None::<&str>).map(|s| s.to_string()).unwrap_or_else(|| cid.to_string()),
        )
    });
    match found {
        Some((mime, bytes)) => {
            let len = bytes.len() as i64;
            let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
            request.finish(&stream, len, Some(&mime));
        }
        None => {
            let mut e = glib::Error::new(gio::IOErrorEnum::NotFound, "not in this message");
            request.finish_error(&mut e);
        }
    }
}

/// Starts the web process (a blank page in a view never shown).
fn prewarm() {
    if WARM.with(|w| w.borrow().is_some()) {
        return;
    }
    let (ctx, session) = context();
    let view = webkit6::WebView::builder().web_context(&ctx).network_session(&session).settings(&locked_settings()).build();
    view.load_html("<!doctype html><html></html>", None);
    WARM.with(|w| *w.borrow_mut() = Some(view));
}

fn web_view() -> webkit6::WebView {
    if let Some(view) = POOL.with(|p| p.borrow_mut().pop()) {
        return view;
    }
    let settings = locked_settings();
    let warm = WARM.with(|w| w.borrow().clone());
    let view = match warm {
        // Shares the warm view's process, context and session.
        Some(warm) => webkit6::WebView::builder().related_view(&warm).settings(&settings).build(),
        None => {
            let (ctx, session) = context();
            webkit6::WebView::builder().web_context(&ctx).network_session(&session).settings(&settings).build()
        }
    };
    configure(&view);
    watch_load(&view);
    view
}

/// Page script off (our measuring runs in an isolated world), nothing stored,
/// no media, no navigation gestures.
fn locked_settings() -> webkit6::Settings {
    let settings = webkit6::Settings::new();
    // Page script stays off; the API's isolated world still runs (to measure).
    settings.set_enable_javascript(true);
    settings.set_enable_javascript_markup(false);
    settings.set_auto_load_images(true);
    settings.set_enable_developer_extras(false);
    settings.set_enable_html5_database(false);
    settings.set_enable_html5_local_storage(false);
    settings.set_enable_page_cache(false);
    settings.set_enable_webgl(false);
    settings.set_enable_webaudio(false);
    settings.set_enable_webrtc(false);
    settings.set_enable_media(false);
    settings.set_enable_media_stream(false);
    settings.set_enable_back_forward_navigation_gestures(false);
    settings.set_allow_file_access_from_file_urls(false);
    settings.set_allow_universal_access_from_file_urls(false);
    settings.set_allow_modal_dialogs(false);
    settings.set_allow_top_navigation_to_data_urls(false);
    settings.set_enable_fullscreen(false);
    settings
}

fn configure(view: &webkit6::WebView) {
    view.set_hexpand(true);
    view.set_background_color(&gdk::RGBA::WHITE);
    view.connect_decide_policy(|_, decision, kind| {
        use webkit6::PolicyDecisionType as T;
        if !matches!(kind, T::NavigationAction | T::NewWindowAction) {
            return false;
        }
        let Some(nav) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() else { return false };
        let uri =
            nav.navigation_action().and_then(|a| a.request()).and_then(|r| r.uri()).map(|u| u.to_string()).unwrap_or_default();
        // The message itself loads; anything else is a link the user clicked.
        if kind == T::NavigationAction && (uri.starts_with(&format!("{SCHEME}://message/")) || uri == "about:blank") {
            decision.use_();
            return true;
        }
        decision.ignore();
        open_link(&uri);
        true
    });
    view.connect_context_menu(|_, menu, _| {
        use webkit6::ContextMenuAction as A;
        let keep = [A::Copy, A::CopyLinkToClipboard, A::CopyImageToClipboard, A::CopyImageUrlToClipboard, A::SelectAll];
        for item in menu.items() {
            if !keep.contains(&item.stock_action()) {
                menu.remove(&item);
            }
        }
        menu.first().is_none()
    });
}

/// Sizes the view to its content (the conversation scrolls, not the view).
fn fit_height(view: &webkit6::WebView, load: u64) {
    // Never left invisible if measuring fails (unless it shows another body by now).
    let weak = view.downgrade();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
        if let Some(v) = weak.upgrade()
            && v.widget_name() == format!("body-{load}")
        {
            v.set_opacity(1.0);
        }
    });
}

/// Measures as a body loads (connected once per view; views are reused).
fn watch_load(view: &webkit6::WebView) {
    view.connect_load_changed(|view, event| {
        // The document is in by Committed (it's one string); images and fonts
        // from the store finish later and can make it taller.
        // Shown once loaded (Finished), already at its height.
        let (delays, reveal): (&[u64], bool) = match event {
            webkit6::LoadEvent::Committed => (&[0, 30], false),
            webkit6::LoadEvent::Finished => (&[0, 250, 1000, 3000], true),
            _ => (&[], false),
        };
        let body = view.widget_name();
        for delay in delays {
            let (v, body) = (view.clone(), body.clone());
            // Still the same body (a reused view may show the next one by then).
            glib::timeout_add_local_once(std::time::Duration::from_millis(*delay), move || {
                measure(&v, reveal && v.widget_name() == body)
            });
        }
    });
}

fn measure(view: &webkit6::WebView, reveal: bool) {
    let v = view.clone();
    view.evaluate_javascript(
        "Math.max(document.documentElement.scrollHeight, document.body ? document.body.scrollHeight : 0)",
        Some("nyx-mail"),
        None,
        gio::Cancellable::NONE,
        move |r| {
            if let Ok(value) = r {
                let h = value.to_int32().clamp(40, 200_000);
                if v.height_request() != h {
                    v.set_size_request(-1, h);
                }
                if reveal {
                    v.set_opacity(1.0);
                }
            }
        },
    );
}

/// Re-measures every HTML body (after the pane's width changed).
pub fn remeasure() {
    for v in with(|r| r.views.clone()).unwrap_or_default() {
        measure(&v, true);
    }
}
