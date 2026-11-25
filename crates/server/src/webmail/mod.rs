//! Webmail: browser access to a mailbox at `/mail`, for ordinary users.
//!
//! Any enabled mailbox can sign in (no administrator rights required) and read,
//! write and organise mail, calendar and contacts. The JSON API lives under
//! `/mail/api`; it reuses the same storage and submission paths as EWS and
//! ActiveSync, so changes made here show up in connected clients and vice
//! versa.

use std::net::IpAddr;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;

use crate::app::Shared;
use crate::auth::{self, LoginError};
use crate::model::flags;
use crate::store::User;

mod calendar_api;
mod contacts_api;
mod mail_api;
mod pim_api;
mod settings_api;
#[cfg(test)]
mod tests;

const INDEX: &str = include_str!("index.html");
const LOGO: &str = include_str!("../admin/logo.svg");
const COOKIE: &str = "ondura_webmail";
const SESSION_LIFETIME: i64 = 8 * 3600;
/// Writer tag for changes made through webmail, so a user's own clients can tell
/// their echoes apart (same role as "ews" or an ActiveSync device id).
pub(crate) const WRITER: &str = "webmail";

/// serde default for booleans that should default to `true`.
pub(crate) fn default_true() -> bool {
    true
}

pub(crate) type ApiResult = Result<Value, (StatusCode, String)>;

pub(crate) fn bad(msg: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.into())
}

pub(crate) fn not_found() -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, "Not found".into())
}

pub(crate) fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub(crate) fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, (StatusCode, String)> {
    serde_json::from_slice(body).map_err(|e| bad(format!("invalid request: {e}")))
}

/// Attachment download, shared with the client API.
pub(crate) fn attachment(app: &Shared, user: &User, id: &str, idx: &str) -> Response {
    security_headers(mail_api::attachment(app, user, id, idx))
}

pub(crate) fn json_response(r: ApiResult) -> Response {
    match r {
        Ok(v) => axum::Json(v).into_response(),
        Err((code, msg)) => (code, axum::Json(json!({ "error": msg }))).into_response(),
    }
}

fn security_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert("X-Frame-Options", HeaderValue::from_static("DENY"));
    h.insert("X-Content-Type-Options", HeaderValue::from_static("nosniff"));
    h.insert("Referrer-Policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "Content-Security-Policy",
        // The message pane renders sanitized HTML mail; images are allowed from
        // https and data: so remote and inline pictures display.
        HeaderValue::from_static("default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data: https:; connect-src 'self'; frame-ancestors 'none'"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.to_string())
}

pub(crate) fn session_user(app: &Shared, headers: &HeaderMap) -> Option<User> {
    let token = session_token(headers)?;
    // Any enabled mailbox may use webmail; room and shared mailboxes are not
    // meant for interactive sign-in.
    app.store.session_user(&token).ok().flatten().filter(|u| u.enabled && u.kind == "user")
}

pub async fn handle(app: Shared, method: Method, path: &str, query: &str, headers: &HeaderMap, body: Bytes, ip: IpAddr) -> Response {
    let rest = path.trim_start_matches("/mail");
    if !rest.starts_with("/api") {
        if rest.is_empty() {
            return security_headers(Redirect::temporary("/mail/").into_response());
        }
        if rest == "/logo.svg" {
            let mut r = (StatusCode::OK, LOGO).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
            return security_headers(r);
        }
        let mut r = (StatusCode::OK, INDEX).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        return security_headers(r);
    }
    let api = rest.trim_start_matches("/api");
    // Mutating requests must carry the webmail header; blocks cross-site POSTs.
    if method != Method::GET && headers.get("X-Ondura-Mail").is_none() {
        return json_response(Err((StatusCode::FORBIDDEN, "missing request header".into())));
    }
    if api == "/login" && method == Method::POST {
        return security_headers(login(&app, &body, ip, headers).await);
    }
    let Some(user) = session_user(&app, headers) else {
        return json_response(Err((StatusCode::UNAUTHORIZED, "Not signed in".into())));
    };
    if api == "/logout" && method == Method::POST {
        if let Some(t) = session_token(headers) {
            let _ = app.store.delete_session(&t);
        }
        return security_headers(json_response(Ok(json!({ "ok": true }))));
    }
    // Attachment downloads return binary, handled before the JSON router.
    if method == Method::GET
        && let Some(rest) = api.strip_prefix("/message/")
        && let Some((id, idx)) = rest.split_once("/attachment/")
    {
        return security_headers(mail_api::attachment(&app, &user, id, idx));
    }
    // Avatars (binary / upload), shared with the native client API.
    if let Some(target) = api.strip_prefix("/avatar/") {
        if method == Method::GET {
            return security_headers(crate::api::serve_avatar(&app, target));
        }
    } else if api == "/avatar" {
        let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok());
        match method {
            Method::POST => return security_headers(crate::api::set_avatar(&app, user.id, content_type, &body)),
            Method::DELETE => return security_headers(crate::api::delete_avatar(&app, user.id)),
            _ => {}
        }
    }
    let result = route(&app, &user, &method, api, query, &body).await;
    security_headers(json_response(result))
}

