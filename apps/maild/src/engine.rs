//! The engine thread: owns the writable database, the store and a tokio
//! runtime. It syncs every account (every few minutes, on request, and when an
//! inbox's IMAP IDLE says something arrived), sends queued mail, and tells the
//! D-Bus side what changed.

use crate::accounts::{self, MailAccount, Skipped};
use nixe_mail_core::db::Role;
use nixe_mail_core::net::{self, NetError};
use nixe_mail_core::send::{self, Draft};
use nixe_mail_core::store::Store;
use nixe_mail_core::sync::{self, Limits, Sync};
use nixe_mail_core::{Db, Paths, async_imap, flags, rusqlite};
use serde_json::json;
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const SYNC_EVERY: Duration = Duration::from_secs(5 * 60);
const IDLE_FOR: Duration = Duration::from_secs(25 * 60);

pub type Reply<T> = oneshot::Sender<Result<T, String>>;

pub enum Command {
    Sync(Option<String>),
    Flags(Vec<i64>, i64, i64),
    /// Messages to a role (`archive`, `trash`, `junk`, `inbox`) or `mailbox:<id>`.
    Move(Vec<i64>, String),
    /// Delete for good (from Trash or Junk).
    Delete(Vec<i64>),
    /// Fetch the whole message now (opened before its body arrived).
    FetchBody(i64, Reply<()>),
    /// Send; replies "sent" or "queued" (offline: it goes when back online).
    Send(Box<Draft>, Reply<String>),
    SaveDraft(Option<i64>, serde_json::Value, Reply<i64>),
    DeleteDraft(i64),
    /// The window is in front: no notifications.
    Active(bool),
    /// Forget an account's mail on this computer (after it was removed from
    /// Online Accounts; nothing on the server changes).
    Forget(String),
}

/// What the engine reports.
#[derive(Debug, Clone)]
pub enum Event {
    /// Something to re-read: `messages`, `mailboxes` or `status`.
    Changed(&'static str),
    /// New unread mail in an inbox while the window isn't in front.
    NewMail(Vec<(String, String)>),
}

pub fn start(events: impl Fn(Event) + Send + 'static) -> mpsc::UnboundedSender<Command> {
    let (tx, rx) = mpsc::unbounded_channel();
    let me = tx.clone();
    std::thread::Builder::new()
        .name("mail-engine".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&rt, run(rx, me, events));
        })
        .expect("start the mail engine");
    tx
}

struct State {
    db: Db,
    store: Store,
    goa: Option<nixe_accounts::Client>,
    accounts: Vec<MailAccount>,
    skipped: Vec<Skipped>,
    syncing: HashSet<String>,
    offline: HashSet<String>,
    idling: HashSet<String>,
    active: bool,
}

type Shared = Rc<RefCell<State>>;

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn paths() -> Paths {
    Paths::new(nixe_core::paths::data_dir())
}

