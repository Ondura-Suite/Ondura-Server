//! Nextcloud-compatible surface so the official Nextcloud mobile/desktop clients
//! can connect to the Ondura cloud and, in particular, auto-upload photos.
//!
//! It exposes just enough of the Nextcloud protocol on top of the cloud storage
//! ([`crate::cloud`]): `status.php` for discovery, Login Flow v2 to obtain an app
//! password, a slice of the OCS API (capabilities and user), and a WebDAV
//! endpoint mapped onto each user's personal drive — including chunked uploads
//! for large media. The whole surface is gated behind `[cloud] enabled`.

use std::io::Write as _;
use std::path::PathBuf;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::app::Shared;
use crate::cloud::Item;
use crate::db::{params, OptionalExtension};
use crate::store::{now, Store};
use crate::store::User;

const DAV_FILES: &str = "/remote.php/dav/files/";
const DAV_UPLOADS: &str = "/remote.php/dav/uploads/";
const DAV_WEBDAV: &str = "/remote.php/webdav";

fn token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    hex::encode(bytes)
}

// ---------------------------------------------------------------- storage

impl Store {
    /// Issue an app password (a long-lived token the Nextcloud client uses for
    /// Basic auth); returns the token.
    pub fn create_app_password(&self, user_id: i64, name: &str) -> anyhow::Result<String> {
        let t = token();
        self.db().execute(
            "INSERT INTO app_passwords (token, user_id, name, created, last_used) VALUES (?1, ?2, ?3, ?4, 0)",
            params![&t, user_id, name, now()],
        )?;
        Ok(t)
    }

    pub fn app_password_user(&self, token: &str) -> anyhow::Result<Option<i64>> {
        self.db().query_row("SELECT user_id FROM app_passwords WHERE token = ?1", [token], |r| r.get(0)).optional()
    }

    pub fn touch_app_password(&self, token: &str) {
        let _ = self.db().execute("UPDATE app_passwords SET last_used = ?1 WHERE token = ?2", params![now(), token]);
    }

    /// Start a Login Flow v2; returns `(login_token, poll_token)`.
    pub fn create_login_flow(&self) -> anyhow::Result<(String, String)> {
        let (login, poll) = (token(), token());
        self.db().execute(
            "INSERT INTO login_flows (login_token, poll_token, created) VALUES (?1, ?2, ?3)",
            params![&login, &poll, now()],
        )?;
        Ok((login, poll))
    }

    /// Whether a login-flow token exists and is still awaiting authorization.
    pub fn login_flow_pending(&self, login_token: &str) -> anyhow::Result<bool> {
        let v: Option<Option<i64>> = self
            .db()
            .query_row("SELECT user_id FROM login_flows WHERE login_token = ?1", [login_token], |r| r.get(0))
            .optional()?;
        Ok(matches!(v, Some(None)))
    }

    /// Record the granted account and app password for a login flow.
    pub fn complete_login_flow(&self, login_token: &str, user_id: i64, app_password: &str) -> anyhow::Result<()> {
        self.db().execute(
            "UPDATE login_flows SET user_id = ?1, app_password = ?2 WHERE login_token = ?3 AND user_id IS NULL",
            params![user_id, app_password, login_token],
        )?;
        Ok(())
    }

    /// Poll a login flow: returns `(login_name, app_password)` once granted.
    pub fn poll_login_flow(&self, poll_token: &str) -> anyhow::Result<Option<(String, String)>> {
        let row: Option<(i64, String)> = self
            .db()
            .query_row(
                "SELECT user_id, app_password FROM login_flows WHERE poll_token = ?1 AND app_password IS NOT NULL",
                [poll_token],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((user_id, app_password)) = row else { return Ok(None) };
        let email = self.user(user_id)?.map(|u| u.email).unwrap_or_default();
        let _ = self.db().execute("DELETE FROM login_flows WHERE poll_token = ?1", [poll_token]);
        Ok(Some((email, app_password)))
    }
}

// --------------------------------------------------------------- helpers

/// Whether the Nextcloud interface is enabled for a mailbox (off by default).
fn nextcloud_on(app: &Shared, user_id: i64) -> bool {
    app.store.settings(user_id).map(|s| s.nextcloud).unwrap_or(false)
}

fn unauthorized() -> Response {
    let mut r = (StatusCode::UNAUTHORIZED, "").into_response();
    r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"Ondura\""));
    r
}

