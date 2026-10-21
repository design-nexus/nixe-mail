//! Sync and sending against a real IMAP server (Dovecot) and an SMTP sink.
//! Run through `tools/test-imap.sh`, which starts a throwaway Dovecot and sets
//! the environment these tests read; plain `cargo test` skips them.
//!
//! Every test signs in as its own user, so each has its own empty mail store.

use futures::TryStreamExt;
use nixe_mail_core::db::{Db, Role};
use nixe_mail_core::flags::{FLAGGED, SEEN};
use nixe_mail_core::mime::Person;
use nixe_mail_core::net::{self, Auth, NetError, Security, Server, Session};
use nixe_mail_core::send::{self, Draft};
use nixe_mail_core::store::Store;
use nixe_mail_core::sync::{self, Limits, Report, Sync};
use nixe_mail_core::testing::{Dovecot, Sink, env, now};
use std::path::PathBuf;

async fn session(user: &str) -> Result<Session, NetError> {
    Dovecot::from_env().session(user).await
}

async fn deliver(user: &str, mailbox: &str, subject: &str, body: &str) {
    Dovecot::from_env().deliver(user, mailbox, subject, body).await
}

async fn server_flags(user: &str, mailbox: &str, subject: &str) -> Vec<String> {
    Dovecot::from_env().flags(user, mailbox, subject).await
}

async fn server_count(user: &str, mailbox: &str) -> u32 {
    Dovecot::from_env().count(user, mailbox).await
}

/// One account's local side: database and store in a fresh directory.
struct Local {
    account: String,
    db: Db,
    store: Store,
    dir: PathBuf,
}

