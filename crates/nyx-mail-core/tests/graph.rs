//! Sync and sending for Microsoft 365 accounts, against the fake Graph server
//! in `nyx_mail_core::fake_graph` (no network, no real mailbox).
//! `cargo test -p nyx-mail-core --features testing --test graph`.

use nyx_mail_core::db::{Db, Role};
use nyx_mail_core::fake_graph::FakeGraph;
use nyx_mail_core::flags::{FLAGGED, SEEN};
use nyx_mail_core::graph::{Graph, GraphSync};
use nyx_mail_core::net::NetError;
use nyx_mail_core::store::Store;
use nyx_mail_core::sync::{self, Limits, Report};
use nyx_mail_core::testing::{message, message_from, now};
use std::path::PathBuf;

/// `secs` as Graph writes times.
fn iso(secs: i64) -> String {
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Civil date from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

fn ago(hours: i64) -> String {
    iso(now() - hours * 3600)
}

struct Local {
    account: String,
    db: Db,
    store: Store,
    _dir: PathBuf,
}

impl Local {
    fn new(account: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("nyx-mail-graph-test-{}-{account}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("mail.sqlite")).unwrap();
        db.upsert_account(account, &format!("{account}@example.test"), "").unwrap();
        Self { account: account.into(), db, store: Store::new(dir.join("store")), _dir: dir }
    }

    async fn sync_with(&mut self, fake: &FakeGraph, token: &str, limits: Limits) -> Result<Report, NetError> {
        let graph = Graph::new(&fake.base, token.into())?;
        GraphSync::new(&graph, &mut self.db, &self.store, &self.account, limits).run().await
    }

    async fn sync(&mut self, fake: &FakeGraph) -> Report {
        self.sync_with(fake, "secret", Limits::default()).await.unwrap()
    }

    fn mailbox(&self, name: &str) -> i64 {
        self.db
            .mailboxes(Some(&self.account))
            .unwrap()
            .into_iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("no {name}"))
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

fn fake_with_mail() -> FakeGraph {
    let fake = FakeGraph::start();
    fake.add_folder("Finance", None);
    fake.add_folder("Taxes", Some("Finance"));
    for (i, subject) in ["One", "Two", "Three", "Four"].iter().enumerate() {
        fake.deliver("Inbox", &message(subject, &format!("Body of {subject}")), &ago(10 - i as i64));
    }
    fake.deliver("Taxes", &message("Receipt", "Your receipt"), &ago(20));
    fake
}

#[tokio::test]
async fn folders_messages_and_server_changes() {
    let fake = fake_with_mail();
    let mut local = Local::new("folders");
    let report = local.sync(&fake).await;
    assert_eq!(report.new, 5);
    assert!(report.new_in_inbox.is_empty(), "the first sync isn't new mail");

    let boxes = local.db.mailboxes(Some("folders")).unwrap();
    let role = |name: &str| boxes.iter().find(|m| m.name == name).and_then(|m| m.role);
    assert_eq!(role("Inbox"), Some(Role::Inbox));
    assert_eq!(role("Sent Items"), Some(Role::Sent));
    assert_eq!(role("Deleted Items"), Some(Role::Trash));
    assert_eq!(role("Junk Email"), Some(Role::Junk));
    assert_eq!(role("Archive"), Some(Role::Archive));
    assert!(boxes.iter().any(|m| m.name == "Finance/Taxes"), "children are named under their parents");
    assert!(!boxes.iter().any(|m| m.name == "Outbox"), "Outlook's own folders are left out");
    assert_eq!(local.subjects("Inbox"), ["Four", "One", "Three", "Two"]);
    assert_eq!(local.subjects("Finance/Taxes"), ["Receipt"]);

    // Recent bodies come for reading offline.
    let one = local.db.message(local.id_of("Inbox", "One")).unwrap().unwrap();
    assert!(one.raw.is_some(), "recent bodies are fetched");
    assert!(local.store.get(one.raw.as_ref().unwrap()).unwrap().windows(11).any(|w| w == b"Body of One"));

    // Another client reads one, deletes one, and new mail arrives.
    let server = fake.messages("Inbox");
    let id = |s: &str| server.iter().find(|m| nyx_mail_core::mime::parse(&m.raw).unwrap().subject == s).unwrap().id.clone();
    fake.set_read(&id("Two"), true);
    fake.delete(&id("Three"));
    fake.deliver("Inbox", &message("Five", "Body of Five"), &ago(1));
    let report = local.sync(&fake).await;
    assert_eq!(report.new, 1);
    assert_eq!(report.removed, 1);
    assert_eq!(report.new_in_inbox.len(), 1);
    assert_eq!(local.subjects("Inbox"), ["Five", "Four", "One", "Two"]);
    let two = local.db.message(local.id_of("Inbox", "Two")).unwrap().unwrap();
    assert_eq!(two.flags & SEEN, SEEN);

    // Nothing changed: nothing comes.
    let report = local.sync(&fake).await;
    assert_eq!((report.new, report.changed, report.removed), (0, 0, 0));
}

#[tokio::test]
async fn changes_made_here_reach_the_server() {
    let fake = fake_with_mail();
    let mut local = Local::new("changes");
    local.sync(&fake).await;

    sync::queue_flags(&local.db, local.id_of("Inbox", "One"), SEEN | FLAGGED, 0).unwrap();
    sync::queue_move(&local.db, local.id_of("Inbox", "Two"), "Archive").unwrap();
    sync::queue_move(&local.db, local.id_of("Inbox", "Four"), "Finance/Taxes").unwrap();
    sync::queue_expunge(&local.db, local.id_of("Inbox", "Three")).unwrap();
    let report = local.sync(&fake).await;
    assert_eq!(report.ops_sent, 4);

    let subject = |m: &nyx_mail_core::fake_graph::Message| nyx_mail_core::mime::parse(&m.raw).unwrap().subject;
    let inbox = fake.messages("Inbox");
    assert_eq!(inbox.len(), 1);
    assert!(inbox[0].read && inbox[0].flagged, "read and flagged on the server");
    assert_eq!(fake.messages("Archive").iter().map(subject).collect::<Vec<_>>(), ["Two"]);
    assert_eq!(fake.messages("Taxes").iter().map(subject).collect::<Vec<_>>().len(), 2);
    // Here, the same sync brought the moved messages back in their new places.
    assert_eq!(local.subjects("Inbox"), ["One"]);
    assert_eq!(local.subjects("Archive"), ["Two"]);
    assert_eq!(local.subjects("Finance/Taxes"), ["Four", "Receipt"]);

    // Unflagging and marking unread.
    sync::queue_flags(&local.db, local.id_of("Inbox", "One"), 0, SEEN | FLAGGED).unwrap();
    local.sync(&fake).await;
    let inbox = fake.messages("Inbox");
    assert!(!inbox[0].read && !inbox[0].flagged);

    // A change to a message the server no longer has is dropped, not retried.
    let id = local.id_of("Inbox", "One");
    fake.delete(&inbox[0].id);
    sync::queue_flags(&local.db, id, FLAGGED, 0).unwrap();
    let report = local.sync(&fake).await;
    assert_eq!(report.ops_failed, 1);
    assert!(local.db.due_ops("changes", i64::MAX).unwrap().is_empty());
}

#[tokio::test]
async fn first_sync_keeps_the_newest_window() {
    let fake = FakeGraph::start();
    for i in 0..7 {
        fake.deliver("Inbox", &message(&format!("M{i}"), "x"), &ago(100 - i));
    }
    let mut local = Local::new("window");
    let limits = Limits { window: 3, ..Limits::default() };
    let report = local.sync_with(&fake, "secret", limits).await.unwrap();
    assert_eq!(report.new, 3);
    assert_eq!(local.subjects("Inbox"), ["M4", "M5", "M6"]);
    // Later mail still comes in through the same delta.
    fake.deliver("Inbox", &message("M7", "x"), &ago(1));
    local.sync_with(&fake, "secret", limits).await.unwrap();
    assert_eq!(local.subjects("Inbox"), ["M4", "M5", "M6", "M7"]);
}

#[tokio::test]
async fn expired_delta_links_start_over() {
    let fake = fake_with_mail();
    let mut local = Local::new("expired");
    local.sync(&fake).await;
    fake.expire_delta_links();
    fake.delete(&fake.messages("Inbox")[0].id);
    local.sync(&fake).await;
    local.sync(&fake).await;
    assert_eq!(local.subjects("Inbox").len(), 3);
}

#[tokio::test]
async fn sign_in_failures_say_so() {
    let fake = fake_with_mail();
    let mut local = Local::new("signin");
    let err = local.sync_with(&fake, "wrong", Limits::default()).await.unwrap_err();
    assert!(matches!(err, NetError::SignIn(_)), "{err:?}");
}

#[tokio::test]
async fn sending_keeps_bcc_for_graph_only() {
    let fake = FakeGraph::start();
    let graph = Graph::new(&fake.base, "secret".into()).unwrap();
    let raw = message_from("me@example.test", "Hello", "Hi there");
    let everyone = ["sam@example.test".to_string(), "secret@example.test".into()];
    let raw = String::from_utf8(raw).unwrap().replace("To: ", "To: sam@example.test, ").into_bytes();
    let bcc = nyx_mail_core::graph::bcc_of(&raw, &everyone);
    graph.send_mime(&raw, &bcc).await.unwrap();
    let sent = fake.sent();
    assert_eq!(sent.len(), 1);
    let text = String::from_utf8_lossy(&sent[0]);
    assert!(text.starts_with("Bcc: secret@example.test\r\n"), "{text}");
    assert!(text.contains("Subject: Hello"));
    assert_eq!(fake.messages("Sent Items").len(), 1, "Outlook files it in Sent Items");
}
