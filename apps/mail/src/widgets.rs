//! Nyx-style building blocks.

pub use nyx_ui::widgets::*;

/// nyx-ui's icon button, also named for screen readers (its tooltip, without
/// the key hint in parentheses).
pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = nyx_ui::widgets::icon_button(icon, tooltip);
    let name = tooltip.split(" (").next().unwrap_or(tooltip);
    b.update_property(&[gtk::accessible::Property::Label(name)]);
    b
}

use gtk::prelude::*;

/// The swatches avatars take, by a hash of who it is (the theme colours them).
const AVATAR_SWATCHES: [&str; 8] = ["blue", "violet", "green", "pink", "coral", "teal", "red", "amber"];

/// "Sam Rivera" → "SR", "GitHub" → "G".
pub fn initials(name: &str) -> String {
    let words: Vec<&str> = name.split_whitespace().filter(|w| w.chars().any(char::is_alphanumeric)).collect();
    let first = |w: &str| w.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect::<String>());
    match words.as_slice() {
        [] => "?".into(),
        [one] => first(one).unwrap_or_else(|| "?".into()),
        [a, .., b] => format!("{}{}", first(a).unwrap_or_default(), first(b).unwrap_or_default()),
    }
}

/// A round avatar `size` px across: someone's initials on a swatch picked by
/// their address (or name), so a person keeps their colour.
pub fn avatar(name: &str, key: &str, size: i32) -> gtk::Box {
    let hash = key.to_lowercase().bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let swatch = AVATAR_SWATCHES[hash as usize % AVATAR_SWATCHES.len()];
    let disc = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    disc.add_css_class("mail-avatar");
    disc.add_css_class(&format!("section-swatch-{swatch}"));
    disc.add_css_class(&format!("size-{}", if size >= 40 { "l" } else if size >= 32 { "m" } else { "s" }));
    disc.set_size_request(size, size);
    disc.set_valign(gtk::Align::Start);
    disc.set_halign(gtk::Align::Start);
    disc.set_hexpand(false);
    let text = gtk::Label::new(Some(&initials(name)));
    text.set_hexpand(true);
    text.set_halign(gtk::Align::Center);
    disc.append(&text);
    disc
}

pub struct Page {
    pub root: gtk::ScrolledWindow,
    pub body: gtk::Box,
}

pub fn page(section: &str) -> Page {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.add_css_class("settings-page");
    body.add_css_class(&format!("page-{section}"));
    body.set_hexpand(true);
    let root = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&body)
        .vexpand(true)
        .build();
    Page { root, body }
}

pub struct Group {
    pub wrapper: gtk::Box,
    pub list: gtk::Box,
}

impl Page {
    pub fn group(&self, title: &str) -> Group {
        let wrapper = gtk::Box::new(gtk::Orientation::Vertical, 0);
        wrapper.add_css_class("settings-group");
        wrapper.set_widget_name(title);
        if !title.is_empty() {
            let label = gtk::Label::new(Some(&title.to_uppercase()));
            label.add_css_class("group-title");
            label.set_xalign(0.0);
            wrapper.append(&label);
        }
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list.add_css_class("group-list");
        wrapper.append(&list);
        self.body.append(&wrapper);
        Group { wrapper, list }
    }
}

impl Group {
    pub fn add(&self, widget: &impl IsA<gtk::Widget>) {
        self.list.append(widget);
    }

    pub fn note(&self, text: &str) {
        let label = gtk::Label::new(Some(text));
        label.add_css_class("group-note");
        label.set_xalign(0.0);
        label.set_wrap(true);
        self.wrapper.insert_child_after(&label, self.wrapper.first_child().as_ref());
    }
}

// ---------- Right-click menus ----------

/// One line of a right-click menu.
pub enum MenuEntry {
    Item {
        label: String,
        key: Option<&'static str>,
        danger: bool,
        enabled: bool,
        run: Box<dyn Fn()>,
    },
    /// Opens a page of its own (Move to…, Snooze…), with a way back.
    Sub {
        label: String,
        entries: Vec<MenuEntry>,
    },
    Separator,
}

pub fn item(label: &str, run: impl Fn() + 'static) -> MenuEntry {
    MenuEntry::Item { label: label.into(), key: None, danger: false, enabled: true, run: Box::new(run) }
}

/// An item with its keyboard shortcut shown beside it.
pub fn keyed(label: &str, key: &'static str, run: impl Fn() + 'static) -> MenuEntry {
    MenuEntry::Item { label: label.into(), key: Some(key), danger: false, enabled: true, run: Box::new(run) }
}

/// An item that deletes or can't be undone, shown in the danger color.
pub fn danger(label: &str, key: Option<&'static str>, run: impl Fn() + 'static) -> MenuEntry {
    MenuEntry::Item { label: label.into(), key, danger: true, enabled: true, run: Box::new(run) }
}

