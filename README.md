# Mail

Mail for Omarchy: your Google, Microsoft 365, Outlook.com, Fastmail, iCloud and other mail accounts in one inbox,
readable offline and driven from the keyboard. Part of the
[Nixe](https://github.com/design-nexus/nixe) apps.

- Every account's Inbox, Drafts, Sent, Archive, Junk and Trash together, then each account's folders and labels
- Conversations, search across all of your mail, and new-mail notifications
- Write, reply, reply to all and forward, with signatures, attachments and drafts that save as you type
- Archive, Trash, Junk, star and mark read, each with Undo
- Messages show without images from the web until you allow them for a message or a sender
- Imbox, The Feed and Paper Trail sort mail by kind, and the Screener holds mail from first-time senders if you switch it on
- Snooze a conversation, send a message later, or get a reminder if nobody replies
- A Senders page with everyone who writes: where their mail goes, Archive all, Unsubscribe
- Works offline and sends your changes when you're back online
- Follows your Omarchy theme; works from the keyboard and tiled at half-screen

Accounts are added in **Settings → Online Accounts** with Mail switched on. Mail never
stores passwords or sign-ins; they stay there. Your messages are kept on this computer in
`~/.local/share/nixe-mail/`. Sorting, snoozes and scheduled sends happen on this
computer, so it has to be on for a scheduled message to go out.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/design-nexus/nixe-mail/main/install.sh | bash
```

## Keys

| Key | Action |
| --- | --- |
| C | New message |
| J / K | Next / previous conversation |
| R / A / F | Reply, reply to all, forward |
| E | Archive |
| # | Move to Trash |
| ! | Move to Junk |
| S | Star or unstar |
| U | Mark read or unread |
| Z | Snooze |
| 1 / 2 / 3 / 4 | Inbox, Drafts, Sent, Archive |
| Ctrl+R | Check for mail |
| Ctrl+F | Find messages |
| F1 | All shortcuts |
