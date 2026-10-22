//! The tables behind `crate::organize`: the user's choices for senders, the
//! Screener, snoozes, follow-ups, mail scheduled to send, and the sender list.

use super::{Db, ListItem, Role, list_row};
use crate::organize::{Kind, unsubscribe_link};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// A sender as the sender list shows them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderRow {
    pub address: String,
    pub name: String,
    pub messages: i64,
    pub unread: i64,
    pub last: i64,
    /// Where their mail goes (the user's choice, else the newest message's kind).
    pub kind: Kind,
    /// The user chose the kind.
    pub chosen: bool,
    /// `in`, `out`, or none yet.
    pub screened: Option<String>,
    /// How to unsubscribe (an https or mailto link), if they said.
    pub unsubscribe: Option<String>,
}

/// Mail waiting to send at a time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scheduled {
    pub op: i64,
    pub account: String,
    pub at: i64,
    pub to: Vec<String>,
    pub subject: String,
    pub raw: String,
}

/// A conversation waiting for a reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowUp {
    pub thread: String,
    pub account: String,
    pub subject: String,
    pub since: i64,
    pub due: i64,
}

impl Db {
    // ---------- senders and the Screener ----------

    /// Records the user's choice for a sender. `None` leaves that part as it is;
    /// `Some(None)` clears it.
    pub fn set_sender(
        &self,
        address: &str,
        kind: Option<Option<Kind>>,
        screened: Option<Option<&str>>,
        now: i64,
    ) -> rusqlite::Result<()> {
        let address = address.trim().to_lowercase();
        self.conn.execute(
            "INSERT INTO senders (address, updated) VALUES (?1, ?2) ON CONFLICT (address) DO NOTHING",
            params![address, now],
        )?;
        if let Some(kind) = kind {
            self.conn.execute(
                "UPDATE senders SET category = ?2, updated = ?3 WHERE address = ?1",
                params![address, kind.map(Kind::id), now],
            )?;
        }
        if let Some(screened) = screened {
            let screened = screened.filter(|s| matches!(*s, "in" | "out"));
            self.conn
                .execute("UPDATE senders SET screened = ?2, updated = ?3 WHERE address = ?1", params![address, screened, now])?;
        }
        Ok(())
    }

    /// People the user writes to are let through the Screener.
    pub(super) fn know_recipients(&self, mailbox: i64, recipients: &[(String, String)]) -> rusqlite::Result<()> {
        let role: Option<String> =
            self.conn.query_row("SELECT role FROM mailboxes WHERE id = ?1", [mailbox], |r| r.get(0)).optional()?.flatten();
        if role.as_deref() != Some(Role::Sent.id()) {
            return Ok(());
        }
        for (_, email) in recipients {
            self.conn.execute(
                "INSERT INTO senders (address, screened, updated) VALUES (lower(?1), 'in', 0)
                 ON CONFLICT (address) DO UPDATE SET screened = COALESCE(screened, 'in')",
                [email],
            )?;
        }
        Ok(())
    }

    /// When the Screener was switched on (`None`: it's off).
    pub fn screener_since(&self) -> rusqlite::Result<Option<i64>> {
        Ok(self.setting("screener_since")?.and_then(|v| v.parse().ok()))
    }

    pub fn set_screener(&self, on: bool, now: i64) -> rusqlite::Result<()> {
        if on == self.screener_since()?.is_some() {
            return Ok(());
        }
        if on {
            self.set_setting("screener_since", &now.to_string())
        } else {
            self.conn.execute("DELETE FROM settings WHERE key = 'screener_since'", [])?;
            Ok(())
        }
    }

    fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    /// Whether a sender's mail waits in the Screener (it's on, and they
    /// haven't been let in or written to, and weren't around before).
    pub fn screened_out_or_waiting(&self, address: &str) -> rusqlite::Result<bool> {
        let Some(since) = self.screener_since()? else { return Ok(false) };
        let screened: Option<String> = self
            .conn
            .query_row("SELECT screened FROM senders WHERE address = lower(?1)", [address], |r| r.get(0))
            .optional()?
            .flatten();
        if let Some(s) = screened {
            return Ok(s == "out");
        }
        let first: Option<i64> =
            self.conn.query_row("SELECT min(date) FROM messages WHERE lower(from_addr) = lower(?1)", [address], |r| r.get(0))?;
        Ok(first.is_none_or(|f| f >= since))
    }

