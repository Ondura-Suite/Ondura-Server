# Ondura client API

The native API an Ondura client uses to talk to an Ondura server, instead of
EWS or ActiveSync. It is a thin JSON/REST surface over the same mailbox the
other protocols serve, plus incremental sync and a push WebSocket. Other
clients keep using EWS/ActiveSync, which are unchanged.

Everything lives under `/api/v1`.

## Discovery

A client first confirms it is talking to an Ondura server and learns the
endpoints from the identity document (no authentication needed):

```
GET https://<host>/.well-known/ondura-server
```

The response lists `protocols` (includes `"OnduraClient"`) and `endpoints`,
including `clientApi` (`https://<host>/api/v1`) and `clientApiWebsocket`
(`wss://<host>/api/v1/ws`). Every response from the server also carries an
`X-Ondura-Server: Ondura-Server/<version>` header. `GET /api/v1/identity`
returns the same information scoped to the API.

## Authentication

HTTP Basic (email address and password) on every request, like EWS/ActiveSync —
no cookie needed. Every request must also carry an `X-Ondura-Client` header
(any value, e.g. the client name and version). Without it the API returns `400`
with a hint to use EWS/ActiveSync, so the native surface stays reserved for
Ondura clients.

```
Authorization: Basic base64(email:password)
X-Ondura-Client: Ondura-Mail/1.0
```

## Mailbox endpoints

These mirror the web front end exactly (same request and response shapes):

| Method & path | Purpose |
| --- | --- |
| `GET /api/v1/me` | Account and folder list (with unread counts) |
| `GET /api/v1/messages?folder=<id>&limit=&offset=` | List messages in a folder |
| `GET /api/v1/message/<id>` | Full message (headers, body, flags) |
| `POST /api/v1/send` | Send a message |
| `POST /api/v1/draft` | Save a draft |
| `POST /api/v1/flag` | Set read/flagged state |
| `POST /api/v1/move` | Move a message to another folder |
| `POST /api/v1/delete` | Delete a message |
| `POST /api/v1/bulk` | Bulk mark/move/delete |
| `GET /api/v1/calendar?…`, `POST /api/v1/event[/delete|/respond]` | Calendar |
| `GET /api/v1/contacts`, `POST /api/v1/contact[/delete]` | Contacts |
| `GET /api/v1/tasks`, `POST /api/v1/task[/delete]` | Tasks |
| `GET /api/v1/notes`, `POST /api/v1/note[/delete]` | Notes |
| `POST /api/v1/folder[/rename|/delete|/read]` | Folders |
| `GET/POST /api/v1/settings`, `POST /api/v1/password` | Settings |
| `GET /api/v1/avatar/<id-or-email>` | Fetch a user's avatar image |
| `POST /api/v1/avatar` (raw image body, `Content-Type`) | Set the caller's avatar |
| `DELETE /api/v1/avatar` | Remove the caller's avatar |

`GET /api/v1/me` includes the caller's own numeric `id` and an `avatar` field
(the time the avatar was last set, or `null`), so the client can build avatar
URLs and cache-bust them. Avatars are PNG/JPEG/WebP/GIF up to 1 MiB.

## Incremental sync

The mailbox uses a per-user monotonic change counter (*modseq*). The client
keeps the last modseq it has seen and asks for everything newer:

```
GET /api/v1/sync?since=<modseq>
```

```json
{
  "modseq": 412,
  "changes": [
    { "type": "add",    "folder": 3, "item": { "id": 91, "kind": "mail", "subject": "…", "flags": 0, "received": 1764000000, "from": {…}, "preview": "…", "hasAttachments": false } },
    { "type": "update", "folder": 3, "item": { "id": 74, "flags": 1, … } },
    { "type": "delete", "id": 70, "modseq": 410 }
  ]
}
```

Apply the changes, then store the returned `modseq` as the next `since`. Start
a fresh client at `since=0` to get the current state as a series of `add`s. PIM
items (calendar, contacts, tasks, notes) carry their payload in the item's
`data` field.

## Push

Open the WebSocket to be told when to sync, instead of polling:

```
wss://<host>/api/v1/ws        (Authorization + X-Ondura-Client on the upgrade)
```

Messages are small JSON notifications:

- `{"type":"sync"}` — do a full `/sync` (sent on connect and after the server
  falls behind).
- `{"type":"changed","folder":<id>}` — something changed; call `/sync` to fetch
  the delta.

The client never needs to send anything; it reacts to these by calling
`/api/v1/sync`.