/// Authenticate a Nextcloud client via HTTP Basic, accepting either the account
/// password or an app password.
fn authenticate(app: &Shared, headers: &HeaderMap) -> Result<User, Box<Response>> {
    let fail = || Box::new(unauthorized());
    let h = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).ok_or_else(fail)?;
    let (login, secret) = crate::auth::parse_basic(h).ok_or_else(fail)?;
    let user = app.store.user_by_email(&login).ok().flatten().ok_or_else(fail)?;
    if user.kind != "user" || !user.enabled || !nextcloud_on(app, user.id) {
        return Err(fail());
    }
    if crate::auth::verify_hash(&secret, &user.password) {
        return Ok(user);
    }
    if app.store.app_password_user(&secret).ok().flatten() == Some(user.id) {
        app.store.touch_app_password(&secret);
        return Ok(user);
    }
    Err(fail())
}

fn base_url(app: &Shared) -> String {
    format!("https://{}", app.cfg.hostname)
}

/// Minimal percent-decoding for a single path segment.
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encode a path, keeping `/` separators.
fn pct_encode_path(s: &str) -> String {
    let mut out = String::new();
    for ch in s.bytes() {
        match ch {
            b'/' | b'-' | b'_' | b'.' | b'~' | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' => out.push(ch as char),
            _ => out.push_str(&format!("%{ch:02X}")),
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn http_date(ts: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(ts, 0).single().map(|d| d.format("%a, %d %b %Y %H:%M:%S GMT").to_string()).unwrap_or_default()
}

fn files_root(app: &Shared) -> PathBuf {
    app.cfg.cloud.files_dir(&app.cfg.data_dir)
}

fn item_file(app: &Shared, drive_id: i64, item_id: i64) -> PathBuf {
    files_root(app).join(drive_id.to_string()).join(item_id.to_string())
}

// ------------------------------------------------------------- dispatch

pub async fn handle(app: Shared, method: Method, raw_path: &str, query: &str, headers: &HeaderMap, body: Bytes) -> Response {
    if !app.cfg.cloud.enabled {
        return StatusCode::NOT_FOUND.into_response();
    }
    let path = raw_path;
    if path == "/status.php" {
        return status(&app);
    }
    if let Some(rest) = path.strip_prefix("/index.php/login/v2") {
        return login_flow(&app, &method, rest, headers, &body).await;
    }
    if path.starts_with("/ocs/") {
        return ocs(&app, path, query, headers);
    }
    if path.starts_with("/remote.php/dav") || path.starts_with(DAV_WEBDAV) {
        let user = match authenticate(&app, headers) {
            Ok(u) => u,
            Err(r) => return *r,
        };
        return webdav(&app, &user, &method, path, headers, body).await;
    }
    StatusCode::NOT_FOUND.into_response()
}

// --------------------------------------------------------------- status

fn status(app: &Shared) -> Response {
    let body = json!({
        "installed": true,
        "maintenance": false,
        "needsDbUpgrade": false,
        "version": "28.0.0.0",
        "versionstring": "28.0.0",
        "edition": "",
        "productname": format!("{} Cloud", app.cfg.hostname),
        "extendedSupport": false,
    });
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

// ------------------------------------------------------------------ OCS

fn ocs(app: &Shared, path: &str, _query: &str, headers: &HeaderMap) -> Response {
    let user = match authenticate(app, headers) {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let meta = json!({ "status": "ok", "statuscode": 200, "message": "OK" });
    let data = if path.contains("/capabilities") {
        json!({
            "version": { "major": 28, "minor": 0, "micro": 0, "string": "28.0.0", "edition": "", "extendedSupport": false },
            "capabilities": {
                "core": { "pollinterval": 60, "webdav-root": "remote.php/webdav" },
                "dav": { "chunking": "1.0" },
                "files": { "bigfilechunking": true, "undelete": false, "versioning": false }
            }
        })
    } else if path.contains("/user") {
        let used = app.store.ensure_personal_drive(user.id).ok().and_then(|d| drive_used(app, d)).unwrap_or(0);
        let total = if user.quota > 0 { user.quota } else { -3 };
        json!({
            "id": user.email,
            "display-name": user.name,
            "email": user.email,
            "quota": { "free": if user.quota > 0 { (user.quota - used).max(0) } else { -3 }, "used": used, "total": total, "relative": 0, "quota": if user.quota > 0 { user.quota } else { -3 } }
        })
    } else {
        return (StatusCode::NOT_FOUND, "").into_response();
    };
    let body = json!({ "ocs": { "meta": meta, "data": data } });
    (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], body.to_string()).into_response()
}

fn drive_used(app: &Shared, drive_id: i64) -> Option<i64> {
    app.store.drive_total_size(drive_id).ok()
}

// ------------------------------------------------------------ login flow

async fn login_flow(app: &Shared, method: &Method, rest: &str, _headers: &HeaderMap, body: &Bytes) -> Response {
    // POST /index.php/login/v2  -> start a flow.
    if rest.is_empty() && method == Method::POST {
        let Ok((login, poll)) = app.store.create_login_flow() else {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        };
        let base = base_url(app);
        let out = json!({
            "poll": { "token": poll, "endpoint": format!("{base}/index.php/login/v2/poll") },
            "login": format!("{base}/index.php/login/v2/flow/{login}")
        });
        return (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], out.to_string()).into_response();
    }
    // POST /index.php/login/v2/poll  (form: token=<poll>) -> result once granted.
    if rest == "/poll" && method == Method::POST {
        let form = String::from_utf8_lossy(body);
        let poll = form_value(&form, "token").unwrap_or_default();
        return match app.store.poll_login_flow(&poll) {
            Ok(Some((login_name, app_password))) => {
                let out = json!({ "server": base_url(app), "loginName": login_name, "appPassword": app_password });
                (StatusCode::OK, [(header::CONTENT_TYPE, "application/json")], out.to_string()).into_response()
            }
            _ => (StatusCode::NOT_FOUND, "").into_response(),
        };
    }
    // The browser grant page: GET shows a form, POST verifies and grants.
    if let Some(login_token) = rest.strip_prefix("/flow/") {
        let login_token = login_token.trim_end_matches('/');
        if !app.store.login_flow_pending(login_token).unwrap_or(false) {
            return (StatusCode::GONE, html_page("This login request is no longer valid.")).into_response();
        }
        if method == Method::GET {
            return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], grant_page(login_token)).into_response();
        }
        if method == Method::POST {
            let form = String::from_utf8_lossy(body);
            let email = form_value(&form, "user").unwrap_or_default();
            let password = form_value(&form, "password").unwrap_or_default();
            match crate::auth::login(&app.store, &email, &password, None) {
                Ok(u) if u.kind == "user" && u.enabled && !nextcloud_on(app, u.id) => {
                    return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html_page("Nextcloud access is turned off for this account. Enable it in Ondura webmail settings, then try again.")).into_response();
                }
                Ok(u) if u.kind == "user" && u.enabled => {
                    let app_pw = app.store.create_app_password(u.id, "Nextcloud client").unwrap_or_default();
                    let _ = app.store.complete_login_flow(login_token, u.id, &app_pw);
                    return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html_page("Account connected. You can return to the app.")).into_response();
                }
                _ => {
                    return (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], grant_page_error(login_token)).into_response();
                }
            }
        }
    }
    StatusCode::NOT_FOUND.into_response()
}

