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

### 1. Vertical slice
- [ ] `nixe-mail-core`: schema, store, FTS5
- [ ] `nixe-maild`: IMAP sync of folders and messages against local Dovecot; GOA credentials
- [ ] UI: sidebar, message list, reader (WebKitGTK 6, locked down)
- [ ] Offline reading; auth failure → banner that opens Settings

### 2. MVP
- [ ] Unified inbox, multiple accounts, identities and aliases
- [ ] IDLE, CONDSTORE/QRESYNC where offered; Gmail labels and thread ids
- [ ] Compose, reply, reply all, forward; drafts autosave; signatures; attachments
- [ ] Archive, delete, spam, trash; flags; Undo toasts
- [ ] Offline ops queue with backoff and conflict detection
- [ ] Threading; search; notifications; per-account settings; keyboard shortcuts
- [ ] Remote images off by default, per sender or per message
- [ ] Control tools for Assistant

### 3. Advanced
- [ ] Screener for first-time senders; Imbox, Feed, Paper Trail; switch between this and folders without moving mail
- [ ] Snooze, send later, follow-ups (local scheduling labeled as such; server scheduling when offered)
- [ ] Sender dashboard and bulk rules
- [ ] Assistant: summarize, draft, categorize, important (local processing)
- [ ] "Remind me" through Nixe Reminders
- [ ] Microsoft 365 through Microsoft Graph
- [ ] OpenPGP / S/MIME

## Out of scope

Hosted backends; storing credentials; sending without the user's explicit action.
