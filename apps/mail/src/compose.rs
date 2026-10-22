//! Writing a message: a window of its own. Drafts save a moment after each
//! change (through the engine, into `mail.sqlite`) and on close; sending hands
//! the message to the engine, which queues it while offline.

use crate::data;
use crate::engine;
use crate::prefs;
use crate::widgets;
use crate::window;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use nixe_mail_core::db::MessageRow;
use nixe_mail_core::flags;
use nixe_mail_core::mime::{self, Person};
use nixe_mail_core::send::{self, Draft, FileAttachment};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// How a message starts.
pub enum Start {
    New,
    Mailto(String),
    Reply {
        message: i64,
        all: bool,
    },
    Forward(i64),
    /// A saved draft.
    Draft(i64),
}

struct Compose {
    window: gtk::Window,
    from: gtk::DropDown,
    accounts: Vec<(String, Person)>,
    to: gtk::Entry,
    cc: gtk::Entry,
    bcc: gtk::Entry,
    cc_row: gtk::Box,
    bcc_row: gtk::Box,
    subject: gtk::Entry,
    body: gtk::TextView,
    files: RefCell<Vec<FileAttachment>>,
    file_box: gtk::FlowBox,
    note: gtk::Label,
    send: gtk::Button,
    /// The saved draft's id, once saved.
    draft_id: Cell<Option<i64>>,
    /// Bumped by every change; a save waits for it to settle.
    edits: Cell<u64>,
    saved_edits: Cell<u64>,
    in_reply_to: Option<String>,
    references: Vec<String>,
    /// The message replied to (marked Answered once sent).
    answering: Option<i64>,
    sending: Cell<bool>,
    closing: Cell<bool>,
}

type Shared = Rc<Compose>;

pub fn open(start: Start) {
    // Reply and forward need the original's body.
    let original = match &start {
        Start::Reply { message, .. } | Start::Forward(message) => match data::message(*message) {
            Some(row) => Some(row),
            None => {
                window::toast("That message is gone.");
                return;
            }
        },
        _ => None,
    };
    if let Some(row) = &original
        && row.raw.is_none()
    {
        window::toast("Downloading the message first…");
        engine::fetch_body(row.id, move |r| match r {
            Ok(()) => open(start),
            Err(e) => window::toast(&e),
        });
        return;
    }
    let accounts: Vec<(String, Person)> =
        data::accounts().into_iter().map(|a| (a.id.clone(), Person { name: a.name.clone(), email: a.address.clone() })).collect();
    if accounts.is_empty() {
        window::toast("Add a mail account in Settings → Online Accounts first.");
        window::open_online_accounts();
        return;
    }
    let (draft, draft_id, answering, title) = prepare(start, original.as_ref(), &accounts);
    let Some(draft) = draft else { return };
    build(draft, draft_id, answering, title, accounts);
}

fn account_person(accounts: &[(String, Person)], id: &str) -> Person {
    accounts.iter().find(|(a, _)| a == id).or(accounts.first()).map(|(_, p)| p.clone()).unwrap_or_default()
}

fn default_account(accounts: &[(String, Person)]) -> String {
    let chosen = prefs::get().default_account;
    if accounts.iter().any(|(a, _)| *a == chosen) {
        return chosen;
    }
    accounts.first().map(|(a, _)| a.clone()).unwrap_or_default()
}

fn signature() -> String {
    let sig = prefs::get().signature;
    if sig.trim().is_empty() { String::new() } else { format!("\n\n-- \n{}", sig.trim_end()) }
}

