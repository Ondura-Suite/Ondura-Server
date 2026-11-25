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

use crate::app::Shared;
use crate::store::{Change, Item};
use crate::webmail;

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
}
