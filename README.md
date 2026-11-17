# Mail

Mail for NyxOS and Omarchy: your Gmail, iCloud, Fastmail and other IMAP mail accounts in one inbox,
readable offline and driven from the keyboard. Part of the
[Nyx](https://github.com/design-nexus/nyx) apps.

- Every account's Inbox, Drafts, Sent, Archive, Junk and Trash together, then each account's folders and labels
- Conversations, search across all of your mail, and new-mail notifications
- Write, reply, reply to all and forward, with signatures, attachments and drafts that save as you type
- Archive, Trash, Junk, star and mark read, each with Undo
- Messages show without images from the web until you allow them for a message or a sender
- Imbox, The Feed and Paper Trail sort mail by kind, and the Screener holds mail from first-time senders if you switch it on
- Snooze a conversation, send a message later, or get a reminder if nobody replies
- A Senders page with everyone who writes: where their mail goes, Archive all, Unsubscribe
- Works offline and sends your changes when you're back online
- Follows the desktop theme; works from the keyboard and tiled at half-screen

Accounts are added in **Settings → Online Accounts** with Mail switched on, with an app
password from the provider (Gmail and iCloud make them in their security settings).
Microsoft accounts, and signing in to Google directly, need an OAuth client you register
with them yourself (Online Accounts links the steps); work and school Microsoft 365
accounts aren't supported. Mail never stores passwords or sign-ins; they stay there. Your messages are kept on this computer in
`~/.local/share/nyx-mail/`. Sorting, snoozes and scheduled sends happen on this
computer, so it has to be on for a scheduled message to go out.

## Install

On NyxOS, choose Mail in Welcome when you set up, or install it later from the Store or with `nyx install nyx-mail`. On Omarchy:

```sh
curl -fsSL https://raw.githubusercontent.com/design-nexus/nyx-mail/main/install.sh | bash
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
| Shift+U | Show only unread |
| Z | Snooze |
| 1 / 2 / 3 / 4 | Inbox, Drafts, Sent, Archive |
| Ctrl+R | Check for mail |
| Ctrl+F | Find messages |
| F1 | All shortcuts |
