//! Microsoft 365 and Outlook.com accounts, over Microsoft Graph.
//!
//! The same rows as IMAP mail: each Graph folder is a mailbox (children named
//! `Parent/Child`), each message gets a UID here (UIDVALIDITY 1), and
//! Outlook's conversation id threads them. Per folder, the delta query brings
//! new and changed messages and the ones gone since the last sync; the first
//! sync starts from the newest `Limits::window`. Bodies come as MIME
//! (`$value`). Changes made offline wait in the `ops` table, as for IMAP.
//! The access token comes from GOA for each sync and is never kept or logged.

use crate::db::{Db, NewMessage, Op, Role};
use crate::flags;
use crate::mime::{self, Attachment, Person};
use crate::net::NetError;
use crate::store::Store;
use crate::sync::{Limits, Report};
use base64::Engine;
use serde_json::{Value, json};

pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";

/// Graph accounts use one UIDVALIDITY; a folder that changes starts over instead.
pub const VALIDITY: u32 = 1;

const SELECT: &str = "id,internetMessageId,conversationId,receivedDateTime,from,toRecipients,ccRecipients,subject,\
bodyPreview,isRead,flag,hasAttachments,isDraft,internetMessageHeaders";

/// Well-known folders and what they're for.
const ROLES: &[(&str, Role)] = &[
    ("inbox", Role::Inbox),
    ("sentitems", Role::Sent),
    ("drafts", Role::Drafts),
    ("deleteditems", Role::Trash),
    ("junkemail", Role::Junk),
    ("archive", Role::Archive),
];

/// Folders Outlook keeps for itself.
const SKIP: &[&str] = &["outbox", "syncissues", "conversationhistory"];

/// A signed-in connection to Graph. Not `Debug`: it holds the token.
pub struct Graph {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Graph {
    /// `base` is `GRAPH`, or a test server.
    pub fn new(base: &str, token: String) -> Result<Self, NetError> {
        let http = reqwest::Client::builder()
            .user_agent("nixe-mail")
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| NetError::Protocol(e.to_string()))?;
        Ok(Self { http, base: base.trim_end_matches('/').to_string(), token })
    }

    /// A path under the base, or a link Graph gave (only ever to the base: the
    /// token goes nowhere else).
    fn url(&self, path_or_link: &str) -> Result<String, NetError> {
        if path_or_link.starts_with('/') {
            Ok(format!("{}{path_or_link}", self.base))
        } else if path_or_link.starts_with(&format!("{}/", self.base)) {
            Ok(path_or_link.to_string())
        } else {
            Err(NetError::Protocol("Microsoft 365 sent a link to somewhere else".into()))
        }
    }

    async fn call(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, NetError> {
        let response = request.bearer_auth(&self.token).send().await.map_err(request_error)?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| status.to_string());
        Err(match status.as_u16() {
            401 => NetError::SignIn(message),
            410 => NetError::Refused(format!("gone: {message}")),
            408 | 429 | 500..=599 => NetError::Protocol(format!("Microsoft 365 is busy: {message}")),
            _ => NetError::Refused(message),
        })
    }

    async fn get(&self, path: &str) -> Result<Value, NetError> {
        let request = self.http.get(self.url(path)?).header("Prefer", "odata.maxpagesize=100");
        self.call(request).await?.json().await.map_err(request_error)
    }

    /// Every item of a list, following its pages.
    async fn all(&self, path: &str) -> Result<Vec<Value>, NetError> {
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        while let Some(link) = next {
            let page = self.get(&link).await?;
            out.extend(page["value"].as_array().cloned().unwrap_or_default());
            next = page["@odata.nextLink"].as_str().map(str::to_string);
        }
        Ok(out)
    }

    async fn post(&self, path: &str, body: Value) -> Result<(), NetError> {
        self.call(self.http.post(self.url(path)?).json(&body)).await.map(drop)
    }

    async fn patch(&self, path: &str, body: Value) -> Result<(), NetError> {
        self.call(self.http.patch(self.url(path)?).json(&body)).await.map(drop)
    }

    /// The whole message, as MIME.
    pub async fn raw(&self, item: &str) -> Result<Vec<u8>, NetError> {
        let response = self.call(self.http.get(self.url(&format!("/me/messages/{item}/$value"))?)).await?;
        Ok(response.bytes().await.map_err(request_error)?.to_vec())
    }