    /// Everyone who has written, busiest first: for the sender list.
    pub fn senders(&self, inboxes: &[i64], limit: usize) -> rusqlite::Result<Vec<SenderRow>> {
        if inboxes.is_empty() {
            return Ok(Vec::new());
        }
        let ids = inboxes.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT lower(m.from_addr) AS address, max(m.from_name), count(*), sum(m.flags & 1 = 0), max(m.date),
                    (SELECT kind FROM messages k WHERE lower(k.from_addr) = lower(m.from_addr) ORDER BY date DESC LIMIT 1),
                    s.category, s.screened,
                    (SELECT unsubscribe FROM messages u WHERE lower(u.from_addr) = lower(m.from_addr) AND unsubscribe IS NOT NULL
                       ORDER BY date DESC LIMIT 1)
             FROM messages m LEFT JOIN senders s ON s.address = lower(m.from_addr)
             WHERE m.mailbox IN ({ids}) AND m.flags & 16 = 0 AND m.from_addr <> ''
             GROUP BY lower(m.from_addr) ORDER BY count(*) DESC, max(m.date) DESC LIMIT ?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        stmt.query_map([limit as i64], |r| {
            let guessed: String = r.get(5)?;
            let chosen: Option<String> = r.get(6)?;
            let unsubscribe: Option<String> = r.get(8)?;
            Ok(SenderRow {
                address: r.get(0)?,
                name: r.get(1)?,
                messages: r.get(2)?,
                unread: r.get(3)?,
                last: r.get(4)?,
                kind: chosen.as_deref().or(Some(guessed.as_str())).and_then(Kind::from_id).unwrap_or(Kind::Imbox),
                chosen: chosen.is_some(),
                screened: r.get(7)?,
                unsubscribe: unsubscribe.as_deref().and_then(unsubscribe_link),
            })
        })?
        .collect()
    }

    /// A sender's messages in some mailboxes (for "archive all").
    pub fn messages_from(&self, address: &str, mailboxes: &[i64]) -> rusqlite::Result<Vec<i64>> {
        if mailboxes.is_empty() {
            return Ok(Vec::new());
        }
        let ids = mailboxes.iter().map(|m| m.to_string()).collect::<Vec<_>>().join(",");
        let mut stmt = self.conn.prepare(&format!(
            "SELECT id FROM messages WHERE lower(from_addr) = lower(?1) AND mailbox IN ({ids}) AND flags & 16 = 0"
        ))?;
        stmt.query_map([address], |r| r.get(0))?.collect()
    }

    // ---------- snoozes ----------