async fn run(mut rx: mpsc::UnboundedReceiver<Command>, me: mpsc::UnboundedSender<Command>, events: impl Fn(Event) + 'static) {
    let paths = paths();
    let _ = paths.ensure();
    let db = match Db::open(&paths.db()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("nixe-maild: can't open {}: {e}", paths.db().display());
            return;
        }
    };
    let state: Shared = Rc::new(RefCell::new(State {
        db,
        store: Store::new(paths.store()),
        goa: nixe_accounts::Client::connect().ok(),
        accounts: Vec::new(),
        skipped: Vec::new(),
        syncing: HashSet::new(),
        offline: HashSet::new(),
        idling: HashSet::new(),
        active: false,
    }));
    let events: Rc<dyn Fn(Event)> = {
        let state = state.clone();
        Rc::new(move |e: Event| {
            if matches!(e, Event::Changed(_)) {
                publish(&state);
            }
            events(e)
        })
    };
    rediscover(&state);
    events(Event::Changed("status"));

    // Periodic sync.
    let tick = me.clone();
    tokio::task::spawn_local(async move {
        loop {
            let _ = tick.send(Command::Sync(None));
            tokio::time::sleep(SYNC_EVERY).await;
        }
    });

    while let Some(command) = rx.recv().await {
        match command {
            Command::Sync(which) => {
                rediscover(&state);
                let accounts: Vec<MailAccount> =
                    state.borrow().accounts.iter().filter(|a| which.as_ref().is_none_or(|w| *w == a.id)).cloned().collect();
                for account in accounts {
                    let (state, events, me) = (state.clone(), events.clone(), me.clone());
                    tokio::task::spawn_local(async move { sync_account(&state, &account, &events, &me).await });
                }
            }
            Command::Flags(ids, add, remove) => {
                let touched = with_db(&state, |db| {
                    let mut accounts = HashSet::new();
                    for id in &ids {
                        sync::queue_flags(db, *id, add, remove)?;
                        if let Some(m) = db.message(*id)? {
                            accounts.insert(m.account);
                        }
                    }
                    Ok(accounts)
                });
                events(Event::Changed("messages"));
                push_soon(&me, touched);
            }
            Command::Move(ids, target) => {
                let result = with_db(&state, |db| {
                    let mut accounts = HashSet::new();
                    for id in &ids {
                        let Some(m) = db.message(*id)? else { continue };
                        let to = match target.strip_prefix("mailbox:").and_then(|i| i.parse::<i64>().ok()) {
                            Some(mb) => db.mailbox(mb)?.filter(|b| b.account == m.account).map(|b| b.name),
                            None => Role::from_id(&target).and_then(|role| destination(db, &m.account, role).ok().flatten()),
                        };
                        if let Some(to) = to {
                            sync::queue_move(db, *id, &to)?;
                            accounts.insert(m.account);
                        }
                    }
                    Ok(accounts)
                });
                events(Event::Changed("messages"));
                push_soon(&me, result);
            }
            Command::Delete(ids) => {
                let touched = with_db(&state, |db| {
                    let mut accounts = HashSet::new();
                    for id in &ids {
                        if let Some(m) = db.message(*id)? {
                            sync::queue_expunge(db, *id)?;
                            accounts.insert(m.account);
                        }
                    }
                    Ok(accounts)
                });
                events(Event::Changed("messages"));
                push_soon(&me, touched);
            }
            Command::FetchBody(id, reply) => {
                let (state, events) = (state.clone(), events.clone());
                tokio::task::spawn_local(async move {
                    let result = fetch_body(&state, id).await;
                    if result.is_ok() {
                        events(Event::Changed("messages"));
                    }
                    let _ = reply.send(result);
                });
            }
            Command::Send(draft, reply) => {
                let (state, events, me) = (state.clone(), events.clone(), me.clone());
                tokio::task::spawn_local(async move {
                    let result = queue_send(&state, &draft);
                    let result = match result {
                        Ok(account) => {
                            let sent = send_queued(&state, &account).await;
                            events(Event::Changed("status"));
                            let _ = me.send(Command::Sync(Some(account)));
                            sent
                        }
                        Err(e) => Err(e),
                    };
                    let _ = reply.send(result);
                });
            }
            Command::SaveDraft(id, data, reply) => {
                let account = data["account"].as_str().unwrap_or_default().to_string();
                let result = state.borrow().db.save_draft(id, &account, &data, now()).map_err(|e| e.to_string());
                let _ = reply.send(result);
            }
            Command::DeleteDraft(id) => {
                let _ = state.borrow().db.delete_draft(id);
            }
            Command::Active(active) => state.borrow_mut().active = active,
            Command::Forget(account) => {
                let orphans = state.borrow_mut().db.remove_account(&account).unwrap_or_default();
                for raw in orphans {
                    let _ = state.borrow().store.remove(&raw);
                }
                events(Event::Changed("mailboxes"));
            }
        }
    }
}

