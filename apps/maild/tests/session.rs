//! nixe-maild end to end, on a private session bus with scratch XDG
//! directories, against the test Dovecot and an SMTP sink (run through
//! tools/test-imap.sh). This test plays the desktop's notification server and
//! the window: it calls MailEngine1 and reads mail.sqlite.

use gio::prelude::*;
use nixe_mail_core::flags::FLAGGED;
use nixe_mail_core::mime::Person;
use nixe_mail_core::send::Draft;
use nixe_mail_core::testing::{Dovecot, Sink, env};
use nixe_mail_core::{Db, Role};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

const NOTIFICATIONS: &str = r#"<node><interface name="org.freedesktop.Notifications">
  <method name="Notify"><arg type="s" direction="in"/><arg type="u" direction="in"/><arg type="s" direction="in"/>
    <arg type="s" direction="in"/><arg type="s" direction="in"/><arg type="as" direction="in"/>
    <arg type="a{sv}" direction="in"/><arg type="i" direction="in"/><arg type="u" direction="out"/></method>
  <method name="GetCapabilities"><arg type="as" direction="out"/></method>
</interface></node>"#;

const USER: &str = "engine";

/// Runs the main loop until `done` or the deadline.
fn pump(seconds: u64, mut done: impl FnMut() -> bool) -> bool {
    let ctx = glib::MainContext::default();
    let until = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < until {
        while ctx.iteration(false) {}
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

fn call(conn: &gio::DBusConnection, method: &str, args: Option<glib::Variant>) -> Result<glib::Variant, glib::Error> {
    conn.call_sync(
        Some("io.github.design_nexus.nixe.MailEngine"),
        "/io/github/design_nexus/nixe/MailEngine",
        "io.github.design_nexus.nixe.MailEngine1",
        method,
        args.as_ref(),
        None,
        gio::DBusCallFlags::NONE,
        30_000,
        gio::Cancellable::NONE,
    )
}

fn status(conn: &gio::DBusConnection) -> serde_json::Value {
    call(conn, "Status", None)
        .ok()
        .and_then(|v| v.child_value(0).get::<String>())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// The engine's database, read-only as the window reads it.
fn db() -> Db {
    let path = std::path::PathBuf::from(env("XDG_DATA_HOME")).join("nixe-mail/mail.sqlite");
    Db::open_read_only(&path).unwrap()
}

fn message_id(subject: &str) -> Option<i64> {
    let db = db();
    let inbox = db.mailbox_by_role(USER, Role::Inbox).ok()??;
    db.list(&[inbox.id], 50, 0).ok()?.into_iter().find(|m| m.subject == subject).map(|m| m.id)
}

struct Killed(std::process::Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "needs tools/test-imap.sh"]
fn syncs_pushes_new_mail_sends_and_saves_changes() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dovecot = Dovecot::from_env();
    let sink = rt.block_on(Sink::start(&env("NIXE_MAIL_TEST_CERT"), &env("NIXE_MAIL_TEST_KEY")));
    rt.block_on(dovecot.deliver(USER, "INBOX", "Welcome", "Hello there"));

    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
    let shown: Rc<RefCell<Vec<(String, String)>>> = Rc::default();
    let info = gio::DBusNodeInfo::for_xml(NOTIFICATIONS).unwrap().lookup_interface("org.freedesktop.Notifications").unwrap();
    let keep = shown.clone();
    let _object = conn
        .register_object("/org/freedesktop/Notifications", &info)
        .method_call(move |_, _, _, _, method, params, invocation| match method {
            "Notify" => {
                let summary = params.child_value(3).get::<String>().unwrap_or_default();
                let body = params.child_value(4).get::<String>().unwrap_or_default();
                keep.borrow_mut().push((summary, body));
                invocation.return_value(Some(&(1u32,).to_variant()));
            }
            _ => invocation.return_value(Some(&(vec!["body".to_string()],).to_variant())),
        })
        .build()
        .unwrap();
    let _name = gio::bus_own_name_on_connection(
        &conn,
        "org.freedesktop.Notifications",
        gio::BusNameOwnerFlags::NONE,
        |_, _| {},
        |_, _| {},
    );

    let accounts = std::path::PathBuf::from(env("NIXE_MAIL_TEST_DIR")).join("accounts.json");
    let server = |port: u16| serde_json::json!({"host": "127.0.0.1", "port": port, "security": "tls", "user": USER});
    let list = serde_json::json!([{
        "id": USER, "address": "engine@example.test", "name": "Engine",
        "imap": server(dovecot.port), "smtp": server(sink.port), "password": "secret",
    }]);
    std::fs::write(&accounts, list.to_string()).unwrap();
    let _maild = Killed(
        std::process::Command::new(env!("CARGO_BIN_EXE_nixe-maild")).env("NIXE_MAIL_TEST_ACCOUNTS", &accounts).spawn().unwrap(),
    );

    // The first sync.
    assert!(pump(30, || status(&conn)["accounts"][0]["last_sync"].is_i64()), "first sync: {}", status(&conn));
    assert!(message_id("Welcome").is_some());
    assert!(shown.borrow().is_empty(), "a first sync doesn't notify");

    // New mail arrives: IDLE notices, and it's announced.
    rt.block_on(dovecot.deliver(USER, "INBOX", "Tickets", "Row 7"));
    assert!(pump(30, || !shown.borrow().is_empty()), "no notification for new mail");
    assert_eq!(shown.borrow()[0], ("Sam Lee".to_string(), "Tickets".to_string()));
    assert!(pump(10, || message_id("Tickets").is_some()));

    // Not while the window is in front.
    call(&conn, "SetActive", Some((true,).to_variant())).unwrap();
    rt.block_on(dovecot.deliver(USER, "INBOX", "Quiet", "x"));
    assert!(pump(30, || message_id("Quiet").is_some()));
    assert_eq!(shown.borrow().len(), 1);

    // Flag a message: shown at once, then saved on the server.
    let welcome = message_id("Welcome").unwrap();
    call(&conn, "SetFlags", Some((vec![welcome], FLAGGED, 0i64).to_variant())).unwrap();
    assert!(pump(5, || db().message(welcome).unwrap().unwrap().flags & FLAGGED != 0));
    assert!(pump(30, || rt.block_on(dovecot.flags(USER, "INBOX", "Welcome")).contains(&"Flagged".to_string())));

    // Archive one.
    call(&conn, "Move", Some((vec![message_id("Quiet").unwrap()], "archive").to_variant())).unwrap();
    assert!(pump(30, || rt.block_on(dovecot.count(USER, "Archive")) == 1));

    // Send: it goes out, and a copy lands in Sent.
    let draft = Draft {
        account: USER.into(),
        from: Person { name: "Engine".into(), email: "engine@example.test".into() },
        to: vec![Person { name: "Sam".into(), email: "sam@example.test".into() }],
        subject: "Friday".into(),
        body: "Ramen at noon?".into(),
        ..Draft::default()
    };
    let reply = call(&conn, "Send", Some((serde_json::to_string(&draft).unwrap(),).to_variant())).unwrap();
    assert_eq!(reply.child_value(0).get::<String>().as_deref(), Some("sent"));
    assert_eq!(sink.messages.lock().unwrap().len(), 1, "sent exactly once");
    assert!(pump(30, || rt.block_on(dovecot.count(USER, "Sent")) == 1), "no copy in Sent");
    assert!(pump(10, || status(&conn)["pending"] == 0), "{}", status(&conn));
    assert_eq!(sink.messages.lock().unwrap().len(), 1, "still exactly once");

    // Errors come back as D-Bus errors, not panics.
    let nobody = Draft { to: Vec::new(), ..draft };
    let error = call(&conn, "Send", Some((serde_json::to_string(&nobody).unwrap(),).to_variant())).unwrap_err();
    assert!(error.message().contains("Add someone"), "{error}");
}
