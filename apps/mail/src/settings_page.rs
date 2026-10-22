//! The settings card's groups.

use crate::prefs;
use crate::theme;
use crate::widgets::{self, Page};
use crate::{data, window};
use gtk::prelude::*;

pub fn build(page: &Page) {
    window_group(page);
    reading(page);
    writing(page);
    accounts(page);
    keyboard(page);
}

fn window_group(page: &Page) {
    let group = page.group("Mail window");
    group.add(&nixe_ui::widgets::start_row());
    let prefs = prefs::get();
    let options: Vec<(String, String)> = theme::all().iter().map(|t| (t.id.clone(), t.name.clone())).collect();
    let (theme_row, theme_dd) =
        widgets::choice_row("Theme", "A bundled theme, or one of your own.", options, &prefs.theme, |id| {
            prefs::update(|p| {
                p.theme = id;
                p.mode = prefs::ThemeMode::Theme;
            });
            theme::apply();
        });
    theme_dd.set_sensitive(prefs.mode == prefs::ThemeMode::Theme || !theme::omarchy_available());
    if theme::omarchy_available() {
        let dd = theme_dd.clone();
        let (row, _) = widgets::switch_row(
            "Follow Omarchy theme",
            "Match the desktop's colors and update when the Omarchy theme changes.",
            prefs.mode == prefs::ThemeMode::Omarchy,
            move |on| {
                prefs::update(|p| p.mode = if on { prefs::ThemeMode::Omarchy } else { prefs::ThemeMode::Theme });
                dd.set_sensitive(!on);
                theme::apply();
            },
        );
        group.add(&row);
    }
    group.add(&theme_row);
    let (row, _) = widgets::switch_row("Glow", "Soft accent glow around focused and selected elements.", prefs.glow, |on| {
        prefs::update(|p| p.glow = on);
        theme::apply();
    });
    group.add(&row);
    let (row, _) = widgets::switch_row("Reduce motion", "Turn off transitions in this window.", prefs.reduce_motion, |on| {
        prefs::update(|p| p.reduce_motion = on);
        theme::apply();
    });
    group.add(&row);
}

fn reading(page: &Page) {
    let group = page.group("Reading");
    let prefs = prefs::get();
    let (row, _) = widgets::switch_row(
        "Mark as read when opened",
        "Off: conversations stay unread until you mark them (U).",
        prefs.mark_read,
        |on| prefs::update(|p| p.mark_read = on),
    );
    group.add(&row);
    let (row, _) = widgets::switch_row(
        "Screener",
        "Mail from people who haven't written before waits in the Screener until you let them in. People you've written to, and everyone who wrote before, come straight in.",
        crate::data::screener_on(),
        crate::engine::set_screener,
    );
    group.add(&row);
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
    group.add(&widgets::row("Images from the web", &desc, Some(clear.upcast_ref())));
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
}

fn accounts(page: &Page) {
    let group = page.group("Accounts");
    group.note("Mail accounts come from Settings → Online Accounts (with Mail switched on). Passwords and sign-ins stay there.");
    if let Some(status) = window::status() {
        for a in &status.accounts {
            let desc = match (&a.error, a.offline, a.last_sync) {
                (Some(e), _, _) => e.clone(),
                (None, true, _) => "Offline".into(),
                (None, false, Some(t)) => format!("Updated {}", window::when_text(t, true)),
                _ => "Not checked yet".into(),
            };
            group.add(&widgets::row(&a.address, &desc, None));
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

fn keyboard(page: &Page) {
    let group = page.group("Keyboard");
    for (keys, what) in window::SHORTCUTS {
        group.add(&widgets::row(what, "", Some(widgets::key_caps(keys).upcast_ref())));
    }
}
