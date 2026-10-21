//! Mail store, MIME, search and sync for Nixe Mail. No GTK.
//!
//! `mail.sqlite` holds mailboxes, message envelopes, flags, threads, the
//! offline change queue and a full-text index; raw messages are files in the
//! store. `nixe-maild` is the only writer; the window reads.

pub mod db;
pub mod mime;
pub mod store;
pub mod thread;

pub use db::{AccountRow, Db, ListItem, Mailbox, MessageRow, NewMessage, Op, Role};
pub use mime::{Attachment, Parsed};

/// IMAP flags kept as bits.
pub mod flags {
    pub const SEEN: i64 = 1;
    pub const FLAGGED: i64 = 2;
    pub const ANSWERED: i64 = 4;
    pub const DRAFT: i64 = 8;
    pub const DELETED: i64 = 16;

    /// Bits from IMAP flag names (`\Seen`, `\Flagged`, ...); others are ignored.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> i64 {
        names.into_iter().fold(0, |bits, name| {
            bits | match name.to_ascii_lowercase().as_str() {
                "\\seen" => SEEN,
                "\\flagged" => FLAGGED,
                "\\answered" => ANSWERED,
                "\\draft" => DRAFT,
                "\\deleted" => DELETED,
                _ => 0,
            }
        })
    }

    /// IMAP names for bits.
    pub fn names(bits: i64) -> Vec<&'static str> {
        [(SEEN, "\\Seen"), (FLAGGED, "\\Flagged"), (ANSWERED, "\\Answered"), (DRAFT, "\\Draft"), (DELETED, "\\Deleted")]
            .into_iter()
            .filter(|(bit, _)| bits & bit != 0)
            .map(|(_, name)| name)
            .collect()
    }
}

/// Where Mail keeps things, under a data directory (`~/.local/share/nixe-mail`).
pub struct Paths {
    pub root: std::path::PathBuf,
}

impl Paths {
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn db(&self) -> std::path::PathBuf {
        self.root.join("mail.sqlite")
    }

    pub fn store(&self) -> std::path::PathBuf {
        self.root.join("store")
    }

    /// Creates the data directory, readable by this user only.
    pub fn ensure(&self) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(&self.root)?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
    }
}

#[cfg(test)]
mod tests {
    use super::flags::*;

    #[test]
    fn flag_names_round_trip() {
        let bits = from_names(["\\Seen", "\\FLAGGED", "$Junk", "\\Answered"]);
        assert_eq!(bits, SEEN | FLAGGED | ANSWERED);
        assert_eq!(names(bits), vec!["\\Seen", "\\Flagged", "\\Answered"]);
    }
}
