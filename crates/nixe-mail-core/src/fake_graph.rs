//! Test support (the `testing` feature): a small Microsoft Graph mail server
//! on 127.0.0.1, enough of it for `graph` — folders, the newest-messages
//! listing, delta queries (paged, with removals and an optional date filter),
//! `$value`, flag changes, moves, deletes and sendMail. It runs on a thread of
//! its own, so both tests and a test engine can use it. The token is `secret`.

use crate::mime;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Items per page, small so tests follow `@odata.nextLink`.
const PAGE: usize = 3;

#[derive(Clone, Debug)]
struct Folder {
    id: String,
    name: String,
    parent: Option<String>,
    well_known: Option<&'static str>,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub folder: String,
    pub raw: Vec<u8>,
    pub read: bool,
    pub flagged: bool,
    pub received: String,
    pub conversation: String,
}

#[derive(Default)]
struct State {
    folders: Vec<Folder>,
    messages: Vec<Message>,
    /// (version, folder, message): what changed, for delta queries.
    log: Vec<(u64, String, String)>,
    version: u64,
    next: u64,
    /// Delta links from before this version answer 410 Gone.
    expired_before: u64,
    sent: Vec<Vec<u8>>,
}

impl State {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    fn touch(&mut self, folder: &str, message: &str) {
        self.version += 1;
        self.log.push((self.version, folder.to_string(), message.to_string()));
    }

    fn folder(&self, key: &str) -> Option<&Folder> {
        self.folders.iter().find(|f| f.id == key || f.well_known == Some(key))
    }

    fn folder_named(&self, name: &str) -> String {
        self.folders.iter().find(|f| f.name == name).map(|f| f.id.clone()).unwrap_or_else(|| panic!("no folder {name}"))
    }
}

pub struct FakeGraph {
    /// What `Graph::new` takes: `http://127.0.0.1:PORT/v1.0`.
    pub base: String,
    state: Arc<Mutex<State>>,
}

impl FakeGraph {
    /// Starts with Outlook's usual folders (and an Outbox Mail should skip).
    pub fn start() -> Self {
        let mut state = State::default();
        for (name, well_known) in [
            ("Inbox", "inbox"),
            ("Sent Items", "sentitems"),
            ("Drafts", "drafts"),
            ("Deleted Items", "deleteditems"),
            ("Junk Email", "junkemail"),
            ("Archive", "archive"),
            ("Outbox", "outbox"),
        ] {
            let id = state.id("F");
            state.folders.push(Folder { id, name: name.into(), parent: None, well_known: Some(well_known) });
        }
        let state = Arc::new(Mutex::new(state));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}/v1.0", listener.local_addr().unwrap());
        let (shared, link) = (state.clone(), base.clone());
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                loop {
                    let Ok((stream, _)) = listener.accept().await else { continue };
                    let (state, base) = (shared.clone(), link.clone());
                    tokio::spawn(async move {
                        let _ = serve(stream, &state, &base).await;
                    });
                }
            });
        });
        Self { base, state }
    }

    /// Adds a folder (under another, by name); returns its id.
    pub fn add_folder(&self, name: &str, parent: Option<&str>) -> String {
        let mut s = self.state.lock().unwrap();
        let parent = parent.map(|p| s.folder_named(p));
        let id = s.id("F");
        s.folders.push(Folder { id: id.clone(), name: name.into(), parent, well_known: None });
        id
    }

    /// Puts a message in a folder, received at `received` (`2026-10-24T12:00:00Z`).
    pub fn deliver(&self, folder: &str, raw: &[u8], received: &str) -> String {
        let mut s = self.state.lock().unwrap();
        let folder = s.folder_named(folder);
        let id = s.id("M");
        let subject = mime::parse(raw).map(|p| p.subject).unwrap_or_default();
        let conversation = format!("conv:{}", subject.trim_start_matches("Re: "));
        s.messages.push(Message {
            id: id.clone(),
            folder: folder.clone(),
            raw: raw.to_vec(),
            read: false,
            flagged: false,
            received: received.into(),
            conversation,
        });
        s.touch(&folder, &id);
        id
    }

    /// Marks a message read or unread, as another client would.
    pub fn set_read(&self, id: &str, read: bool) {
        let mut s = self.state.lock().unwrap();
        let Some(m) = s.messages.iter_mut().find(|m| m.id == id) else { return };
        m.read = read;
        let folder = m.folder.clone();
        s.touch(&folder, id);
    }

    /// Deletes a message, as another client would.
    pub fn delete(&self, id: &str) {
        let mut s = self.state.lock().unwrap();
        let Some(pos) = s.messages.iter().position(|m| m.id == id) else { return };
        let m = s.messages.remove(pos);
        s.touch(&m.folder, id);
    }

    /// Makes every delta link given so far answer 410 Gone.
    pub fn expire_delta_links(&self) {
        let mut s = self.state.lock().unwrap();
        s.version += 1;
        s.expired_before = s.version;
    }

    /// The messages in a folder, by name.
    pub fn messages(&self, folder: &str) -> Vec<Message> {
        let s = self.state.lock().unwrap();
        let id = s.folder_named(folder);
        s.messages.iter().filter(|m| m.folder == id).cloned().collect()
    }

    /// The MIME given to sendMail, as decoded.
    pub fn sent(&self) -> Vec<Vec<u8>> {
        self.state.lock().unwrap().sent.clone()
    }
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(v) = text.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn json_of(m: &Message) -> Value {
    let parsed = mime::parse(&m.raw).unwrap_or_default();
    let person = |p: &mime::Person| json!({"emailAddress": {"name": p.name, "address": p.email}});
    // Header lines, unfolded.
    let head = m.raw.split(|b| *b == b'\n').take_while(|l| !l.iter().all(|b| b.is_ascii_whitespace())).collect::<Vec<_>>();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in head {
        let line = String::from_utf8_lossy(line).trim_end().to_string();
        if line.starts_with([' ', '\t']) {
            if let Some(last) = headers.last_mut() {
                last.1.push(' ');
                last.1.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().into(), value.trim().into()));
        }
    }
    json!({
        "id": m.id,
        "parentFolderId": m.folder,
        "conversationId": m.conversation,
        "receivedDateTime": m.received,
        "internetMessageId": parsed.message_id.as_ref().map(|id| format!("<{id}>")),
        "subject": parsed.subject,
        "bodyPreview": parsed.preview(),
        "isRead": m.read,
        "isDraft": false,
        "hasAttachments": parsed.has_attachments(),
        "flag": {"flagStatus": if m.flagged { "flagged" } else { "notFlagged" }},
        "from": parsed.from.as_ref().map(person),
        "toRecipients": parsed.to.iter().map(person).collect::<Vec<_>>(),
        "ccRecipients": parsed.cc.iter().map(person).collect::<Vec<_>>(),
        "internetMessageHeaders": headers.iter().map(|(n, v)| json!({"name": n, "value": v})).collect::<Vec<_>>(),
    })
}

