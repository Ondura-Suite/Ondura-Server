# Nextcloud-compatible access

The Ondura cloud (see `cloud.md`) speaks enough of the Nextcloud protocol that
the official **Nextcloud mobile and desktop clients** can connect to a user's
personal drive. The main use is **automatic photo upload** from a phone.

This surface is part of the cloud module and turns off with it
(`[cloud] enabled = false`).

## Per-user, off by default

The interface is **disabled per mailbox by default**. A user turns it on under
**Settings → Mobile app (Nextcloud)** in webmail; until then every WebDAV and
OCS request for that account is rejected and the login flow refuses to grant an
app password. The administrator can also turn the whole module off with
`[cloud] enabled = false`.

## Connecting the Nextcloud app

1. In the Nextcloud app, enter the server address — your Ondura host, e.g.
   `https://mail.mirranet.de`. The app detects the server via `/status.php`.
2. The app opens a browser page (Login Flow v2). Sign in with the mailbox
   e-mail and password and press **Grant access**.
3. The app receives an **app password** and connects. Everything the app does
   from then on maps to the user's personal drive (**My Files**).
4. Turn on **Auto upload** for the camera folder in the app; photos and videos
   are uploaded into the chosen folder in the personal drive.

Auto-upload uses WebDAV, with chunked uploads for large media, so big videos
upload reliably.

## What is implemented

- `GET /status.php` — server discovery (reports a Nextcloud-compatible version).
- **Login Flow v2** — `POST /index.php/login/v2`, the browser grant page at
  `/index.php/login/v2/flow/<token>`, and `POST /index.php/login/v2/poll`. It
  issues an app password stored per account.
- **OCS API** — `GET /ocs/v{1,2}.php/cloud/capabilities` and `.../cloud/user`
  (JSON), enough for the client to learn chunking support and the user's quota.
- **WebDAV** at `/remote.php/dav/files/<user>/…` and `/remote.php/webdav/…`,
  mapped to the user's personal drive: `OPTIONS`, `PROPFIND` (Depth 0/1), `GET`,
  `HEAD`, `PUT`, `MKCOL`, `DELETE`, `MOVE`, `PROPPATCH`. `X-OC-Mtime` is honoured
  so an uploaded photo keeps its original timestamp.
- **Chunked uploads (v2)** at `/remote.php/dav/uploads/<user>/<transfer>/…`:
  `MKCOL` opens a transfer, `PUT` stores numbered chunks, and `MOVE …/.file`
  to a destination assembles them into the final file.

## Authentication

Clients use HTTP Basic auth with the mailbox e-mail and **either** the account
password **or** an app password obtained through the login flow. App passwords
are long-lived tokens; each is stored and can be used until removed.

## Notes and limits

- WebDAV is scoped to the authenticated user's **personal drive**. Shared
  libraries are managed in webmail, not over this endpoint (yet).
- File versioning, trash/undelete and sharing via OCS are reported as
  unsupported in capabilities.
- The server identifies itself with a recent Nextcloud version string so that
  the client enables modern login and chunking; it is a compatibility shim, not
  a full Nextcloud server.