fn form_value(form: &str, key: &str) -> Option<String> {
    form.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| pct_decode(&v.replace('+', " ")))
}

fn html_page(msg: &str) -> String {
    format!("<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Ondura</title><body style=\"font-family:system-ui;background:#0b0d12;color:#e7e9ee;display:flex;align-items:center;justify-content:center;height:100vh;margin:0\"><div style=\"max-width:360px;padding:28px;border:1px solid #232733;border-radius:14px;background:#141824\"><h2 style=\"margin:0 0 8px\">Ondura Cloud</h2><p style=\"color:#9aa3b2\">{}</p></div></body>", xml_escape(msg))
}

fn grant_page(token: &str) -> String {
    grant_page_inner(token, "")
}
fn grant_page_error(token: &str) -> String {
    grant_page_inner(token, "<p style=\"color:#ef4444;font-size:13px;margin:0 0 10px\">Wrong e-mail or password.</p>")
}
fn grant_page_inner(token: &str, err: &str) -> String {
    format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\"><title>Connect app</title>\
         <body style=\"font-family:system-ui;background:#0b0d12;color:#e7e9ee;display:flex;align-items:center;justify-content:center;height:100vh;margin:0\">\
         <form method=post action=\"/index.php/login/v2/flow/{token}\" style=\"max-width:340px;width:100%;padding:28px;border:1px solid #232733;border-radius:14px;background:#141824\">\
         <h2 style=\"margin:0 0 4px\">Connect app</h2><p style=\"color:#9aa3b2;margin:0 0 16px;font-size:13px\">Sign in to grant the app access to your files.</p>{err}\
         <input name=user type=email placeholder=\"E-mail\" autocomplete=username style=\"width:100%;box-sizing:border-box;margin:0 0 10px;padding:10px;border-radius:8px;border:1px solid #232733;background:#0b0d12;color:#e7e9ee\">\
         <input name=password type=password placeholder=\"Password\" autocomplete=current-password style=\"width:100%;box-sizing:border-box;margin:0 0 16px;padding:10px;border-radius:8px;border:1px solid #232733;background:#0b0d12;color:#e7e9ee\">\
         <button type=submit style=\"width:100%;padding:10px;border:0;border-radius:8px;background:#4f6ef7;color:#fff;font-weight:600;cursor:pointer\">Grant access</button></form></body>"
    )
}

