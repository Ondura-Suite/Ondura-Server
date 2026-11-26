# Collaborative documents

Ondura Server can be a collaboration server, not only a mail server: several
people signed in with their accounts can edit the same document at the same
time. This is an optional feature — set `enabled = false` under `[collab]` to run
a mail-only server, in which case every endpoint below returns `404`.

A document is plain text. Each belongs to one owner and can be shared two ways —
with other Ondura accounts (read-only or read-write), or by a share link that
anyone can open, signed in or as a guest, with the view or edit permission the
link carries. Editing is real-time over a WebSocket; the server reconciles
concurrent edits with Operational Transform, so no edit is lost and every
participant converges on the same text.

In the webmail/desktop editor, the owner's **Share** dialog creates and copies
share links (view or edit), changes a link's permission or revokes it, and
invites accounts by e-mail. A link looks like `https://host/mail#link=<token>`;
opening it signs-in users straight into the document and lets guests edit it
without an account under a "Guest" identity.

## Authentication

Every endpoint uses the account. Two ways:

- **HTTP Basic** (`Authorization: Basic …` with the e-mail address and
  password) — for native clients such as the Ondura Mail app.
- **Webmail session cookie** — used automatically by the browser webmail.

Mutating REST calls must also send the header `X-Ondura-Collab: 1` (a CSRF
guard; native clients using Basic auth can simply always send it).

## REST API

Base path `…/collab/api`. Bodies and responses are JSON.

| Method & path | Purpose |
| --- | --- |
| `GET /docs` | List documents the user owns or that are shared with them. |
| `POST /docs` | Create a document. Body `{ "name": "…" }`. Returns `{ "id": N }`. |
| `GET /docs/{id}` | Full document: `name`, `revision`, `content`, `owned`, `canWrite`, `shares`. |
| `POST /docs/{id}/rename` | Owner only. Body `{ "name": "…" }`. |
| `DELETE /docs/{id}` | Owner only. |
| `POST /docs/{id}/share` | Owner only. Body `{ "email": "…", "canWrite": true }`. |
| `POST /docs/{id}/unshare` | Owner only. Body `{ "email": "…" }`. |
| `POST /docs/{id}/link` | Owner only. Create a share link. Body `{ "canWrite": true }`. Returns `{ "token": "…" }`. |
| `POST /docs/{id}/link/update` | Owner only. Change a link's permission. Body `{ "token": "…", "canWrite": false }`. |
| `POST /docs/{id}/link/revoke` | Owner only. Body `{ "token": "…" }`. |
| `GET /link/{token}` | **Public, no auth.** Resolve a share link to `{ "docId": N, "title": "…", "canWrite": … }`. |
| `GET /docs/{id}/ops?since=R` | Operations committed after revision `R` (for reconnect/catch-up). |
| `GET /docs/{id}/history` | Revision log: each entry has `rev`, editor `email`/`name`, `ts`, and `added`/`removed` unit counts. |
| `GET /docs/{id}/at/{rev}` | The document content as it stood at revision `rev` (replayed from the log). |

## Real-time editing (WebSocket)

Connect to `…/collab/ws?doc={id}` (use `wss://` behind TLS). The same account
authentication applies. A guest connects with `…/collab/ws?link={token}` and no
credentials; the link's permission decides whether they may edit. The connection
speaks JSON text frames.

An **operation** is the compact array form: a positive number *retains* that
many UTF-16 units, a negative number *deletes* that many, and a string is
*inserted*. Example: `[5, " world", -3]` keeps 5 units, inserts `" world"`,
deletes 3. Positions are UTF-16 code units, matching JavaScript string offsets.

**Server → client**

- `{"type":"init","rev":R,"content":"…","write":bool,"name":…,"color":…}` — sent
  once on connect: the current revision and full text.
- `{"type":"ack","rev":R,"op":[…]}` — your submitted operation was committed as
  revision `R`; `op` is the (possibly transformed) operation that was actually
  applied. Advance your base revision to `R`.
- `{"type":"op","rev":R,"op":[…],"by":"name"}` — another participant's operation,
  now committed at revision `R`. Apply it (transforming it against any of your
  own not-yet-acknowledged operations).
- `{"type":"presence","users":[{"name","color"}]}` — who is in the document.
- `{"type":"cursor","by":id,"name","color","pos","anchor"}` — another
  participant's caret (optional to display).
- `{"type":"error","message":"…","rev":R}` — a rejected operation; `rev` is the
  current server revision so the client can resynchronise.

**Client → server**

- `{"type":"op","rev":R,"op":[…]}` — submit an operation made against the
  revision `R` you last knew. The server transforms it over anything committed
  since and replies with `ack`.
- `{"type":"cursor","pos":N,"anchor":M}` — report your caret (broadcast to
  others). The webmail shows each remote caret as a coloured marker with the
  participant's name.

### Client algorithm

The standard OT client keeps three states — *synchronised*, *awaiting ack*, and
*awaiting ack with a buffer* — and transforms incoming operations against its
own pending and buffered operations. The browser webmail implements exactly this
(`crates/server/src/webmail/index.html`, the collab section) and is a working
reference. The server-side transform is in
`crates/server/src/collab/` (`ot.rs` and `docstore::apply_op`).

## Discovery

The identity document at `/.well-known/ondura-server` reports whether collab is
enabled and its endpoints:

```json
{
  "features": { "collab": true, "webmail": true, "adminCenter": true },
  "endpoints": {
    "collab": "https://mail.example.com/collab",
    "collabWebsocket": "wss://mail.example.com/collab/ws"
  }
}
```
