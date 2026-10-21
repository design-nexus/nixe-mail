//! Bringing an account up to date over IMAP, and sending queued changes.
//!
//! Per mailbox: a changed UIDVALIDITY drops what was kept; new messages come in
//! as headers (newest first, up to a window), flags are refreshed (only what
//! changed when the server offers CONDSTORE), messages gone from the server
//! are dropped, and the bodies of recent messages are fetched for offline
//! reading. Changes made while offline wait in the `ops` table and are sent
//! first; they're deltas (add or remove a flag, move), so the server's own
//! changes in the meantime aren't overwritten.

use crate::db::{Db, NewMessage, Op, Role};
use crate::net::{NetError, Session};
use crate::store::Store;
use crate::{flags, mime};
use async_imap::types::{Fetch, Flag, NameAttribute};
use futures::TryStreamExt;
use serde_json::json;
use std::collections::HashSet;

/// How much to keep when an account is new.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// The newest this many messages per mailbox, as headers.
    pub window: usize,
    /// Fetch whole messages this many days back, for reading offline.
    pub body_days: i64,
    /// At most this many bodies per mailbox per sync.
    pub bodies_per_sync: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { window: 500, body_days: 30, bodies_per_sync: 200 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub mailboxes: usize,
    pub new: usize,
    pub changed: usize,
    pub removed: usize,
    pub bodies: usize,
    pub ops_sent: usize,
    pub ops_failed: usize,
    /// Messages that arrived in an inbox (for notifications).
    pub new_in_inbox: Vec<i64>,
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn flag_bits(fetch: &Fetch) -> i64 {
    fetch.flags().fold(0, |bits, f| {
        bits | match f {
            Flag::Seen => flags::SEEN,
            Flag::Flagged => flags::FLAGGED,
            Flag::Answered => flags::ANSWERED,
            Flag::Draft => flags::DRAFT,
            Flag::Deleted => flags::DELETED,
            _ => 0,
        }
    })
}

fn special_use(attrs: &[NameAttribute<'_>]) -> (bool, Vec<&'static str>) {
    let mut selectable = true;
    let mut out = Vec::new();
    for a in attrs {
        match a {
            NameAttribute::NoSelect => selectable = false,
            NameAttribute::All => out.push("\\All"),
            NameAttribute::Archive => out.push("\\Archive"),
            NameAttribute::Drafts => out.push("\\Drafts"),
            NameAttribute::Flagged => out.push("\\Flagged"),
            NameAttribute::Junk => out.push("\\Junk"),
            NameAttribute::Sent => out.push("\\Sent"),
            NameAttribute::Trash => out.push("\\Trash"),
            NameAttribute::Extension(e) if e.eq_ignore_ascii_case("\\NonExistent") => selectable = false,
            _ => {}
        }
    }
    (selectable, out)
}

/// A UID set like `1:5,9,12:14`.
pub fn uid_set(uids: &[u32]) -> String {
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        parts.push(if start == end { start.to_string() } else { format!("{start}:{end}") });
        i += 1;
    }
    parts.join(",")
}

pub struct Sync<'a> {
    pub session: &'a mut Session,
    pub db: &'a mut Db,
    pub store: &'a Store,
    pub account: &'a str,
    pub limits: Limits,
    capabilities: HashSet<String>,
}

impl<'a> Sync<'a> {
    pub async fn new(session: &'a mut Session, db: &'a mut Db, store: &'a Store, account: &'a str, limits: Limits) -> Result<Self, NetError> {
        let caps = session.capabilities().await?;
        let capabilities = caps.iter().map(|c| format!("{c:?}").to_ascii_uppercase()).collect();
        Ok(Self { session, db, store, account, limits, capabilities })
    }