// --------------------------------------------------------------- WebDAV

async fn webdav(app: &Shared, user: &User, method: &Method, path: &str, headers: &HeaderMap, body: Bytes) -> Response {
    let drive = match app.store.ensure_personal_drive(user.id) {
        Ok(d) => d,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Chunked uploads live under their own path space.
    if let Some(rest) = path.strip_prefix(DAV_UPLOADS) {
        return uploads(app, user, drive, method, rest, headers, body).await;
    }

    let rel = dav_rel(path);
    let m = method.as_str();
    match m {
        "OPTIONS" => dav_options(),
        "PROPFIND" => propfind(app, user, drive, &rel, headers, DAV_FILES),
        "GET" | "HEAD" => dav_get(app, drive, &rel, m == "HEAD"),
        "PUT" => dav_put(app, user, drive, &rel, headers, &body),
        "MKCOL" => dav_mkcol(app, user, drive, &rel),
        "DELETE" => dav_delete(app, drive, &rel),
        "MOVE" => dav_move(app, user, drive, &rel, headers),
        "PROPPATCH" => multistatus_ok(&rel, DAV_FILES),
        "COPY" => (StatusCode::NOT_IMPLEMENTED, "").into_response(),
        _ => (StatusCode::METHOD_NOT_ALLOWED, "").into_response(),
    }
}

/// Strip the WebDAV prefix and the user segment, returning the drive-relative
/// path with decoded segments.
fn dav_rel(path: &str) -> String {
    let after = if let Some(r) = path.strip_prefix(DAV_FILES) {
        // Drop the first segment (the account name).
        r.split_once('/').map(|(_, rest)| rest).unwrap_or("")
    } else if let Some(r) = path.strip_prefix(DAV_WEBDAV) {
        r.trim_start_matches('/')
    } else {
        ""
    };
    after.split('/').filter(|s| !s.is_empty()).map(pct_decode).collect::<Vec<_>>().join("/")
}

fn split_parent(rel: &str) -> (&str, &str) {
    match rel.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", rel),
    }
}

fn dav_options() -> Response {
    let mut r = (StatusCode::OK, "").into_response();
    let h = r.headers_mut();
    h.insert("DAV", HeaderValue::from_static("1, 2, 3"));
    h.insert(header::ALLOW, HeaderValue::from_static("OPTIONS, GET, HEAD, POST, PUT, DELETE, PROPFIND, PROPPATCH, MKCOL, MOVE, COPY"));
    h.insert("MS-Author-Via", HeaderValue::from_static("DAV"));
    r
}

fn etag(it: &Item) -> String {
    format!("\"{}-{}\"", it.id, it.updated)
}

