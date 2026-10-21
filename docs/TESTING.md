# Mail testing

Never against a real mailbox during development.

## Fixtures

- MIME: plain, alternative, mixed with attachments, nested, broken charsets and encodings.
- Hostile HTML: script, SVG, CSS that loads URLs, `javascript:` links, forms, iframes,
  tracking pixels. The sanitizer output is compared, and the WebKit view's scheme handler
  counts requests: zero remote loads by default.
- Threading, FTS queries, Screener and category rules.

## Local servers

A test harness starts Dovecot (IMAP, CONDSTORE, IDLE) and an SMTP sink in a scratch
directory, locally and in CI from Arch packages. Cases:

- first sync, incremental sync, UIDVALIDITY reset
- flag conflicts; ops queued offline and replayed after a server restart
- send, drafts, append to Sent
- authentication failure through a mock credential source → needs-sign-in state
- account removal and local cleanup

## Security

Scan `mail.sqlite`, `settings.toml` and logs for credential-shaped values; check file
modes; check `mail.send` is refused without approval; prompt-injection fixtures stay
marked untrusted.

## UI

Real-window screenshots at 1120 and 800 px with the Omarchy theme in a scratch
`XDG_STATE_HOME`.

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```