    fn has(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c.contains(cap))
    }

    fn gmail(&self) -> bool {
        self.has("X-GM-EXT-1")
    }

    /// Sends queued changes, then brings every mailbox up to date.
    pub async fn run(&mut self) -> Result<Report, NetError> {
        let mut report = Report::default();
        self.send_ops(&mut report).await?;
        let mailboxes = self.list().await?;
        for (id, name, role) in mailboxes {
            // Gmail's All Mail and Starred repeat other mailboxes' messages.
            if matches!(role, Some(Role::All) | Some(Role::Flagged)) {
                continue;
            }
            self.mailbox(id, &name, role, &mut report).await?;
            report.mailboxes += 1;
        }
        let _ = self.db.set_account_state(self.account, Some(now()), None);
        Ok(report)
    }

    async fn list(&mut self) -> Result<Vec<(i64, String, Option<Role>)>, NetError> {
        let names: Vec<_> = self.session.list(Some(""), Some("*")).await?.try_collect().await?;
        let mut out = Vec::new();
        let mut seen = Vec::new();
        for n in &names {
            let (selectable, uses) = special_use(n.attributes());
            seen.push(n.name().to_string());
            if !selectable {
                continue;
            }
            let role = Role::detect(n.name(), uses.iter().copied());
            let id = self.db.upsert_mailbox(self.account, n.name(), n.delimiter(), role).map_err(db_err)?;
            out.push((id, n.name().to_string(), role));
        }
        self.db.retain_mailboxes(self.account, &seen).map_err(db_err)?;
        Ok(out)
    }

    async fn mailbox(&mut self, id: i64, name: &str, role: Option<Role>, report: &mut Report) -> Result<(), NetError> {
        let condstore = self.has("CONDSTORE");
        let selected = if condstore { self.session.select_condstore(name).await? } else { self.session.select(name).await? };
        let known = self.db.mailbox(id).map_err(db_err)?.unwrap_or_else(|| unreachable_mailbox(id));
        let Some(validity) = selected.uid_validity else { return Ok(()) };
        if known.uidvalidity.is_some_and(|v| v != validity) {
            self.db.clear_mailbox(id).map_err(db_err)?;
        }
        let mut local: Vec<(u32, i64)> = self.db.uids(id, validity).map_err(db_err)?;
        let highest_local = local.iter().map(|(u, _)| *u).max();

        // Messages gone from the server, within the range we keep.
        if let Some(lowest) = local.iter().map(|(u, _)| *u).min() {
            let on_server = self.session.uid_search(format!("UID {lowest}:*")).await?;
            let gone: Vec<u32> = local.iter().map(|(u, _)| *u).filter(|u| !on_server.contains(u)).collect();
            if !gone.is_empty() {
                report.removed += self.db.remove_uids(id, validity, &gone).map_err(db_err)?;
                local.retain(|(u, _)| !gone.contains(u));
            }
        }

        // New messages: above the highest we have, or the newest window at first.
        let new_uids: Vec<u32> = match highest_local {
            Some(h) => self.session.uid_search(format!("UID {}:*", h + 1)).await?.into_iter().filter(|u| *u > h).collect(),
            None if selected.exists == 0 => Vec::new(),
            None => {
                let mut all: Vec<u32> = self.session.uid_search("ALL").await?.into_iter().collect();
                all.sort_unstable();
                let skip = all.len().saturating_sub(self.limits.window);
                all.split_off(skip)
            }
        };
        if !new_uids.is_empty() {
            let mut query = String::from("(UID FLAGS INTERNALDATE RFC822.SIZE BODY.PEEK[HEADER]");
            if self.gmail() {
                query.push_str(" X-GM-LABELS X-GM-MSGID");
            }
            if condstore {
                query.push_str(" MODSEQ");
            }
            query.push(')');
            for chunk in new_uids.chunks(200) {
                let fetched: Vec<Fetch> = self.session.uid_fetch(uid_set(chunk), &query).await?.try_collect().await?;
                for f in fetched {
                    let Some(uid) = f.uid else { continue };
                    let parsed = f.header().and_then(mime::parse).unwrap_or_default();
                    let message = NewMessage {
                        uid,
                        uidvalidity: validity,
                        modseq: f.modseq,
                        flags: flag_bits(&f),
                        labels: f.gmail_labels().map(|l| l.iter().map(|s| s.to_string()).collect()).unwrap_or_default(),
                        gm_msgid: f.gmail_msg_id().copied(),
                        gm_thrid: None,
                        size: f.size.unwrap_or(0) as i64,
                        internal_date: f.internal_date().map(|d| d.timestamp()),
                        parsed,
                        raw: None,
                        body_text: None,
                    };
                    let row = self.db.put_message(self.account, id, &message).map_err(db_err)?;
                    report.new += 1;
                    if role == Some(Role::Inbox) && message.flags & flags::SEEN == 0 && highest_local.is_some() {
                        report.new_in_inbox.push(row);
                    }
                }
            }
        }

        // Flag changes on messages we already had.
        if !local.is_empty() {
            let since = known.highestmodseq.filter(|_| condstore && known.uidvalidity == Some(validity));
            let range = format!("{}:{}", local.iter().map(|(u, _)| *u).min().unwrap_or(1), highest_local.unwrap_or(1));
            let query = match since {
                Some(m) => format!("(UID FLAGS MODSEQ) (CHANGEDSINCE {m})"),
                None => "(UID FLAGS)".to_string(),
            };
            let fetched: Vec<Fetch> = self.session.uid_fetch(range, query).await?.try_collect().await?;
            let current: std::collections::HashMap<u32, i64> = local.iter().copied().collect();
            for f in fetched {
                let Some(uid) = f.uid else { continue };
                let bits = flag_bits(&f);
                if current.get(&uid).is_some_and(|old| *old != bits) {
                    self.db.set_flags(id, validity, uid, bits, f.modseq).map_err(db_err)?;
                    report.changed += 1;
                }
            }
        }

        // Bodies of recent messages, for reading offline.
        let since = now() - self.limits.body_days * 86_400;
        let missing = self.db.missing_bodies(id, since, self.limits.bodies_per_sync).map_err(db_err)?;
        let uids: Vec<u32> = missing.iter().filter(|(_, v)| *v == validity).map(|(u, _)| *u).collect();
        for chunk in uids.chunks(25) {
            report.bodies += self.fetch_bodies(id, validity, chunk).await?;
        }

        self.db.set_mailbox_state(id, Some(validity), selected.uid_next, selected.highest_modseq).map_err(db_err)?;
        self.db.recount(id).map_err(db_err)?;
        Ok(())
    }

    /// Fetches whole messages into the store; returns how many arrived.
    pub async fn fetch_bodies(&mut self, mailbox: i64, validity: u32, uids: &[u32]) -> Result<usize, NetError> {
        if uids.is_empty() {
            return Ok(0);
        }
        let fetched: Vec<Fetch> = self.session.uid_fetch(uid_set(uids), "(UID FLAGS BODY.PEEK[])").await?.try_collect().await?;
        let mut n = 0;
        for f in fetched {
            let (Some(uid), Some(raw)) = (f.uid, f.body()) else { continue };
            let Some(parsed) = mime::parse(raw) else { continue };
            let name = self.store.put(raw).map_err(|e| NetError::Protocol(format!("Couldn't save a message: {e}")))?;
            let body_text = parsed.plain_text();
            let message = NewMessage {
                uid,
                uidvalidity: validity,
                flags: flag_bits(&f),
                size: raw.len() as i64,
                parsed,
                raw: Some(name),
                body_text: Some(body_text),
                ..NewMessage::default()
            };
            self.db.put_message(self.account, mailbox, &message).map_err(db_err)?;
            n += 1;
        }
        Ok(n)
    }

    /// Sends queued changes for this account.
    pub async fn send_ops(&mut self, report: &mut Report) -> Result<(), NetError> {
        let due = self.db.due_ops(self.account, now()).map_err(db_err)?;
        for op in due {
            match self.apply(&op).await {
                Ok(()) => {
                    self.db.op_done(op.id).map_err(db_err)?;
                    report.ops_sent += 1;
                }
                Err(e) if e.transient() && op.attempts < 8 => {
                    let wait = (30i64 << op.attempts.min(7)).min(3600);
                    self.db.op_failed(op.id, &e.to_string(), now() + wait).map_err(db_err)?;
                    report.ops_failed += 1;
                    // Off the network: the rest would fail the same way.
                    if matches!(e, NetError::Unreachable(_)) {
                        return Err(e);
                    }
                }
                Err(e) => {
                    // The server won't take it (the message is gone, say): drop it.
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
        let validity = p["uidvalidity"].as_u64().unwrap_or(0) as u32;
        let uid = p["uid"].as_u64().unwrap_or(0) as u32;
        let target = || -> Result<crate::db::Mailbox, NetError> {
            self.db
                .mailbox(mailbox)
                .map_err(db_err)?
                .filter(|m| m.uidvalidity == Some(validity))
                .ok_or_else(|| NetError::SignIn("the mailbox changed on the server".into()))
        };
        match op.kind.as_str() {
            "flags" => {
                let mb = target()?;
                self.session.select(&mb.name).await?;
                let add = p["add"].as_i64().unwrap_or(0);
                let remove = p["remove"].as_i64().unwrap_or(0);
                if add != 0 {
                    let _: Vec<_> = self.session.uid_store(uid.to_string(), format!("+FLAGS.SILENT ({})", flags::names(add).join(" "))).await?.try_collect().await?;
                }
                if remove != 0 {
                    let _: Vec<_> = self.session.uid_store(uid.to_string(), format!("-FLAGS.SILENT ({})", flags::names(remove).join(" "))).await?.try_collect().await?;
                }
                Ok(())
            }
            "move" => {
                let mb = target()?;
                let to = p["to"].as_str().unwrap_or_default().to_string();
                self.session.select(&mb.name).await?;
                if self.has("MOVE") {
                    self.session.uid_mv(uid.to_string(), &to).await?;
                } else {
                    self.session.uid_copy(uid.to_string(), &to).await?;
                    let _: Vec<_> = self.session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
                    let _: Vec<_> = self.session.uid_expunge(uid.to_string()).await?.try_collect().await?;
                }
                Ok(())
            }
            "expunge" => {
                let mb = target()?;
                self.session.select(&mb.name).await?;
                let _: Vec<_> = self.session.uid_store(uid.to_string(), "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
                let _: Vec<_> = self.session.uid_expunge(uid.to_string()).await?.try_collect().await?;
                Ok(())
            }
            "append" => {
                let to = p["to"].as_str().unwrap_or_default().to_string();
                let raw = p["raw"].as_str().unwrap_or_default();
                let bytes = self.store.get(raw).map_err(|e| NetError::SignIn(format!("the message to save is gone: {e}")))?;
                let flag_list = flags::names(p["flags"].as_i64().unwrap_or(0)).join(" ");
                let flags_arg = (!flag_list.is_empty()).then(|| format!("({flag_list})"));
                self.session.append(&to, flags_arg.as_deref(), None, &bytes).await?;
                Ok(())
            }
            other => Err(NetError::SignIn(format!("unknown change “{other}”"))),
        }
    }
}

fn unreachable_mailbox(id: i64) -> crate::db::Mailbox {
    crate::db::Mailbox {
        id,
        account: String::new(),
        name: String::new(),
        delimiter: None,
        role: None,
        uidvalidity: None,
        uidnext: None,
        highestmodseq: None,
        unread: 0,
        total: 0,
    }
}

fn db_err(e: rusqlite::Error) -> NetError {
    NetError::Protocol(format!("Mail's database: {e}"))
}

/// Queues a flag change and makes it locally at once.
pub fn queue_flags(db: &Db, message: i64, add: i64, remove: i64) -> rusqlite::Result<()> {
    let Some(m) = db.message(message)? else { return Ok(()) };
    db.change_flags(message, add, remove)?;
    db.recount(m.mailbox)?;
    db.queue(&m.account, "flags", &json!({"mailbox": m.mailbox, "uidvalidity": m.uidvalidity, "uid": m.uid, "add": add, "remove": remove}), now())?;
    Ok(())
}

/// Queues a move (archive, trash, junk, a folder) and hides the message at once.
pub fn queue_move(db: &Db, message: i64, to: &str) -> rusqlite::Result<()> {
    let Some(m) = db.message(message)? else { return Ok(()) };
    db.change_flags(message, flags::DELETED, 0)?;
    db.recount(m.mailbox)?;
    db.queue(&m.account, "move", &json!({"mailbox": m.mailbox, "uidvalidity": m.uidvalidity, "uid": m.uid, "to": to}), now())?;
    Ok(())
}

/// Queues deleting a message for good (from Trash) and hides it at once.
pub fn queue_expunge(db: &Db, message: i64) -> rusqlite::Result<()> {
    let Some(m) = db.message(message)? else { return Ok(()) };
    db.change_flags(message, flags::DELETED, 0)?;
    db.recount(m.mailbox)?;
    db.queue(&m.account, "expunge", &json!({"mailbox": m.mailbox, "uidvalidity": m.uidvalidity, "uid": m.uid}), now())?;
    Ok(())
}

/// Queues saving a raw message (a sent copy, a draft) into a mailbox.
pub fn queue_append(db: &Db, account: &str, to: &str, raw: &str, flag_bits: i64) -> rusqlite::Result<()> {
    db.queue(account, "append", &json!({"to": to, "raw": raw, "flags": flag_bits}), now())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_sets() {
        assert_eq!(uid_set(&[5, 1, 2, 3, 9, 11, 12]), "1:3,5,9,11:12");
        assert_eq!(uid_set(&[7]), "7");
        assert_eq!(uid_set(&[]), "");
    }
}