impl Local {
    fn new(account: &str) -> Self {
        let dir = PathBuf::from(env("NIXE_MAIL_TEST_DIR")).join("local").join(account);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("mail.sqlite")).unwrap();
        db.upsert_account(account, &format!("{account}@example.test"), "").unwrap();
        Self { account: account.into(), db, store: Store::new(dir.join("store")), dir }
    }

    async fn sync(&mut self) -> Result<Report, NetError> {
        let mut s = session(&self.account).await?;
        let report = {
            let mut sync = Sync::new(&mut s, &mut self.db, &self.store, &self.account, Limits::default()).await?;
            sync.run().await?
        };
        let _ = s.logout().await;
        Ok(report)
    }

    fn mailbox(&self, name: &str) -> i64 {
        self.db
            .mailboxes(Some(&self.account))
            .unwrap()
            .into_iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("no mailbox {name}"))
            .id
    }

    fn subjects(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self.db.list(&[self.mailbox(name)], 100, 0).unwrap().into_iter().map(|m| m.subject).collect();
        out.sort();
        out
    }

    fn id_of(&self, name: &str, subject: &str) -> i64 {
        self.db.list(&[self.mailbox(name)], 100, 0).unwrap().into_iter().find(|m| m.subject == subject).unwrap().id
    }
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn first_sync_then_new_mail() {
    let user = "first";
    deliver(user, "INBOX", "Lunch plans", "Shall we get ramen on Friday?").await;
    deliver(user, "INBOX", "Invoice 42", "Your invoice is attached.").await;
    let mut local = Local::new(user);

    let report = local.sync().await.unwrap();
    assert_eq!(report.new, 2);
    assert!(report.new_in_inbox.is_empty(), "a first sync doesn't notify");
    assert_eq!(report.bodies, 2, "recent bodies come down for offline reading");
    assert_eq!(local.subjects("INBOX"), vec!["Invoice 42", "Lunch plans"]);
    let roles: Vec<Option<Role>> = local.db.mailboxes(Some(user)).unwrap().iter().map(|m| m.role).collect();
    for role in [Role::Inbox, Role::Sent, Role::Drafts, Role::Trash, Role::Archive, Role::Junk] {
        assert!(roles.contains(&Some(role)), "{role:?} found by special use");
    }
    assert_eq!(local.db.search("ramen", 10).unwrap().len(), 1);

    deliver(user, "INBOX", "Tickets", "Row 7").await;
    let report = local.sync().await.unwrap();
    assert_eq!((report.new, report.new_in_inbox.len()), (1, 1));
    assert_eq!(local.db.mailbox(local.mailbox("INBOX")).unwrap().unwrap().unread, 3);

    // Nothing changed: nothing comes down.
    let report = local.sync().await.unwrap();
    assert_eq!((report.new, report.changed, report.removed, report.bodies), (0, 0, 0, 0));
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn server_side_changes_come_down() {
    let user = "changes";
    for subject in ["One", "Two", "Three"] {
        deliver(user, "INBOX", subject, "x").await;
    }
    let mut local = Local::new(user);
    local.sync().await.unwrap();

    // Another client reads One and deletes Two.
    let mut s = session(user).await.unwrap();
    s.select("INBOX").await.unwrap();
    let _: Vec<_> = s.store("1", "+FLAGS (\\Seen)").await.unwrap().try_collect().await.unwrap();
    let _: Vec<_> = s.store("2", "+FLAGS (\\Deleted)").await.unwrap().try_collect().await.unwrap();
    let _: Vec<_> = s.expunge().await.unwrap().try_collect().await.unwrap();
    s.logout().await.unwrap();

    let report = local.sync().await.unwrap();
    assert_eq!((report.changed, report.removed), (1, 1));
    assert_eq!(local.subjects("INBOX"), vec!["One", "Three"]);
    let one = local.db.message(local.id_of("INBOX", "One")).unwrap().unwrap();
    assert_eq!(one.flags & SEEN, SEEN);
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn uidvalidity_reset_starts_the_mailbox_over() {
    let user = "validity";
    let mut s = session(user).await.unwrap();
    s.create("Projects").await.unwrap();
    s.logout().await.unwrap();
    deliver(user, "Projects", "Old A", "x").await;
    deliver(user, "Projects", "Old B", "x").await;
    let mut local = Local::new(user);
    local.sync().await.unwrap();
    assert_eq!(local.subjects("Projects"), vec!["Old A", "Old B"]);
    let before = local.db.mailbox(local.mailbox("Projects")).unwrap().unwrap().uidvalidity;

    // The mailbox is recreated on the server: same name, new UIDs.
    let mut s = session(user).await.unwrap();
    s.delete("Projects").await.unwrap();
    s.create("Projects").await.unwrap();
    s.logout().await.unwrap();
    deliver(user, "Projects", "New", "x").await;

    local.sync().await.unwrap();
    let after = local.db.mailbox(local.mailbox("Projects")).unwrap().unwrap().uidvalidity;
    assert_ne!(before, after);
    assert_eq!(local.subjects("Projects"), vec!["New"]);
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn local_and_server_flag_changes_merge() {
    let user = "conflict";
    deliver(user, "INBOX", "Report", "Numbers").await;
    let mut local = Local::new(user);
    local.sync().await.unwrap();
    let id = local.id_of("INBOX", "Report");

    // Read here while another client flags it there.
    sync::queue_flags(&local.db, id, SEEN, 0).unwrap();
    let mut s = session(user).await.unwrap();
    s.select("INBOX").await.unwrap();
    let _: Vec<_> = s.store("1", "+FLAGS (\\Flagged)").await.unwrap().try_collect().await.unwrap();
    s.logout().await.unwrap();

    let report = local.sync().await.unwrap();
    assert_eq!(report.ops_sent, 1);
    assert_eq!(local.db.message(id).unwrap().unwrap().flags & (SEEN | FLAGGED), SEEN | FLAGGED);
    let flags = server_flags(user, "INBOX", "Report").await;
    assert!(flags.contains(&"Seen".into()) && flags.contains(&"Flagged".into()), "{flags:?}");

    // Unflag here; the server keeps Seen.
    sync::queue_flags(&local.db, id, 0, FLAGGED).unwrap();
    local.sync().await.unwrap();
    assert_eq!(server_flags(user, "INBOX", "Report").await, vec!["Seen".to_string()]);
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn moves_and_deletes() {
    let user = "moves";
    for subject in ["Keep", "Archive me", "Trash me"] {
        deliver(user, "INBOX", subject, "x").await;
    }
    let mut local = Local::new(user);
    local.sync().await.unwrap();
    sync::queue_move(&local.db, local.id_of("INBOX", "Archive me"), "Archive").unwrap();
    sync::queue_move(&local.db, local.id_of("INBOX", "Trash me"), "Trash").unwrap();
    // Hidden at once, before the server hears about it.
    assert_eq!(local.subjects("INBOX"), vec!["Keep"]);

    local.sync().await.unwrap();
    assert_eq!(local.subjects("INBOX"), vec!["Keep"]);
    assert_eq!(local.subjects("Archive"), vec!["Archive me"]);
    assert_eq!(local.subjects("Trash"), vec!["Trash me"]);
    assert_eq!(server_count(user, "INBOX").await, 1);

    // Deleting for good from Trash.
    sync::queue_expunge(&local.db, local.id_of("Trash", "Trash me")).unwrap();
    local.sync().await.unwrap();
    assert!(local.subjects("Trash").is_empty());
    assert_eq!(server_count(user, "Trash").await, 0);
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn changes_wait_while_the_server_is_down() {
    let user = "offline";
    deliver(user, "INBOX", "Later", "x").await;
    let mut local = Local::new(user);
    local.sync().await.unwrap();
    let id = local.id_of("INBOX", "Later");

    run(&env("NIXE_MAIL_TEST_STOP"));
    wait_for_server(false).await;
    sync::queue_flags(&local.db, id, FLAGGED, 0).unwrap();
    let error = local.sync().await.unwrap_err();
    assert!(matches!(error, NetError::Unreachable(_)), "{error:?}");
    assert!(error.transient());
    assert_eq!(local.db.pending_ops().unwrap(), 1, "the change waits");
    assert_eq!(local.db.message(id).unwrap().unwrap().flags & FLAGGED, FLAGGED, "and shows here meanwhile");

    run(&env("NIXE_MAIL_TEST_START"));
    wait_for_server(true).await;
    let report = local.sync().await.unwrap();
    assert_eq!(report.ops_sent, 1);
    assert_eq!(local.db.pending_ops().unwrap(), 0);
    assert!(server_flags(user, "INBOX", "Later").await.contains(&"Flagged".into()));
}

fn run(command: &str) {
    let status = std::process::Command::new("sh").arg("-c").arg(command).status().unwrap();
    assert!(status.success(), "{command}");
}

async fn wait_for_server(up: bool) {
    let addr = format!("127.0.0.1:{}", env("NIXE_MAIL_TEST_IMAPS"));
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(&addr).await.is_ok() == up {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the server didn't {}", if up { "start" } else { "stop" });
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn bad_password_is_a_sign_in_error() {
    let error = net::imap(&Dovecot::from_env().server("nobody"), Auth::Password("wrong".into())).await.unwrap_err();
    assert!(matches!(error, NetError::SignIn(_)), "{error:?}");
    assert!(!error.transient());
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn send_then_save_to_sent() {
    let user = "sender";
    let sink = Sink::start(&env("NIXE_MAIL_TEST_CERT"), &env("NIXE_MAIL_TEST_KEY")).await;
    let mut local = Local::new(user);
    local.sync().await.unwrap();

    let draft = Draft {
        account: user.into(),
        from: Person { name: "Me".into(), email: "sender@example.test".into() },
        to: vec![Person { name: "Sam".into(), email: "sam@example.test".into() }],
        bcc: vec![Person { name: String::new(), email: "boss@example.test".into() }],
        subject: "Friday".into(),
        body: "Ramen at noon?".into(),
        attachments: vec![send::FileAttachment {
            name: "menu.txt".into(),
            mime: "text/plain".into(),
            bytes: b"Shoyu, miso".to_vec(),
        }],
        ..Draft::default()
    };
    let raw = draft.build(&send::message_id(&draft.from.email), now());
    let smtp = Server { host: "127.0.0.1".into(), port: sink.port, security: Security::Implicit, user: user.into() };
    send::smtp(&smtp, Auth::Password("secret".into()), &draft.from.email, &draft.recipients(), &raw).await.unwrap();

    let got = sink.messages.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    let (auth, from, to, data) = &got[0];
    assert!(auth.contains("secret"), "signed in with the password");
    assert_eq!(from, "sender@example.test");
    assert_eq!(to, &vec!["sam@example.test".to_string(), "boss@example.test".to_string()]);
    let text = String::from_utf8_lossy(data);
    assert!(text.contains("Subject: Friday") && text.contains("menu.txt"));
    assert!(!text.to_lowercase().contains("bcc"), "Bcc stays off the message");

    // The sent copy goes to Sent, read.
    let name = local.store.put(&raw).unwrap();
    sync::queue_append(&local.db, user, "Sent", &name, SEEN).unwrap();
    local.sync().await.unwrap();
    assert_eq!(local.subjects("Sent"), vec!["Friday"]);
    assert_eq!(server_flags(user, "Sent", "Friday").await, vec!["Seen".to_string()]);
}

#[tokio::test]
#[ignore = "needs tools/test-imap.sh"]
async fn removing_the_account_frees_its_files() {
    let user = "removal";
    deliver(user, "INBOX", "Bye", "x").await;
    let mut local = Local::new(user);
    local.sync().await.unwrap();
    let raw = local.db.message(local.id_of("INBOX", "Bye")).unwrap().unwrap().raw.expect("body kept");
    assert!(local.store.has(&raw));

    for name in local.db.remove_account(user).unwrap() {
        local.store.remove(&name).unwrap();
    }
    assert!(!local.store.has(&raw));
    assert!(local.db.mailboxes(Some(user)).unwrap().is_empty());
    assert!(local.db.search("bye", 10).unwrap().is_empty());
    // The server keeps everything.
    assert_eq!(server_count(user, "INBOX").await, 1);
    let _ = std::fs::remove_dir_all(&local.dir);
}
