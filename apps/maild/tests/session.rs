//! nixe-maild end to end, on a private session bus with scratch XDG
//! directories, against the test Dovecot and an SMTP sink (run through
//! tools/test-imap.sh). This test plays the desktop's notification server and
//! the window: it calls MailEngine1 and reads mail.sqlite.

use gio::prelude::*;
use nixe_mail_core::fake_graph::FakeGraph;
use nixe_mail_core::flags::FLAGGED;
use nixe_mail_core::mime::Person;
use nixe_mail_core::send::Draft;
use nixe_mail_core::testing::{Dovecot, Sink, env, message};
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
    db_in(&std::path::PathBuf::from(env("XDG_DATA_HOME")))
}

fn db_in(data: &std::path::Path) -> Db {
    Db::open_read_only(&data.join("nixe-mail/mail.sqlite")).unwrap()
}

fn message_id(subject: &str) -> Option<i64> {
    message_in(&db(), USER, subject)
}

fn message_in(db: &Db, account: &str, subject: &str) -> Option<i64> {
    let inbox = db.mailbox_by_role(account, Role::Inbox).ok()??;
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
    // Stands in for the Mail window: notes what it was asked to open.
    let dir = std::path::PathBuf::from(env("NIXE_MAIL_TEST_DIR"));
    let opened = dir.join("opened");
    let program = dir.join("fake-mail");
    std::fs::write(&program, format!("#!/bin/sh\necho \"$@\" > {}\n", opened.display())).unwrap();
    std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let _maild = Killed(
        std::process::Command::new(env!("CARGO_BIN_EXE_nixe-maild"))
            .env("NIXE_MAIL_TEST_ACCOUNTS", &accounts)
            .env("NIXE_MAIL_PROGRAM", &program)
            .env("NIXE_MAIL_TICK_SECONDS", "1")
            .spawn()
            .unwrap(),
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

    // Clicking it opens Mail on that message.
    conn.emit_signal(
        None,
        "/org/freedesktop/Notifications",
        "org.freedesktop.Notifications",
        "ActionInvoked",
        Some(&(1u32, "default").to_variant()),
    )
    .unwrap();
    let tickets = message_id("Tickets").unwrap();
    assert!(
        pump(10, || std::fs::read_to_string(&opened).is_ok_and(|t| t.trim() == format!("--message {tickets}"))),
        "notification click didn't open Mail"
    );

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

    // Scheduled for two seconds from now: it waits, then goes.
    let soon = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 + 2;
    let later = Draft { subject: "Later".into(), ..draft.clone() };
    let options = serde_json::json!({"at": soon()}).to_string();
    let reply = call(&conn, "SendWith", Some((serde_json::to_string(&later).unwrap(), options).to_variant())).unwrap();
    assert_eq!(reply.child_value(0).get::<String>().as_deref(), Some("scheduled"));
    assert_eq!(status(&conn)["scheduled"], 1);
    assert_eq!(sink.messages.lock().unwrap().len(), 1, "not yet");
    assert!(pump(15, || sink.messages.lock().unwrap().len() == 2), "the scheduled message didn't go");
    assert!(pump(5, || status(&conn)["scheduled"] == 0));

    // A follow-up nobody answers comes back as a notice.
    call(&conn, "SetActive", Some((false,).to_variant())).unwrap();
    let asking = Draft { subject: "Anyone?".into(), ..draft.clone() };
    let options = serde_json::json!({"follow_up": soon()}).to_string();
    call(&conn, "SendWith", Some((serde_json::to_string(&asking).unwrap(), options).to_variant())).unwrap();
    assert!(
        pump(15, || shown.borrow().iter().any(|(s, b)| s == "No reply yet" && b == "Anyone?")),
        "no follow-up notice: {:?}",
        shown.borrow()
    );

    // Snoozed: out of the inbox, then back, unread.
    let tickets = message_id("Tickets").unwrap();
    call(&conn, "Snooze", Some((tickets, soon()).to_variant())).unwrap();
    assert!(pump(5, || message_id("Tickets").is_none()), "still in the inbox");
    assert!(pump(15, || shown.borrow().iter().any(|(s, b)| s == "Back from snooze" && b == "Tickets")));
    assert!(pump(5, || message_id("Tickets")
        .is_some_and(|id| db().message(id).unwrap().unwrap().flags & nixe_mail_core::flags::SEEN == 0)));

    // With the Screener on, a stranger's mail arrives quietly.
    call(&conn, "SetScreener", Some((true,).to_variant())).unwrap();
    let before = shown.borrow().len();
    rt.block_on(dovecot.deliver_from(USER, "INBOX", "stranger@new.test", "Hello there", "x"));
    assert!(pump(30, || message_id("Hello there").is_some()));
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(shown.borrow().len(), before, "no notification for a stranger");
    assert_eq!(status(&conn)["screener"], true);

    // Errors come back as D-Bus errors, not panics.
    let nobody = Draft { to: Vec::new(), ..draft.clone() };
    let error = call(&conn, "Send", Some((serde_json::to_string(&nobody).unwrap(),).to_variant())).unwrap_err();
    assert!(error.message().contains("Add someone"), "{error}");
}

/// A Microsoft 365 account, against the fake Graph server: it syncs, polls for
/// new mail, saves changes there and sends through sendMail (Bcc kept).
#[test]
#[ignore = "needs tools/test-imap.sh"]
fn microsoft_365_accounts_sync_change_and_send() {
    const OUTLOOK: &str = "outlook";
    let fake = FakeGraph::start();
    fake.deliver("Inbox", &message("Welcome", "Hello there"), "2026-01-01T09:00:00Z");
    let dir = std::path::PathBuf::from(env("NIXE_MAIL_TEST_DIR")).join("graph");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let accounts = dir.join("accounts.json");
    let list = serde_json::json!([{
        "id": OUTLOOK, "address": "me@outlook.test", "name": "Me", "graph": fake.base, "password": "secret",
    }]);
    std::fs::write(&accounts, list.to_string()).unwrap();
    let data = dir.join("data");
    let conn = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).unwrap();
    let _maild = Killed(
        std::process::Command::new(env!("CARGO_BIN_EXE_nixe-maild"))
            .env("NIXE_MAIL_TEST_ACCOUNTS", &accounts)
            .env("XDG_DATA_HOME", &data)
            .env("NIXE_MAIL_TICK_SECONDS", "1")
            .env("NIXE_MAIL_POLL_SECONDS", "1")
            .spawn()
            .unwrap(),
    );
    let find = |subject: &str| message_in(&db_in(&data), OUTLOOK, subject);

    assert!(pump(30, || status(&conn)["accounts"][0]["last_sync"].is_i64()), "first sync: {}", status(&conn));
    assert!(find("Welcome").is_some());

    // New mail turns up on the next poll.
    fake.deliver("Inbox", &message("Tickets", "Row 7"), "2026-01-02T09:00:00Z");
    assert!(pump(30, || find("Tickets").is_some()), "new mail didn't come");

    // Flag one and archive the other: saved on the server.
    let subject = |m: &nixe_mail_core::fake_graph::Message| nixe_mail_core::mime::parse(&m.raw).unwrap().subject;
    call(&conn, "SetFlags", Some((vec![find("Welcome").unwrap()], FLAGGED, 0i64).to_variant())).unwrap();
    assert!(pump(30, || fake.messages("Inbox").iter().any(|m| subject(m) == "Welcome" && m.flagged)), "not flagged there");
    call(&conn, "Move", Some((vec![find("Tickets").unwrap()], "archive").to_variant())).unwrap();
    assert!(pump(30, || fake.messages("Archive").len() == 1), "not archived there");

    // Send, with someone on Bcc.
    let draft = Draft {
        account: OUTLOOK.into(),
        from: Person { name: "Me".into(), email: "me@outlook.test".into() },
        to: vec![Person { name: "Sam".into(), email: "sam@example.test".into() }],
        bcc: vec![Person { name: String::new(), email: "quiet@example.test".into() }],
        subject: "Friday".into(),
        body: "Ramen at noon?".into(),
        ..Draft::default()
    };
    let reply = call(&conn, "Send", Some((serde_json::to_string(&draft).unwrap(),).to_variant())).unwrap();
    assert_eq!(reply.child_value(0).get::<String>().as_deref(), Some("sent"));
    let sent = fake.sent();
    assert_eq!(sent.len(), 1, "sent exactly once");
    assert!(String::from_utf8_lossy(&sent[0]).starts_with("Bcc: quiet@example.test\r\n"));
    assert!(pump(10, || status(&conn)["pending"] == 0), "{}", status(&conn));
    assert_eq!(fake.messages("Sent Items").len(), 1, "Outlook keeps the copy; Mail adds none");
    assert_eq!(fake.sent().len(), 1, "still exactly once");
}
