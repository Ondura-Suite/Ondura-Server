# Nextcloud-compatible access

The Ondura cloud (see `cloud.md`) speaks enough of the Nextcloud protocol that
the official **Nextcloud mobile and desktop clients** can connect to a user's
personal drive and the shared libraries they belong to. The main uses are
**automatic photo upload** from a phone and syncing files with the desktop client.

This surface is part of the cloud module and turns off with it
(`[cloud] enabled = false`).

## Per-user, off by default

The interface is **disabled per account by default**. A user turns it on under
**Settings → Mobile app (Nextcloud)** in webmail; until then every WebDAV and
OCS request for that account is rejected and the login flow refuses to grant an
app password. The administrator can also turn the whole module off with
`[cloud] enabled = false`.

## Connecting the Nextcloud app

1. In the Nextcloud app, enter the server address — your Ondura host, e.g.
   `https://mail.mirranet.de`. The app detects the server via `/status.php`.
2. The app opens a browser page (Login Flow v2). Sign in with the account
   e-mail and password and press **Grant access**.
3. The app receives an **app password** and connects. The home folder is the
   user's personal drive (**My Files**); the shared libraries appear inside it in
   a **SharePoint** folder.
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
- **WebDAV** at `/remote.php/dav/files/<user>/…` and `/remote.php/webdav/…`:
  `OPTIONS`, `PROPFIND` (Depth 0/1), `GET`, `HEAD`, `PUT`, `MKCOL`, `DELETE`,
  `MOVE`, `PROPPATCH`. `X-OC-Mtime` is honoured so an uploaded photo keeps its
  original timestamp.
- **Chunked uploads (v2)** at `/remote.php/dav/uploads/<user>/<transfer>/…`:
  `MKCOL` opens a transfer, `PUT` stores numbered chunks, and `MOVE …/.file`
  to a destination assembles them into the final file.

## Folder layout

- The WebDAV home is the personal drive.
- `SharePoint/` is a virtual folder with one folder per library the user is a
  member of (two libraries with the same name get the library id appended). A
  personal item named `SharePoint` at the root of the personal drive is not
  reachable over WebDAV.
- Library members with the **viewer** role get read-only access: the folders
  report read-only permissions and writes are refused with `403`.
- Folders with their own permissions (see `cloud.md`) follow their access list:
  a folder the account isn't on is not listed and can't be reached, and a
  read-only entry makes it read-only. Accounts that only have access to such a
  folder, without being library members, reach it through the webmail.
- A file checked out by someone else can't be overwritten, moved or deleted;
  those requests get `423 Locked`, as does deleting or moving a folder that
  contains one.
- Nothing can be created directly in `SharePoint/`, and a library folder cannot
  be renamed, moved or deleted over WebDAV.
- `MOVE` works within a drive and between drives (personal drive ↔ library, or
  library ↔ library); the contents and version history follow the item.
- `DELETE` moves the item to that drive's recycle bin, and an item replaced by a
  `MOVE` goes there too, so nothing removed from a synced device is lost.
- Overwriting a file keeps the previous contents as a version (see `cloud.md`).
- Folder ETags change whenever anything inside the folder changes, so the
  desktop client finds changes in subfolders.
- Personal-drive uploads are checked against the account's quota; libraries are
  not metered.

## Authentication

Clients use HTTP Basic auth with the account e-mail and **either** the account
password **or** an app password obtained through the login flow. App passwords
are long-lived tokens; each is stored and can be used until removed.

## Notes and limits

- Versions, the recycle bin and share links are managed in webmail; the OCS
  capabilities report them as unsupported, so the client does not offer them.
- `COPY` is not implemented and answers `501 Not Implemented`.
- The server identifies itself with a recent Nextcloud version string so that
  the client enables modern login and chunking; it is a compatibility shim, not
  a full Nextcloud server.
