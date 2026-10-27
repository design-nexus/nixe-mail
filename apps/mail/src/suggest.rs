//! Recipient suggestions under the To, Cc and Bcc fields: people from the
//! user's mail (`Db::people`) at once, then Nyx Contacts' matches
//! (`nyx-contacts --find`, which reads its address book without a window).

use crate::data;
use crate::widgets;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use nyx_mail_core::mime::Person;
use nyx_mail_core::send;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const SHOWN: usize = 6;

thread_local! {
    static OPEN: Cell<bool> = const { Cell::new(false) };
}

/// A suggestion list is showing (its keys come first).
pub fn showing() -> bool {
    OPEN.with(Cell::get)
}

struct State {
    entry: gtk::Entry,
    popover: gtk::Popover,
    list: gtk::ListBox,
    people: RefCell<Vec<Person>>,
    chosen: Cell<i32>,
    /// Bumped by each keystroke; late Contacts answers for older text are dropped.
    asked: Cell<u64>,
    /// Set while this module rewrites the entry (no new suggestions for that).
    filling: Cell<bool>,
}

/// The part being typed: after the last comma or semicolon.
fn typing(text: &str) -> (usize, String) {
    let start = text.rfind([',', ';']).map(|i| i + 1).unwrap_or(0);
    (start, text[start..].trim().to_string())
}

pub fn attach(entry: &gtk::Entry) {
    let popover = gtk::Popover::new();
    popover.add_css_class("suggestions");
    popover.set_parent(entry);
    popover.set_autohide(false);
    popover.set_has_arrow(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_halign(gtk::Align::Start);
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::Single);
    list.set_can_focus(false);
    popover.set_child(Some(&list));
    let s = Rc::new(State {
        entry: entry.clone(),
        popover: popover.clone(),
        list: list.clone(),
        people: RefCell::new(Vec::new()),
        chosen: Cell::new(0),
        asked: Cell::new(0),
        filling: Cell::new(false),
    });
    popover.connect_closed(|_| OPEN.with(|o| o.set(false)));
    entry.connect_destroy(move |_| popover.unparent());
    {
        let s2 = s.clone();
        entry.connect_changed(move |_| changed(&s2));
    }
    {
        let s2 = s.clone();
        list.connect_row_activated(move |_, row| pick(&s2, row.index()));
    }
    {
        let s2 = s.clone();
        let focus = gtk::EventControllerFocus::new();
        focus.connect_leave(move |_| {
            // Let a click on a suggestion land first.
            let s3 = s2.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(150), move || s3.popover.popdown());
        });
        entry.add_controller(focus);
    }
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let s2 = s.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if !s2.popover.is_visible() {
            return glib::Propagation::Proceed;
        }
        let n = s2.people.borrow().len() as i32;
        match key {
            gdk::Key::Down => highlight(&s2, (s2.chosen.get() + 1).min(n - 1)),
            gdk::Key::Up => highlight(&s2, (s2.chosen.get() - 1).max(0)),
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Tab => pick(&s2, s2.chosen.get()),
            gdk::Key::Escape => s2.popover.popdown(),
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    entry.add_controller(keys);
}

fn changed(s: &Rc<State>) {
    if s.filling.get() {
        return;
    }
    let n = s.asked.get() + 1;
    s.asked.set(n);
    let (_, word) = typing(&s.entry.text());
    if word.chars().count() < 2 || word.contains('<') {
        s.popover.popdown();
        return;
    }
    let from_mail = data::people(&word, SHOWN);
    show(s, from_mail.clone());
    let Some(program) = glib::find_program_in_path("nyx-contacts") else { return };
    let s2 = s.clone();
    // A moment's pause, so a fast typist doesn't start a lookup per letter.
    glib::timeout_add_local_once(std::time::Duration::from_millis(180), move || {
        if s2.asked.get() != n {
            return;
        }
        let argv = [program.as_os_str(), "--find".as_ref(), word.as_ref()];
        let Ok(proc) = gio::Subprocess::newv(&argv, gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE)
        else {
            return;
        };
        proc.communicate_utf8_async(None, gio::Cancellable::NONE, move |r| {
            if s2.asked.get() != n {
                return;
            }
            let Ok((Some(out), _)) = r else { return };
            let mut people = contacts(&out);
            for p in from_mail {
                if !people.iter().any(|q| q.email.eq_ignore_ascii_case(&p.email)) {
                    people.push(p);
                }
            }
            people.truncate(SHOWN);
            show(&s2, people);
        });
    });
}

/// Every email of every contact Contacts found.
fn contacts(json: &str) -> Vec<Person> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else { return Vec::new() };
    let mut out = Vec::new();
    for c in v["contacts"].as_array().into_iter().flatten() {
        let name = c["name"].as_str().unwrap_or_default();
        for e in c["emails"].as_array().into_iter().flatten() {
            if let Some(email) = e["value"].as_str().filter(|e| send::valid_address(e)) {
                out.push(Person { name: name.to_string(), email: email.to_string() });
            }
        }
    }
    out
}

fn show(s: &Rc<State>, people: Vec<Person>) {
    // Leave out who's already in the field.
    let already = send::parse_people(&s.entry.text()[..typing(&s.entry.text()).0]).unwrap_or_default();
    let people: Vec<Person> =
        people.into_iter().filter(|p| !already.iter().any(|a| a.email.eq_ignore_ascii_case(&p.email))).collect();
    while let Some(row) = s.list.first_child() {
        s.list.remove(&row);
    }
    for p in &people {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
        row.add_css_class("suggestion");
        if !p.name.is_empty() {
            let name = widgets::label(&p.name, "suggestion-name");
            name.set_xalign(0.0);
            row.append(&name);
        }
        let email = widgets::label(&p.email, if p.name.is_empty() { "suggestion-name" } else { "dim" });
        email.set_xalign(0.0);
        row.append(&email);
        s.list.append(&row);
    }
    *s.people.borrow_mut() = people;
    let focused = s.entry.root().and_then(|r| r.focus()).is_some_and(|f| f.is_ancestor(&s.entry));
    if s.people.borrow().is_empty() || !focused {
        s.popover.popdown();
        return;
    }
    highlight(s, 0);
    s.popover.popup();
    OPEN.with(|o| o.set(true));
}

fn highlight(s: &State, i: i32) {
    s.chosen.set(i.max(0));
    if let Some(row) = s.list.row_at_index(i) {
        s.list.select_row(Some(&row));
    }
}

/// Puts the person in place of what was being typed, followed by ", ".
fn pick(s: &State, i: i32) {
    let Some(p) = s.people.borrow().get(i.max(0) as usize).cloned() else { return };
    let text = s.entry.text().to_string();
    let (start, _) = typing(&text);
    let head = text[..start].trim_end();
    let filled = format!("{}{}{}, ", head, if head.is_empty() { "" } else { " " }, send::people_text(&[p]));
    s.filling.set(true);
    s.entry.set_text(&filled);
    s.entry.set_position(-1);
    s.filling.set(false);
    s.popover.popdown();
}