fn with_db<T>(state: &Shared, f: impl FnOnce(&Db) -> rusqlite::Result<T>) -> Option<T> {
    match f(&state.borrow().db) {
        Ok(v) => Some(v),
        Err(e) => {
            eprintln!("nixe-maild: database: {e}");
            None
        }
    }
}

/// Sends queued changes for these accounts in a moment.
fn push_soon(me: &mpsc::UnboundedSender<Command>, accounts: Option<HashSet<String>>) {
    for account in accounts.unwrap_or_default() {
        let me = me.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let _ = me.send(Command::Sync(Some(account)));
        });
    }
}

/// The mailbox name for a role. Archiving on Gmail means All Mail.
fn destination(db: &Db, account: &str, role: Role) -> rusqlite::Result<Option<String>> {
    if let Some(mb) = db.mailbox_by_role(account, role)? {
        return Ok(Some(mb.name));
    }
    if role == Role::Archive {
        return Ok(db.mailbox_by_role(account, Role::All)?.map(|m| m.name));
    }
    Ok(None)
}

/// Re-reads the accounts from GOA; forgets mail of accounts that are gone.
fn rediscover(state: &Shared) {
    let (accounts, skipped) = accounts::discover(state.borrow().goa.as_ref());
    let mut s = state.borrow_mut();
    for a in &accounts {
        let _ = s.db.upsert_account(&a.id, &a.address, &a.name);
    }
    s.accounts = accounts;
    s.skipped = skipped;
}

async fn sync_account(state: &Shared, account: &MailAccount, events: &Rc<dyn Fn(Event)>, me: &mpsc::UnboundedSender<Command>) {
    if !state.borrow_mut().syncing.insert(account.id.clone()) {
        return;
    }
    events(Event::Changed("status"));
    let _ = send_queued(state, &account.id).await;
    let result = sync_once(state, account).await;
    let mut s = state.borrow_mut();
    s.syncing.remove(&account.id);
    match result {
        Ok(report) => {
            s.offline.remove(&account.id);
            let _ = s.db.set_account_state(&account.id, Some(now()), None);
            let new_mail: Vec<(String, String)> = report
                .new_in_inbox
                .iter()
                .filter_map(|id| s.db.message(*id).ok().flatten())
                .map(|m| (if m.from_name.is_empty() { m.from_addr } else { m.from_name }, m.subject))
                .collect();
            let active = s.active;
            let start_idle = !s.idling.contains(&account.id);
            drop(s);
            events(Event::Changed("messages"));
            if !new_mail.is_empty() && !active {
                events(Event::NewMail(new_mail));
            }
            if start_idle {
                idle(state.clone(), account.clone(), me.clone());
            }
        }
        Err(e) => {
            match &e {
                NetError::Unreachable(_) => {
                    s.offline.insert(account.id.clone());
                }
                NetError::SignIn(_) => {
                    account.check_sign_in(s.goa.as_ref());
                    let _ = s.db.set_account_state(&account.id, None, Some("Sign in again in Settings → Online Accounts."));
                }
                other => {
                    let _ = s.db.set_account_state(&account.id, None, Some(&other.to_string()));
                }
            }
            drop(s);
            eprintln!("nixe-maild: {}: {e}", account.address);
        }
    }
    events(Event::Changed("status"));
}

async fn connect(state: &Shared, account: &MailAccount) -> Result<net::Session, NetError> {
    let auth = account.imap_auth(state.borrow().goa.as_ref()).map_err(NetError::SignIn)?;
    net::imap(&account.imap, auth).await
}

/// A connection of its own for a sync (SQLite in WAL mode serializes writers),
/// so nothing borrowed is held across network waits.
fn own_db() -> Result<(Db, Store), NetError> {
    let paths = paths();
    let db = Db::open(&paths.db()).map_err(|e| NetError::Protocol(format!("Mail's database: {e}")))?;
    Ok((db, Store::new(paths.store())))
}

