# Mail plan

Nixe Mail: read, search, write and file mail for the accounts connected in Settings →
Online Accounts, from the keyboard, offline as well as online. Shared foundation:
[nixe-core docs/pim](https://github.com/design-nexus/nixe-core/blob/main/docs/pim/ARCHITECTURE.md).
This replaces `~/Projects/MAIL_PLAN.md`.

| | |
| --- | --- |
| Packages | `nixe-mail` (UI), `nixe-maild` (sync engine, systemd user unit), `nixe-mail-glimpse` |
| App id | `io.github.design_nexus.nixe.Mail` |
| Data | `~/.local/share/nixe-mail/` (0700): `mail.sqlite` (WAL, FTS5), `store/` (raw messages) |
| Config | `~/.config/nixe-mail/settings.toml` (never credentials) |
| MIME | `x-scheme-handler/mailto` |

## Phases

### 1. Vertical slice (done)
- [x] `nixe-mail-core`: schema, store, FTS5
- [x] `nixe-maild`: IMAP sync of folders and messages against local Dovecot; GOA credentials
- [x] UI: sidebar, message list, reader (WebKitGTK 6, locked down)
- [x] Offline reading; auth failure → banner that opens Settings

### 2. MVP (done)
- [x] Unified inbox, multiple accounts; replies come from the account the message arrived at
- [ ] Aliases (each account sends from its own address only)
- [x] IDLE, CONDSTORE where offered; Gmail labels
- [ ] QRESYNC; Gmail thread ids (threads come from message headers)
- [x] Compose, reply, reply all, forward; drafts autosave; signatures; attachments
- [x] Archive, delete, spam, trash; flags; Undo toasts
- [x] Offline ops queue with backoff and conflict detection
- [x] Threading; search; notifications; per-account settings; keyboard shortcuts
- [x] Remote images off by default, per sender or per message
- [x] Control tools for Assistant (`mail.search/inbox/read/open/draft`; nothing sends)

### 3. Advanced
- [x] Screener for first-time senders; Imbox, Feed, Paper Trail; switch between this and folders without moving mail
- [x] Snooze, send later, follow-ups (local scheduling labeled as such)
- [ ] Server-side send later when offered (SMTP FUTURERELEASE)
- [x] Sender dashboard and bulk rules
- [ ] Assistant: summarize, categorize, important (draft is done)
- [ ] "Remind me" through Nixe Reminders
- [x] Microsoft 365 and Outlook.com through Microsoft Graph
- [ ] OpenPGP / S/MIME

## Out of scope

Hosted backends; storing credentials; sending without the user's explicit action.