/// Dispatch an already-authenticated API call to the shared PIM handlers. Used
/// by the webmail front end (cookie auth) and the native client API (Basic
/// auth) so both speak the same endpoints.
pub(crate) async fn dispatch(app: &Shared, user: &User, method: &Method, api: &str, query: &str, body: &Bytes) -> ApiResult {
    route(app, user, method, api, query, body).await
}

async fn route(app: &Shared, user: &User, method: &Method, api: &str, query: &str, body: &Bytes) -> ApiResult {
    match (method, api) {
        (&Method::GET, "/me") => me(app, user),
        (&Method::GET, "/messages") => mail_api::messages(app, user, query),
        (&Method::GET, "/calendar") => calendar_api::list(app, user, query),
        (&Method::GET, "/contacts") => contacts_api::list(app, user),
        (&Method::POST, "/send") => mail_api::send(app, user, body).await,
        (&Method::POST, "/draft") => mail_api::draft(app, user, body),
        (&Method::POST, "/flag") => mail_api::flag(app, user, body),
        (&Method::GET, "/categories") => mail_api::categories(app, user),
        (&Method::POST, "/category") => mail_api::save_category(app, user, body),
        (&Method::POST, "/category/delete") => mail_api::delete_category(app, user, body),
        (&Method::POST, "/categorize") => mail_api::categorize(app, user, body),
        (&Method::GET, "/rules") => mail_api::rules(app, user),
        (&Method::POST, "/rule") => mail_api::save_rule(app, user, body),
        (&Method::POST, "/rule/delete") => mail_api::delete_rule(app, user, body),
        (&Method::POST, "/sweep") => mail_api::sweep(app, user, body),
        (&Method::POST, "/move") => mail_api::move_message(app, user, body),
        (&Method::POST, "/delete") => mail_api::delete(app, user, body),
        (&Method::POST, "/empty") => mail_api::empty(app, user, body),
        (&Method::POST, "/event") => calendar_api::save(app, user, body),
        (&Method::POST, "/event/delete") => calendar_api::delete(app, user, body),
        (&Method::POST, "/event/respond") => calendar_api::respond(app, user, body).await,
        (&Method::POST, "/contact") => contacts_api::save(app, user, body),
        (&Method::POST, "/contact/delete") => contacts_api::delete(app, user, body),
        (&Method::GET, "/tasks") => pim_api::tasks(app, user),
        (&Method::POST, "/task") => pim_api::save_task(app, user, body),
        (&Method::POST, "/task/delete") => pim_api::delete_task(app, user, body),
        (&Method::GET, "/notes") => pim_api::notes(app, user),
        (&Method::POST, "/note") => pim_api::save_note(app, user, body),
        (&Method::POST, "/note/delete") => pim_api::delete_note(app, user, body),
        (&Method::GET, "/settings") => settings_api::get(app, user),
        (&Method::POST, "/settings") => settings_api::save(app, user, body),
        (&Method::POST, "/password") => settings_api::password(app, user, body),
        (&Method::POST, "/folder") => pim_api::create_folder(app, user, body),
        (&Method::POST, "/folder/rename") => pim_api::rename_folder(app, user, body),
        (&Method::POST, "/folder/delete") => pim_api::delete_folder(app, user, body),
        (&Method::POST, "/folder/read") => pim_api::mark_folder_read(app, user, body),
        (&Method::POST, "/bulk") => pim_api::bulk(app, user, body),
        (&Method::GET, _) if api.starts_with("/message/") => mail_api::message(app, user, api.trim_start_matches("/message/")),
        _ => Err(not_found()),
    }
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

async fn login(app: &Shared, body: &Bytes, ip: IpAddr, headers: &HeaderMap) -> Response {
    let Ok(req) = serde_json::from_slice::<LoginBody>(body) else { return json_response(Err(bad("invalid request"))) };
    let store = app.store.clone();
    let res = tokio::task::spawn_blocking(move || auth::login(&store, &req.email, &req.password, Some(ip))).await;
    let user = match res {
        Ok(Ok(u)) => u,
        Ok(Err(LoginError::Locked)) => return json_response(Err((StatusCode::TOO_MANY_REQUESTS, "Too many failed sign-ins. Try again later.".into()))),
        _ => return json_response(Err((StatusCode::UNAUTHORIZED, "The e-mail address or password is incorrect.".into()))),
    };
    if user.kind != "user" || !user.enabled {
        return json_response(Err((StatusCode::FORBIDDEN, "This mailbox cannot be used for webmail sign-in.".into())));
    }
    let token = match app.store.create_session(user.id, SESSION_LIFETIME) {
        Ok(t) => t,
        Err(e) => return json_response(Err(internal(e))),
    };
    app.store.audit(&user.email, "webmail.login", &ip.to_string());
    info!(user = %user.email, %ip, "Webmail sign-in");
    let plain = headers.get("X-Forwarded-Proto").is_some_and(|v| v.as_bytes() == b"http") || (!app.cfg.http.behind_proxy && app.https_tls.is_none());
    let secure = if plain { "" } else { " Secure;" };
    let mut r = axum::Json(json!({ "email": user.email, "name": user.name })).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        // Path=/ so the same session also authorises the collab endpoints
        // (/collab) that the webmail UI calls, not just /mail.
        HeaderValue::from_str(&format!("{COOKIE}={token}; Path=/; HttpOnly;{secure} SameSite=Strict; Max-Age={SESSION_LIFETIME}")).expect("cookie"),
    );
    r
}