fn folder_json(s: &State, f: &Folder) -> Value {
    let children = s.folders.iter().filter(|c| c.parent.as_deref() == Some(&f.id)).count();
    json!({"id": f.id, "displayName": f.name, "childFolderCount": children})
}

/// One page of `items`, linking to the next with `skip`.
fn page(items: Vec<Value>, query: &HashMap<String, String>, base: &str, path: &str, last: Option<String>) -> Value {
    let skip: usize = query.get("skip").and_then(|s| s.parse().ok()).unwrap_or(0);
    let more = items.len() > skip + PAGE;
    let value: Vec<Value> = items.into_iter().skip(skip).take(PAGE).collect();
    let mut out = json!({ "value": value });
    if more {
        let mut q: Vec<String> =
            query.iter().filter(|(k, _)| *k != "skip").map(|(k, v)| format!("{k}={}", v.replace(' ', "%20"))).collect();
        q.sort();
        q.push(format!("skip={}", skip + PAGE));
        // `path` already starts with /v1.0.
        out["@odata.nextLink"] = json!(format!("{}{path}?{}", base.trim_end_matches("/v1.0"), q.join("&")));
    } else if let Some(link) = last {
        out["@odata.deltaLink"] = json!(link);
    }
    out
}

/// (status, body)
fn route(s: &mut State, method: &str, path: &str, query: &HashMap<String, String>, body: &[u8], base: &str) -> (u16, Value) {
    let parts: Vec<&str> = path.trim_start_matches("/v1.0/me/").split('/').collect();
    let missing = (404, json!({"error": {"code": "ErrorItemNotFound", "message": "Not found"}}));
    match (method, parts.as_slice()) {
        ("GET", ["mailFolders"]) => {
            let top: Vec<Value> = s.folders.iter().filter(|f| f.parent.is_none()).map(|f| folder_json(s, f)).collect();
            (200, page(top, query, base, path, None))
        }
        ("GET", ["mailFolders", key]) => match s.folder(key) {
            Some(f) => (200, folder_json(s, f)),
            None => missing,
        },
        ("GET", ["mailFolders", id, "childFolders"]) => {
            let children: Vec<Value> =
                s.folders.iter().filter(|f| f.parent.as_deref() == Some(*id)).map(|f| folder_json(s, f)).collect();
            (200, page(children, query, base, path, None))
        }
        ("GET", ["mailFolders", id, "messages"]) => {
            let mut list: Vec<&Message> = s.messages.iter().filter(|m| m.folder == *id).collect();
            list.sort_by(|a, b| b.received.cmp(&a.received));
            let top: usize = query.get("$top").and_then(|t| t.parse().ok()).unwrap_or(10);
            (200, json!({"value": list.iter().take(top).map(|m| json_of(m)).collect::<Vec<_>>()}))
        }
        ("GET", ["mailFolders", id, "messages", "delta"]) => {
            let since: Option<u64> = query.get("token").and_then(|t| t.parse().ok());
            if since.is_some_and(|v| v < s.expired_before) {
                return (410, json!({"error": {"code": "SyncStateNotFound", "message": "The sync state is gone"}}));
            }
            let floor = query.get("$filter").and_then(|f| f.split(" ge ").nth(1)).map(str::to_string);
            let items: Vec<Value> = match since {
                None => {
                    let mut list: Vec<&Message> = s
                        .messages
                        .iter()
                        .filter(|m| m.folder == *id && floor.as_ref().is_none_or(|f| m.received >= *f))
                        .collect();
                    list.sort_by(|a, b| b.received.cmp(&a.received));
                    list.into_iter().map(json_of).collect()
                }
                Some(v) => {
                    let mut seen = Vec::new();
                    for (_, folder, message) in s.log.iter().filter(|(at, f, _)| *at > v && f == id) {
                        let _ = folder;
                        if !seen.contains(message) {
                            seen.push(message.clone());
                        }
                    }
                    seen.iter()
                        .map(|mid| match s.messages.iter().find(|m| m.id == *mid && m.folder == *id) {
                            Some(m) => json_of(m),
                            None => json!({"id": mid, "@removed": {"reason": "deleted"}}),
                        })
                        .collect()
                }
            };
            let link = format!("{base}/me/mailFolders/{id}/messages/delta?token={}", s.version);
            (200, page(items, query, base, path, Some(link)))
        }
        ("GET", ["messages", id, "$value"]) => match s.messages.iter().find(|m| m.id == *id) {
            Some(m) => (200, Value::String(String::from_utf8_lossy(&m.raw).into_owned())),
            None => missing,
        },
        ("PATCH", ["messages", id]) => {
            let change: Value = serde_json::from_slice(body).unwrap_or_default();
            let Some(m) = s.messages.iter_mut().find(|m| m.id == *id) else { return missing };
            if let Some(read) = change["isRead"].as_bool() {
                m.read = read;
            }
            if let Some(status) = change["flag"]["flagStatus"].as_str() {
                m.flagged = status == "flagged";
            }
            let (folder, out) = (m.folder.clone(), json_of(m));
            s.touch(&folder, id);
            (200, out)
        }
        ("POST", ["messages", id, "move"]) => {
            let to =
                serde_json::from_slice::<Value>(body).unwrap_or_default()["destinationId"].as_str().unwrap_or("").to_string();
            let Some(dest) = s.folder(&to).map(|f| f.id.clone()) else { return missing };
            let Some(pos) = s.messages.iter().position(|m| m.id == *id) else { return missing };
            let mut m = s.messages.remove(pos);
            s.touch(&m.folder, id);
            m.id = s.id("M");
            m.folder = dest.clone();
            let out = json_of(&m);
            let new_id = m.id.clone();
            s.messages.push(m);
            s.touch(&dest, &new_id);
            (201, out)
        }
        ("POST", ["messages", id, "permanentDelete"]) => {
            let Some(pos) = s.messages.iter().position(|m| m.id == *id) else { return missing };
            let m = s.messages.remove(pos);
            s.touch(&m.folder, id);
            (204, Value::Null)
        }
        ("POST", ["sendMail"]) => {
            let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(body.trim_ascii()) else {
                return (400, json!({"error": {"code": "ErrorMimeContentInvalidBase64String", "message": "Bad MIME"}}));
            };
            s.sent.push(raw.clone());
            let sent = s.folder("sentitems").map(|f| f.id.clone()).unwrap_or_default();
            let id = s.id("M");
            s.messages.push(Message {
                id: id.clone(),
                folder: sent.clone(),
                raw,
                read: true,
                flagged: false,
                received: "2030-01-01T00:00:00Z".into(),
                conversation: "conv:sent".into(),
            });
            s.touch(&sent, &id);
            (202, Value::Null)
        }
        _ => (400, json!({"error": {"code": "BadRequest", "message": format!("{method} {path} isn't here")}})),
    }
}

async fn serve(mut stream: tokio::net::TcpStream, state: &Mutex<State>, base: &str) -> std::io::Result<()> {
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    let head_end = loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        data.extend_from_slice(&buf[..n]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.lines();
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("").to_string());
    let mut length = 0;
    let mut authorized = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => authorized = value.trim() == "Bearer secret",
                _ => {}
            }
        }
    }
    while data.len() < head_end + length {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buf[..n]);
    }
    let body = &data[head_end..(head_end + length).min(data.len())];
    let (path, query_text) = target.split_once('?').unwrap_or((&target, ""));
    let path = decode(path);
    let query: HashMap<String, String> = query_text
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let (status, value) = if authorized {
        route(&mut state.lock().unwrap(), &method, &path, &query, body, base)
    } else {
        (401, json!({"error": {"code": "InvalidAuthenticationToken", "message": "Access token is empty."}}))
    };
    let (content_type, text) = match &value {
        Value::String(raw) => ("message/rfc822", raw.clone()),
        Value::Null => ("application/json", String::new()),
        other => ("application/json", other.to_string()),
    };
    let reply = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.shutdown().await
}
