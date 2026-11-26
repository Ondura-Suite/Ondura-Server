//! Native Ondura client API (`/api/v1`).
//!
//! The Ondura client detects an Ondura server (via the identity document and
//! the `X-Ondura-Server` header) and then talks to this API instead of EWS or
//! ActiveSync. Other clients never use it and keep working over EWS/EAS, which
//! are unchanged.
//!
//! It is deliberately a thin layer: the mailbox operations reuse the same
//! handlers as the web front end (`webmail::dispatch`), so both speak one set of
//! endpoints. On top it adds what a sync client needs and the web app does not:
//! an incremental delta feed (`/sync`, by modseq) and a WebSocket that wakes the
//! client when something changes (`/ws`).
//!
//! Authentication is HTTP Basic (address and password), like EWS/ActiveSync, so
//! a native app needs no cookie. Every request must carry an `X-Ondura-Client`
//! header; without it the API responds with a hint to use EWS/ActiveSync, so
//! the native surface is reserved for Ondura clients.

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;

use crate::app::Shared;
use crate::model::flags;
use crate::store::{Change, Item, NewMail, User};
use crate::webmail::{self, ApiResult, bad, internal, own_item};

/// Writer tag of changes made through this API (see `webmail::WRITER`).
const WRITER: &str = "ondura-client";

/// The header an Ondura client sends to opt into the native API.
const CLIENT_HEADER: &str = "X-Ondura-Client";

/// The API version served under `/api/v1`.
pub const API_VERSION: &str = "v1";

/// Handle an `/api/...` request. `path` is the full request path.
pub async fn handle(app: Shared, method: Method, path: &str, query: &str, headers: &HeaderMap, body: Bytes, ip: std::net::IpAddr) -> Response {
    // Only versioned paths exist.
    let Some(rest) = path.strip_prefix("/api/v1") else {
        return (StatusCode::NOT_FOUND, "Unknown API version").into_response();
    };
    let rest = if rest.is_empty() { "/" } else { rest };

    // Identity is public so a client can discover the API before signing in.
    if rest == "/identity" || rest == "/" {
        return axum::Json(identity(&app)).into_response();
    }

    // Reserve the native API for Ondura clients.
    if !headers.contains_key(CLIENT_HEADER) {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "This endpoint is for the Ondura client. Other clients use EWS or ActiveSync.",
                "ews": "/EWS/Exchange.asmx",
                "activesync": "/Microsoft-Server-ActiveSync",
            })),
        )
            .into_response();
    }

    // Authenticate with the account (HTTP Basic), like EWS/ActiveSync.
    let user = match crate::http::basic_auth(&app, headers, ip).await {
        Ok(u) => u,
        Err(r) => return *r,
    };

    match (&method, rest) {
        // Avatars: set/remove the caller's own, fetch anyone's by id or email.
        (&Method::POST, "/avatar") => set_avatar(&app, user.id, content_type_header(headers), &body),
        (&Method::DELETE, "/avatar") => delete_avatar(&app, user.id),
        (&Method::GET, p) if p.starts_with("/avatar/") => serve_avatar(&app, p.trim_start_matches("/avatar/")),
        // Incremental sync: changes across the mailbox since a modseq.
        (&Method::GET, "/sync") => {
            let since: i64 = query_value(query, "since").and_then(|v| v.parse().ok()).unwrap_or(0);
            let store = app.store.clone();
            let uid = user.id;
            match tokio::task::spawn_blocking(move || store.account_changes(uid, since)).await {
                Ok(Ok((changes, modseq))) => {
                    let items: Vec<Value> = changes.iter().map(change_json).collect();
                    axum::Json(json!({ "modseq": modseq, "changes": items })).into_response()
                }
                Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "error": e.to_string() }))).into_response(),
                Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            }
        }
        // Raw message source (RFC 5322), so the client renders and files
        // exactly what the server stores (inline images, S/MIME, headers).
        (&Method::GET, p) if p.starts_with("/message/") && p.ends_with("/raw") => {
            let id = p.trim_start_matches("/message/").trim_end_matches("/raw");
            raw_message(&app, &user, id)
        }
        (&Method::GET, p) if p.starts_with("/message/") && p.contains("/attachment/") => {
            let (id, idx) = p.trim_start_matches("/message/").split_once("/attachment/").unwrap_or_default();
            webmail::attachment(&app, &user, id, idx)
        }
        (&Method::POST, "/submit") => webmail::json_response(submit(&app, &user, &body).await),
        (&Method::POST, "/import") => webmail::json_response(import(&app, &user, &body)),
        (&Method::POST, "/categories") => webmail::json_response(categories(&app, &user, &body)),
        (&Method::POST, "/destroy") => webmail::json_response(destroy(&app, &user, &body)),
        // Everything else reuses the shared PIM handlers (folders, messages,
        // send, flag, move, delete, calendar, contacts, tasks, notes, …).
        _ => webmail::json_response(webmail::dispatch(&app, &user, &method, rest, query, &body).await),
    }
}