    /// Sends a built message. Graph takes the recipients from its headers, so
    /// Bcc (left out of what's built) goes back in for it; Outlook keeps it off
    /// what others get and files the message in Sent Items.
    pub async fn send_mime(&self, raw: &[u8], bcc: &[String]) -> Result<(), NetError> {
        let mut message = Vec::with_capacity(raw.len() + 64);
        if !bcc.is_empty() {
            message.extend_from_slice(format!("Bcc: {}\r\n", bcc.join(", ")).as_bytes());
        }
        message.extend_from_slice(raw);
        let body = base64::engine::general_purpose::STANDARD.encode(&message);
        let request = self.http.post(self.url("/me/sendMail")?).header("Content-Type", "text/plain").body(body);
        self.call(request).await.map(drop)
    }
}

fn request_error(e: reqwest::Error) -> NetError {
    if e.is_connect() || e.is_timeout() || e.is_request() {
        NetError::Unreachable(e.to_string())
    } else {
        NetError::Protocol(e.to_string())
    }
}

/// Bcc recipients of a queued send: everyone it goes to who isn't in To or Cc.
pub fn bcc_of(raw: &[u8], everyone: &[String]) -> Vec<String> {
    let parsed = mime::parse(raw).unwrap_or_default();
    let shown: Vec<String> = parsed.to.iter().chain(&parsed.cc).map(|p| p.email.to_lowercase()).collect();
    everyone.iter().filter(|a| !shown.contains(&a.to_lowercase())).cloned().collect()
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn db_err(e: rusqlite::Error) -> NetError {
    NetError::Protocol(format!("Mail's database: {e}"))
}

/// Unix seconds from Graph's `2026-10-24T23:47:31Z` (always UTC).
pub fn timestamp(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

fn person(v: &Value) -> Option<Person> {
    let email = v["emailAddress"]["address"].as_str()?.trim().to_string();
    (!email.is_empty()).then(|| Person { name: v["emailAddress"]["name"].as_str().unwrap_or("").trim().to_string(), email })
}

fn people(v: &Value) -> Vec<Person> {
    v.as_array().map(|a| a.iter().filter_map(person).collect()).unwrap_or_default()
}

/// Outlook's conversation id as a thread number.
fn conversation(id: &str) -> u64 {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(id.as_bytes());
    u64::from_be_bytes(hash[..8].try_into().unwrap_or_default())
}

/// A message's headers and Graph's own fields, as a header-only message.
pub fn message_from(item: &Value, uid: u32) -> NewMessage {
    // The headers as the server has them, parsed like an IMAP header fetch;
    // drafts have none, so Graph's fields fill in.
    let mut header = String::new();
    for h in item["internetMessageHeaders"].as_array().into_iter().flatten() {
        if let (Some(name), Some(value)) = (h["name"].as_str(), h["value"].as_str()) {
            header.push_str(&format!("{name}: {}\r\n", value.replace(['\r', '\n'], " ")));
        }
    }
    header.push_str("\r\n");
    let mut parsed = mime::parse(header.as_bytes()).unwrap_or_default();
    let received = item["receivedDateTime"].as_str().and_then(timestamp);
    if parsed.subject.is_empty() {
        parsed.subject = item["subject"].as_str().unwrap_or("").to_string();
    }
    if parsed.from.is_none() {
        parsed.from = person(&item["from"]);
    }
    if parsed.to.is_empty() && parsed.cc.is_empty() {
        parsed.to = people(&item["toRecipients"]);
        parsed.cc = people(&item["ccRecipients"]);
    }
    if parsed.message_id.is_none() {
        parsed.message_id = item["internetMessageId"].as_str().map(|s| s.trim_matches(['<', '>']).to_string());
    }
    parsed.date = parsed.date.or(received);
    // Until the body is here: Graph's preview, and a stand-in so the list
    // shows the paper clip (the real ones come with the body).
    parsed.text = item["bodyPreview"].as_str().map(str::to_string);
    if item["hasAttachments"].as_bool() == Some(true) {
        parsed.attachments.push(Attachment {
            index: 0,
            name: String::new(),
            mime: String::new(),
            size: 0,
            content_id: None,
            inline: false,
        });
    }
    let mut bits = 0;
    if item["isRead"].as_bool() == Some(true) {
        bits |= flags::SEEN;
    }
    if item["flag"]["flagStatus"].as_str() == Some("flagged") {
        bits |= flags::FLAGGED;
    }
    if item["isDraft"].as_bool() == Some(true) {
        bits |= flags::DRAFT;
    }
    NewMessage {
        uid,
        uidvalidity: VALIDITY,
        flags: bits,
        gm_thrid: item["conversationId"].as_str().map(conversation),
        internal_date: received,
        parsed,
        ..NewMessage::default()
    }
}

/// One folder as listed: its Graph id, the name kept here and what it's for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub id: String,
    pub name: String,
    pub role: Option<Role>,
}

pub struct GraphSync<'a> {
    graph: &'a Graph,
    db: &'a mut Db,
    store: &'a Store,
    account: &'a str,
    limits: Limits,
}