fn prepare(
    start: Start,
    original: Option<&MessageRow>,
    accounts: &[(String, Person)],
) -> (Option<Draft>, Option<i64>, Option<i64>, &'static str) {
    let parsed = original.and_then(data::parsed).map(|(_, p)| p);
    match start {
        Start::New => {
            let account = default_account(accounts);
            let draft = Draft { from: account_person(accounts, &account), account, body: signature(), ..Draft::default() };
            (Some(draft), None, None, "New message")
        }
        Start::Mailto(uri) => {
            let account = default_account(accounts);
            let mut draft = send::parse_mailto(&uri);
            draft.from = account_person(accounts, &account);
            draft.account = account;
            draft.body.push_str(&signature());
            (Some(draft), None, None, "New message")
        }
        Start::Reply { all, .. } => {
            let (Some(row), Some(p)) = (original, parsed) else { return (None, None, None, "") };
            let me = data::own_addresses();
            let from = account_person(accounts, &row.account);
            let mut draft = send::reply(&p, all, &me, from, &row.account, &window::full_date(row.date));
            draft.body = format!("{}{}", signature(), draft.body);
            (Some(draft), None, Some(row.id), if all { "Reply to all" } else { "Reply" })
        }
        Start::Forward(_) => {
            let (Some(row), Some(p)) = (original, parsed) else { return (None, None, None, "") };
            let raw = data::raw(row.raw.as_deref().unwrap_or_default()).unwrap_or_default();
            let files: Vec<FileAttachment> = p
                .attachments
                .iter()
                .filter(|a| !(a.inline && a.content_id.is_some()))
                .filter_map(|a| {
                    Some(FileAttachment {
                        name: a.name.clone(),
                        mime: a.mime.clone(),
                        bytes: mime::attachment_bytes(&raw, a.index)?,
                    })
                })
                .collect();
            let from = account_person(accounts, &row.account);
            let mut draft = send::forward(&p, files, from, &row.account, &window::full_date(row.date));
            draft.body = format!("{}\n{}", signature(), draft.body);
            (Some(draft), None, None, "Forward")
        }
        Start::Draft(id) => {
            let saved = data::drafts().into_iter().find(|(d, _, _, _)| *d == id);
            let Some(draft) = saved.and_then(|(_, _, json, _)| serde_json::from_value::<Draft>(json).ok()) else {
                window::toast("That draft is gone.");
                return (None, None, None, "");
            };
            (Some(draft), Some(id), None, "Draft")
        }
    }
}

fn field(label: &str, entry: &gtk::Entry) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("compose-field");
    let l = widgets::label(label, "compose-label");
    l.set_xalign(0.0);
    l.set_width_chars(7);
    row.append(&l);
    entry.set_hexpand(true);
    entry.add_css_class("compose-entry");
    row.append(entry);
    row
}

