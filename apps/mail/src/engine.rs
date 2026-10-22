//! Talking to nixe-maild over `io.github.design_nexus.nixe.MailEngine1`. Every
//! change goes through the engine (it is the only writer of `mail.sqlite`);
//! the window reads the database itself.

use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use nixe_mail_core::send::Draft;
use serde::Deserialize;
use std::cell::RefCell;

const NAME: &str = "io.github.design_nexus.nixe.MailEngine";
const PATH: &str = "/io/github/design_nexus/nixe/MailEngine";
const INTERFACE: &str = "io.github.design_nexus.nixe.MailEngine1";

/// What the engine reports (`Status()`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Status {
    pub accounts: Vec<AccountStatus>,
    pub skipped: Vec<SkippedStatus>,
    pub pending: i64,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct AccountStatus {
    pub id: String,
    pub address: String,
    pub name: String,
    pub syncing: bool,
    pub offline: bool,
    pub error: Option<String>,
    pub last_sync: Option<i64>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SkippedStatus {
    pub address: String,
    pub reason: String,
}

thread_local! {
    static CONN: RefCell<Option<gio::DBusConnection>> = const { RefCell::new(None) };
}

fn conn() -> Option<gio::DBusConnection> {
    CONN.with(|c| {
        if c.borrow().is_none() {
            *c.borrow_mut() = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok();
        }
        c.borrow().clone()
    })
}

/// Starts the engine if it isn't running: its user service when installed,
/// else the program next to this one or on PATH.
pub fn ensure_running() {
    let unit = nixe_core::paths::config_home().join("systemd/user/nixe-maild.service");
    if unit.exists() {
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "start", "--no-block", "nixe-maild.service"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        return;
    }
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("nixe-maild"))).filter(|p| p.exists());
    let program = beside.unwrap_or_else(|| "nixe-maild".into());
    let _ = std::process::Command::new(program).stdout(std::process::Stdio::null()).spawn();
}

/// Calls `appeared` whenever the engine (re)starts and `gone` when it stops,
/// for as long as the app runs.
pub fn watch(appeared: impl Fn() + 'static, gone: impl Fn() + 'static) {
    // The watch lasts until unwatched, which this never does.
    let _ = gio::bus_watch_name(
        gio::BusType::Session,
        NAME,
        gio::BusNameWatcherFlags::NONE,
        move |_, _, _| appeared(),
        move |_, _| gone(),
    );
}

/// Calls `f` with what changed (`messages`, `mailboxes`, `status`).
pub fn on_changed(f: impl Fn(&str) + 'static) {
    let Some(conn) = conn() else { return };
    #[allow(deprecated)]
    conn.signal_subscribe(
        Some(NAME),
        Some(INTERFACE),
        Some("Changed"),
        Some(PATH),
        None,
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            if let Some((what,)) = params.get::<(String,)>() {
                f(&what);
            }
        },
    );
}

fn call(method: &str, args: Option<glib::Variant>, done: impl FnOnce(Result<glib::Variant, String>) + 'static) {
    let Some(conn) = conn() else {
        done(Err("The session bus isn't available.".into()));
        return;
    };
    conn.call(
        Some(NAME),
        PATH,
        INTERFACE,
        method,
        args.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        120_000,
        gio::Cancellable::NONE,
        move |r| {
            done(r.map_err(|mut e| {
                if e.matches(gio::DBusError::ServiceUnknown) || e.matches(gio::DBusError::NameHasNoOwner) {
                    return "Mail's engine isn't running.".to_string();
                }
                gio::DBusError::strip_remote_error(&mut e);
                e.message().to_string()
            }))
        },
    );
}

fn fire(method: &str, args: glib::Variant) {
    call(method, Some(args), |r| {
        if let Err(e) = r {
            crate::window::toast(&e);
        }
    });
}

pub fn sync(account: &str) {
    fire("Sync", (account,).to_variant());
}

pub fn set_flags(ids: &[i64], add: i64, remove: i64) {
    if !ids.is_empty() {
        fire("SetFlags", (ids.to_vec(), add, remove).to_variant());
    }
}

/// To a role (`archive`, `trash`, `junk`, `inbox`) or `mailbox:<id>`.
pub fn move_to(ids: &[i64], target: &str) {
    if !ids.is_empty() {
        fire("Move", (ids.to_vec(), target).to_variant());
    }
}

/// Deletes for good (from Trash or Junk).
pub fn delete(ids: &[i64]) {
    if !ids.is_empty() {
        fire("Delete", (ids.to_vec(),).to_variant());
    }
}

pub fn fetch_body(id: i64, done: impl FnOnce(Result<(), String>) + 'static) {
    call("FetchBody", Some((id,).to_variant()), move |r| done(r.map(|_| ())));
}

/// Sends; `Ok("sent")`, or `Ok("queued")` when offline (it goes later).
pub fn send(draft: &Draft, done: impl FnOnce(Result<String, String>) + 'static) {
    let json = serde_json::to_string(draft).unwrap_or_default();
    call("Send", Some((json,).to_variant()), move |r| done(r.map(|v| v.child_value(0).get::<String>().unwrap_or_default())));
}

pub fn save_draft(id: Option<i64>, draft: &Draft, done: impl FnOnce(Result<i64, String>) + 'static) {
    let json = serde_json::to_string(draft).unwrap_or_default();
    call("SaveDraft", Some((id.unwrap_or(0), json).to_variant()), move |r| {
        done(r.map(|v| v.child_value(0).get::<i64>().unwrap_or(0)))
    });
}

pub fn delete_draft(id: i64) {
    fire("DeleteDraft", (id,).to_variant());
}

pub fn forget(account: &str) {
    fire("ForgetAccount", (account,).to_variant());
}

/// The window is in front (no notifications) or not.
pub fn set_active(active: bool) {
    call("SetActive", Some((active,).to_variant()), |_| {});
}

pub fn status(done: impl FnOnce(Option<Status>) + 'static) {
    call("Status", None, move |r| {
        done(r.ok().and_then(|v| v.child_value(0).get::<String>()).and_then(|s| serde_json::from_str(&s).ok()))
    });
}
