//! The tables behind `crate::graph`: Graph folder ids and delta links, and the
//! UID each Graph message id is known by here. Messages keep their usual rows;
//! Graph accounts use UIDVALIDITY 1 throughout.

use super::Db;
use rusqlite::{OptionalExtension, params};

impl Db {
    /// Records a mailbox's Graph folder; a different folder id starts its sync over.
    pub fn set_graph_folder(&self, mailbox: i64, folder: &str) -> rusqlite::Result<()> {
        let known: Option<String> = self
            .conn
            .query_row("SELECT folder FROM graph_folders WHERE mailbox = ?1", params![mailbox], |r| r.get(0))
            .optional()?;
        if known.as_deref() != Some(folder) {
            self.clear_mailbox(mailbox)?;
            self.conn.execute("DELETE FROM graph_items WHERE mailbox = ?1", params![mailbox])?;
            self.conn.execute(
                "INSERT INTO graph_folders (mailbox, folder) VALUES (?1, ?2)
                 ON CONFLICT (mailbox) DO UPDATE SET folder = ?2, delta = NULL",
                params![mailbox, folder],
            )?;
        }
        Ok(())
    }

    pub fn graph_folder(&self, mailbox: i64) -> rusqlite::Result<Option<(String, Option<String>)>> {
        self.conn
            .query_row("SELECT folder, delta FROM graph_folders WHERE mailbox = ?1", params![mailbox], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()
    }

    /// The Graph folder id of an account's mailbox, by its name.
    pub fn graph_folder_named(&self, account: &str, name: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT g.folder FROM graph_folders g JOIN mailboxes m ON m.id = g.mailbox WHERE m.account = ?1 AND m.name = ?2",
                params![account, name],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn set_graph_delta(&self, mailbox: i64, delta: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE graph_folders SET delta = ?2 WHERE mailbox = ?1", params![mailbox, delta])?;
        Ok(())
    }

    /// The UID for a Graph message, given one if it's new (and whether it was).
    pub fn graph_uid(&self, mailbox: i64, item: &str) -> rusqlite::Result<(u32, bool)> {
        let known: Option<u32> = self
            .conn
            .query_row("SELECT uid FROM graph_items WHERE mailbox = ?1 AND item = ?2", params![mailbox, item], |r| r.get(0))
            .optional()?;
        if let Some(uid) = known {
            return Ok((uid, false));
        }
        let uid: u32 = self.conn.query_row(
            "SELECT COALESCE(MAX(uid), 0) + 1 FROM graph_items WHERE mailbox = ?1",
            params![mailbox],
            |r| r.get(0),
        )?;
        self.conn.execute("INSERT INTO graph_items (mailbox, uid, item) VALUES (?1, ?2, ?3)", params![mailbox, uid, item])?;
        Ok((uid, true))
    }

    /// The UID a Graph message is known by, if it is.
    pub fn graph_known_uid(&self, mailbox: i64, item: &str) -> rusqlite::Result<Option<u32>> {
        self.conn
            .query_row("SELECT uid FROM graph_items WHERE mailbox = ?1 AND item = ?2", params![mailbox, item], |r| r.get(0))
            .optional()
    }

    /// The Graph message id behind a UID.
    pub fn graph_item(&self, mailbox: i64, uid: u32) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT item FROM graph_items WHERE mailbox = ?1 AND uid = ?2", params![mailbox, uid], |r| r.get(0))
            .optional()
    }

    /// Forgets a Graph message (gone from its folder) and its row.
    pub fn forget_graph_item(&self, mailbox: i64, item: &str) -> rusqlite::Result<usize> {
        let Some(uid) = self.graph_known_uid(mailbox, item)? else { return Ok(0) };
        let n = self.remove_uids(mailbox, 1, &[uid])?;
        self.conn.execute("DELETE FROM graph_items WHERE mailbox = ?1 AND uid = ?2", params![mailbox, uid])?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use crate::db::Db;

    #[test]
    fn graph_ids() {
        let dir = std::env::temp_dir().join(format!("nixe-mail-graph-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = std::fs::remove_file(dir.join("mail.sqlite"));
        let db = Db::open(&dir.join("mail.sqlite")).unwrap();
        db.upsert_account("a", "me@example.test", "").unwrap();
        let inbox = db.upsert_mailbox("a", "Inbox", Some("/"), None).unwrap();
        db.set_graph_folder(inbox, "F1").unwrap();
        assert_eq!(db.graph_uid(inbox, "m1").unwrap(), (1, true));
        assert_eq!(db.graph_uid(inbox, "m2").unwrap(), (2, true));
        assert_eq!(db.graph_uid(inbox, "m1").unwrap(), (1, false));
        assert_eq!(db.graph_item(inbox, 2).unwrap().as_deref(), Some("m2"));
        assert_eq!(db.graph_folder_named("a", "Inbox").unwrap().as_deref(), Some("F1"));
        db.set_graph_delta(inbox, Some("next")).unwrap();
        assert_eq!(db.graph_folder(inbox).unwrap(), Some(("F1".into(), Some("next".into()))));
        // The same folder keeps its state; another one starts over.
        db.set_graph_folder(inbox, "F1").unwrap();
        assert_eq!(db.graph_item(inbox, 1).unwrap().as_deref(), Some("m1"));
        db.set_graph_folder(inbox, "F2").unwrap();
        assert_eq!(db.graph_item(inbox, 1).unwrap(), None);
        assert_eq!(db.graph_folder(inbox).unwrap(), Some(("F2".into(), None)));
    }
}
