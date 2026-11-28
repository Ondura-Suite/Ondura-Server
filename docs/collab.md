# Collaborative documents

Ondura Server can be a collaboration server, not only a mail server: several
people signed in with their accounts can edit the same document at the same
time. This is an optional feature — set `enabled = false` under `[collab]` to run
a mail-only server, in which case every endpoint below returns `404`.

Documents live inside the cloud storage (see `cloud.md`): each is an item in a
drive, created and opened from the **Files** area, not a separate list.

A document is text written in a small Markdown dialect (headings, bold, italic,
strikethrough, inline code, links, bullet and numbered lists, task lists, quotes,
code blocks and horizontal rules). The server stores the text as typed; the
formatting is applied when the document is previewed or exported. Each belongs to one owner and can be shared two ways —
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

Documents are created and listed through the cloud API (`cloud.md`): a document
is an item in a drive, made with `POST /mail/api/cloud/doc` and found by browsing
its drive. The endpoints here operate on an existing document by id.

| Method & path | Purpose |
| --- | --- |
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
| `GET /docs/{id}/comments` | All comments, oldest first: `id`, `parentId` (replies), `author`, `email`, `start`/`end` (anchored range), `quote`, `body`, `created`, `resolved`. |
| `POST /docs/{id}/comments` | Add a comment or reply. Body `{ "body": "…", "start": N, "end": M, "parent": null }`. Returns `{ "id": N, "noAccess": ["…"] }`. Needs read access. |
| `POST /docs/{id}/comments/{cid}/resolve` | Body `{ "resolved": true }`. The comment's author or anyone who can edit. |
| `DELETE /docs/{id}/comments/{cid}` | The comment's author or anyone who can edit; replies go with it. |
| `GET /docs/{id}/export?format=md\|txt\|html\|docx` | Download the document. `html` is a standalone page, `docx` a Word document; both carry the formatting. |
| `POST /render` | Body `{ "text": "…" }`. Returns `{ "html": "…" }`, the preview rendering. Signed-in users, or a guest passing `?link={token}` with a valid share link. |

### Comments

A comment is anchored to a range of the text (`start`/`end`, UTF-16 offsets,
like operations). As the document is edited, the server moves every anchor
along with the text it was attached to, so a comment stays on its passage; if
the passage is deleted entirely, the anchor collapses to where it stood.
Replies reference their parent with `parent` and are not anchored themselves.

A comment body may mention accounts as `@user@example.com`. Each mentioned
account that can open the document gets a notice in the web app (see
`webmail.md`) and, if it has a mailbox, an e-mail with the comment and a link
to the document. Mentioned accounts without access are not notified; their
addresses come back in `noAccess`, so the client can offer to share the
document with them.

The preview HTML escapes all text and only produces links with `http`, `https`
or `mailto` targets.

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
- `{"type":"comments"}` — the comments changed (added, resolved or deleted);
  fetch them again.
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
