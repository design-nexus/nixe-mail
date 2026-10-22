//! The Mail window: Music's sidebar (every account's Inbox, Drafts, Sent…
//! together, then each account's folders), the conversation list (340 px) and
//! the reading pane. The sidebar collapses to icons under 980 px or when the
//! user collapses it.
//!
//! Everything shown is read from `mail.sqlite`; every change is a call to
//! nixe-maild, whose `Changed` signal brings the window up to date.

use crate::compose::{self, Start};
use crate::data::{self, View};
use crate::engine::{self, Status};
use crate::prefs;
use crate::reader;
use crate::settings_dialog;
use crate::widgets;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use nixe_mail_core::db::{ListItem, Mailbox, Role};
use nixe_mail_core::flags;
use nixe_mail_core::send::Draft;
use nixe_ui::chrome;
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
    (&["1"], "Inbox"),
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
enum Row {
    Conversation(ListItem),
    /// A draft being written, not sent yet.
    Draft {
        id: i64,
        draft: Box<Draft>,
        updated: i64,
    },
}

impl Row {
    fn key(&self) -> String {
        match self {
            Self::Conversation(c) => c.thread.clone(),
            Self::Draft { id, .. } => format!("draft:{id}"),
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
    search: gtk::SearchEntry,
    banners: gtk::Box,
    list_title: gtk::Label,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    list_stack: gtk::Stack,
    empty: gtk::Box,
    reader_bar: gtk::Box,
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
        let start = nixe_theme::start_page("inbox", &prefs::get().last_section);
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
    if let Some(n) = std::env::var("MAIL_SNAPSHOT_SELECT").ok().and_then(|v| v.parse::<u32>().ok()) {
        glib::timeout_add_local_once(std::time::Duration::from_millis(delay / 3), move || {
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

fn show_view(view: View) {
    let Some(ui) = ui() else { return };
    prefs::update(|p| p.last_section = view.id());
    {
        let mut u = ui.borrow_mut();
        u.view = view;
        u.limit = PAGE;
        u.open = None;
    }
    reader::show(None, &[]);
    reload();
}

fn view_title(u: &Ui) -> String {
    if searching(u) {
        return format!("Results for “{}”", u.search.text().trim());
    }
    match &u.view {
        View::All(role) => role_name(*role).to_string(),
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
        Some(Role::Archive) => "nixe-mail-archive-symbolic",
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
            if u.view == View::All(Role::Drafts) {
                for (id, _, json, updated) in data::drafts() {
                    if let Ok(draft) = serde_json::from_value::<Draft>(json) {
                        rows.push(Row::Draft { id, draft: Box::new(draft), updated });
                    }
                }
            }
            let mailboxes = u.view.mailboxes(&u.mailboxes);
            rows.extend(data::list(&mailboxes, u.limit).into_iter().map(Row::Conversation));
        }
        rows.retain(|r| match r {
            Row::Conversation(c) => !u.hidden.contains(&c.id),
            Row::Draft { .. } => true,
        });
        (rows, view_title(&u))
    };
    ui.borrow().list_title.set_text(&title);
    chrome::set_title(&ui.borrow().window, "Mail", &title);

    let (store, selection, open) = {
        let u = ui.borrow();
        (u.store.clone(), u.selection.clone(), u.open.clone())
    };
    let objects: Vec<glib::Object> = rows.iter().map(|r| glib::BoxedAnyObject::new(r.clone()).upcast()).collect();
    ui.borrow_mut().restoring = true;
    store.splice(0, store.n_items(), &objects);
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
        let text = match u.view.role(&u.mailboxes) {
            Some(Role::Inbox) => "Your inbox is empty",
            Some(Role::Drafts) => "No drafts",
            Some(Role::Trash) => "Trash is empty",
            Some(Role::Junk) => "No junk",
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
                let to = nixe_mail_core::send::people_text(&draft.to);
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
    if !preview.trim().is_empty() {
        let p = widgets::label(preview.trim(), "mail-preview");
        p.set_xalign(0.0);
        p.set_wrap(true);
        p.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        p.set_lines(2);
        p.set_ellipsize(gtk::pango::EllipsizeMode::End);
        outer.append(&p);
    }
    outer
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
            ui.borrow_mut().open = Some(c.thread.clone());
            let mailboxes = current_mailboxes(&ui.borrow());
            reader::show(Some(&c.thread), &mailboxes);
            if prefs::get().mark_read {
                let unread: Vec<i64> = reader::messages().iter().filter(|m| m.flags & flags::SEEN == 0).map(|m| m.id).collect();
                engine::set_flags(&unread, flags::SEEN, 0);
            }
        }
        Some(Row::Draft { id, .. }) => {
            ui.borrow_mut().open = None;
            reader::show(None, &[]);
            compose::open(Start::Draft(id));
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
fn select(position: u32) {
    let Some(selection) = ui().map(|u| u.borrow().selection.clone()) else { return };
    selection.set_selected(position);
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
    selection.set_selected(next as u32);
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
    if n > 0 && pos != gtk::INVALID_LIST_POSITION {
        select(pos.min(n - 1));
    } else {
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

fn archive() {
    act(Some("archive"), "Archived.");
}

fn trash() {
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

fn junk() {
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

fn toggle_star() {
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

fn toggle_unread() {
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

fn reply(all: bool) {
    if let Some(id) = reply_target() {
        compose::open(Start::Reply { message: id, all });
    }
}

fn forward() {
    if let Some(m) = reader::messages().last() {
        compose::open(Start::Forward(m.id));
    }
}

fn check_mail() {
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
        parts.push(match n {
            0 => "No conversations".to_string(),
            1 => "1 conversation".to_string(),
            n if n >= u.limit => format!("{n}+ conversations"),
            n => format!("{n} conversations"),
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
        for a in &status.accounts {
            let Some(error) = &a.error else { continue };
            let sign_in = error.contains("Sign in again");
            let text = format!("<b>{}</b>: {}", glib::markup_escape_text(&a.address), glib::markup_escape_text(error));
            let banner = widgets::banner(&text, true);
            if sign_in {
                let button = gtk::Button::with_label("Sign in");
                button.set_valign(gtk::Align::Center);
                button.connect_clicked(|_| open_online_accounts());
                banner.append(&button);
            }
            banners.append(&banner);
        }
        for s in &status.skipped {
            let text = format!(
                "<b>{}</b> can't be used for mail: {}",
                glib::markup_escape_text(&s.address),
                glib::markup_escape_text(&s.reason)
            );
            banners.append(&widgets::banner(&text, false));
        }
        if status.accounts.iter().any(|a| a.offline) && !status.accounts.iter().all(|a| a.offline) {
            let names: Vec<&str> = status.accounts.iter().filter(|a| a.offline).map(|a| a.address.as_str()).collect();
            banners.append(&widgets::banner(
                &format!("Can't reach {}. Showing saved mail.", glib::markup_escape_text(&names.join(", "))),
                false,
            ));
        } else if !status.accounts.is_empty() && status.accounts.iter().all(|a| a.offline) {
            banners.append(&widgets::banner(
                "Offline. Showing saved mail; changes and sent mail go when you're back online.",
                false,
            ));
        }
        // Accounts removed from Online Accounts whose mail is still here.
        for a in data::accounts().into_iter().filter(|a| !status.accounts.iter().any(|s| s.id == a.id)) {
            let text = format!(
                "<b>{}</b> was removed from Online Accounts. Its mail is still on this computer.",
                glib::markup_escape_text(&a.address)
            );
            let banner = widgets::banner(&text, false);
            let button = gtk::Button::with_label("Forget");
            button.set_tooltip_text(Some("Remove its mail from this computer (the server keeps it)"));
            button.set_valign(gtk::Align::Center);
            let (id, address) = (a.id.clone(), a.address.clone());
            button.connect_clicked(move |_| forget_account(&id, &address));
            banner.append(&button);
            banners.append(&banner);
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
    let settings = nixe_accounts::find_bin("nixe-settings");
    let started =
        settings.is_some_and(|bin| std::process::Command::new(bin).args(["--section", "online-accounts"]).spawn().is_ok());
    if !started && nixe_accounts::open_management().is_err() {
        toast("Install Nixe Settings or gnome-online-accounts-gtk to add accounts.");
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

fn rebuild_nav() {
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
    list_box.append(&group_label(if several { "All accounts" } else { "Mailboxes" }, true, false));
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
        list_box.append(&button);
    }

    for (i, account) in accounts.iter().enumerate() {
        let mine: Vec<&Mailbox> = mailboxes
            .iter()
            .filter(|m| m.account == account.id)
            .filter(|m| !matches!(m.role, Some(Role::All) | Some(Role::Flagged)))
            .filter(|m| several && m.role == Some(Role::Inbox) || m.role.is_none())
            .collect();
        list_box.append(&group_label(&account.address, false, i == 0));
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
            list_box.append(&button);
        }
    }
    if accounts.is_empty() {
        list_box.append(&group_label("Accounts", false, true));
    }

    let spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    spacer.set_vexpand(true);
    list_box.append(&spacer);
    let accounts_button =
        nav_button("system-users-symbolic", "Online accounts", "Add or sign in to accounts in Settings", 0, false);
    accounts_button.connect_clicked(|_| open_online_accounts());
    list_box.append(&accounts_button);

    let nav = ui.borrow().nav.clone();
    apply_compact(&nav, NARROW.with(|n| n.get()) || prefs::get().sidebar_collapsed);
}

// ---------- Building ----------

fn install_icons() {
    let dir = nixe_core::paths::cache_dir().join("icons");
    let name = "nixe-mail-archive-symbolic.svg";
    let svg = include_str!("../data/nixe-mail-archive-symbolic.svg");
    let path = dir.join(name);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(svg) {
        let _ = nixe_core::cmd::atomic_write(&path, svg);
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
    window.set_icon_name(Some("io.github.design_nexus.nixe.Mail"));
    nixe_ui::keys::install(
        &window,
        nixe_ui::keys::Common {
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
    let head = chrome::sidebar_head("Mail", Some("Find messages (Ctrl+F)"));
    head.toggle.connect_clicked(|_| toggle_sidebar());
    nav.append(&head.root);
    nav.append(&nav_scroll);

    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some("Find messages: people, words, subjects"));
    search.set_visible(false);
    search.connect_search_changed(|_| {
        if let Some(ui) = ui() {
            ui.borrow_mut().limit = PAGE;
        }
        rebuild_list();
        rebuild_nav();
        refresh_readout();
    });
    search.connect_stop_search(|entry| {
        entry.set_text("");
        entry.set_visible(false);
    });
    if let Some(find) = &head.search {
        let entry = search.clone();
        find.connect_clicked(move |_| {
            if entry.is_visible() && entry.text().is_empty() {
                entry.set_visible(false);
            } else {
                entry.set_visible(true);
                entry.grab_focus();
            }
        });
    }

    let banners = gtk::Box::new(gtk::Orientation::Vertical, 8);
    banners.add_css_class("banners");
    banners.set_visible(false);

    // The list column.
    let list_bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    list_bar.add_css_class("toolbar");
    let list_title = widgets::label("", "toolbar-title");
    list_title.set_hexpand(true);
    list_title.set_xalign(0.0);
    list_title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    list_bar.append(&list_title);
    let check = widgets::icon_button("view-refresh-symbolic", "Check for mail (Ctrl+R)");
    check.connect_clicked(|_| check_mail());
    list_bar.append(&check);
    let new = widgets::icon_button("mail-message-new-symbolic", "New message (C)");
    new.connect_clicked(|_| compose::open(Start::New));
    list_bar.append(&new);

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
        item.set_child(Some(&list_row(&row)));
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
    let reader_bar = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    reader_bar.add_css_class("toolbar");
    reader_bar.add_css_class("reader-bar");
    let buttons: [(&str, &str, fn()); 8] = [
        ("mail-reply-sender-symbolic", "Reply (R)", || reply(false)),
        ("mail-reply-all-symbolic", "Reply to all (A)", || reply(true)),
        ("mail-forward-symbolic", "Forward (F)", forward),
        ("nixe-mail-archive-symbolic", "Archive (E)", archive),
        ("mail-mark-junk-symbolic", "Junk or not junk (!)", junk),
        ("user-trash-symbolic", "Move to Trash (#)", trash),
        ("mail-unread-symbolic", "Mark read or unread (U)", toggle_unread),
        ("starred-symbolic", "Star or unstar (S)", toggle_star),
    ];
    for (i, (icon, tip, run)) in buttons.into_iter().enumerate() {
        if i == 3 || i == 6 {
            let gap = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            gap.add_css_class("bar-gap");
            reader_bar.append(&gap);
        }
        let b = widgets::icon_button(icon, tip);
        b.connect_clicked(move |_| run());
        reader_bar.append(&b);
    }
    let reader_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reader_column.set_hexpand(true);
    reader_column.append(&reader_bar);
    reader_column.append(&reader::build());

    let panes = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    panes.set_vexpand(true);
    panes.append(&list_column);
    panes.append(&reader_column);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("settings-content");
    content.set_hexpand(true);
    content.append(&chrome::search_strip(&search));
    content.append(&banners);
    content.append(&panes);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.set_vexpand(true);
    body.append(&nav);
    body.append(&content);

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("window-frame");
    frame.append(&body);
    frame.append(&status_bar());

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&frame));
    window.set_child(Some(&overlay));
    install_keys(&window, &search);
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
        search,
        banners,
        list_title,
        store,
        selection,
        list_stack,
        empty,
        reader_bar,
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

fn status_bar() -> gtk::Box {
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    bar.add_css_class("status-bar");
    bar.append(&chrome::shortcuts_button(show_shortcuts));
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    bar.append(&spacer);
    let readout = widgets::label("", "status-readout");
    readout.set_ellipsize(gtk::pango::EllipsizeMode::Start);
    STATUS.with(|s| *s.borrow_mut() = Some(readout.clone()));
    bar.append(&readout);
    bar.append(&chrome::gear(settings_dialog::open));
    bar
}

fn apply_compact(nav: &gtk::Box, compact: bool) {
    if let Some(head) = nav.first_child().and_downcast::<gtk::Box>() {
        chrome::set_head_compact(&head, compact);
    }
    if compact {
        nav.add_css_class("compact");
    } else {
        nav.remove_css_class("compact");
    }
    fn walk(widget: &gtk::Widget, compact: bool) {
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
                    search.set_visible(true);
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
            if search.is_visible() {
                search.set_text("");
                search.set_visible(false);
                rebuild_list();
                rebuild_nav();
                return glib::Propagation::Stop;
            }
            return glib::Propagation::Proceed;
        }
        if typing(win.upcast_ref()) {
            return glib::Propagation::Proceed;
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
        eprintln!("nixe-mail: {message}");
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
