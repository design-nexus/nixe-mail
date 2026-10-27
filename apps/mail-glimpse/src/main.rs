//! Glimpse plugin for Mail. It opens `mail.sqlite` read-only (Glimpse mounts
//! it into the sandbox) and never touches the network or the message store.
//!
//! `mail` lists the newest inbox conversations, `mail unread` the unread ones;
//! other words search every account. A result opens in Mail through the host:
//! its token is `{"api_version": 1, "app": "mail", "op": "open", "message": ID}`.

use anyhow::{Context, Result, bail};
use glimpse_plugin_sdk::{Action, Hit, Request, serve};
use nyx_mail_core::db::{Db, ListItem, Role};
use serde_json::{Value, json};
use std::path::PathBuf;

fn main() {
    if let Err(error) = serve(&["search", "preview"], answer) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}

fn database() -> PathBuf {
    if let Some(path) = std::env::var_os("NYX_MAIL_DB") {
        return PathBuf::from(path);
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    data.unwrap_or_else(|| PathBuf::from(".local/share")).join("nyx-mail/mail.sqlite")
}

fn open() -> Result<Db> {
    let path = database();
    if !path.exists() {
        bail!("Mail has no mail on this computer yet.");
    }
    Db::open_read_only(&path).context("Mail's database couldn't be read")
}

/// What was asked: the inbox (optionally unread only), or a search.
#[derive(Debug, PartialEq, Eq)]
enum Asked {
    Inbox { unread: bool },
    Search(String),
}

fn asked(query: &str) -> Option<Asked> {
    let q = query.trim();
    let lower = q.to_lowercase();
    match lower.as_str() {
        "mail" | "inbox" | "email" => return Some(Asked::Inbox { unread: false }),
        "mail unread" | "unread mail" | "unread" => return Some(Asked::Inbox { unread: true }),
        _ => {}
    }
    let words = lower.strip_prefix("mail ").map(|_| q[5..].trim()).unwrap_or(q);
    // Short words would match half the mailbox.
    (words.chars().count() >= 3).then(|| Asked::Search(words.to_string()))
}

fn token(id: i64) -> String {
    json!({"api_version": 1, "app": "mail", "op": "open", "message": id}).to_string()
}

fn hit(m: &ListItem) -> Hit {
    let from = if m.from_name.trim().is_empty() { &m.from_addr } else { &m.from_name };
    let subject = if m.subject.trim().is_empty() { "(no subject)" } else { m.subject.trim() };
    Hit {
        id: m.id.to_string(),
        title: if m.unread { format!("● {subject}") } else { subject.to_string() },
        subtitle: format!("{from} · {}", day(m.date)),
        action: Action::Activate { token: token(m.id) },
    }
}

/// The date as YYYY-MM-DD in UTC (the sandbox has no time zone data to
/// trust; a day is precise enough for a search result).
fn day(ts: i64) -> String {
    let days = ts.div_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn answer(request: &Request) -> Result<Value> {
    match request.op.as_str() {
        "search" => {
            let query = request.args["query"].as_str().unwrap_or("");
            let limit = request.args["limit"].as_i64().unwrap_or(10).clamp(1, 10) as usize;
            let Some(what) = asked(query) else { return Ok(json!([])) };
            let db = open()?;
            let items = match what {
                Asked::Inbox { unread } => {
                    let inboxes: Vec<i64> =
                        db.mailboxes(None)?.into_iter().filter(|m| m.role == Some(Role::Inbox)).map(|m| m.id).collect();
                    db.list(&inboxes, if unread { 500 } else { limit }, 0)?
                        .into_iter()
                        .filter(|m| !unread || m.unread)
                        .take(limit)
                        .collect()
                }
                Asked::Search(words) => db.search(&words, limit)?,
            };
            Ok(serde_json::to_value(items.iter().map(hit).collect::<Vec<_>>())?)
        }
        "preview" => {
            let id: i64 = request.args["result_id"].as_str().unwrap_or("").parse().context("missing message")?;
            let db = open()?;
            let Some(m) = db.message(id)? else { bail!("that message is no longer on this computer") };
            let preview: String =
                db.conn().query_row("SELECT preview FROM messages WHERE id = ?1", [id], |r| r.get(0)).unwrap_or_default();
            let from = if m.from_name.is_empty() { m.from_addr.clone() } else { format!("{} <{}>", m.from_name, m.from_addr) };
            let text = [m.subject, from, day(m.date), String::new(), preview].join("\n");
            Ok(json!({ "text": text }))
        }
        other => bail!("unsupported operation {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nyx_mail_core::db::NewMessage;
    use nyx_mail_core::mime::{Parsed, Person};

    #[test]
    fn what_was_asked() {
        assert_eq!(asked("mail"), Some(Asked::Inbox { unread: false }));
        assert_eq!(asked("Mail unread"), Some(Asked::Inbox { unread: true }));
        assert_eq!(asked("mail invoice"), Some(Asked::Search("invoice".into())));
        assert_eq!(asked("ramen place"), Some(Asked::Search("ramen place".into())));
        assert_eq!(asked("ra"), None);
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(1_792_591_200), "2026-10-21");
    }

    #[test]
    fn search_and_preview_read_the_database() {
        let dir = std::env::temp_dir().join(format!("nyx-mail-glimpse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        {
            let db = Db::open(&path).unwrap();
            db.upsert_account("a", "me@example.com", "").unwrap();
            let inbox = db.upsert_mailbox("a", "INBOX", None, Some(Role::Inbox)).unwrap();
            let message = NewMessage {
                uid: 1,
                uidvalidity: 1,
                parsed: Parsed {
                    subject: "Ramen on Friday".into(),
                    from: Some(Person { name: "Sam Lee".into(), email: "sam@example.com".into() }),
                    date: Some(1_792_591_200),
                    text: Some("Shall we book for 12:30?".into()),
                    ..Parsed::default()
                },
                body_text: Some("Shall we book for 12:30?".into()),
                ..NewMessage::default()
            };
            db.put_message("a", inbox, &message).unwrap();
            db.recount(inbox).unwrap();
        }
        // SAFETY: the only test touching this variable.
        unsafe { std::env::set_var("NYX_MAIL_DB", &path) };
        let hits = answer(&request("search", json!({"query": "ramen"}))).unwrap();
        assert_eq!(hits[0]["title"], "● Ramen on Friday");
        assert_eq!(hits[0]["subtitle"], "Sam Lee · 2026-10-21");
        let token: Value = serde_json::from_str(hits[0]["action"]["token"].as_str().unwrap()).unwrap();
        assert_eq!((token["app"].as_str(), token["op"].as_str()), (Some("mail"), Some("open")));
        assert_eq!(answer(&request("search", json!({"query": "mail unread"}))).unwrap().as_array().unwrap().len(), 1);
        let preview = answer(&request("preview", json!({"result_id": hits[0]["id"]}))).unwrap();
        assert!(preview["text"].as_str().unwrap().contains("12:30"));
        assert!(answer(&request("search", json!({"query": "zz"}))).unwrap().as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn request(op: &str, args: Value) -> Request {
        Request { api_version: 1, id: 1, op: op.into(), args }
    }
}
