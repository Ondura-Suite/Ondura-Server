//! HTTP surface for collaborative documents: a REST API for managing documents
//! and sharing, and a WebSocket for real-time editing.
//!
//! Clients authenticate with the account (webmail session cookie, or HTTP Basic
//! for native clients such as the Ondura Mail app). The WebSocket carries the
//! document's current text on join and then a stream of operations; the server
//! reconciles concurrent edits (see `docstore::apply_op`) and fans the resulting
//! operations out to the other participants.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::app::Shared;
use crate::auth;
use crate::store::User;

// -------------------------------------------------------------- realtime hub

#[derive(Clone)]
enum Target {
    All,
    Others(u64),
    Only(u64),
}

#[derive(Clone)]
struct Frame {
    target: Target,
    text: Arc<str>,
}

struct Room {
    tx: broadcast::Sender<Frame>,
    participants: Mutex<HashMap<u64, (String, String)>>, // conn_id -> (name, color)
}

struct Hub {
    rooms: Mutex<HashMap<i64, Arc<Room>>>,
}

static HUB: LazyLock<Hub> = LazyLock::new(|| Hub { rooms: Mutex::new(HashMap::new()) });
static CONN_SEQ: AtomicU64 = AtomicU64::new(1);

impl Hub {
    fn room(&self, doc_id: i64) -> Arc<Room> {
        let mut rooms = self.rooms.lock().expect("hub");
        rooms
            .entry(doc_id)
            .or_insert_with(|| {
                let (tx, _) = broadcast::channel(1024);
                Arc::new(Room { tx, participants: Mutex::new(HashMap::new()) })
            })
            .clone()
    }

    fn drop_if_empty(&self, doc_id: i64) {
        let mut rooms = self.rooms.lock().expect("hub");
        if let Some(room) = rooms.get(&doc_id)
            && room.participants.lock().expect("participants").is_empty()
        {
            rooms.remove(&doc_id);
        }
    }
}

fn presence_frame(room: &Room) -> Frame {
    let parts = room.participants.lock().expect("participants");
    let users: Vec<Value> = parts.values().map(|(name, color)| json!({ "name": name, "color": color })).collect();
    Frame { target: Target::All, text: Arc::from(json!({ "type": "presence", "users": users }).to_string().as_str()) }
}

const PALETTE: [&str; 8] = ["#3b5bdb", "#2f9e44", "#e8590c", "#9c36b5", "#1098ad", "#c2255c", "#f08c00", "#495057"];

fn color_for(id: i64) -> String {
    PALETTE[(id.unsigned_abs() as usize) % PALETTE.len()].to_string()
}

// -------------------------------------------------------------- auth

fn collab_user(app: &Shared, headers: &HeaderMap) -> Option<User> {
    if let Some(u) = crate::webmail::session_user(app, headers) {
        return Some(u);
    }
    let auth_header = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())?;
    let (email, pass) = auth::parse_basic(auth_header)?;
    auth::login(&app.store, &email, &pass, None).ok()
}

// -------------------------------------------------------------- WebSocket

#[derive(Deserialize)]
pub struct WsQuery {
    doc: i64,
}

/// Axum route handler for `GET /collab/ws?doc=<id>` (registered in the router).
pub async fn ws_route(ws: WebSocketUpgrade, State(app): State<Shared>, headers: HeaderMap, Query(q): Query<WsQuery>) -> Response {
    if !app.cfg.collab.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let Some(user) = collab_user(&app, &headers) else {
        return (StatusCode::UNAUTHORIZED, "Not signed in").into_response();
    };
    let access = match app.store.doc_access(user.id, q.doc) {
        Ok(Some(a)) => a,
        _ => return (StatusCode::NOT_FOUND, "No such document").into_response(),
    };
    let doc_id = q.doc;
    // Bound per-message memory: operations are small; a 1 MiB cap still allows a
    // large paste but stops a hostile client allocating huge frames.
    ws.max_message_size(1 << 20)
        .max_frame_size(1 << 20)
        .on_upgrade(move |socket| handle_socket(app, socket, user, doc_id, access.write))
}

