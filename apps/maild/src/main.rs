//! nixe-maild: Nixe Mail's engine. It syncs the mail accounts set up in
//! Settings → Online Accounts, keeps `mail.sqlite` and the message store, sends
//! mail (queuing it while offline) and notifies about new mail. No GTK.
//!
//! The window talks to it over `io.github.design_nexus.nixe.MailEngine1` and
//! reads `mail.sqlite` itself, read-only. No method returns credentials.

mod accounts;
mod engine;

use engine::{Command, Event};
use gio::prelude::*;
use nixe_mail_core::send::Draft;
use std::cell::RefCell;

const BUS_NAME: &str = "io.github.design_nexus.nixe.MailEngine";
const PATH: &str = "/io/github/design_nexus/nixe/MailEngine";
const INTERFACE: &str = "io.github.design_nexus.nixe.MailEngine1";

const XML: &str = r#"<node><interface name="io.github.design_nexus.nixe.MailEngine1">
  <method name="Sync"><arg type="s" name="account" direction="in"/></method>
  <method name="SetFlags"><arg type="ax" name="messages" direction="in"/><arg type="x" name="add" direction="in"/><arg type="x" name="remove" direction="in"/></method>
  <method name="Move"><arg type="ax" name="messages" direction="in"/><arg type="s" name="target" direction="in"/></method>
  <method name="Delete"><arg type="ax" name="messages" direction="in"/></method>
  <method name="FetchBody"><arg type="x" name="message" direction="in"/></method>
  <method name="Send"><arg type="s" name="draft" direction="in"/><arg type="s" name="result" direction="out"/></method>
  <method name="SaveDraft"><arg type="x" name="id" direction="in"/><arg type="s" name="draft" direction="in"/><arg type="x" name="saved" direction="out"/></method>
  <method name="DeleteDraft"><arg type="x" name="id" direction="in"/></method>
  <method name="ForgetAccount"><arg type="s" name="account" direction="in"/></method>
  <method name="SetActive"><arg type="b" name="active" direction="in"/></method>
  <method name="Status"><arg type="s" name="status" direction="out"/></method>
  <signal name="Changed"><arg type="s" name="what"/></signal>
</interface></node>"#;

thread_local! {
    static CONN: RefCell<Option<gio::DBusConnection>> = const { RefCell::new(None) };
    /// Notifications shown, and the message each opens.
    static SHOWN: RefCell<std::collections::HashMap<u32, i64>> = RefCell::new(std::collections::HashMap::new());
}

const USAGE: &str = "Usage: nixe-maild\n\n  Syncs and sends mail for Nixe Mail. Runs as a user service.\n";

fn main() {
    nixe_core::app::init(nixe_core::app::App { slug: "nixe-mail", name: "Mail" });
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    let _owner = gio::bus_own_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameOwnerFlags::DO_NOT_QUEUE,
        |conn, _| {
            let engine = engine::start(|event| {
                glib::MainContext::default().invoke(move || emit(event));
            });
            serve(&conn, engine);
        },
        |_, _| {},
        move |_, _| {
            eprintln!("nixe-maild: already running");
            quit.quit();
        },
    );
    main_loop.run();
}

fn serve(conn: &gio::DBusConnection, engine: tokio::sync::mpsc::UnboundedSender<Command>) {
    CONN.with(|c| *c.borrow_mut() = Some(conn.clone()));
    watch_notifications(conn);
    let info = gio::DBusNodeInfo::for_xml(XML).expect("interface XML").lookup_interface(INTERFACE).expect("interface");
    let registered = conn
        .register_object(PATH, &info)
        .method_call(move |_, _, _, _, method, params, invocation| {
            let send = |c: Command| {
                let _ = engine.send(c);
            };
            match method {
                "Sync" => {
                    let account = params.get::<(String,)>().map(|(a,)| a).unwrap_or_default();
                    send(Command::Sync(Some(account).filter(|a| !a.is_empty())));
                    invocation.return_value(None);
                }
                "SetFlags" => {
                    let Some((ids, add, remove)) = params.get::<(Vec<i64>, i64, i64)>() else { return bad(invocation) };
                    send(Command::Flags(ids, add, remove));
                    invocation.return_value(None);
                }
                "Move" => {
                    let Some((ids, target)) = params.get::<(Vec<i64>, String)>() else { return bad(invocation) };
                    send(Command::Move(ids, target));
                    invocation.return_value(None);
                }
                "Delete" => {
                    let Some((ids,)) = params.get::<(Vec<i64>,)>() else { return bad(invocation) };
                    send(Command::Delete(ids));
                    invocation.return_value(None);
                }
                "FetchBody" => {
                    let Some((id,)) = params.get::<(i64,)>() else { return bad(invocation) };
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    send(Command::FetchBody(id, tx));
                    answer(invocation, rx, |()| None);
                }
                "Send" => {
                    let draft: Option<Draft> = params.get::<(String,)>().and_then(|(j,)| serde_json::from_str(&j).ok());
                    let Some(draft) = draft else { return bad(invocation) };
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    send(Command::Send(Box::new(draft), tx));
                    answer(invocation, rx, |result: String| Some((result,).to_variant()));
                }
                "SaveDraft" => {
                    let Some((id, json)) = params.get::<(i64, String)>() else { return bad(invocation) };
                    let Ok(data) = serde_json::from_str::<serde_json::Value>(&json) else { return bad(invocation) };
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    send(Command::SaveDraft((id > 0).then_some(id), data, tx));
                    answer(invocation, rx, |saved: i64| Some((saved,).to_variant()));
                }
                "DeleteDraft" => {
                    let Some((id,)) = params.get::<(i64,)>() else { return bad(invocation) };
                    send(Command::DeleteDraft(id));
                    invocation.return_value(None);
                }
                "ForgetAccount" => {
                    let Some((account,)) = params.get::<(String,)>() else { return bad(invocation) };
                    send(Command::Forget(account));
                    invocation.return_value(None);
                }
                "SetActive" => {
                    let Some((active,)) = params.get::<(bool,)>() else { return bad(invocation) };
                    send(Command::Active(active));
                    invocation.return_value(None);
                }
                "Status" => {
                    let status = engine::STATUS.lock().ok().and_then(|s| s.clone()).unwrap_or_default();
                    invocation.return_value(Some(&(serde_json::to_string(&status).unwrap_or_default(),).to_variant()));
                }
                other => invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", other),
            }
        })
        .build();
    if let Err(e) = registered {
        eprintln!("nixe-maild: can't serve {INTERFACE}: {e}");
    }
}

