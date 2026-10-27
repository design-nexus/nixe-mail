//! Mail's tools on the control interface (`nyx_core::control`), for Nyx
//! Assistant: search and read mail, list the inbox, show a conversation, and
//! start a message for the user to check and send. Nothing here sends mail;
//! sending is always the user's own press of Send. Message text is returned as
//! untrusted content.

use crate::compose::{self, Start};
use crate::data;
use crate::window;
use nyx_core::control::{Reply, text};
use nyx_mail_core::db::{ListItem, Role};
use nyx_mail_core::flags;
use nyx_mail_core::send::{self, Draft};
use serde_json::{Value, json};

const MAX: usize = 50;
/// A message's text is cut here (a model reads it, not a person).
const MAX_TEXT: usize = 20_000;

pub fn tools() -> Value {
    json!([
        {"name": "mail.search", "description": "Search every account's mail by words, people or subjects; newest first.", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["query"]}},
        {"name": "mail.inbox", "description": "The newest conversations in every inbox (only unread ones with unread=true).", "inputSchema": {"type": "object", "properties": {"unread": {"type": "boolean"}, "limit": {"type": "integer"}}}},
        {"name": "mail.read", "description": "Read a message (by id from mail.search or mail.inbox): sender, recipients, date, subject, text and attachment names.", "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}},
        {"name": "mail.open", "description": "Show a message's conversation in the Mail window.", "inputSchema": {"type": "object", "properties": {"id": {"type": "integer"}}, "required": ["id"]}},
        {"name": "mail.draft", "description": "Open a new message, filled in, for the user to check and send themselves (it is not sent). Give reply_to (a message id) to answer a message.", "inputSchema": {"type": "object", "properties": {"to": {"type": "string", "description": "Addresses, comma-separated: \"Sam Lee <sam@example.com>, ann@example.com\""}, "cc": {"type": "string"}, "subject": {"type": "string"}, "body": {"type": "string"}, "reply_to": {"type": "integer"}}}},
    ])
}

fn item(m: &ListItem) -> Value {
    json!({
        "id": m.id, "from": m.from_name, "from_address": m.from_addr, "subject": m.subject,
        "date": window::full_date(m.date), "preview": m.preview, "unread": m.unread, "messages": m.count,
        "starred": m.flags & flags::FLAGGED != 0, "attachments": m.has_attachments,
    })
}

fn limit(args: &Value) -> usize {
    args["limit"].as_u64().map(|n| n as usize).unwrap_or(20).clamp(1, MAX)
}

pub fn handle(name: &str, args: Value, reply: Reply) {
    reply(answer(name, &args));
}

fn answer(name: &str, args: &Value) -> Result<Value, String> {
    match name {
        "mail.search" => {
            let query = text(args, "query")?;
            let found: Vec<Value> = data::search(&query, limit(args)).iter().map(item).collect();
            Ok(json!({"messages": found, "untrusted_content": true}))
        }
        "mail.inbox" => {
            let inboxes: Vec<i64> = data::mailboxes().into_iter().filter(|m| m.role == Some(Role::Inbox)).map(|m| m.id).collect();
            let unread = args["unread"].as_bool().unwrap_or(false);
            let found: Vec<Value> = data::list(&inboxes, if unread { 500 } else { limit(args) })
                .iter()
                .filter(|m| !unread || m.unread)
                .take(limit(args))
                .map(item)
                .collect();
            Ok(json!({"messages": found, "untrusted_content": true}))
        }
        "mail.read" => {
            let id = args["id"].as_i64().ok_or("id is required")?;
            let row = data::message(id).ok_or("There's no message with that id.")?;
            let parsed = data::parsed(&row).map(|(_, p)| p);
            let mut body = parsed
                .as_ref()
                .map(|p| p.plain_text())
                .unwrap_or_else(|| "(The message's text isn't downloaded yet; mail.open shows it.)".into());
            if body.len() > MAX_TEXT {
                let mut cut = MAX_TEXT;
                while !body.is_char_boundary(cut) {
                    cut -= 1;
                }
                body.truncate(cut);
                body.push_str("\n[…cut]");
            }
            let attachments: Vec<String> =
                parsed.as_ref().map(|p| p.attachments.iter().map(|a| a.name.clone()).collect()).unwrap_or_default();
            Ok(json!({
                "id": row.id, "from": row.from_name, "from_address": row.from_addr,
                "to": row.recipients.iter().map(|(n, e)| if n.is_empty() { e.clone() } else { format!("{n} <{e}>") }).collect::<Vec<_>>(),
                "date": window::full_date(row.date), "subject": row.subject, "text": body, "attachments": attachments,
                "untrusted_content": true,
            }))
        }
        "mail.open" => {
            let id = args["id"].as_i64().ok_or("id is required")?;
            if window::open_message(id) { Ok(json!({"shown": id})) } else { Err("There's no message with that id.".into()) }
        }
        "mail.draft" => {
            let people = |key: &str| -> Result<_, String> {
                send::parse_people(args[key].as_str().unwrap_or(""))
                    .map_err(|bad| format!("“{bad}” in {key} isn't an email address"))
            };
            let draft = Draft {
                to: people("to")?,
                cc: people("cc")?,
                subject: args["subject"].as_str().unwrap_or("").chars().take(500).collect(),
                body: args["body"].as_str().unwrap_or("").chars().take(MAX_TEXT).collect(),
                ..Draft::default()
            };
            let start = match args["reply_to"].as_i64() {
                Some(id) => Start::ReplyWith { message: id, draft: Box::new(draft) },
                None => Start::Prepared(Box::new(draft)),
            };
            compose::open(start);
            Ok(json!({"opened": true, "sent": false, "note": "The message is open for the user to check and send."}))
        }
        other => Err(format!("unknown tool {other}")),
    }
}