async fn handle_socket(app: Shared, socket: WebSocket, user: User, doc_id: i64, can_write: bool) {
    let conn_id = CONN_SEQ.fetch_add(1, Ordering::Relaxed);
    let room = HUB.room(doc_id);
    let mut rx = room.tx.subscribe();
    let name = if user.name.trim().is_empty() { user.email.clone() } else { user.name.clone() };
    let color = color_for(user.id);

    let (mut sender, mut receiver) = socket.split();

    // Send the current document state before anything else.
    let init = match app.store.doc(doc_id) {
        Ok(Some(d)) => json!({ "type": "init", "rev": d.revision, "content": d.content, "write": can_write, "name": name, "color": color }),
        _ => {
            let _ = sender.send(Message::Text(json!({ "type": "error", "message": "document not found" }).to_string().into())).await;
            return;
        }
    };
    if sender.send(Message::Text(init.to_string().into())).await.is_err() {
        return;
    }

    // Register and announce presence.
    room.participants.lock().expect("participants").insert(conn_id, (name.clone(), color.clone()));
    let _ = room.tx.send(presence_frame(&room));

    // Forward room frames to this socket.
    let forward_room = room.clone();
    let mut forward = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    let deliver = match frame.target {
                        Target::All => true,
                        Target::Others(src) => src != conn_id,
                        Target::Only(c) => c == conn_id,
                    };
                    if deliver && sender.send(Message::Text(frame.text.as_ref().to_string().into())).await.is_err() {
                        break;
                    }
                }
                // The client fell too far behind and missed broadcasts; tell it to
                // reload so it does not diverge, instead of dropping it.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let reset = json!({ "type": "reset" }).to_string();
                    if sender.send(Message::Text(reset.into())).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        let _ = forward_room; // keep the room alive for the task's lifetime
    });

    // Read client messages until the socket closes.
    loop {
        tokio::select! {
            msg = receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => handle_client_frame(&app, &room, conn_id, &user, doc_id, can_write, &name, &color, t.as_str()),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            _ = &mut forward => break,
        }
    }

    forward.abort();
    room.participants.lock().expect("participants").remove(&conn_id);
    let _ = room.tx.send(presence_frame(&room));
    HUB.drop_if_empty(doc_id);
}

#[allow(clippy::too_many_arguments)]
fn handle_client_frame(app: &Shared, room: &Room, conn_id: u64, user: &User, doc_id: i64, can_write: bool, name: &str, color: &str, text: &str) {
    let Ok(v) = serde_json::from_str::<Value>(text) else { return };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("op") => {
            if !can_write {
                let _ = room.tx.send(Frame { target: Target::Only(conn_id), text: Arc::from(json!({ "type": "error", "message": "read-only" }).to_string().as_str()) });
                return;
            }
            let base = v.get("rev").and_then(|r| r.as_i64()).unwrap_or(-1);
            let op = v.get("op").cloned().unwrap_or(Value::Null);
            match app.store.apply_op(doc_id, user.id, base, &op) {
                Ok((rev, transformed)) => {
                    // Acknowledge to the sender with the committed (transformed) op.
                    let ack = json!({ "type": "ack", "rev": rev, "op": transformed });
                    let _ = room.tx.send(Frame { target: Target::Only(conn_id), text: Arc::from(ack.to_string().as_str()) });
                    // Broadcast to the other participants.
                    let frame = json!({ "type": "op", "rev": rev, "op": transformed, "by": name });
                    let _ = room.tx.send(Frame { target: Target::Others(conn_id), text: Arc::from(frame.to_string().as_str()) });
                }
                Err(e) => {
                    let _ = room.tx.send(Frame { target: Target::Only(conn_id), text: Arc::from(json!({ "type": "error", "message": e.to_string(), "rev": app.store.doc(doc_id).ok().flatten().map(|d| d.revision) }).to_string().as_str()) });
                }
            }
        }
        Some("cursor") => {
            let frame = json!({ "type": "cursor", "by": conn_id, "name": name, "color": color, "pos": v.get("pos"), "anchor": v.get("anchor") });
            let _ = room.tx.send(Frame { target: Target::Others(conn_id), text: Arc::from(frame.to_string().as_str()) });
        }
        _ => {}
    }
}

