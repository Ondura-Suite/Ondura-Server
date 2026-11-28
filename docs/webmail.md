# Webmail

Ondura Server includes browser webmail at `/mail`, so a mailbox can be used
without setting up a mail program. It offers mail, calendar and contacts and
uses the same storage as ActiveSync and EWS — anything changed in webmail shows
up in connected clients and vice versa.

Signing in lands on a **home dashboard** with a tile for each app, and each app
has its own URL:

| App | URL | Contents |
| --- | --- | --- |
| Mail | `/mail` | Mail, calendar and contacts (like Outlook), switched from the app's own left navigation. Present only for accounts with a mailbox. |
| Files | `/files` | The personal drive (OneDrive-style), with a Recent list, previews, share links and version history. |
| SharePoint | `/sharepoint`, `/sharepoint/<id>` | Shared libraries; each library has its own link, so it can be shared or bookmarked. |
| Tasks | `/tasks` | Tasks. |
| Notes | `/notes` | Notes. |
| Home | `/` | The dashboard. |

Each app is its own space with its own left navigation. Switch apps from the
home tiles or the app launcher (the grid button, top left); the Ondura logo
returns to the dashboard. Admins also get an **Admin Center** tile linking to
`/admin`. The Compose button and mail search appear only inside the Mail app.

An account does not have to be a mailbox (see `admin.md`). An account without a
mailbox signs in and uses Files, SharePoint, Tasks and Notes as usual, but the
Mail, Calendar and Contacts app is not shown.

![Webmail](screenshots/webmail.png)

## Signing in

Open `https://<your host name>/mail` and sign in with any enabled user account
(the same address and password used for a mail client, when the account has a
mailbox). The root URL `/` redirects here. Room and shared mailboxes cannot sign
in interactively.

The interface is in German or English (switch with the globe icon) and follows
the system's light or dark appearance.

## What it does

**Mail** — folders with unread counts, message list with search across all mail
folders, reading pane, compose with attachments and signature, reply, reply all
and forward, Bcc, flag, mark read/unread, archive, move between folders, print,
and delete (to Deleted Items, then permanently). The reading pane carries a full
command bar and every message row has a right-click context menu with the same
actions. Select several messages at once to categorize, flag, archive, move,
delete or mark them read/unread in one go, and empty Deleted Items or mark a
whole folder read. Create, rename and delete your own folders. HTML messages are
shown in a sandboxed frame so message content cannot run scripts. Drafts reopen
in the composer and are kept up to date automatically while you write. A filter
bar above the list narrows it to Unread, Flagged or messages with attachments,
and a select-all box picks every message currently shown. A conversation-view
toggle in the list header groups a folder's messages into conversations by the
Message-ID / In-Reply-To / References chain (with a shared-subject fallback),
newest first, that expand and collapse; the choice is remembered. ActiveSync
derives its conversation id from the same thread root, so phones group the same
conversations.

**Keyboard shortcuts** — the mail view is fully keyboard-drivable: arrow keys or
`j`/`k` move through the list, `Enter` opens, `c`/`n` compose, `r` reply,
`Shift+R` reply all, `f` forward, `e` archive, `Delete` delete, `u` toggle
read/unread, `m` move, `/` focuses search and `Esc` steps back out. Press `?`
for the full list.

**Categories** — colour-coded labels you define once and assign to messages from
the reading pane, the context menu or a multi-selection. Click a category chip
to filter the current folder to it. Manage categories (create, rename, recolour,
delete) from the account menu; deleting or renaming a category updates every
message that carried it. Category names travel with the message over EWS and
ActiveSync, so a category set in webmail also shows in Outlook and on phones.
Outlook also reads the colours: the server serves the webmail categories as the
Exchange master category list (EWS GetUserConfiguration "CategoryList"), and a
category created or recoloured in Outlook is merged back into the registry.

**Rules** — inbox rules run automatically on newly delivered mail. Each rule
matches on From, To or Subject (all or any conditions) and can move the message
to a folder, mark it read, categorize it or send it to Deleted Items, optionally
stopping further rules. Manage rules from the account menu. Because rules run on
the server at delivery, they apply to every client; Outlook can also display
them, and create or edit them over EWS (UpdateInboxRules).

**Sweep** — right-click a message and choose Sweep to act on every message from
that sender in the current folder at once: delete them all, keep only the newest,
or move them all to another folder.

**Calendar** — a month view with your events; series are expanded by the
server. Create and edit events with a description, attendees, a reminder, a
free/busy status, a private flag and a repeat pattern (daily, weekdays, weekly,
monthly, yearly, with an optional end date). Opening an occurrence of a series
asks whether to work on that occurrence or on the whole series: an occurrence can
be moved, renamed or deleted on its own, and the rest of the series stays as it
is. Accept, decline or tentatively answer meeting invitations, for one occurrence
or the whole series.

**Contacts** — list, create, edit and delete contacts with the common fields.

**Tasks and notes** — keep a task list (with due dates and completion) and
simple notes.

**Settings** (gear icon) — change your display name and signature, set an
out-of-office auto-reply (with separate texts for internal and external
senders), and change your password.

**Multiple addresses** — a mailbox can have several e-mail addresses
(aliases); mail to any of them arrives in the one inbox. An administrator
assigns aliases in the Admin Center or with `ondura-server alias add`. When a
mailbox has more than one address, the composer shows a **From** picker so you
can choose which address a message is sent from.

## Turning it off

Webmail is on by default. To disable it, set `webmail = false` in the `[http]`
section of the configuration and restart the service; `/mail` then returns
404 and `/` redirects to the Admin Center instead.

```toml
[http]
webmail = true   # set to false to disable browser webmail
```

## Security notes

- The session cookie is `HttpOnly`, `SameSite=Strict` and (over HTTPS)
  `Secure`, scoped to `/mail`. Mutating requests require an `X-Ondura-Mail`
  header, which blocks cross-site requests.
- Each mailbox only ever sees its own items; every lookup is checked against the
  signed-in user.
- Serve webmail over HTTPS only. Behind a TLS-terminating reverse proxy, set
  `behind_proxy = true`.

**Meetings** — adding attendees to a calendar event turns it into a meeting:
saving it sends an iCalendar invitation (iMIP `METHOD:REQUEST`) to each
attendee, an update raises the meeting's sequence so clients accept it, an
attendee taken off the list gets a cancellation, and deleting the meeting — or a
single occurrence of it — sends a cancellation, matching Outlook and ActiveSync.
The editor lists each attendee's response. Below the attendees an availability view
shows everyone's busy times for the meeting day next to the chosen slot, and
**Next free time** moves the meeting to the first weekday slot within working
hours (07:00–20:00) when everybody with known availability is free. Accounts on
the same server always share their free/busy times; event titles appear only
for calendars shared at least for reading.
Attendees accept, decline or tentatively accept from the invitation, which
sends a reply back to the organizer; incoming invitations and replies update
the calendar automatically on delivery.
