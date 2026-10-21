# Mail security

1. No passwords or tokens in `settings.toml`, `mail.sqlite`, logs or command lines.
   Credentials come from GOA in-process for each connection, as a redacted `Secret`.
2. TLS only. Certificate errors fail visibly.
3. `~/.local/share/nixe-mail` is 0700; files in it 0600.
4. HTML is sanitized, rendered with JavaScript off and with no network unless images are
   allowed. Remote images are off by default, which also blocks tracking pixels.
5. Attachments are never executed. Opening one copies it to a 0600 temp file and hands it
   to `xdg-open`; executable types ask first.
6. Nothing is sent without the user pressing Send, or approving `mail.send` with a key
   press in Assistant.
7. Mail content given to Assistant is marked untrusted. Instructions inside mail are data.
8. AI features use the local Assistant (`complete` IPC). Nothing goes to an outside
   service without the user choosing it.
9. Removing an account stops sync; deleting the local copy is offered, and remote mail is
   never touched.
