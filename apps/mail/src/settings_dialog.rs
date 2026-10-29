//! Settings as a card over the window.

use crate::settings_page;
use crate::widgets;
use crate::window;
use gtk::prelude::*;
use nyx_ui::settings_card::{self, About, Spec, Standard};

pub use nyx_ui::settings_card::fit;

const GROUPS: &[(&str, &str, &str)] = &[
    ("Reading", "mail-read-symbolic", "Marking read, previews, images from the web"),
    ("Writing", "document-edit-symbolic", "Signature, sending from"),
    ("Notifications", "preferences-system-notifications-symbolic", "New mail"),
    ("Checking mail", "view-refresh-symbolic", "How often, mail kept offline"),
    ("Accounts", "system-users-symbolic", "Turn accounts on or off; Online accounts"),
];

const SPEC: Spec = Spec {
    groups: GROUPS,
    search_text: None,
    standard: Some(Standard {
        app: "mail",
        about: About {
            name: "Mail",
            icon: "io.github.design_nexus.nyx.Mail",
            version: env!("CARGO_PKG_VERSION"),
            description: "Every account in one inbox, kept on this computer.",
            licence: "MIT",
            website: "https://github.com/design-nexus/nyx-mail",
            issues: "https://github.com/design-nexus/nyx-mail/issues",
            credits: &[],
        },
        shortcuts: window::SHORTCUTS,
        reduce_motion: || crate::prefs::get().reduce_motion,
        set_reduce_motion: |on| crate::prefs::update(|p| p.reduce_motion = on),
    }),
};

pub fn open() {
    let (Some(overlay), Some(window)) = (window::overlay(), window::window()) else { return };
    settings_card::open(&overlay, &window, SPEC, || {
        let page = widgets::page("settings");
        settings_page::build(&page);
        (page.root.upcast(), page.body)
    });
}