fn propfind(app: &Shared, user: &User, drive: i64, rel: &str, headers: &HeaderMap, prefix: &str) -> Response {
    let depth = headers.get("Depth").and_then(|v| v.to_str().ok()).unwrap_or("1");
    let acct = user.email.clone();
    // Resolve the target.
    let target = if rel.is_empty() {
        None // the drive root (a virtual collection)
    } else {
        match app.store.resolve_path(drive, rel) {
            Ok(Some(it)) => Some(it),
            Ok(None) => return (StatusCode::NOT_FOUND, "").into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };
    let is_collection = target.as_ref().map(|t| t.kind == "folder").unwrap_or(true);
    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">");
    // The target itself.
    body.push_str(&response_xml(app, &acct, prefix, rel, target.as_ref(), drive));
    // Its children, when it is a collection and Depth: 1.
    if is_collection && depth != "0" {
        let parent_id = target.as_ref().map(|t| t.id);
        if let Ok(items) = app.store.drive_items(drive, parent_id) {
            for it in &items {
                let child_rel = if rel.is_empty() { it.name.clone() } else { format!("{rel}/{}", it.name) };
                body.push_str(&response_xml(app, &acct, prefix, &child_rel, Some(it), drive));
            }
        }
    }
    body.push_str("</d:multistatus>");
    (StatusCode::MULTI_STATUS, [(header::CONTENT_TYPE, "application/xml; charset=utf-8")], body).into_response()
}

fn response_xml(app: &Shared, acct: &str, prefix: &str, rel: &str, it: Option<&Item>, drive: i64) -> String {
    let collection = it.map(|i| i.kind == "folder").unwrap_or(true);
    let mut href = format!("{prefix}{}", pct_encode_path(acct));
    if prefix == DAV_WEBDAV {
        href = DAV_WEBDAV.to_string();
    }
    if !rel.is_empty() {
        href.push('/');
        href.push_str(&pct_encode_path(rel));
    }
    if collection && !href.ends_with('/') {
        href.push('/');
    }
    let modified = http_date(it.map(|i| i.updated).unwrap_or_else(now));
    let tag = it.map(etag).unwrap_or_else(|| format!("\"root-{drive}\""));
    let mut props = String::new();
    if collection {
        props.push_str("<d:resourcetype><d:collection/></d:resourcetype>");
        props.push_str("<oc:permissions>RGDNVCK</oc:permissions>");
        if rel.is_empty() {
            let used = app.store.drive_total_size(drive).unwrap_or(0);
            props.push_str(&format!("<d:quota-used-bytes>{used}</d:quota-used-bytes><d:quota-available-bytes>-3</d:quota-available-bytes>"));
        }
    } else {
        let i = it.unwrap();
        props.push_str("<d:resourcetype/>");
        props.push_str(&format!("<d:getcontentlength>{}</d:getcontentlength>", i.size));
        props.push_str(&format!("<d:getcontenttype>{}</d:getcontenttype>", xml_escape(&i.content_type)));
        props.push_str("<oc:permissions>RGDNVW</oc:permissions>");
        props.push_str(&format!("<oc:size>{}</oc:size>", i.size));
    }
    if let Some(i) = it {
        props.push_str(&format!("<oc:id>{}</oc:id>", i.id));
    }
    format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:getlastmodified>{}</d:getlastmodified><d:getetag>{}</d:getetag>{}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        xml_escape(&href), modified, xml_escape(&tag), props
    )
}

fn multistatus_ok(rel: &str, prefix: &str) -> Response {
    let href = format!("{prefix}{}", pct_encode_path(rel));
    let body = format!(
        "<?xml version=\"1.0\"?>\n<d:multistatus xmlns:d=\"DAV:\"><d:response><d:href>{}</d:href><d:propstat><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response></d:multistatus>",
        xml_escape(&href)
    );
    (StatusCode::MULTI_STATUS, [(header::CONTENT_TYPE, "application/xml; charset=utf-8")], body).into_response()
}

fn dav_get(app: &Shared, drive: i64, rel: &str, head_only: bool) -> Response {
    let it = match app.store.resolve_path(drive, rel) {
        Ok(Some(it)) if it.kind == "file" => it,
        Ok(_) => return (StatusCode::NOT_FOUND, "").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let data = std::fs::read(item_file(app, drive, it.id)).unwrap_or_default();
    let ct = HeaderValue::from_str(&it.content_type).unwrap_or(HeaderValue::from_static("application/octet-stream"));
    if head_only {
        let mut r = StatusCode::OK.into_response();
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, ct);
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(it.size as u64));
        h.insert(header::ETAG, HeaderValue::from_str(&etag(&it)).unwrap());
        return r;
    }
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, ct);
    h.insert(header::ETAG, HeaderValue::from_str(&etag(&it)).unwrap());
    h.insert(header::LAST_MODIFIED, HeaderValue::from_str(&http_date(it.updated)).unwrap());
    r
}

fn oc_mtime(headers: &HeaderMap) -> Option<i64> {
    headers.get("X-OC-Mtime").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())
}