fn bad(invocation: gio::DBusMethodInvocation) {
    invocation.return_dbus_error("org.freedesktop.DBus.Error.InvalidArgs", "Those arguments don't fit this method.");
}

/// Answers a call when the engine replies.
fn answer<T: 'static>(
    invocation: gio::DBusMethodInvocation,
    rx: tokio::sync::oneshot::Receiver<Result<T, String>>,
    out: impl FnOnce(T) -> Option<glib::Variant> + 'static,
) {
    glib::MainContext::default().spawn_local(async move {
        match rx.await {
            Ok(Ok(v)) => invocation.return_value(out(v).as_ref()),
            Ok(Err(e)) => invocation.return_dbus_error("io.github.design_nexus.nixe.MailEngine1.Error", &e),
            Err(_) => invocation.return_dbus_error("io.github.design_nexus.nixe.MailEngine1.Error", "The mail engine stopped."),
        }
    });
}

fn emit(event: Event) {
    CONN.with(|c| {
        let Some(conn) = c.borrow().clone() else { return };
        match event {
            Event::Changed(what) => {
                let _ = conn.emit_signal(None, PATH, INTERFACE, "Changed", Some(&(what,).to_variant()));
            }
            Event::NewMail(list) => notify(&conn, &list),
        }
    });
}

/// One notification for new mail: the sender and subject, or a count.
/// Clicking it opens Mail on the (newest) message.
fn notify(conn: &gio::DBusConnection, list: &[(i64, String, String)]) {
    let (summary, body) = match list {
        [] => return,
        [(_, from, subject)] => (from.clone(), subject.clone()),
        many => (
            format!("{} new messages", many.len()),
            many.iter().take(3).map(|(_, f, s)| format!("{f}: {s}")).collect::<Vec<_>>().join("\n"),
        ),
    };
    let message = list.iter().map(|(id, _, _)| *id).max().unwrap_or(0);
    let mut hints = std::collections::HashMap::<String, glib::Variant>::new();
    hints.insert("desktop-entry".into(), "io.github.design_nexus.nixe.Mail".to_variant());
    hints.insert("category".into(), "email.arrived".to_variant());
    let actions: Vec<String> = vec!["default".into(), "Open".into()];
    let params =
        ("Mail", 0u32, "io.github.design_nexus.nixe.Mail", summary.as_str(), body.as_str(), actions, hints, -1i32).to_variant();
    conn.call(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "Notify",
        Some(&params),
        None,
        gio::DBusCallFlags::NONE,
        5000,
        gio::Cancellable::NONE,
        move |r| {
            if let Some(id) = r.ok().and_then(|v| v.get::<(u32,)>()).map(|(id,)| id) {
                SHOWN.with(|s| s.borrow_mut().insert(id, message));
            }
        },
    );
}

/// Opens Mail when one of its notifications is clicked.
fn watch_notifications(conn: &gio::DBusConnection) {
    #[allow(deprecated)]
    conn.signal_subscribe(
        Some("org.freedesktop.Notifications"),
        Some("org.freedesktop.Notifications"),
        None,
        Some("/org/freedesktop/Notifications"),
        None,
        gio::DBusSignalFlags::NONE,
        |_, _, _, _, signal, params| match signal {
            "ActionInvoked" => {
                let Some((id, action)) = params.get::<(u32, String)>() else { return };
                let Some(message) = SHOWN.with(|s| s.borrow_mut().remove(&id)) else { return };
                if action == "default" {
                    open_mail(message);
                }
            }
            "NotificationClosed" => {
                if let Some((id, _)) = params.get::<(u32, u32)>() {
                    SHOWN.with(|s| s.borrow_mut().remove(&id));
                }
            }
            _ => {}
        },
    );
}

/// `nixe-mail --message ID`: the one installed beside this program, else on PATH
/// (tests name another with `NIXE_MAIL_PROGRAM`).
fn open_mail(message: i64) {
    if let Some(program) = std::env::var_os("NIXE_MAIL_PROGRAM") {
        let _ = std::process::Command::new(program).args(["--message", &message.to_string()]).spawn();
        return;
    }
    let beside = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("nixe-mail"))).filter(|p| p.exists());
    let program = beside.unwrap_or_else(|| "nixe-mail".into());
    let _ = std::process::Command::new(program)
        .args(["--message", &message.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