/// WebSocket push: notify the client whenever something in its mailbox changes,
/// so it can call `/sync`. Served at `/api/v1/ws`.
pub async fn ws_route(ws: WebSocketUpgrade, State(app): State<Shared>, headers: HeaderMap) -> Response {
    if !headers.contains_key(CLIENT_HEADER) {
        return (StatusCode::BAD_REQUEST, "This endpoint is for the Ondura client.").into_response();
    }
    // WebSocket upgrades carry the Basic credentials in the request headers.
    let Some(user) = crate::webmail::session_user(&app, &headers).or_else(|| basic_user(&app, &headers)) else {
        return (StatusCode::UNAUTHORIZED, "Not signed in").into_response();
    };
    ws.max_message_size(1 << 16).max_frame_size(1 << 16).on_upgrade(move |socket| push_loop(app, socket, user.id))
}

/// Resolve HTTP Basic credentials to a user without rate-limiting side effects
/// of a full login (used on the WebSocket upgrade, already past Basic on the
/// REST side). Returns None when absent or invalid.
fn basic_user(app: &Shared, headers: &HeaderMap) -> Option<crate::store::User> {
    let raw = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let (login, password) = crate::auth::parse_basic(raw)?;
    let user = app.store.user_for_login(&login).ok()??;
    if crate::auth::verify_hash(&password, &user.password) {
        Some(user)
    } else {
        None
    }
}

