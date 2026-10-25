//! Settings as a card over the window.

use crate::settings_page;
use crate::widgets;
use crate::window;
use gtk::prelude::*;
use nixe_ui::settings_card::{self, Spec};

pub use nixe_ui::settings_card::fit;

const GROUPS: &[(&str, &str, &str)] = &[
    ("Mail window", "applications-graphics-symbolic", "Theme, glow, motion"),
    ("Reading", "mail-read-symbolic", "Marking read, previews, images from the web"),
    ("Writing", "document-edit-symbolic", "Signature, sending from"),
    ("Notifications", "preferences-system-notifications-symbolic", "New mail"),
    ("Checking mail", "view-refresh-symbolic", "How often, mail kept offline"),
    ("Accounts", "system-users-symbolic", "Turn accounts on or off; Online accounts"),
    ("Keyboard", "input-keyboard-symbolic", "Shortcuts"),
];

const SPEC: Spec = Spec { groups: GROUPS, search_text: None };

pub fn open() {
    let (Some(overlay), Some(window)) = (window::overlay(), window::window()) else { return };
    settings_card::open(&overlay, &window, SPEC, || {
        let page = widgets::page("settings");
        settings_page::build(&page);
        (page.root.upcast(), page.body)
    });
}