// -------------------------------------------------------------- REST API

pub async fn handle(app: Shared, method: Method, path: &str, query: &str, headers: &HeaderMap, body: Bytes) -> Response {
    if !app.cfg.collab.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let api = path.trim_start_matches("/collab/api");
    if method != Method::GET && headers.get("X-Ondura-Collab").is_none() {
        return (StatusCode::FORBIDDEN, "missing request header").into_response();
    }
    let Some(user) = collab_user(&app, headers) else {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": "Not signed in" }))).into_response();
    };
    let r = route(&app, &user, &method, api, query, &body);
    match r {
        Ok(v) => axum::Json(v).into_response(),
        Err((code, msg)) => (code, axum::Json(json!({ "error": msg }))).into_response(),
    }
}

type ApiResult = Result<Value, (StatusCode, String)>;

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, (StatusCode, String)> {
    serde_json::from_slice(body).map_err(|e| (StatusCode::BAD_REQUEST, format!("invalid request: {e}")))
}

fn route(app: &Shared, user: &User, method: &Method, api: &str, _query: &str, body: &Bytes) -> ApiResult {
    let seg: Vec<&str> = api.trim_matches('/').split('/').filter(|s| !s.is_empty()).collect();
    match (method, seg.as_slice()) {
        (&Method::GET, ["docs"]) => list_docs(app, user),
        (&Method::POST, ["docs"]) => create_doc(app, user, body),
        (&Method::GET, ["docs", id]) => get_doc(app, user, id),
        (&Method::DELETE, ["docs", id]) => delete_doc(app, user, id),
        (&Method::POST, ["docs", id, "rename"]) => rename_doc(app, user, id, body),
        (&Method::POST, ["docs", id, "share"]) => share_doc(app, user, id, body),
        (&Method::POST, ["docs", id, "unshare"]) => unshare_doc(app, user, id, body),
        (&Method::GET, ["docs", id, "ops"]) => ops_since(app, user, id, _query),
        (&Method::GET, ["docs", id, "history"]) => history(app, user, id),
        (&Method::GET, ["docs", id, "at", rev]) => content_at(app, user, id, rev),
        _ => Err((StatusCode::NOT_FOUND, "Not found".into())),
    }
}

fn owned_or_shared(app: &Shared, user: &User, id: &str) -> Result<i64, (StatusCode, String)> {
    let id: i64 = id.parse().map_err(|_| (StatusCode::NOT_FOUND, "Not found".into()))?;
    match app.store.doc_access(user.id, id) {
        Ok(Some(_)) => Ok(id),
        _ => Err((StatusCode::NOT_FOUND, "Not found".into())),
    }
}

fn require_owner(app: &Shared, user: &User, id: i64) -> Result<(), (StatusCode, String)> {
    let doc = app.store.doc(id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, "Not found".into()))?;
    if doc.owner_id != user.id {
        return Err((StatusCode::FORBIDDEN, "Only the owner can do that.".into()));
    }
    Ok(())
}

fn ise(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn list_docs(app: &Shared, user: &User) -> ApiResult {
    let docs = app.store.docs_for(user.id).map_err(ise)?;
    let list: Vec<Value> = docs
        .iter()
        .map(|d| json!({ "id": d.id, "name": d.name, "revision": d.revision, "updated": d.updated, "canWrite": d.can_write, "owned": d.owned }))
        .collect();
    Ok(json!({ "documents": list }))
}

fn create_doc(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        #[serde(default)]
        name: String,
    }
    let b: B = parse(body)?;
    let id = app.store.create_doc(user.id, &b.name).map_err(ise)?;
    Ok(json!({ "id": id }))
}

