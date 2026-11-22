# Webmail

Ondura Server includes browser webmail at `/mail`, so a mailbox can be used
without setting up a mail program. It offers mail, calendar and contacts and
uses the same storage as ActiveSync and EWS — anything changed in webmail shows
up in connected clients and vice versa.

![Webmail](screenshots/webmail.png)

## Signing in

Open `https://<your host name>/mail` and sign in with any enabled user mailbox
(the same address and password used for a mail client). The root URL `/`
redirects here. Room and shared mailboxes cannot sign in interactively.

The interface is in German or English (switch with the globe icon) and follows
the system's light or dark appearance.

## What it does

**Mail** — folders with unread counts, message list with search across all mail
folders, reading pane, compose with attachments and signature, reply and
forward, flag, mark read/unread, move between folders, and delete (to Deleted
Items, then permanently). Select several messages at once to move, delete or
mark them read in one go, and empty Deleted Items or mark a whole folder read.
Create, rename and delete your own folders. HTML messages are shown in a
sandboxed frame so message content cannot run scripts. Drafts reopen in the
composer.

**Calendar** — a month view with your events; series are expanded by the
server. Create and edit events, delete them, and accept, decline or tentatively
answer meeting invitations.

**Contacts** — list, create, edit and delete contacts with the common fields.

**Tasks and notes** — keep a task list (with due dates and completion) and
simple notes.

**Settings** (gear icon) — change your display name and signature, set an
out-of-office auto-reply (with separate texts for internal and external
senders), and change your password.

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