fn dav_put(app: &Shared, user: &User, drive: i64, rel: &str, headers: &HeaderMap, body: &Bytes) -> Response {
    if rel.is_empty() {
        return (StatusCode::CONFLICT, "").into_response();
    }
    let (dir, name) = split_parent(rel);
    let parent = match app.store.ensure_folders(drive, dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    let existing = app.store.child_by_name(drive, parent, name).ok().flatten();
    if existing.as_ref().map(|e| e.kind != "file").unwrap_or(false) {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).unwrap_or("application/octet-stream");
    let (id, created) = match &existing {
        Some(e) => {
            if app.store.set_file_meta(e.id, body.len() as i64, ct, user.id).is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            (e.id, false)
        }
        None => match app.store.create_drive_file(drive, parent, name, body.len() as i64, ct, user.id) {
            Ok(id) => (id, true),
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        },
    };
    let dir_path = item_file(app, drive, id);
    if std::fs::create_dir_all(dir_path.parent().unwrap()).and_then(|_| std::fs::write(&dir_path, body)).is_err() {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if let Some(mt) = oc_mtime(headers) {
        let _ = app.store.set_item_mtime(id, mt);
    }
    let it = app.store.drive_item(id).ok().flatten();
    let mut r = (if created { StatusCode::CREATED } else { StatusCode::NO_CONTENT }, "").into_response();
    let h = r.headers_mut();
    if let Some(it) = &it {
        h.insert(header::ETAG, HeaderValue::from_str(&etag(it)).unwrap());
        h.insert("OC-FileId", HeaderValue::from(it.id as u64));
    }
    if oc_mtime(headers).is_some() {
        h.insert("X-OC-MTime", HeaderValue::from_static("accepted"));
    }
    r
}

fn dav_mkcol(app: &Shared, user: &User, drive: i64, rel: &str) -> Response {
    if rel.is_empty() {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    let (dir, name) = split_parent(rel);
    let parent = match app.store.ensure_folders(drive, dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    if app.store.child_by_name(drive, parent, name).ok().flatten().is_some() {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    match app.store.create_drive_folder(drive, parent, name, user.id) {
        Ok(_) => (StatusCode::CREATED, "").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

fn dav_delete(app: &Shared, drive: i64, rel: &str) -> Response {
    let it = match app.store.resolve_path(drive, rel) {
        Ok(Some(it)) => it,
        Ok(None) => return (StatusCode::NOT_FOUND, "").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match app.store.delete_drive_item(it.id) {
        Ok(files) => {
            for id in files {
                let _ = std::fs::remove_file(item_file(app, drive, id));
            }
            (StatusCode::NO_CONTENT, "").into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn dest_rel(headers: &HeaderMap) -> Option<String> {
    let d = headers.get("Destination").and_then(|v| v.to_str().ok())?;
    // Destination is an absolute URL or an absolute path; reduce to the DAV rel.
    let path = if let Some(rest) = d.strip_prefix("https://").or_else(|| d.strip_prefix("http://")) {
        rest.split_once('/').map(|(_, p)| format!("/{p}")).unwrap_or_else(|| "/".to_string())
    } else {
        d.to_string()
    };
    let path = path.split('?').next().unwrap_or("");
    Some(dav_rel(path))
}

fn dav_move(app: &Shared, user: &User, drive: i64, rel: &str, headers: &HeaderMap) -> Response {
    let src = match app.store.resolve_path(drive, rel) {
        Ok(Some(it)) => it,
        Ok(None) => return (StatusCode::NOT_FOUND, "").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let Some(dest) = dest_rel(headers) else { return (StatusCode::BAD_REQUEST, "").into_response() };
    let (dir, name) = split_parent(&dest);
    let parent = match app.store.ensure_folders(drive, dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    let overwrite = headers.get("Overwrite").and_then(|v| v.to_str().ok()) != Some("F");
    let existing = app.store.child_by_name(drive, parent, name).ok().flatten();
    let replaced = existing.is_some();
    if let Some(e) = existing {
        if !overwrite {
            return (StatusCode::PRECONDITION_FAILED, "").into_response();
        }
        if let Ok(files) = app.store.delete_drive_item(e.id) {
            for id in files {
                let _ = std::fs::remove_file(item_file(app, drive, id));
            }
        }
    }
    match app.store.relocate_item(src.id, parent, name, user.id) {
        Ok(_) => (if replaced { StatusCode::NO_CONTENT } else { StatusCode::CREATED }, "").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

// ------------------------------------------------------- chunked uploads

fn chunk_dir(app: &Shared, user_id: i64, transfer: &str) -> PathBuf {
    files_root(app).join("_chunks").join(user_id.to_string()).join(transfer)
}

/// Handle the `/remote.php/dav/uploads/<user>/...` chunking-v2 namespace.
async fn uploads(app: &Shared, user: &User, drive: i64, method: &Method, rest: &str, headers: &HeaderMap, body: Bytes) -> Response {
    // rest = "<user>/<transfer>[/<chunk>]"
    let decoded: Vec<String> = rest.split('/').filter(|s| !s.is_empty()).map(pct_decode).collect();
    if decoded.len() < 2 {
        // "<user>" only — accept OPTIONS/PROPFIND trivially.
        return match method.as_str() {
            "OPTIONS" => dav_options(),
            _ => (StatusCode::NOT_FOUND, "").into_response(),
        };
    }
    let transfer = sanitize_transfer(&decoded[1]);
    let dir = chunk_dir(app, user.id, &transfer);
    match method.as_str() {
        "OPTIONS" => dav_options(),
        "MKCOL" => {
            if std::fs::create_dir_all(&dir).is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            (StatusCode::CREATED, "").into_response()
        }
        "PUT" => {
            // decoded[2..] is the chunk name.
            let Some(chunk) = decoded.get(2) else { return (StatusCode::BAD_REQUEST, "").into_response() };
            let name = sanitize_transfer(chunk);
            if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(&name), &body)).is_err() {
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
            (StatusCode::CREATED, "").into_response()
        }
        "MOVE" => {
            // MOVE <uploads>/<transfer>/.file -> Destination in the user's files.
            let Some(dest) = dest_rel(headers) else { return (StatusCode::BAD_REQUEST, "").into_response() };
            assemble_upload(app, user, drive, &dir, &dest, headers)
        }
        "DELETE" => {
            let _ = std::fs::remove_dir_all(&dir);
            (StatusCode::NO_CONTENT, "").into_response()
        }
        "PROPFIND" => {
            // Report the chunks already stored, so the client can resume.
            let acct = &user.email;
            let mut b = String::from("<?xml version=\"1.0\"?>\n<d:multistatus xmlns:d=\"DAV:\">");
            b.push_str(&format!("<d:response><d:href>{}{}/{}/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>", DAV_UPLOADS, pct_encode_path(acct), pct_encode_path(&transfer)));
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                    let nm = e.file_name().to_string_lossy().into_owned();
                    b.push_str(&format!("<d:response><d:href>{}{}/{}/{}</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>{}</d:getcontentlength></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>", DAV_UPLOADS, pct_encode_path(acct), pct_encode_path(&transfer), pct_encode_path(&nm), len));
                }
            }
            b.push_str("</d:multistatus>");
            (StatusCode::MULTI_STATUS, [(header::CONTENT_TYPE, "application/xml; charset=utf-8")], b).into_response()
        }
        _ => (StatusCode::METHOD_NOT_ALLOWED, "").into_response(),
    }
}

fn sanitize_transfer(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.').collect()
}

fn assemble_upload(app: &Shared, user: &User, drive: i64, dir: &std::path::Path, dest: &str, headers: &HeaderMap) -> Response {
    // Collect chunk files, excluding the ".file" marker, sorted numerically.
    let mut chunks: Vec<(u64, PathBuf)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return (StatusCode::NOT_FOUND, "").into_response() };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == ".file" {
            continue;
        }
        let key: u64 = name.parse().unwrap_or(0);
        chunks.push((key, e.path()));
    }
    chunks.sort_by_key(|(k, _)| *k);

    let (parent_dir, fname) = split_parent(dest);
    let parent = match app.store.ensure_folders(drive, parent_dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    let existing = app.store.child_by_name(drive, parent, fname).ok().flatten();
    let ct = "application/octet-stream";
    let id = match &existing {
        Some(e) if e.kind == "file" => e.id,
        Some(_) => return (StatusCode::METHOD_NOT_ALLOWED, "").into_response(),
        None => match app.store.create_drive_file(drive, parent, fname, 0, ct, user.id) {
            Ok(id) => id,
            Err(e) => return (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
        },
    };
    let out_path = item_file(app, drive, id);
    let _ = std::fs::create_dir_all(out_path.parent().unwrap());
    let Ok(mut out) = std::fs::File::create(&out_path) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    let mut total: i64 = 0;
    for (_, p) in &chunks {
        let data = std::fs::read(p).unwrap_or_default();
        total += data.len() as i64;
        if out.write_all(&data).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let created = existing.is_none();
    let _ = app.store.set_file_meta(id, total, ct, user.id);
    if let Some(mt) = oc_mtime(headers) {
        let _ = app.store.set_item_mtime(id, mt);
    }
    let _ = std::fs::remove_dir_all(dir);
    let it = app.store.drive_item(id).ok().flatten();
    let mut r = (if created { StatusCode::CREATED } else { StatusCode::NO_CONTENT }, "").into_response();
    if let Some(it) = &it {
        r.headers_mut().insert(header::ETAG, HeaderValue::from_str(&etag(it)).unwrap());
        r.headers_mut().insert("OC-FileId", HeaderValue::from(it.id as u64));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use axum::body::Bytes;
    use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
    use base64::Engine;

    fn basic(email: &str, secret: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        let enc = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{secret}"));
        h.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!("Basic {enc}")).unwrap());
        h
    }

    async fn body_string(r: Response) -> String {
        let b = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&b).into_owned()
    }

    fn m(s: &str) -> Method {
        Method::from_bytes(s.as_bytes()).unwrap()
    }

    #[tokio::test]
    async fn status_reports_installed() {
        let (app, _u) = App::test();
        let r = handle(app, Method::GET, "/status.php", "", &HeaderMap::new(), Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert!(body_string(r).await.contains("\"installed\":true"));
    }

    #[test]
    fn login_flow_and_app_password_roundtrip() {
        let (app, user) = App::test();
        let s = &app.store;
        let (login, poll) = s.create_login_flow().unwrap();
        assert!(s.login_flow_pending(&login).unwrap());
        // Not granted yet -> poll returns nothing.
        assert!(s.poll_login_flow(&poll).unwrap().is_none());
        let pw = s.create_app_password(user.id, "Nextcloud").unwrap();
        s.complete_login_flow(&login, user.id, &pw).unwrap();
        let (login_name, app_pw) = s.poll_login_flow(&poll).unwrap().unwrap();
        assert_eq!(login_name, user.email);
        assert_eq!(app_pw, pw);
        // The app password authenticates that user.
        assert_eq!(s.app_password_user(&pw).unwrap(), Some(user.id));
    }

    #[tokio::test]
    async fn webdav_requires_auth() {
        let (app, _u) = App::test();
        let r = handle(app, m("PROPFIND"), "/remote.php/dav/files/x/", "", &HeaderMap::new(), Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn webdav_mkcol_propfind_and_put_get() {
        let dir = std::env::temp_dir().join(format!("ondura-nc-{}", token()));
        let (app, user) = App::test_in(dir.clone());
        // The Nextcloud interface is off by default; enable it for this user.
        let mut st = app.store.settings(user.id).unwrap();
        st.nextcloud = true;
        app.store.set_settings(user.id, &st).unwrap();
        let pw = app.store.create_app_password(user.id, "t").unwrap();
        let auth = basic(&user.email, &pw);
        let root = format!("/remote.php/dav/files/{}/", user.email);

        // Create a folder, then it shows up in a PROPFIND of the root.
        let mkcol = format!("/remote.php/dav/files/{}/Photos", user.email);
        let r = handle(app.clone(), m("MKCOL"), &mkcol, "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let drive = app.store.ensure_personal_drive(user.id).unwrap();
        assert!(app.store.resolve_path(drive, "Photos").unwrap().is_some());

        let mut pf = auth.clone();
        pf.insert("Depth", HeaderValue::from_static("1"));
        let r = handle(app.clone(), m("PROPFIND"), &root, "", &pf, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::MULTI_STATUS);
        assert!(body_string(r).await.contains("Photos"));

        // Upload a file and read it back.
        let put = format!("/remote.php/dav/files/{}/Photos/a.txt", user.email);
        let r = handle(app.clone(), m("PUT"), &put, "", &auth, Bytes::from_static(b"hello")).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let r = handle(app.clone(), Method::GET, &put, "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(body_string(r).await, "hello");

        // Delete removes it.
        let r = handle(app.clone(), m("DELETE"), &put, "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(app.store.resolve_path(drive, "Photos/a.txt").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
