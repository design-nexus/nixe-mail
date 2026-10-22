//! What the window reads: `mail.sqlite` (read-only; nixe-maild writes it) and
//! the message store.

use nixe_mail_core::db::{AccountRow, Db, ListItem, Mailbox, MessageRow, Role};
use nixe_mail_core::mime::{self, Parsed};
use nixe_mail_core::store::Store;
use nixe_mail_core::{Paths, rusqlite};
use std::cell::RefCell;

thread_local! {
    static DB: RefCell<Option<Db>> = const { RefCell::new(None) };
}

pub fn paths() -> Paths {
    Paths::new(nixe_core::paths::data_dir())
}

/// Runs `f` with the database, opening it on first use (it appears once the
/// engine has run).
fn with<T: Default>(f: impl FnOnce(&Db) -> rusqlite::Result<T>) -> T {
    DB.with(|slot| {
        if slot.borrow().is_none() {
            let path = paths().db();
            if path.exists() {
                *slot.borrow_mut() = Db::open_read_only(&path).ok();
            }
        }
        let borrowed = slot.borrow();
        let Some(db) = borrowed.as_ref() else { return T::default() };
        f(db).unwrap_or_else(|e| {
            eprintln!("nixe-mail: reading mail: {e}");
            T::default()
        })
    })
}

/// What the list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// A role across every account (Inbox, Sent…).
    All(Role),
    Mailbox(i64),
}

impl View {
    pub fn id(&self) -> String {
        match self {
            Self::All(role) => role.id().to_string(),
            Self::Mailbox(id) => format!("mailbox:{id}"),
        }
    }

    pub fn from_id(id: &str) -> Self {
        match id.strip_prefix("mailbox:").and_then(|i| i.parse().ok()) {
            Some(mb) => Self::Mailbox(mb),
            None => Self::All(Role::from_id(id).unwrap_or(Role::Inbox)),
        }
    }

    /// The mailboxes it covers.
    pub fn mailboxes(&self, all: &[Mailbox]) -> Vec<i64> {
        match self {
            Self::All(role) => all.iter().filter(|m| m.role == Some(*role)).map(|m| m.id).collect(),
            Self::Mailbox(id) => vec![*id],
        }
    }

    /// The role of what it shows (a folder has none).
    pub fn role(&self, all: &[Mailbox]) -> Option<Role> {
        match self {
            Self::All(role) => Some(*role),
            Self::Mailbox(id) => all.iter().find(|m| m.id == *id).and_then(|m| m.role),
        }
    }
}

pub fn accounts() -> Vec<AccountRow> {
    with(|db| db.accounts())
}

pub fn mailboxes() -> Vec<Mailbox> {
    with(|db| db.mailboxes(None))
}

pub fn list(mailboxes: &[i64], limit: usize) -> Vec<ListItem> {
    with(|db| db.list(mailboxes, limit, 0))
}

pub fn search(query: &str, limit: usize) -> Vec<ListItem> {
    with(|db| db.search(query, limit))
}

pub fn message(id: i64) -> Option<MessageRow> {
    with(|db| db.message(id))
}

/// A conversation's messages, oldest first. Within a mailbox view, the user's
/// own replies (in Sent) show too.
pub fn conversation(thread: &str, mailboxes: &[i64]) -> Vec<MessageRow> {
    with(|db| db.conversation(thread, mailboxes))
}

/// Local drafts (being written; not yet on the server).
pub fn drafts() -> Vec<(i64, String, serde_json::Value, i64)> {
    with(|db| db.drafts())
}

pub fn store() -> Store {
    Store::new(paths().store())
}

pub fn raw(name: &str) -> Option<Vec<u8>> {
    store().get(name).ok()
}

/// A message read from the store, if its body is here.
pub fn parsed(row: &MessageRow) -> Option<(Vec<u8>, Parsed)> {
    let raw = raw(row.raw.as_deref()?)?;
    let parsed = mime::parse(&raw)?;
    Some((raw, parsed))
}

/// Every address the user sends from.
pub fn own_addresses() -> Vec<String> {
    accounts().into_iter().map(|a| a.address).collect()
}