/// An item shown but not available here.
pub fn disabled(label: &str) -> MenuEntry {
    MenuEntry::Item { label: label.into(), key: None, danger: false, enabled: false, run: Box::new(|| {}) }
}

pub fn sub(label: &str, entries: Vec<MenuEntry>) -> MenuEntry {
    MenuEntry::Sub { label: label.into(), entries }
}

pub fn separator() -> MenuEntry {
    MenuEntry::Separator
}

fn menu_row(label: &str, end: Option<gtk::Widget>, danger: bool) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("menu-item");
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let text = gtk::Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    if danger {
        text.add_css_class("danger-text");
    }
    row.append(&text);
    if let Some(end) = end {
        row.append(&end);
    }
    button.set_child(Some(&row));
    button
}

fn menu_page(popover: &gtk::Popover, pages: &gtk::Stack, entries: Vec<MenuEntry>, back: Option<&str>) -> gtk::Box {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 1);
    if let Some(title) = back {
        let icon = gtk::Image::from_icon_name("go-previous-symbolic");
        let row = menu_row(title, None, false);
        if let Some(content) = row.child().and_downcast::<gtk::Box>() {
            content.prepend(&icon);
        }
        let p = pages.clone();
        row.connect_clicked(move |_| p.set_visible_child_name("main"));
        list.append(&row);
        list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    }
    for (i, entry) in entries.into_iter().enumerate() {
        match entry {
            MenuEntry::Separator => list.append(&gtk::Separator::new(gtk::Orientation::Horizontal)),
            MenuEntry::Item { label, key, danger, enabled, run } => {
                let hint = key.map(|k| {
                    let l = gtk::Label::new(Some(k));
                    l.add_css_class("dim");
                    l.upcast::<gtk::Widget>()
                });
                let row = menu_row(&label, hint, danger);
                row.set_sensitive(enabled);
                let p = popover.clone();
                row.connect_clicked(move |_| {
                    p.popdown();
                    run();
                });
                list.append(&row);
            }
            MenuEntry::Sub { label, entries } => {
                let name = format!("sub{i}");
                let page = menu_page(popover, pages, entries, Some(&label));
                pages.add_named(&page, Some(&name));
                let row = menu_row(&label, Some(gtk::Image::from_icon_name("go-next-symbolic").upcast()), false);
                let p = pages.clone();
                row.connect_clicked(move |_| p.set_visible_child_name(&name));
                list.append(&row);
            }
        }
    }
    list
}

/// Shows a menu at a point in `anchor` (a right-click's position).
pub fn popup_menu(anchor: &impl IsA<gtk::Widget>, x: f64, y: f64, entries: Vec<MenuEntry>) {
    if entries.is_empty() {
        return;
    }
    let anchor = anchor.as_ref().clone();
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.set_parent(&anchor);
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_halign(gtk::Align::Start);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.add_css_class("menu-popover");
    let pages = gtk::Stack::new();
    pages.set_vhomogeneous(false);
    pages.set_hhomogeneous(false);
    pages.set_transition_type(gtk::StackTransitionType::SlideLeftRight);
    let main = menu_page(&popover, &pages, entries, None);
    pages.add_named(&main, Some("main"));
    pages.set_visible_child_name("main");
    popover.set_child(Some(&pages));
    let weak = anchor.downgrade();
    popover.connect_closed(move |p| {
        let (p, a) = (p.clone(), weak.clone());
        gtk::glib::idle_add_local_once(move || {
            if a.upgrade().is_some() {
                p.unparent();
            }
        });
    });
    popover.popup();
}

/// Calls `open(widget, x, y)` on a right-click (or a long press) on `widget`.
pub fn on_right_click(widget: &impl IsA<gtk::Widget>, open: impl Fn(&gtk::Widget, f64, f64) + 'static) {
    let open = std::rc::Rc::new(open);
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_SECONDARY);
    let o = open.clone();
    click.connect_pressed(move |g, _, x, y| {
        if let Some(w) = g.widget() {
            g.set_state(gtk::EventSequenceState::Claimed);
            o(&w, x, y);
        }
    });
    widget.add_controller(click);
    let press = gtk::GestureLongPress::new();
    press.set_touch_only(true);
    press.connect_pressed(move |g, x, y| {
        if let Some(w) = g.widget() {
            open(&w, x, y);
        }
    });
    widget.add_controller(press);
}

/// The Menu key or Shift+F10.
pub fn is_menu_key(key: gtk::gdk::Key, state: gtk::gdk::ModifierType) -> bool {
    key == gtk::gdk::Key::Menu || (key == gtk::gdk::Key::F10 && state.contains(gtk::gdk::ModifierType::SHIFT_MASK))
}