/// Identity plus the folder tree with unread counts.
fn me(app: &Shared, user: &User) -> ApiResult {
    let folders = app.store.folders(user.id).map_err(internal)?;
    let mut list = Vec::new();
    for f in &folders {
        let (total, unread) = app.store.unread_count(f.id).unwrap_or((0, 0));
        list.push(json!({
            "id": f.id,
            "name": f.name,
            "role": f.role,
            "class": format!("{:?}", f.class).to_ascii_lowercase(),
            "parent": f.parent_id,
            "total": total,
            "unread": unread,
        }));
    }
    let signature = app.store.settings(user.id).map(|s| s.signature).unwrap_or_default();
    let addresses = app.store.send_as_addresses(user.id).unwrap_or_default();
    Ok(json!({
        "email": user.email,
        "name": user.name,
        "quota": user.quota,
        "used": app.store.used_bytes(user.id).unwrap_or(0),
        "signature": signature,
        "addresses": addresses,
        "collab": app.collab_enabled(),
        "avatar": app.store.avatar_updated(user.id).ok().flatten(),
        "id": user.id,
        "folders": list,
    }))
}

/// Shared helper: confirm an item id belongs to this user and load it.
pub(crate) fn own_item(app: &Shared, user: &User, id: i64) -> Result<crate::store::Item, (StatusCode, String)> {
    app.store
        .item(id)
        .map_err(internal)?
        .filter(|i| i.user_id == user.id)
        .ok_or_else(not_found)
}

/// Mark unread/flagged state handled here so webmail and flag share the mapping.
pub(crate) fn flag_bits(read: Option<bool>, flagged: Option<bool>) -> (i64, i64) {
    let mut set = 0;
    let mut clear = 0;
    if let Some(r) = read {
        if r {
            set |= flags::READ;
        } else {
            clear |= flags::READ;
        }
    }
    if let Some(f) = flagged {
        if f {
            set |= flags::FLAGGED;
        } else {
            clear |= flags::FLAGGED;
        }
    }
    (set, clear)
}

/// Resolve a well-known destination folder, creating it if missing.
pub(crate) fn role_folder(app: &Shared, user: &User, role: &str) -> Result<crate::store::Folder, (StatusCode, String)> {
    app.store.ensure_folder(user.id, role).map_err(internal)
}