async fn push_loop(app: Shared, socket: WebSocket, user_id: i64) {
    let mut rx = app.store.subscribe();
    let (mut sink, mut stream) = socket.split();
    // Tell the client to do an initial sync on connect.
    let _ = sink.send(Message::Text(json!({ "type": "sync" }).to_string().into())).await;
    loop {
        tokio::select! {
            // A change somewhere in the server.
            notice = rx.recv() => match notice {
                Ok(n) if n.user_id == user_id => {
                    if sink.send(Message::Text(json!({ "type": "changed", "folder": n.folder_id }).to_string().into())).await.is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                // Lagged behind: tell the client to resync from scratch.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    if sink.send(Message::Text(json!({ "type": "sync" }).to_string().into())).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            },
            // Drain client frames (pings/closes); we don't expect data from it.
            msg = stream.next() => match msg {
                Some(Ok(_)) => {}
                _ => break,
            },
        }
    }
}

/// The API's own identity document (mirrors the server identity, API-focused).
fn identity(app: &Shared) -> Value {
    let host = &app.cfg.hostname;
    json!({
        "product": "Ondura Server",
        "api": "ondura-client",
        "version": API_VERSION,
        "hostname": host,
        "auth": "basic",
        "clientHeader": CLIENT_HEADER,
        "endpoints": {
            "base": format!("https://{host}/api/v1"),
            "sync": format!("https://{host}/api/v1/sync"),
            "websocket": format!("wss://{host}/api/v1/ws"),
        },
    })
}

// ------------------------------------------------------------- avatars

/// Image types accepted for avatars.
const AVATAR_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Serve an avatar image for `target` (a numeric user id or an email address).
/// The caller has already authenticated the requester.
pub(crate) fn serve_avatar(app: &Shared, target: &str) -> Response {
    let user_id = match target.parse::<i64>() {
        Ok(id) => Some(id),
        Err(_) => app.store.user_by_email(target).ok().flatten().map(|u| u.id),
    };
    let Some(user_id) = user_id else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match app.store.avatar(user_id) {
        Ok(Some((content_type, data, updated))) => {
            let mut res = (StatusCode::OK, data).into_response();
            let h = res.headers_mut();
            if let Ok(v) = axum::http::HeaderValue::from_str(&content_type) {
                h.insert(axum::http::header::CONTENT_TYPE, v);
            }
            if let Ok(v) = axum::http::HeaderValue::from_str(&format!("\"{updated}\"")) {
                h.insert(axum::http::header::ETAG, v);
            }
            h.insert(axum::http::header::CACHE_CONTROL, axum::http::HeaderValue::from_static("private, max-age=86400"));
            res
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Store the authenticated user's avatar from a raw image upload.
pub(crate) fn set_avatar(app: &Shared, user_id: i64, content_type: Option<&str>, data: &[u8]) -> Response {
    let ct = content_type.unwrap_or("").split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if !AVATAR_TYPES.contains(&ct.as_str()) {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, axum::Json(json!({ "error": "avatar must be a PNG, JPEG, WebP or GIF image" }))).into_response();
    }
    if data.is_empty() || data.len() > crate::store::MAX_AVATAR_BYTES {
        return (StatusCode::PAYLOAD_TOO_LARGE, axum::Json(json!({ "error": "avatar is empty or larger than 1 MiB" }))).into_response();
    }
    match app.store.set_avatar(user_id, &ct, data) {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

pub(crate) fn delete_avatar(app: &Shared, user_id: i64) -> Response {
    match app.store.delete_avatar(user_id) {
        Ok(()) => axum::Json(json!({ "ok": true })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({ "error": e.to_string() }))).into_response(),
    }
}

/// GET /message/<id>/raw — the stored message source.
fn raw_message(app: &Shared, user: &User, id: &str) -> Response {
    let Ok(id) = id.parse::<i64>() else { return StatusCode::NOT_FOUND.into_response() };
    if own_item(app, user, id).is_err() {
        return StatusCode::NOT_FOUND.into_response();
    }
    match app.store.mime(id) {
        Ok(Some(raw)) => ([(axum::http::header::CONTENT_TYPE, "message/rfc822")], raw).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => webmail::json_response(Err(internal(e))),
    }
}

fn decode_mime(data: &str) -> Result<Vec<u8>, (StatusCode, String)> {
    B64.decode(data.as_bytes()).map_err(|_| bad("invalid base64 in mime"))
}

/// POST /submit — send a message the client built itself.
/// Body: `{ mime: base64, recipients?: [..], saveCopy?: true }`. `recipients`
/// is the envelope (it carries Bcc, which the message itself does not).
async fn submit(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct B {
        mime: String,
        #[serde(default)]
        recipients: Option<Vec<String>>,
        #[serde(default = "crate::serde_true")]
        save_copy: bool,
    }
    let b: B = webmail::parse(body)?;
    let raw = decode_mime(&b.mime)?;
    let recipients = b.recipients.filter(|r| !r.is_empty());
    crate::smtp::submit::send(app, user, raw, crate::smtp::submit::Options { recipients, save_copy: b.save_copy, ..Default::default() })
        .await
        .map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "ok": true }))
}

/// POST /import — file a message into a folder (drafts, imports).
/// Body: `{ folder, mime: base64, read?, flagged?, draft?, received? }`.
fn import(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        folder: i64,
        mime: String,
        #[serde(default)]
        read: bool,
        #[serde(default)]
        flagged: bool,
        #[serde(default)]
        draft: bool,
        /// Arrival time (Unix seconds); default now.
        #[serde(default)]
        received: Option<i64>,
    }
    let b: B = webmail::parse(body)?;
    let folder = app.store.folder(b.folder).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(webmail::not_found)?;
    let raw = decode_mime(&b.mime)?;
    let raw = crate::mime::crlf(&raw);
    let mut bits = 0;
    if b.read {
        bits |= flags::READ;
    }
    if b.flagged {
        bits |= flags::FLAGGED;
    }
    if b.draft {
        bits |= flags::DRAFT;
    }
    let id = app
        .store
        .insert_mail(NewMail { user_id: user.id, folder_id: folder.id, mime: &raw, flags: bits, received: b.received.unwrap_or_else(crate::store::now), writer: WRITER })
        .map_err(internal)?;
    Ok(json!({ "ok": true, "id": id }))
}

/// POST /categories — `{ id, categories: [..] }`.
fn categories(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: i64,
        categories: Vec<String>,
    }
    let b: B = webmail::parse(body)?;
    let item = own_item(app, user, b.id)?;
    app.store.update_flags(item.id, 0, 0, Some(&b.categories), WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// POST /destroy — delete a message permanently, wherever it is
/// (`/delete` moves it to Deleted Items first).
fn destroy(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: i64,
    }
    let b: B = webmail::parse(body)?;
    let item = own_item(app, user, b.id)?;
    app.store.delete_item(item.id, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// Compact wire form of an item for the sync feed.
fn item_json(i: &Item) -> Value {
    json!({
        "id": i.id,
        "folderId": i.folder_id,
        "kind": i.kind.as_str(),
        "uid": i.uid,
        "modseq": i.modseq,
        "received": i.received,
        "size": i.size,
        "flags": i.flags,
        "subject": i.subject,
        "from": i.sender,
        "to": i.recipients.to,
        "cc": i.recipients.cc,
        "categories": i.categories,
        "importance": i.importance,
        "messageId": i.message_id,
        "preview": i.preview,
        "hasAttachments": i.has_attachments,
        "messageClass": i.message_class,
        "data": i.data,
    })
}

fn change_json(c: &Change) -> Value {
    match c {
        Change::Add(i) => json!({ "type": "add", "folder": i.folder_id, "item": item_json(i) }),
        Change::Update(i) => json!({ "type": "update", "folder": i.folder_id, "item": item_json(i) }),
        Change::Delete { item_id, modseq } => json!({ "type": "delete", "id": item_id, "modseq": modseq }),
    }
}

fn content_type_header(headers: &HeaderMap) -> Option<&str> {
    headers.get(axum::http::header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
}

fn query_value<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use axum::http::header::{AUTHORIZATION, HeaderValue};
    use base64::Engine;

    fn basic(email: &str, pw: &str) -> String {
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{email}:{pw}")))
    }
    fn ip() -> std::net::IpAddr {
        std::net::IpAddr::from([127, 0, 0, 1])
    }

    #[tokio::test]
    async fn native_api_is_reserved_for_ondura_clients() {
        let (app, user) = App::test();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(&basic(&user.email, "secret")).unwrap());
        // Without the X-Ondura-Client header the API is refused.
        let r = handle(app, Method::GET, "/api/v1/me", "", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn identity_is_public() {
        let (app, _user) = App::test();
        let h = HeaderMap::new();
        let r = handle(app, Method::GET, "/api/v1/identity", "", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn avatar_upload_fetch_delete() {
        let (app, user) = App::test();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(&basic(&user.email, "secret")).unwrap());
        h.insert(CLIENT_HEADER, HeaderValue::from_static("Ondura-Mail/test"));

        // Wrong content type is refused.
        let mut bad = h.clone();
        bad.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        let r = handle(app.clone(), Method::POST, "/api/v1/avatar", "", &bad, Bytes::from_static(b"nope"), ip()).await;
        assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        // Upload a PNG (content is not validated, only type and size).
        let mut up = h.clone();
        up.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
        let img = Bytes::from_static(b"\x89PNG\r\n\x1a\n fake image bytes");
        let r = handle(app.clone(), Method::POST, "/api/v1/avatar", "", &up, img.clone(), ip()).await;
        assert_eq!(r.status(), StatusCode::OK);

        // Fetch it back by id.
        let r = handle(app.clone(), Method::GET, &format!("/api/v1/avatar/{}", user.id), "", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(axum::http::header::CONTENT_TYPE).unwrap(), "image/png");
        let got = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        assert_eq!(got, img);

        // And by email.
        let r = handle(app.clone(), Method::GET, &format!("/api/v1/avatar/{}", user.email), "", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::OK);

        // Remove it.
        let r = handle(app.clone(), Method::DELETE, "/api/v1/avatar", "", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = handle(app.clone(), Method::GET, &format!("/api/v1/avatar/{}", user.id), "", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn me_and_incremental_sync() {
        let (app, user) = App::test();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(&basic(&user.email, "secret")).unwrap());
        h.insert(CLIENT_HEADER, HeaderValue::from_static("Ondura-Mail/test"));

        // Reuses the shared handler: /me lists folders.
        let r = handle(app.clone(), Method::GET, "/api/v1/me", "", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::OK);

        // A new mail must show up in the delta feed from modseq 0.
        let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
        app.store
            .insert_mail(crate::store::NewMail { user_id: user.id, folder_id: inbox.id, mime: b"From: a@b.de\r\nSubject: Hi\r\n\r\nx", flags: 0, received: crate::store::now(), writer: "test" })
            .unwrap();

        let r = handle(app.clone(), Method::GET, "/api/v1/sync", "since=0", &h, Bytes::new(), ip()).await;
        assert_eq!(r.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["modseq"].as_i64().unwrap() > 0);
        let changes = v["changes"].as_array().unwrap();
        assert!(changes.iter().any(|c| c["type"] == "add" && c["item"]["subject"] == "Hi"));
    }

    async fn call(app: &Shared, h: &HeaderMap, method: Method, path: &str, query: &str, body: Value) -> (StatusCode, Value, Vec<u8>) {
        let body = if body.is_null() { Bytes::new() } else { Bytes::from(body.to_string()) };
        let r = handle(app.clone(), method, path, query, h, body, ip()).await;
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap().to_vec();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), bytes)
    }

    #[tokio::test]
    async fn client_mail_operations() {
        let (app, user) = App::test();
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(&basic(&user.email, "secret")).unwrap());
        h.insert(CLIENT_HEADER, HeaderValue::from_static("Ondura-Mail/test"));
        let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
        let drafts = app.store.ensure_folder(user.id, "drafts").unwrap();
        let mime = "From: Niklas <niklas@example.com>\r\nTo: anna@example.com\r\nCc: max@example.com\r\nSubject: Import\r\nMessage-ID: <imp@x>\r\n\r\nHallo";

        // Import: filed with flags, listed in the feed with recipients.
        let (st, v, _) = call(&app, &h, Method::POST, "/api/v1/import", "", json!({ "folder": drafts.id, "mime": B64.encode(mime), "draft": true, "read": true })).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let id = v["id"].as_i64().unwrap();
        let (_, feed, _) = call(&app, &h, Method::GET, "/api/v1/sync", "since=0", Value::Null).await;
        let item = feed["changes"].as_array().unwrap().iter().find(|c| c["item"]["id"] == id).unwrap()["item"].clone();
        assert_eq!(item["flags"].as_i64().unwrap() & (flags::READ | flags::DRAFT), flags::READ | flags::DRAFT);
        assert_eq!(item["to"][0]["email"], "anna@example.com");
        assert_eq!(item["cc"][0]["email"], "max@example.com");
        assert_eq!(item["messageId"], "<imp@x>");

        // Raw source comes back unchanged.
        let (st, _, raw) = call(&app, &h, Method::GET, &format!("/api/v1/message/{id}/raw"), "", Value::Null).await;
        assert_eq!(st, StatusCode::OK);
        assert!(String::from_utf8_lossy(&raw).contains("Subject: Import"));

        // Categories.
        let (st, _, _) = call(&app, &h, Method::POST, "/api/v1/categories", "", json!({ "id": id, "categories": ["Rot"] })).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(app.store.item(id).unwrap().unwrap().categories, vec!["Rot".to_string()]);

        // Destroy removes it permanently and the feed reports the deletion.
        let (_, before, _) = call(&app, &h, Method::GET, "/api/v1/sync", "since=999999999", Value::Null).await;
        let (st, _, _) = call(&app, &h, Method::POST, "/api/v1/destroy", "", json!({ "id": id })).await;
        assert_eq!(st, StatusCode::OK);
        assert!(app.store.item(id).unwrap().is_none());
        let (_, after, _) = call(&app, &h, Method::GET, "/api/v1/sync", &format!("since={}", before["modseq"]), Value::Null).await;
        assert!(after["changes"].as_array().unwrap().iter().any(|c| c["type"] == "delete" && c["id"] == id));

        // Submit: a local recipient gets it, Bcc only via the envelope, a copy lands in Sent.
        let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
        let send = format!("From: {}\r\nTo: max@other.org\r\nSubject: Senden\r\n\r\nText", user.email);
        let (st, v, _) = call(&app, &h, Method::POST, "/api/v1/submit", "", json!({ "mime": B64.encode(&send), "recipients": ["max@other.org", "anna@example.com"] })).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        let anna_inbox = app.store.folder_by_role(anna.id, "inbox").unwrap().unwrap();
        assert_eq!(app.store.items(anna_inbox.id, 0, 10).unwrap().len(), 1);
        let sent = app.store.folder_by_role(user.id, "sentitems").unwrap().unwrap();
        assert_eq!(app.store.items(sent.id, 0, 10).unwrap().len(), 1);
        // Sending as somebody else is refused.
        let (st, _, _) = call(&app, &h, Method::POST, "/api/v1/submit", "", json!({ "mime": B64.encode("From: boss@example.com\r\nTo: anna@example.com\r\n\r\nx") })).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let _ = inbox;
    }
}