async fn sync_once(state: &Shared, account: &MailAccount) -> Result<sync::Report, NetError> {
    let mut session = connect(state, account).await?;
    let (mut db, store) = own_db()?;
    let report = Sync::new(&mut session, &mut db, &store, &account.id, Limits::default()).await?.run().await;
    let _ = session.logout().await;
    report
}

/// Waits on the inbox with IMAP IDLE; asks for a sync when something changes.
fn idle(state: Shared, account: MailAccount, me: mpsc::UnboundedSender<Command>) {
    state.borrow_mut().idling.insert(account.id.clone());
    tokio::task::spawn_local(async move {
        let mut failures = 0u32;
        loop {
            let inbox = state.borrow().db.mailbox_by_role(&account.id, Role::Inbox).ok().flatten().map(|m| m.name);
            let Some(inbox) = inbox else { break };
            let outcome: Result<(), NetError> = async {
                let mut session = connect(&state, &account).await?;
                session.select(&inbox).await?;
                let mut handle = session.idle();
                handle.init().await?;
                let (wait, _stop) = handle.wait_with_timeout(IDLE_FOR);
                let response = wait.await?;
                let _ = handle.done().await;
                if matches!(response, async_imap::extensions::idle::IdleResponse::NewData(_)) {
                    let _ = me.send(Command::Sync(Some(account.id.clone())));
                }
                Ok(())
            }
            .await;
            match outcome {
                Ok(()) => failures = 0,
                Err(e) => {
                    failures += 1;
                    if failures == 1 {
                        eprintln!("nixe-maild: {} idle: {e}", account.address);
                    }
                    // Back off: offline or a server without IDLE.
                    tokio::time::sleep(Duration::from_secs((30u64 << failures.min(5)).min(900))).await;
                }
            }
        }
        state.borrow_mut().idling.remove(&account.id);
    });
}

async fn fetch_body(state: &Shared, id: i64) -> Result<(), String> {
    let (row, account) = {
        let s = state.borrow();
        let row = s.db.message(id).map_err(|e| e.to_string())?.ok_or("That message is gone.")?;
        let account = s.accounts.iter().find(|a| a.id == row.account).cloned().ok_or("That account isn't set up.")?;
        (row, account)
    };
    if row.raw.is_some() {
        return Ok(());
    }
    let mut session = connect(state, &account).await.map_err(|e| e.to_string())?;
    let mailbox = state.borrow().db.mailbox(row.mailbox).map_err(|e| e.to_string())?.ok_or("That mailbox is gone.")?;
    session.examine(&mailbox.name).await.map_err(|e| NetError::from(e).to_string())?;
    let (mut db, store) = own_db().map_err(|e| e.to_string())?;
    let n = Sync::new(&mut session, &mut db, &store, &account.id, Limits::default())
        .await
        .map_err(|e| e.to_string())?
        .fetch_bodies(row.mailbox, row.uidvalidity, &[row.uid])
        .await
        .map_err(|e| e.to_string())?;
    let _ = session.logout().await;
    if n == 0 { Err("The server doesn't have that message any more.".into()) } else { Ok(()) }
}

/// Builds the message, keeps it in the store and queues it; returns the account.
fn queue_send(state: &Shared, draft: &Draft) -> Result<String, String> {
    let s = state.borrow();
    let account = s.accounts.iter().find(|a| a.id == draft.account).ok_or("That account isn't set up for mail.")?;
    if account.smtp.is_none() {
        return Err("This account has no outgoing mail server.".into());
    }
    if draft.recipients().is_empty() {
        return Err("Add someone to send it to.".into());
    }
    let id = send::message_id(&draft.from.email);
    let raw = draft.build(&id, now());
    let name = s.store.put(&raw).map_err(|e| e.to_string())?;
    s.db.queue(&account.id, "send", &json!({"raw": name, "from": draft.from.email, "to": draft.recipients()}), now())
        .map_err(|e| e.to_string())?;
    Ok(account.id.clone())
}

