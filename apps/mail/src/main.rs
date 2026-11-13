//! Mail — every mail account in one inbox, for Omarchy.

mod compose;
mod control;
mod data;
mod engine;
mod menus;
mod organize;
mod prefs;
mod reader;
mod reminders;
mod settings_dialog;
mod settings_page;
mod sorted;
mod suggest;
mod theme;
mod widgets;
mod window;

use gtk::prelude::*;
use gtk::{gio, glib};

pub const APP_ID: &str = "io.github.design_nexus.nyx.Mail";

const USAGE: &str = "Usage: nyx-mail [OPTIONS] [mailto:…]\n\
\n\
  mailto:ADDRESS  write a message (from a mailto: link)\n\
  --compose       write a new message\n\
  --message ID    show a message's conversation (or a nyx-mail:message/ID link)\n\
  --section ID    open inbox, drafts, sent, archive, junk, trash, mailbox:ID or settings\n\
  --toggle        close the window if it's open, otherwise open it\n\
  --describe      print the control tools (JSON) and exit\n";

fn main() -> glib::ExitCode {
    nyx_core::app::init(nyx_core::app::App { slug: "nyx-mail", name: "Mail" });
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }
    // The control tools as JSON, without a window (Nyx Assistant's abilities).
    if args.iter().any(|a| a == "--describe") {
        println!("{}", control::tools());
        return glib::ExitCode::SUCCESS;
    }
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: still single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var("GSK_RENDERER", "ngl") };
    }
    // NYX_MAIL_APP_ID runs a test copy under its own ID, beside the real one.
    let id = std::env::var("NYX_MAIL_APP_ID").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| APP_ID.to_string());
    // Windows without an application, like the compose window, take their Wayland app ID from this.
    glib::set_prgname(Some(id.as_str()));
    let app = gtk::Application::builder().application_id(id.as_str()).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.connect_startup(|app| {
        theme::install();
        // Nyx Assistant searches, reads and starts messages through this.
        nyx_core::control::serve_async(app, control::tools(), control::handle);
    });
    app.connect_command_line(|app, cl| {
        let argv: Vec<String> = cl.arguments().iter().map(|a| a.to_string_lossy().to_string()).collect();
        let has = |flag: &str| argv.iter().any(|a| a == flag);
        let value = |flag: &str| argv.iter().position(|a| a == flag).and_then(|i| argv.get(i + 1)).cloned();
        if has("--toggle")
            && let Some(w) = window::window()
            && w.is_visible()
        {
            w.close();
            return glib::ExitCode::SUCCESS;
        }
        window::present(app, value("--section").as_deref());
        // A nyx-mail: link (a reminder's "From") names a message too.
        let linked = argv.iter().skip(1).find_map(|a| reminders::message_of(a));
        if let Some(id) = value("--message").and_then(|v| v.parse::<i64>().ok()).or(linked)
            && !window::open_message(id)
        {
            window::toast("That message isn't on this computer any more.");
        }
        if let Some(link) = argv.iter().skip(1).find(|a| a.to_ascii_lowercase().starts_with("mailto:")) {
            compose::open(compose::Start::Mailto(link.clone()));
        } else if has("--compose") {
            compose::open(compose::Start::New);
        }
        glib::ExitCode::SUCCESS
    });
    app.connect_shutdown(|_| prefs::flush());
    app.run()
}
