# Mail testing

Never against a real mailbox during development.

## Fixtures

- MIME: plain, alternative, mixed with attachments, nested, broken charsets and encodings.
- Hostile HTML: script, SVG, CSS that loads URLs, `javascript:` links, forms, iframes,
  tracking pixels. The sanitizer output is compared, and the WebKit view's scheme handler
  counts requests: zero remote loads by default.
- Threading, FTS queries, Screener and category rules.

## Local servers

`tools/test-imap.sh` starts a throwaway Dovecot (IMAP over TLS with a test CA,
CONDSTORE, QRESYNC, MOVE, IDLE, SPECIAL-USE) as the current user in a scratch
directory; the tests run an SMTP sink themselves (`nixe-mail-core`'s `testing`
feature). Each test signs in as its own user, so each starts with an empty store.

`nixe-mail-core` (`tests/imap.rs`):

- first sync, new mail, nothing changed → nothing fetched
- another client's flag changes and expunges
- UIDVALIDITY reset (mailbox recreated on the server)
- local and server flag changes merging
- moves to Archive and Trash, deleting from Trash
- a change waiting while the server is down, sent after it restarts
- a refused password is a sign-in error, not a retry
- sending (recipients include Bcc, the message doesn't), the copy appended to Sent
- account removal freeing its stored files and keeping the server's mail

`nixe-mail-core` (`tests/graph.rs`, no Dovecot needed) runs against the fake Graph server
in `fake_graph` (the `testing` feature; token `secret`):

- folders, children and roles; Outlook's own folders left out; recent bodies fetched
- another client's read, delete and new mail coming down through delta; nothing changed → nothing
- flags, moves and deletes made here reaching the server; a change to a gone message dropped
- the first sync keeping the newest window; an expired delta link starting over
- a refused token is a sign-in error; sending keeps Bcc and leaves the Sent copy to Outlook

`nixe-maild` (`tests/session.rs`) also runs a Microsoft 365 account against it: sync,
polling, flag and archive, and sending.

`nixe-maild` (`tests/session.rs`), on a private session bus with scratch XDG
directories, playing the notification server: first sync, IDLE noticing new mail
and a notification for it, none while the window is active, flags and archive
reaching the server, a send going out exactly once with its copy in Sent, and
errors returned as D-Bus errors.

Still to come: an expired OAuth2 token through a mock credential source.

## Security

Scan `mail.sqlite`, `settings.toml` and logs for credential-shaped values; check file
modes; check `mail.send` is refused without approval; prompt-injection fixtures stay
marked untrusted.

## UI

Real-window screenshots at 1120 and 800 px with the Omarchy theme in a scratch
`XDG_STATE_HOME`.

```sh
cargo test --locked --workspace
tools/test-imap.sh            # needs dovecot and openssl
cargo clippy --locked --workspace --all-targets -- -D warnings
```
