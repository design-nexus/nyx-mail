# Mail — notes for working on this app

- GTK4 (`gtk4` 0.11, `v4_18`) + Rust, no libadwaita. The "sorted inbox" layout: the
  sidebar is Imbox, The Feed, Paper Trail and Screener, then Later (Snoozed, Set aside =
  flagged mail, Drafts; Scheduled and Follow-ups only while they hold something), Pinned,
  then each account's Inbox, Sent, Archive, Junk, Trash and folders, folded under its
  heading unless it's in `prefs.open_accounts` (`all` for the every-account group shown
  with several accounts). The Imbox, The Feed and the Screener are pages (`sorted.rs`)
  drawn from the list's rows: opening one selects its row and shows the reader alone,
  with Back (`window::refresh_mode`, `Ui.reading`). Settings → Imbox looks like
  (`prefs.imbox_layout = "list"`) makes the Imbox a list instead (`View::is_page`). Every other view is a 340 px
  conversation list (`gtk::ListView`, 200 rows a page, more on scroll) beside the
  reading pane (`reader.rs`). Under 980 px the sidebar is an icon rail.
- The window never writes mail data. It reads `mail.sqlite` read-only (`data.rs`) and
  asks nyx-maild for every change over `MailEngine1` (`engine.rs`); the engine's
  `Changed` signal reloads the window. The app starts the engine on open.
- Reader: native headers; plain text as labels; HTML (sanitized by
  `nyx_mail_core::mime::sanitize`) in a WebKit view each, with page script off
  (`enable_javascript_markup = false`; only our measuring runs, in an isolated world),
  an ephemeral network session, and a CSP that blocks the network until the user shows
  images (per message, or always for a sender: `prefs.remote_senders`). `cid:` parts
  come from the store through the `nyx-mail-msg:` scheme. Links open outside;
  `mailto:` opens the composer. Attachments save, or open from a 0600 copy in the
  runtime dir; programs and scripts are never opened.
- Archive, Trash, Junk and Forget wait for their Undo toast before the engine hears
  (`window::act`); closing the window sends what's waiting.
- Compose (`compose.rs`): its own window; drafts save 2 s after a change and on close
  (engine `SaveDraft`), show in Drafts, and are deleted once sent.
- Assistant (`control.rs`): `mail.search/inbox/read` (untrusted content), `mail.open`,
  `mail.draft` (opens the composer filled in). Nothing sends; there's no `mail.send`.
  `--describe` prints these tools without a window.
- Recipient suggestions (`suggest.rs`): `Db::people` (people the user wrote to weigh 3×)
  at once, then `nyx-contacts --find` (no window). The composer's window keys defer
  while a suggestion list shows.
- `--message ID` shows a conversation (Glimpse, notifications: nyx-maild runs it when one
  is clicked). `apps/mail-glimpse` searches `mail.sqlite` in Glimpse's sandbox.
- Sorting on this computer only (`nyx_mail_core::organize`, schema v2): each message gets
  a kind when stored (Imbox, The Feed, Paper Trail; `classify`); a sender choice
  (`senders` table) wins. `SORT_VERSION` re-sorts stored mail when the rules change. The
  Screener (off by default; `settings.screener_since`) holds inbox mail from senders not
  let in; people written to are let in. Snoozes, follow-ups and send-later live in
  nyx-maild (30 s tick; `NYX_MAIL_TICK_SECONDS` in tests). `organize.rs` in the app
  has the Snooze/More menus, the banners over a conversation and the Scheduled/Sender
  cards. The reader toolbar must stay ≤ 407 px for 800 px windows beside the list: its
  Reply, Reply all and Forward labels hide there (`Ui.bar_labels`).
- Never call `selection.set_selected` while `UI` is borrowed: its handler borrows it
  (`window::select`).
- Microsoft 365 / Outlook.com (GOA `ms_graph`): `nyx_mail_core::graph` over Microsoft
  Graph (delta per folder, Graph ids mapped to UIDs in `graph_items`); maild's
  `accounts::Backend::Graph`, polled every 3 min (`NYX_MAIL_POLL_SECONDS` in tests). Never
  hold a `state.borrow()` across an `.await` (a `match` on it does).
- Right-click menus (`menus.rs`, built with `widgets::popup_menu`/`on_right_click`; Menu
  key or Shift+F10 for the selected row): list rows (a right-click selects the row
  without marking it read: `window::select_quietly`), folders (Pin, Mark all as read),
  the all-accounts mailboxes and account headings (fold, turn off in Mail). Pinned
  folders and folded accounts are prefs (`pinned` keys are `account` + U+001F + name).
  Engine settings (`notify`, `check_every`, `body_days`, `disabled_accounts`) go through
  `MailEngine1.SetSetting` into `mail.sqlite`'s settings table; turned-off accounts keep
  their mail and are filtered out by `data::accounts()`/`data::mailboxes()`.
- Checks: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `tools/test-imap.sh` (Dovecot + SMTP sink + the engine on a private bus).
  Layout: `tools/test-imap.sh --run SCRIPT` gives a scratch server; snapshots with
  `MAIL_SNAPSHOT=/tmp/m.png MAIL_SNAPSHOT_SELECT=N` (or `_COMPOSE`, `_W`), with
  `GTK_A11Y=none` on a private bus. Web content isn't in widget snapshots.
- "Add to Reminders" (`reminders.rs`): calls Reminders' `reminders.create` over its Control
  interface (D-Bus, no auto-start; starts `nyx-reminders` and retries ~15 s) with
  `source = nyx-mail:message/ID`. The desktop file handles `x-scheme-handler/nyx-mail`, and
  `main.rs` opens such a link like `--message ID`.
- Aliases (prefs `aliases`: account, name, email; Settings → Writing) join that account's
  From choices and `data::own_addresses`; a reply to a message sent to one comes from it.
- IMAP sync enables QRESYNC when offered: the CHANGEDSINCE flag fetch adds VANISHED, read
  from the session's unsolicited responses, instead of a `UID SEARCH lo:*` expunge scan.
  `NYX_MAIL_NO_QRESYNC=1` forces the old path (`tools/test-imap.sh` passes both ways).