impl<'a> GraphSync<'a> {
    pub fn new(graph: &'a Graph, db: &'a mut Db, store: &'a Store, account: &'a str, limits: Limits) -> Self {
        Self { graph, db, store, account, limits }
    }

    pub async fn run(&mut self) -> Result<Report, NetError> {
        let mut report = Report::default();
        self.send_ops(&mut report).await?;
        let folders = self.folders().await?;
        let mut seen = Vec::new();
        for folder in &folders {
            let id = self.db.upsert_mailbox(self.account, &folder.name, Some("/"), folder.role).map_err(db_err)?;
            self.db.set_graph_folder(id, &folder.id).map_err(db_err)?;
            seen.push(folder.name.clone());
            self.mailbox(id, folder, &mut report).await?;
            report.mailboxes += 1;
        }
        self.db.retain_mailboxes(self.account, &seen).map_err(db_err)?;
        let _ = self.db.set_account_state(self.account, Some(now()), None);
        Ok(report)
    }

    /// Every folder, children under their parents, without Outlook's own.
    pub async fn folders(&self) -> Result<Vec<Folder>, NetError> {
        let mut roles = Vec::new();
        for (name, role) in ROLES {
            if let Ok(v) = self.graph.get(&format!("/me/mailFolders/{name}?$select=id")).await
                && let Some(id) = v["id"].as_str()
            {
                roles.push((id.to_string(), *role));
            }
        }
        let mut skip = Vec::new();
        for name in SKIP {
            if let Ok(v) = self.graph.get(&format!("/me/mailFolders/{name}?$select=id")).await
                && let Some(id) = v["id"].as_str()
            {
                skip.push(id.to_string());
            }
        }
        let mut out = Vec::new();
        let mut pending = vec![(String::from("/me/mailFolders"), String::new())];
        while let Some((path, parent)) = pending.pop() {
            for f in self.graph.all(&format!("{path}?$select=id,displayName,childFolderCount&$top=100")).await? {
                let (Some(id), Some(name)) = (f["id"].as_str(), f["displayName"].as_str()) else { continue };
                if skip.iter().any(|s| s == id) {
                    continue;
                }
                let name = if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") };
                if f["childFolderCount"].as_i64().unwrap_or(0) > 0 {
                    pending.push((format!("/me/mailFolders/{id}/childFolders"), name.clone()));
                }
                let role = roles.iter().find(|(r, _)| r == id).map(|(_, role)| *role);
                out.push(Folder { id: id.to_string(), name, role });
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn mailbox(&mut self, id: i64, folder: &Folder, report: &mut Report) -> Result<(), NetError> {
        let (_, delta) = self.db.graph_folder(id).map_err(db_err)?.unwrap_or_default();
        let first = delta.is_none();
        let start = match delta {
            Some(link) => link,
            None => self.first_delta(&folder.id).await?,
        };
        let mut next = Some(start);
        let mut delta_link = None;
        while let Some(link) = next.take() {
            let page = match self.graph.get(&link).await {
                Ok(page) => page,
                // The delta link expired: start this folder over next time.
                Err(NetError::Refused(m)) if m.starts_with("gone") => {
                    self.db.set_graph_folder(id, "").map_err(db_err)?;
                    self.db.set_graph_folder(id, &folder.id).map_err(db_err)?;
                    return Ok(());
                }
                Err(e) => return Err(e),
            };
            for item in page["value"].as_array().into_iter().flatten() {
                let Some(graph_id) = item["id"].as_str() else { continue };
                if !item["@removed"].is_null() {
                    report.removed += self.db.forget_graph_item(id, graph_id).map_err(db_err)?;
                    continue;
                }
                let (uid, new) = self.db.graph_uid(id, graph_id).map_err(db_err)?;
                let message = message_from(item, uid);
                let row = self.db.put_message(self.account, id, &message).map_err(db_err)?;
                if new {
                    report.new += 1;
                    if !first && folder.role == Some(Role::Inbox) && message.flags & flags::SEEN == 0 {
                        report.new_in_inbox.push(row);
                    }
                } else {
                    report.changed += 1;
                }
            }
            next = page["@odata.nextLink"].as_str().map(str::to_string);
            delta_link = page["@odata.deltaLink"].as_str().map(str::to_string).or(delta_link);
        }
        self.db.set_graph_delta(id, delta_link.as_deref()).map_err(db_err)?;
        self.db.set_mailbox_state(id, Some(VALIDITY), None, None).map_err(db_err)?;

        // Bodies of recent messages, for reading offline.
        let since = now() - self.limits.body_days * 86_400;
        let missing = self.db.missing_bodies(id, since, self.limits.bodies_per_sync).map_err(db_err)?;
        for (uid, _) in missing {
            match self.fetch_body(id, uid).await {
                Ok(true) => report.bodies += 1,
                Ok(false) | Err(NetError::Refused(_)) => {}
                Err(e) => return Err(e),
            }
        }
        self.db.recount(id).map_err(db_err)?;
        Ok(())
    }

    /// Where a folder's first delta starts: its newest `window` messages.
    async fn first_delta(&self, folder: &str) -> Result<String, NetError> {
        let window = self.limits.window.clamp(1, 1000);
        let newest = self
            .graph
            .get(&format!(
                "/me/mailFolders/{folder}/messages?$select=receivedDateTime&$orderby=receivedDateTime%20desc&$top={window}"
            ))
            .await?;
        let listed = newest["value"].as_array().cloned().unwrap_or_default();
        let mut path = format!("/me/mailFolders/{folder}/messages/delta?$select={SELECT}");
        if listed.len() >= window
            && let Some(oldest) = listed.last().and_then(|m| m["receivedDateTime"].as_str())
        {
            path.push_str(&format!("&$filter=receivedDateTime%20ge%20{oldest}&$orderby=receivedDateTime%20desc"));
        }
        Ok(path)
    }

    /// Fetches one whole message into the store; false when it's gone.
    pub async fn fetch_body(&mut self, mailbox: i64, uid: u32) -> Result<bool, NetError> {
        let Some(item) = self.db.graph_item(mailbox, uid).map_err(db_err)? else { return Ok(false) };
        let raw = self.graph.raw(&item).await?;
        let Some(parsed) = mime::parse(&raw) else { return Ok(false) };
        let name = self.store.put(&raw).map_err(|e| NetError::Protocol(format!("Couldn't save a message: {e}")))?;
        let flags = self.db.uids(mailbox, VALIDITY).map_err(db_err)?.into_iter().find(|(u, _)| *u == uid).map(|(_, f)| f);
        let body_text = parsed.plain_text();
        let message = NewMessage {
            uid,
            uidvalidity: VALIDITY,
            flags: flags.unwrap_or(0),
            size: raw.len() as i64,
            parsed,
            raw: Some(name),
            body_text: Some(body_text),
            ..NewMessage::default()
        };
        self.db.put_message(self.account, mailbox, &message).map_err(db_err)?;
        Ok(true)
    }

    /// Sends queued changes for this account (sends go through the engine).
    pub async fn send_ops(&mut self, report: &mut Report) -> Result<(), NetError> {
        let due = self.db.due_ops(self.account, now()).map_err(db_err)?;
        for op in due.into_iter().filter(|o| o.kind != "send") {
            match self.apply(&op).await {
                Ok(()) => {
                    self.db.op_done(op.id).map_err(db_err)?;
                    report.ops_sent += 1;
                }
                Err(e) if e.transient() && op.attempts < 8 => {
                    let wait = (30i64 << op.attempts.min(7)).min(3600);
                    self.db.op_failed(op.id, &e.to_string(), now() + wait).map_err(db_err)?;
                    report.ops_failed += 1;
                    if matches!(e, NetError::Unreachable(_)) {
                        return Err(e);
                    }
                }
                Err(NetError::SignIn(m)) => {
                    self.db.op_failed(op.id, &m, now() + 300).map_err(db_err)?;
                    return Err(NetError::SignIn(m));
                }
                Err(e) => {
                    self.db.op_done(op.id).map_err(db_err)?;
                    let _ = self.db.set_account_state(self.account, None, Some(&format!("A change couldn't be saved: {e}")));
                    report.ops_failed += 1;
                }
            }
        }
        Ok(())
    }

    async fn apply(&mut self, op: &Op) -> Result<(), NetError> {
        let p = &op.payload;
        let mailbox = p["mailbox"].as_i64().unwrap_or(0);
        let uid = p["uid"].as_u64().unwrap_or(0) as u32;
        let item = || -> Result<String, NetError> {
            self.db
                .graph_item(mailbox, uid)
                .map_err(db_err)?
                .filter(|_| p["uidvalidity"].as_u64() == Some(VALIDITY as u64))
                .ok_or_else(|| NetError::Refused("the message is gone from Microsoft 365".into()))
        };
        match op.kind.as_str() {
            "flags" => {
                let item = item()?;
                let (add, remove) = (p["add"].as_i64().unwrap_or(0), p["remove"].as_i64().unwrap_or(0));
                let mut change = serde_json::Map::new();
                if (add | remove) & flags::SEEN != 0 {
                    change.insert("isRead".into(), json!(add & flags::SEEN != 0));
                }
                if (add | remove) & flags::FLAGGED != 0 {
                    let status = if add & flags::FLAGGED != 0 { "flagged" } else { "notFlagged" };
                    change.insert("flag".into(), json!({ "flagStatus": status }));
                }
                if change.is_empty() {
                    return Ok(());
                }
                self.graph.patch(&format!("/me/messages/{item}"), Value::Object(change)).await
            }
            "move" => {
                let item = item()?;
                let to = p["to"].as_str().unwrap_or_default();
                let folder = self
                    .db
                    .graph_folder_named(self.account, to)
                    .map_err(db_err)?
                    .ok_or_else(|| NetError::Refused(format!("there's no folder “{to}”")))?;
                self.graph.post(&format!("/me/messages/{item}/move"), json!({ "destinationId": folder })).await
            }
            "expunge" => {
                let item = item()?;
                self.graph.post(&format!("/me/messages/{item}/permanentDelete"), json!({})).await
            }
            // Outlook files sent mail itself; there's nothing else to save.
            "append" => Ok(()),
            other => Err(NetError::Refused(format!("unknown change “{other}”"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(timestamp("2026-10-24T23:47:31Z"), Some(1_792_885_651));
        assert_eq!(timestamp("2024-02-29T12:00:00.000Z"), Some(1_709_208_000));
        assert_eq!(timestamp("yesterday"), None);
    }

    #[test]
    fn messages_from_graph() {
        let item = json!({
            "id": "AAMk1",
            "conversationId": "conv-1",
            "receivedDateTime": "2026-10-24T12:00:00Z",
            "subject": "Lunch?",
            "bodyPreview": "Are you free on Friday",
            "isRead": false,
            "flag": {"flagStatus": "flagged"},
            "hasAttachments": true,
            "isDraft": false,
            "from": {"emailAddress": {"name": "Sam", "address": "sam@example.test"}},
            "toRecipients": [{"emailAddress": {"name": "", "address": "me@example.test"}}],
            "internetMessageHeaders": [
                {"name": "Message-ID", "value": "<abc@example.test>"},
                {"name": "Subject", "value": "=?utf-8?q?Lunch=3F?="},
                {"name": "List-Id", "value": "<news.example.test>"},
            ],
        });
        let m = message_from(&item, 7);
        assert_eq!((m.uid, m.uidvalidity), (7, VALIDITY));
        assert_eq!(m.flags, flags::FLAGGED);
        assert_eq!(m.parsed.subject, "Lunch?");
        assert_eq!(m.parsed.message_id.as_deref(), Some("abc@example.test"));
        assert_eq!(m.parsed.list_id.as_deref(), Some("<news.example.test>"));
        assert_eq!(m.parsed.from.as_ref().map(|p| p.email.as_str()), Some("sam@example.test"));
        assert_eq!(m.parsed.to.len(), 1);
        assert_eq!(m.parsed.date, Some(1_792_843_200));
        assert_eq!(m.parsed.preview(), "Are you free on Friday");
        assert!(m.parsed.has_attachments());
        assert_eq!(m.gm_thrid, Some(conversation("conv-1")));
        assert_ne!(conversation("conv-1"), conversation("conv-2"));
    }

    #[test]
    fn bcc_is_who_the_headers_leave_out() {
        let raw = b"From: me@example.test\r\nTo: Sam <sam@example.test>\r\nCc: kim@example.test\r\nSubject: x\r\n\r\nhi\r\n";
        let everyone = ["sam@example.test".to_string(), "KIM@example.test".into(), "secret@example.test".into()];
        assert_eq!(bcc_of(raw, &everyone), vec!["secret@example.test".to_string()]);
    }

    #[test]
    fn links_stay_with_graph() {
        let g = Graph::new(GRAPH, "t".into()).unwrap();
        assert!(g.url("/me").unwrap().starts_with("https://graph.microsoft.com/v1.0/me"));
        assert!(g.url("https://graph.microsoft.com/v1.0/me/mailFolders/x/messages/delta?$deltatoken=1").is_ok());
        assert!(g.url("https://evil.example/v1.0/me").is_err());
        assert!(g.url("https://graph.microsoft.com/v1.0.evil.example/me").is_err());
    }
}