/// Sends an account's queued mail. "sent" when it all went, "queued" when the
/// server couldn't be reached (it goes later).
async fn send_queued(state: &Shared, account_id: &str) -> Result<String, String> {
    let ops: Vec<_> = {
        let s = state.borrow();
        s.db.due_ops(account_id, now()).map_err(|e| e.to_string())?.into_iter().filter(|o| o.kind == "send").collect()
    };
    if ops.is_empty() {
        return Ok("sent".into());
    }
    let account = state.borrow().accounts.iter().find(|a| a.id == account_id).cloned().ok_or("That account isn't set up.")?;
    let Some(smtp) = account.smtp.clone() else { return Err("This account has no outgoing mail server.".into()) };
    let mut outcome = Ok("sent".to_string());
    for op in ops {
        let raw_name = op.payload["raw"].as_str().unwrap_or_default().to_string();
        let from = op.payload["from"].as_str().unwrap_or_default().to_string();
        let to: Vec<String> = op.payload["to"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let raw = state.borrow().store.get(&raw_name).map_err(|e| e.to_string())?;
        let auth = account.smtp_auth(state.borrow().goa.as_ref());
        let result = match auth {
            Ok(auth) => send::smtp(&smtp, auth, &from, &to, &raw).await,
            Err(e) => Err(NetError::SignIn(e)),
        };
        let s = state.borrow();
        match result {
            Ok(()) => {
                let _ = s.db.op_done(op.id);
                // Gmail files sent mail itself; others get a copy in Sent.
                if !account.gmail
                    && let Ok(Some(sent)) = s.db.mailbox_by_role(&account.id, Role::Sent)
                {
                    let _ = sync::queue_append(&s.db, &account.id, &sent.name, &raw_name, flags::SEEN);
                }
            }
            Err(e) if e.transient() && op.attempts < 8 => {
                let wait = (30i64 << op.attempts.min(7)).min(3600);
                let _ = s.db.op_failed(op.id, &e.to_string(), now() + wait);
                outcome = Ok("queued".into());
            }
            Err(e) => {
                let _ = s.db.op_failed(op.id, &e.to_string(), i64::MAX);
                outcome = Err(format!("Couldn't send: {e}"));
            }
        }
    }
    outcome
}

/// What the window shows about the engine (`Status()` on D-Bus), kept current
/// by the engine thread.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Status {
    pub accounts: Vec<AccountStatus>,
    /// Accounts in Online Accounts that Mail can't use, and why.
    pub skipped: Vec<SkippedStatus>,
    /// Changes and sends waiting for the server.
    pub pending: i64,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct AccountStatus {
    pub id: String,
    pub address: String,
    pub name: String,
    pub syncing: bool,
    pub offline: bool,
    pub error: Option<String>,
    pub last_sync: Option<i64>,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SkippedStatus {
    pub address: String,
    pub reason: String,
}

pub static STATUS: std::sync::Mutex<Option<Status>> = std::sync::Mutex::new(None);

fn publish(state: &Shared) {
    let s = state.borrow();
    let rows = s.db.accounts().unwrap_or_default();
    let status = Status {
        accounts: s
            .accounts
            .iter()
            .map(|a| {
                let row = rows.iter().find(|r| r.id == a.id);
                AccountStatus {
                    id: a.id.clone(),
                    address: a.address.clone(),
                    name: a.name.clone(),
                    syncing: s.syncing.contains(&a.id),
                    offline: s.offline.contains(&a.id),
                    error: row.and_then(|r| r.error.clone()),
                    last_sync: row.and_then(|r| r.last_sync),
                }
            })
            .collect(),
        skipped: s.skipped.iter().map(|k| SkippedStatus { address: k.address.clone(), reason: k.reason.clone() }).collect(),
        pending: s.db.pending_ops().unwrap_or(0),
    };
    if let Ok(mut slot) = STATUS.lock() {
        *slot = Some(status);
    }
}
