//! Mail — every mail account in one inbox, for Omarchy.

mod compose;
mod data;
mod engine;
mod prefs;
mod reader;
mod settings_dialog;
mod settings_page;
mod theme;
mod widgets;
mod window;

use gtk::prelude::*;
use gtk::{gio, glib};

pub const APP_ID: &str = "io.github.design_nexus.nixe.Mail";

const USAGE: &str = "Usage: nixe-mail [OPTIONS] [mailto:…]\n\
\n\
  mailto:ADDRESS  write a message (from a mailto: link)\n\
  --compose       write a new message\n\
  --section ID    open inbox, drafts, sent, archive, junk, trash, mailbox:ID or settings\n\
  --toggle        close the window if it's open, otherwise open it\n";

fn main() -> glib::ExitCode {
    nixe_core::app::init(nixe_core::app::App { slug: "nixe-mail", name: "Mail" });
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return glib::ExitCode::SUCCESS;
    }
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: still single-threaded; nothing else reads the environment yet.
        unsafe { std::env::set_var("GSK_RENDERER", "ngl") };
    }
    let app = gtk::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.connect_startup(|_| theme::install());
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