fn build(draft: Draft, draft_id: Option<i64>, answering: Option<i64>, title: &str, accounts: Vec<(String, Person)>) {
    let window = gtk::Window::builder().title(format!("{title} — Mail")).default_width(760).default_height(680).build();
    window.add_css_class("mail-window");
    window.add_css_class("compose-window");
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.set_icon_name(Some("io.github.design_nexus.nixe.Mail"));
    if let Some(app) = window::window().and_then(|w| w.application()) {
        window.set_application(Some(&app));
    }

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bar.add_css_class("toolbar");
    let heading = widgets::label(title, "toolbar-title");
    heading.set_hexpand(true);
    heading.set_xalign(0.0);
    bar.append(&heading);
    let attach = widgets::icon_button("mail-attachment-symbolic", "Attach files (Ctrl+Shift+A)");
    let more = widgets::icon_button("contact-new-symbolic", "Add Cc and Bcc");
    let discard = widgets::icon_button("user-trash-symbolic", "Discard draft");
    let send_button = gtk::Button::from_icon_name("mail-send-symbolic");
    send_button.add_css_class("suggested-action");
    send_button.add_css_class("send-button");
    send_button.set_tooltip_text(Some("Send (Ctrl+Enter)"));
    send_button.set_valign(gtk::Align::Center);
    bar.append(&attach);
    bar.append(&more);
    bar.append(&discard);
    bar.append(&send_button);

    let fields = gtk::Box::new(gtk::Orientation::Vertical, 0);
    fields.add_css_class("compose-fields");
    let names: Vec<String> = accounts
        .iter()
        .map(|(_, p)| if p.name.is_empty() { p.email.clone() } else { format!("{} <{}>", p.name, p.email) })
        .collect();
    let from = gtk::DropDown::from_strings(&names.iter().map(String::as_str).collect::<Vec<_>>());
    from.set_hexpand(true);
    let chosen = accounts.iter().position(|(a, _)| *a == draft.account).unwrap_or(0);
    from.set_selected(chosen as u32);
    let from_row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    from_row.add_css_class("compose-field");
    let from_label = widgets::label("From", "compose-label");
    from_label.set_xalign(0.0);
    from_label.set_width_chars(7);
    from_row.append(&from_label);
    from_row.append(&from);
    from_row.set_visible(accounts.len() > 1);
    fields.append(&from_row);
    let (to, cc, bcc, subject) = (gtk::Entry::new(), gtk::Entry::new(), gtk::Entry::new(), gtk::Entry::new());
    to.set_text(&send::people_text(&draft.to));
    cc.set_text(&send::people_text(&draft.cc));
    bcc.set_text(&send::people_text(&draft.bcc));
    subject.set_text(&draft.subject);
    to.set_placeholder_text(Some("Sam Lee <sam@example.com>, …"));
    fields.append(&field("To", &to));
    let cc_row = field("Cc", &cc);
    let bcc_row = field("Bcc", &bcc);
    cc_row.set_visible(!draft.cc.is_empty());
    bcc_row.set_visible(!draft.bcc.is_empty());
    more.set_visible(draft.cc.is_empty() || draft.bcc.is_empty());
    fields.append(&cc_row);
    fields.append(&bcc_row);
    fields.append(&field("Subject", &subject));

    let body = gtk::TextView::new();
    body.add_css_class("compose-body");
    body.set_wrap_mode(gtk::WrapMode::WordChar);
    body.set_vexpand(true);
    body.set_top_margin(14);
    body.set_bottom_margin(14);
    body.set_left_margin(20);
    body.set_right_margin(20);
    body.buffer().set_text(&draft.body);
    let body_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&body).build();

    let file_box = gtk::FlowBox::new();
    file_box.add_css_class("attachments");
    file_box.add_css_class("compose-files");
    file_box.set_selection_mode(gtk::SelectionMode::None);
    file_box.set_max_children_per_line(4);
    file_box.set_column_spacing(8);
    file_box.set_row_spacing(8);

    let status = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status.add_css_class("status-bar");
    let note = widgets::label("", "status-readout");
    note.set_xalign(0.0);
    note.set_hexpand(true);
    note.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status.append(&note);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("window-frame");
    frame.append(&bar);
    frame.append(&fields);
    frame.append(&body_scroll);
    frame.append(&file_box);
    frame.append(&status);
    window.set_child(Some(&frame));

    let c: Shared = Rc::new(Compose {
        window: window.clone(),
        from,
        accounts,
        to: to.clone(),
        cc,
        bcc,
        cc_row,
        bcc_row,
        subject: subject.clone(),
        body: body.clone(),
        files: RefCell::new(draft.attachments.clone()),
        file_box,
        note,
        send: send_button.clone(),
        draft_id: Cell::new(draft_id),
        edits: Cell::new(0),
        saved_edits: Cell::new(0),
        in_reply_to: draft.in_reply_to.clone(),
        references: draft.references.clone(),
        answering,
        sending: Cell::new(false),
        closing: Cell::new(false),
    });
    redraw_files(&c);

    for entry in [&c.to, &c.cc, &c.bcc, &c.subject] {
        let c2 = c.clone();
        entry.connect_changed(move |_| changed(&c2));
    }
    {
        let c2 = c.clone();
        c.body.buffer().connect_changed(move |_| changed(&c2));
        let c2 = c.clone();
        c.from.connect_selected_notify(move |_| changed(&c2));
    }
    {
        let c2 = c.clone();
        more.connect_clicked(move |b| {
            c2.cc_row.set_visible(true);
            c2.bcc_row.set_visible(true);
            b.set_visible(false);
            c2.cc.grab_focus();
        });
    }
    {
        let c2 = c.clone();
        attach.connect_clicked(move |_| pick_files(&c2));
    }
    {
        let c2 = c.clone();
        discard.connect_clicked(move |_| {
            if let Some(id) = c2.draft_id.get() {
                engine::delete_draft(id);
            }
            c2.closing.set(true);
            c2.window.close();
            window::toast("Draft discarded.");
        });
    }
    {
        let c2 = c.clone();
        send_button.connect_clicked(move |_| send_now(&c2));
    }
    {
        let c2 = c.clone();
        window.connect_close_request(move |_| {
            if !c2.closing.get() && c2.edits.get() != c2.saved_edits.get() && has_content(&c2) {
                save(&c2, true);
            }
            glib::Propagation::Proceed
        });
    }
    install_keys(&c);
    // Reply bodies start with the cursor above the quote.
    if c.to.text().is_empty() {
        c.to.grab_focus();
    } else {
        let buffer = c.body.buffer();
        buffer.place_cursor(&buffer.start_iter());
        c.body.grab_focus();
    }
    window.present();
}

