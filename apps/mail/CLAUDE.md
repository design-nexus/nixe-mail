# Mail — notes for working on this app

- GTK4 (`gtk4` 0.11, `v4_18`) + Rust, no libadwaita. Music's sidebar (every account's
  Inbox, Drafts, Sent, Archive, Junk, Trash together, then each account's folders), a
  340 px conversation list (`gtk::ListView`, 200 rows a page, more on scroll), and the
  reading pane (`reader.rs`).
- The window never writes mail data. It reads `mail.sqlite` read-only (`data.rs`) and
  asks nixe-maild for every change over `MailEngine1` (`engine.rs`); the engine's
  `Changed` signal reloads the window. The app starts the engine on open.
- Reader: native headers; plain text as labels; HTML (sanitized by
  `nixe_mail_core::mime::sanitize`) in a WebKit view each, with page script off
  (`enable_javascript_markup = false`; only our measuring runs, in an isolated world),
  an ephemeral network session, and a CSP that blocks the network until the user shows
  images (per message, or always for a sender: `prefs.remote_senders`). `cid:` parts
  come from the store through the `nixe-mail-msg:` scheme. Links open outside;
  `mailto:` opens the composer. Attachments save, or open from a 0600 copy in the
  runtime dir; programs and scripts are never opened.
- Archive, Trash, Junk and Forget wait for their Undo toast before the engine hears
  (`window::act`); closing the window sends what's waiting.
- Compose (`compose.rs`): its own window; drafts save 2 s after a change and on close
  (engine `SaveDraft`), show in Drafts, and are deleted once sent.
- Never call `selection.set_selected` while `UI` is borrowed: its handler borrows it
  (`window::select`).
- Checks: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `tools/test-imap.sh` (Dovecot + SMTP sink + the engine on a private bus).
  Layout: `tools/test-imap.sh --run SCRIPT` gives a scratch server; snapshots with
  `MAIL_SNAPSHOT=/tmp/m.png MAIL_SNAPSHOT_SELECT=N` (or `_COMPOSE`, `_W`), with
  `GTK_A11Y=none` on a private bus. Web content isn't in widget snapshots.