    pub fn snooze(&self, thread: &str, until: i64, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO snoozes (thread, until, since) VALUES (?1, ?2, ?3) ON CONFLICT (thread) DO UPDATE SET until = ?2",
            params![thread, until, now],
        )?;
        Ok(())
    }

    pub fn unsnooze(&self, thread: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("DELETE FROM snoozes WHERE thread = ?1", [thread])? > 0)
    }

    pub fn snoozed_until(&self, thread: &str) -> rusqlite::Result<Option<i64>> {
        self.conn.query_row("SELECT until FROM snoozes WHERE thread = ?1", [thread], |r| r.get(0)).optional()
    }

    /// Snoozes that are over: their threads, which wake up (and leave the table).
    pub fn wake_snoozes(&self, now: i64) -> rusqlite::Result<Vec<String>> {
        let threads: Vec<String> = self
            .conn
            .prepare("SELECT thread FROM snoozes WHERE until <= ?1")?
            .query_map([now], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        self.conn.execute("DELETE FROM snoozes WHERE until <= ?1", [now])?;
        Ok(threads)
    }

    /// The newest message of a conversation (to mark unread when it wakes).
    pub fn newest_in_thread(&self, thread: &str) -> rusqlite::Result<Option<ListItem>> {
        self.conn
            .query_row(
                "SELECT id, account, mailbox, thread, date, from_name, from_addr, subject, preview, flags, has_attachments, 1, flags & 1 = 0
                 FROM messages WHERE thread = ?1 AND flags & 16 = 0 ORDER BY date DESC LIMIT 1",
                [thread],
                list_row,
            )
            .optional()
    }

    /// The next time something is due (a snooze, a follow-up or a scheduled
    /// send), for the engine's timer.
    pub fn next_due(&self) -> rusqlite::Result<Option<i64>> {
        self.conn.query_row(
            "SELECT min(t) FROM (SELECT min(until) AS t FROM snoozes UNION ALL SELECT min(due) FROM followups
               UNION ALL SELECT min(next_try) FROM ops WHERE kind = 'send')",
            [],
            |r| r.get(0),
        )
    }

    // ---------- follow-ups ----------

    pub fn follow_up(&self, thread: &str, account: &str, subject: &str, since: i64, due: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO followups (thread, account, subject, since, due) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (thread) DO UPDATE SET due = ?5, since = ?4",
            params![thread, account, subject, since, due],
        )?;
        Ok(())
    }

    pub fn cancel_follow_up(&self, thread: &str) -> rusqlite::Result<bool> {
        Ok(self.conn.execute("DELETE FROM followups WHERE thread = ?1", [thread])? > 0)
    }

    pub fn follow_ups(&self) -> rusqlite::Result<Vec<FollowUp>> {
        self.conn
            .prepare("SELECT thread, account, subject, since, due FROM followups ORDER BY due")?
            .query_map([], |r| {
                Ok(FollowUp { thread: r.get(0)?, account: r.get(1)?, subject: r.get(2)?, since: r.get(3)?, due: r.get(4)? })
            })?
            .collect()
    }

    /// Follow-ups that came due: the ones still unanswered (someone other than
    /// the user wrote since) are returned; answered ones are dropped. Both
    /// leave the table.
    pub fn due_follow_ups(&self, now: i64, own: &[String]) -> rusqlite::Result<Vec<FollowUp>> {
        let due: Vec<FollowUp> = self.follow_ups()?.into_iter().filter(|f| f.due <= now).collect();
        let mut out = Vec::new();
        for f in due {
            let replies: Vec<String> = self
                .conn
                .prepare("SELECT from_addr FROM messages WHERE thread = ?1 AND date > ?2")?
                .query_map(params![f.thread, f.since], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let answered = replies.iter().any(|from| !own.iter().any(|o| o.eq_ignore_ascii_case(from)));
            self.cancel_follow_up(&f.thread)?;
            if !answered {
                out.push(f);
            }
        }
        Ok(out)
    }

    // ---------- mail scheduled to send ----------

    /// Sends waiting for their time (not ones retrying after a failure).
    pub fn scheduled(&self, now: i64) -> rusqlite::Result<Vec<Scheduled>> {
        let mut stmt = self.conn.prepare("SELECT id, account, next_try, payload FROM ops WHERE kind = 'send' AND attempts = 0 AND next_try > ?1 ORDER BY next_try")?;
        stmt.query_map([now], |r| {
            let payload: serde_json::Value = serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default();
            Ok(Scheduled {
                op: r.get(0)?,
                account: r.get(1)?,
                at: r.get(2)?,
                to: payload["to"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default(),
                subject: payload["subject"].as_str().unwrap_or_default().to_string(),
                raw: payload["raw"].as_str().unwrap_or_default().to_string(),
            })
        })?
        .collect()
    }

    /// Takes a scheduled send back (before it went); returns it.
    pub fn unschedule(&self, op: i64, now: i64) -> rusqlite::Result<Option<Scheduled>> {
        let found = self.scheduled(now)?.into_iter().find(|s| s.op == op);
        if found.is_some() {
            self.op_done(op)?;
        }
        Ok(found)
    }

    /// Changes waiting for the server, not counting mail scheduled for later.
    pub fn waiting_ops(&self, now: i64) -> rusqlite::Result<i64> {
        self.conn.query_row("SELECT count(*) FROM ops WHERE NOT (kind = 'send' AND attempts = 0 AND next_try > ?1)", [now], |r| {
            r.get(0)
        })
    }
}