fn install_keys(c: &Shared) {
    let keys = gtk::EventControllerKey::new();
    let c2 = c.clone();
    keys.connect_key_pressed(move |_, key, _, mods| {
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = mods.contains(gdk::ModifierType::SHIFT_MASK);
        match key {
            gdk::Key::Return | gdk::Key::KP_Enter if ctrl => send_now(&c2),
            gdk::Key::a | gdk::Key::A if ctrl && shift => pick_files(&c2),
            gdk::Key::w if ctrl => c2.window.close(),
            gdk::Key::s if ctrl => save(&c2, false),
            gdk::Key::Escape => c2.window.close(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    c.window.add_controller(keys);
}

fn has_content(c: &Compose) -> bool {
    let d = current(c);
    !d.to.is_empty() || !d.subject.trim().is_empty() || d.body.trim() != signature().trim() || !d.attachments.is_empty()
}

/// The draft as it stands (recipients that don't parse are left out; sending
/// checks them).
fn current(c: &Compose) -> Draft {
    let (account, from) = c.accounts.get(c.from.selected() as usize).or(c.accounts.first()).cloned().unwrap_or_default();
    let people = |e: &gtk::Entry| send::parse_people(&e.text()).unwrap_or_default();
    let buffer = c.body.buffer();
    Draft {
        account,
        from,
        to: people(&c.to),
        cc: people(&c.cc),
        bcc: people(&c.bcc),
        subject: c.subject.text().to_string(),
        body: buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string(),
        in_reply_to: c.in_reply_to.clone(),
        references: c.references.clone(),
        attachments: c.files.borrow().clone(),
    }
}

fn changed(c: &Shared) {
    let n = c.edits.get() + 1;
    c.edits.set(n);
    let c2 = c.clone();
    glib::timeout_add_local_once(std::time::Duration::from_secs(2), move || {
        if c2.edits.get() == n && !c2.closing.get() && !c2.sending.get() {
            save(&c2, false);
        }
    });
}

fn save(c: &Shared, closing: bool) {
    let n = c.edits.get();
    let c2 = c.clone();
    engine::save_draft(c.draft_id.get(), &current(c), move |r| match r {
        Ok(id) => {
            c2.draft_id.set(Some(id));
            c2.saved_edits.set(n);
            if closing {
                window::toast("Draft saved.");
            } else {
                c2.note.set_text(&format!("Draft saved {}", window::when_text(window::now(), false)));
            }
        }
        Err(e) => {
            if closing {
                window::toast(&format!("Couldn't save the draft: {e}"));
            } else {
                c2.note.set_text(&format!("Couldn't save the draft: {e}"));
            }
        }
    });
}

fn send_now(c: &Shared) {
    if c.sending.get() {
        return;
    }
    let mut problems = Vec::new();
    for (name, entry) in [("To", &c.to), ("Cc", &c.cc), ("Bcc", &c.bcc)] {
        if let Err(bad) = send::parse_people(&entry.text()) {
            problems.push(format!("“{bad}” in {name} isn't an email address."));
        }
    }
    let draft = current(c);
    if problems.is_empty() && draft.recipients().is_empty() {
        problems.push("Add someone to send it to.".into());
    }
    if let Some(p) = problems.first() {
        c.note.set_text(p);
        c.note.add_css_class("error");
        return;
    }
    c.note.remove_css_class("error");
    if draft.subject.trim().is_empty() && !confirm_no_subject(c) {
        return;
    }
    c.sending.set(true);
    c.send.set_sensitive(false);
    c.note.set_text("Sending…");
    let c2 = c.clone();
    engine::send(&draft, move |r| {
        c2.sending.set(false);
        c2.send.set_sensitive(true);
        match r {
            Ok(outcome) => {
                if let Some(id) = c2.draft_id.get() {
                    engine::delete_draft(id);
                }
                if let Some(original) = c2.answering {
                    engine::set_flags(&[original], flags::ANSWERED, 0);
                }
                c2.closing.set(true);
                c2.window.close();
                window::toast(if outcome == "queued" { "You're offline. It'll send when you're back online." } else { "Sent." });
            }
            Err(e) => {
                c2.note.set_text(&e);
                c2.note.add_css_class("error");
            }
        }
    });
}

/// Sending without a subject asks once: the second press sends.
fn confirm_no_subject(c: &Compose) -> bool {
    if c.note.text() == NO_SUBJECT {
        return true;
    }
    c.note.set_text(NO_SUBJECT);
    c.subject.grab_focus();
    false
}

const NO_SUBJECT: &str = "There's no subject. Send again to send it anyway.";

fn pick_files(c: &Shared) {
    let dialog = gtk::FileDialog::builder().title("Attach files").modal(true).build();
    let c2 = c.clone();
    dialog.open_multiple(Some(&c.window), gio::Cancellable::NONE, move |r| {
        let Ok(list) = r else { return };
        let mut too_big = Vec::new();
        for file in list.iter::<gio::File>().flatten() {
            let Some(path) = file.path() else { continue };
            let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "attachment".into());
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > 25_000_000 => {
                    too_big.push(name);
                    continue;
                }
                Err(_) => continue,
                _ => {}
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let (guess, _) = gio::content_type_guess(Some(&path), &bytes[..bytes.len().min(4096)]);
            let mime = gio::content_type_get_mime_type(&guess)
                .map(|m| m.to_string())
                .unwrap_or_else(|| "application/octet-stream".into());
            c2.files.borrow_mut().push(FileAttachment { name, mime, bytes });
        }
        if !too_big.is_empty() {
            c2.note.set_text(&format!("Most servers refuse files over 25 MB: left out {}.", too_big.join(", ")));
        }
        redraw_files(&c2);
        changed(&c2);
    });
}

fn redraw_files(c: &Shared) {
    while let Some(child) = c.file_box.first_child() {
        c.file_box.remove(&child);
    }
    let files = c.files.borrow().clone();
    c.file_box.set_visible(!files.is_empty());
    for (i, f) in files.iter().enumerate() {
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chip.add_css_class("attachment");
        chip.append(&gtk::Image::from_icon_name("mail-attachment-symbolic"));
        let name = widgets::label(&f.name, "attachment-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
        name.set_max_width_chars(24);
        chip.append(&name);
        chip.append(&widgets::label(&crate::reader::size_text(f.bytes.len()), "dim"));
        let remove = widgets::icon_button("window-close-symbolic", &format!("Remove “{}”", f.name));
        let c2 = c.clone();
        remove.connect_clicked(move |_| {
            if i < c2.files.borrow().len() {
                c2.files.borrow_mut().remove(i);
            }
            redraw_files(&c2);
            changed(&c2);
        });
        chip.append(&remove);
        c.file_box.insert(&chip, -1);
    }
}
