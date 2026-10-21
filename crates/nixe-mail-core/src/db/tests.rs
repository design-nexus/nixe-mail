use super::*;
use crate::flags::{FLAGGED, SEEN};
use crate::mime::{Parsed, Person};

fn db() -> (Db, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("nixe-mail-db-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&dir).unwrap();
    (Db::open(&dir.join("mail.sqlite")).unwrap(), dir)
}

fn rand_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn msg(uid: u32, id: &str, reply_to: Option<&str>, subject: &str, date: i64, body: &str) -> NewMessage {
    NewMessage {
        uid,
        uidvalidity: 7,
        flags: 0,
        parsed: Parsed {
            subject: subject.into(),
            from: Some(Person { name: "Sam Lee".into(), email: "sam@example.com".into() }),
            to: vec![Person { name: String::new(), email: "me@example.com".into() }],
            date: Some(date),
            message_id: Some(id.into()),
            in_reply_to: reply_to.map(|r| vec![r.to_string()]).unwrap_or_default(),
            text: Some(body.into()),
            ..Parsed::default()
        },
        body_text: Some(body.into()),
        raw: Some(format!("raw-{uid}")),
        ..NewMessage::default()
    }
}

#[test]
fn conversations_list_search_and_flags() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "Me").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", Some("/"), Some(Role::Inbox)).unwrap();
    let sent = db.upsert_mailbox("acc", "Sent", Some("/"), Some(Role::Sent)).unwrap();
    db.put_message("acc", inbox, &msg(1, "a@x", None, "Lunch plans", 100, "Shall we get ramen on Friday?")).unwrap();
    let mut reply = msg(2, "b@x", Some("a@x"), "Re: Lunch plans", 200, "Friday works.");
    reply.flags = SEEN;
    db.put_message("acc", sent, &reply).unwrap();
    db.put_message("acc", inbox, &msg(3, "c@x", None, "Invoice", 150, "Your invoice is attached.")).unwrap();
    db.recount(inbox).unwrap();

    // The reply joins the first message's conversation.
    let a = db.message(1).unwrap().unwrap();
    let b = db.message(2).unwrap().unwrap();
    assert_eq!(a.thread, b.thread);

    // Inbox and Sent together show two conversations, the lunch one newest with two messages.
    let both = db.list(&[inbox, sent], 50, 0).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!((both[0].subject.as_str(), both[0].count, both[0].unread), ("Re: Lunch plans", 2, true));
    assert_eq!(db.conversation(&a.thread, &[inbox, sent]).unwrap().len(), 2);
    assert_eq!(db.mailbox(inbox).unwrap().unwrap().unread, 2);

    // Full-text search, by body word prefix and by person.
    assert_eq!(db.search("rame", 10).unwrap().iter().map(|m| m.id).collect::<Vec<_>>(), vec![1]);
    assert_eq!(db.search("sam invoice", 10).unwrap().len(), 1);
    assert!(db.search("\"; DROP TABLE messages", 10).unwrap().is_empty());

    // Local flag changes, then the server's view.
    db.change_flags(1, SEEN | FLAGGED, 0).unwrap();
    assert_eq!(db.message(1).unwrap().unwrap().flags, SEEN | FLAGGED);
    db.set_flags(inbox, 7, 1, SEEN, Some(9)).unwrap();
    assert_eq!(db.uids(inbox, 7).unwrap(), vec![(1, SEEN), (3, 0)]);

    // The server lost message 3.
    assert_eq!(db.remove_uids(inbox, 7, &[3]).unwrap(), 1);
    assert!(db.search("invoice", 10).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn updating_a_message_fetches_its_body_once() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    let mut headers = msg(5, "h@x", None, "Report", 10, "");
    headers.raw = None;
    headers.body_text = None;
    headers.parsed.text = None;
    let id = db.put_message("acc", inbox, &headers).unwrap();
    assert_eq!(db.missing_bodies(inbox, 0, 10).unwrap(), vec![(5, 7)]);
    let full = msg(5, "h@x", None, "Report", 10, "Quarterly numbers inside");
    assert_eq!(db.put_message("acc", inbox, &full).unwrap(), id);
    assert!(db.missing_bodies(inbox, 0, 10).unwrap().is_empty());
    assert_eq!(db.search("quarterly", 5).unwrap().len(), 1);
    assert_eq!(db.message(id).unwrap().unwrap().raw.as_deref(), Some("raw-5"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn uidvalidity_reset_and_account_removal() {
    let (mut db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    db.upsert_account("other", "o@example.com", "").unwrap();
    let inbox = db.upsert_mailbox("acc", "INBOX", None, Some(Role::Inbox)).unwrap();
    let other = db.upsert_mailbox("other", "INBOX", None, Some(Role::Inbox)).unwrap();
    db.put_message("acc", inbox, &msg(1, "a@x", None, "One", 1, "x")).unwrap();
    let mut shared = msg(1, "s@x", None, "Shared", 1, "x");
    shared.raw = Some("raw-shared".into());
    db.put_message("acc", inbox, &NewMessage { uid: 2, ..shared.clone() }).unwrap();
    db.put_message("other", other, &shared).unwrap();

    db.clear_mailbox(inbox).unwrap();
    assert!(db.list(&[inbox], 10, 0).unwrap().is_empty());
    db.put_message("acc", inbox, &NewMessage { uid: 2, ..shared.clone() }).unwrap();
    db.put_message("acc", inbox, &msg(4, "only@x", None, "Mine", 1, "x")).unwrap();

    // Removing an account frees only the raw files nothing else uses.
    let orphans = db.remove_account("acc").unwrap();
    assert_eq!(orphans, vec!["raw-4".to_string()]);
    assert!(db.mailboxes(Some("acc")).unwrap().is_empty());
    assert_eq!(db.list(&[other], 10, 0).unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mailboxes_by_role_and_retain() {
    let (db, dir) = db();
    db.upsert_account("acc", "me@example.com", "").unwrap();
    db.upsert_mailbox("acc", "[Gmail]/Trash", Some("/"), Role::detect("[Gmail]/Trash", ["\\Trash"])).unwrap();
    db.upsert_mailbox("acc", "INBOX", Some("/"), Role::detect("INBOX", [])).unwrap();
    db.upsert_mailbox("acc", "Projects/Nixe", Some("/"), Role::detect("Projects/Nixe", [])).unwrap();
    let names: Vec<String> = db.mailboxes(Some("acc")).unwrap().iter().map(|m| m.label()).collect();
    assert_eq!(names, vec!["Inbox", "Trash", "Nixe"]);
    assert!(db.mailbox_by_role("acc", Role::Trash).unwrap().is_some());
    assert_eq!(db.retain_mailboxes("acc", &["INBOX".into()]).unwrap(), 2);
    assert_eq!(db.mailboxes(Some("acc")).unwrap().len(), 1);
    assert_eq!(Role::detect("Sent Items", []), Some(Role::Sent));
    assert_eq!(Role::detect("Bulk Mail", []), Some(Role::Junk));
    assert_eq!(Role::detect("Receipts", []), None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn queued_changes_retry_later() {
    let (db, dir) = db();
    let id = db.queue("acc", "flags", &serde_json::json!({"uid": 1}), 100).unwrap();
    assert_eq!(db.due_ops("acc", 100).unwrap().len(), 1);
    db.op_failed(id, "offline", 160).unwrap();
    assert!(db.due_ops("acc", 120).unwrap().is_empty());
    let due = db.due_ops("acc", 160).unwrap();
    assert_eq!((due[0].attempts, due[0].payload["uid"].as_i64()), (1, Some(1)));
    db.op_done(id).unwrap();
    assert_eq!(db.pending_ops().unwrap(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn fts_queries_are_literal() {
    assert_eq!(fts_query("sam lunch"), "\"sam\"* \"lunch\"*");
    assert_eq!(fts_query("a OR \"b\""), "\"a\"* \"OR\"* \"b\"*");
    assert_eq!(fts_query("  "), "");
}

#[test]
fn drafts_save_and_delete() {
    let (db, dir) = db();
    let id = db.save_draft(None, "acc", &serde_json::json!({"subject": "Hi"}), 1).unwrap();
    assert_eq!(db.save_draft(Some(id), "acc", &serde_json::json!({"subject": "Hi there"}), 2).unwrap(), id);
    let all = db.drafts().unwrap();
    assert_eq!((all.len(), all[0].2["subject"].as_str()), (1, Some("Hi there")));
    db.delete_draft(id).unwrap();
    assert!(db.drafts().unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}