fn get_doc(app: &Shared, user: &User, id: &str) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    let doc = app.store.doc(id).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, "Not found".into()))?;
    let access = app.store.doc_access(user.id, id).map_err(ise)?.unwrap();
    let shares: Vec<Value> = app
        .store
        .doc_shares(id)
        .map_err(ise)?
        .iter()
        .map(|(_, email, name, w)| json!({ "email": email, "name": name, "canWrite": w }))
        .collect();
    Ok(json!({
        "id": doc.id, "name": doc.name, "revision": doc.revision, "content": doc.content,
        "owned": doc.owner_id == user.id, "canWrite": access.write, "shares": shares
    }))
}

fn delete_doc(app: &Shared, user: &User, id: &str) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    require_owner(app, user, id)?;
    app.store.delete_doc(id).map_err(ise)?;
    Ok(json!({ "ok": true }))
}

fn rename_doc(app: &Shared, user: &User, id: &str, body: &Bytes) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    require_owner(app, user, id)?;
    #[derive(Deserialize)]
    struct B {
        name: String,
    }
    let b: B = parse(body)?;
    app.store.rename_doc(id, &b.name).map_err(ise)?;
    Ok(json!({ "ok": true }))
}

fn share_doc(app: &Shared, user: &User, id: &str, body: &Bytes) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    require_owner(app, user, id)?;
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct B {
        email: String,
        #[serde(default = "yes")]
        can_write: bool,
    }
    fn yes() -> bool {
        true
    }
    let b: B = parse(body)?;
    let target = app.store.user_by_email(&b.email).map_err(ise)?.ok_or((StatusCode::NOT_FOUND, "No such user".into()))?;
    if target.id == user.id {
        return Err((StatusCode::BAD_REQUEST, "The document already belongs to you.".into()));
    }
    app.store.share_doc(id, target.id, b.can_write).map_err(ise)?;
    Ok(json!({ "ok": true }))
}

fn unshare_doc(app: &Shared, user: &User, id: &str, body: &Bytes) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    require_owner(app, user, id)?;
    #[derive(Deserialize)]
    struct B {
        email: String,
    }
    let b: B = parse(body)?;
    if let Some(target) = app.store.user_by_email(&b.email).map_err(ise)? {
        app.store.unshare_doc(id, target.id).map_err(ise)?;
    }
    Ok(json!({ "ok": true }))
}

fn history(app: &Shared, user: &User, id: &str) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    let rows = app.store.doc_history(id, 200).map_err(ise)?;
    let list: Vec<Value> = rows
        .into_iter()
        .map(|(rev, email, name, ts, added, removed)| json!({ "rev": rev, "email": email, "name": name, "ts": ts, "added": added, "removed": removed }))
        .collect();
    Ok(json!({ "history": list }))
}

fn content_at(app: &Shared, user: &User, id: &str, rev: &str) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    let rev: i64 = rev.parse().map_err(|_| (StatusCode::BAD_REQUEST, "bad revision".into()))?;
    let content = app.store.doc_content_at(id, rev).map_err(ise)?;
    Ok(json!({ "rev": rev, "content": content }))
}

fn ops_since(app: &Shared, user: &User, id: &str, query: &str) -> ApiResult {
    let id = owned_or_shared(app, user, id)?;
    let since: i64 = query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == "since").and_then(|(_, v)| v.parse().ok()).unwrap_or(0);
    let ops = app.store.ops_since(id, since).map_err(ise)?;
    let list: Vec<Value> = ops.into_iter().map(|(rev, op)| json!({ "rev": rev, "op": op })).collect();
    Ok(json!({ "ops": list }))
}
