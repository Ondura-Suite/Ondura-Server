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

/// Whether storing `new` bytes (replacing `old`) in a drive would exceed its
/// quota. Personal drives are metered by their owner's quota (0 = unlimited);
/// libraries are not metered.
fn over_quota(app: &Shared, drive: i64, old: i64, new: i64) -> bool {
    match app.store.drive_quota(drive) {
        Ok(Some(q)) if q > 0 => app.store.drive_total_size(drive).unwrap_or(0) - old + new > q,
        _ => false,
    }
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

/// Name of the virtual folder at the WebDAV home root that holds one folder per
/// shared library (SharePoint) the account belongs to. A personal item with this
/// name at the drive root is not reachable over WebDAV.
const LIBRARIES_DIR: &str = "SharePoint";

/// Where a WebDAV path points.
enum Loc {
    /// The virtual folder that lists the account's libraries.
    Libraries,
    /// A path inside a drive.
    Drive(Target),
}

/// A path inside a drive: the drive, the path relative to the drive root,
/// whether the account may write there, and the DAV path of the drive root.
struct Target {
    drive: i64,
    rel: String,
    writable: bool,
    base: String,
}

impl Target {
    /// The DAV-relative path (for hrefs) of `rel` inside this drive.
    fn href(&self, rel: &str) -> String {
        join(&self.base, rel)
    }
    /// The personal drive root, which is also the WebDAV home.
    fn is_home(&self) -> bool {
        self.base.is_empty() && self.rel.is_empty()
    }
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}/{b}"),
    }
}

/// The account's libraries as WebDAV folder names, each with its drive and
/// whether the account may write there. Clashing names get the library id
/// appended so every library has a distinct folder.
fn library_dirs(app: &Shared, user: &User) -> Vec<(String, i64, bool)> {
    let libs: Vec<_> = app.store.list_drives(user.id).unwrap_or_default().into_iter().filter(|d| d.kind == "library").collect();
    let clean = |n: &str| {
        let s: String = n.chars().map(|c| if c == '/' || c == '\\' || c.is_control() { '-' } else { c }).collect();
        let s = s.trim().to_string();
        if s.is_empty() || s == "." || s == ".." { "Library".to_string() } else { s }
    };
    let names: Vec<String> = libs.iter().map(|d| clean(&d.name)).collect();
    libs.iter()
        .zip(names.iter())
        .map(|(d, n)| {
            let clash = names.iter().filter(|x| x.eq_ignore_ascii_case(n)).count() > 1;
            let name = if clash { format!("{n} ({})", d.id) } else { n.clone() };
            (name, d.id, crate::cloud::Role::parse(&d.role).can_write())
        })
        .collect()
}

/// Resolve a DAV-relative path: the personal drive by default, or a library
/// under the virtual `SharePoint` folder. `None` for an unknown library.
fn locate(app: &Shared, user: &User, personal: i64, rel: &str) -> Option<Loc> {
    let (first, rest) = rel.split_once('/').unwrap_or((rel, ""));
    if first != LIBRARIES_DIR {
        return Some(Loc::Drive(Target { drive: personal, rel: rel.to_string(), writable: true, base: String::new() }));
    }
    if rest.is_empty() {
        return Some(Loc::Libraries);
    }
    let (lib, inner) = rest.split_once('/').unwrap_or((rest, ""));
    let (n, drive, _) = library_dirs(app, user).into_iter().find(|(n, _, _)| n == lib)?;
    // Inside a library the access can differ per folder: decide by the deepest
    // existing item on the path. A restricted folder the account isn't on the
    // access list of is treated as if it didn't exist.
    let role = app.store.item_role(user.id, drive, deepest_item(app, drive, inner)).ok().flatten()?;
    Some(Loc::Drive(Target { drive, rel: inner.to_string(), writable: role.can_write(), base: format!("{LIBRARIES_DIR}/{n}") }))
}

/// The deepest existing item on a drive-relative path: the item itself, or the
/// nearest folder above a path that doesn't exist yet (`None`: the drive root).
fn deepest_item(app: &Shared, drive: i64, rel: &str) -> Option<i64> {
    let parts: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
    (1..=parts.len()).rev().find_map(|n| app.store.resolve_path(drive, &parts[..n].join("/")).ok().flatten().map(|it| it.id))
}

/// 423 Locked when someone else has the item (or something below it) checked out.
fn locked(app: &Shared, user: &User, item: i64) -> bool {
    app.store.locked_for(item, Some(user.id)).unwrap_or(true)
}

async fn webdav(app: &Shared, user: &User, method: &Method, path: &str, headers: &HeaderMap, body: Bytes) -> Response {
    let personal = match app.store.ensure_personal_drive(user.id) {
        Ok(d) => d,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Chunked uploads live under their own path space.
    if let Some(rest) = path.strip_prefix(DAV_UPLOADS) {
        return uploads(app, user, personal, method, rest, headers, body).await;
    }

    let rel = dav_rel(path);
    let m = method.as_str();
    if m == "OPTIONS" {
        return dav_options();
    }
    let Some(loc) = locate(app, user, personal, &rel) else {
        // Not one of the account's libraries: nothing to read, and nothing may be
        // created in the virtual folder.
        return match m {
            "PUT" | "MKCOL" | "DELETE" | "MOVE" => (StatusCode::FORBIDDEN, "").into_response(),
            _ => (StatusCode::NOT_FOUND, "").into_response(),
        };
    };
    match (m, loc) {
        ("PROPFIND", Loc::Libraries) => propfind_libraries(app, user, headers),
        ("PROPFIND", Loc::Drive(t)) => propfind(app, user, &t, headers, DAV_FILES),
        ("PROPPATCH", _) => multistatus_ok(&rel, DAV_FILES),
        ("COPY", _) => (StatusCode::NOT_IMPLEMENTED, "").into_response(),
        ("GET" | "HEAD", Loc::Drive(t)) => dav_get(app, t.drive, &t.rel, m == "HEAD"),
        ("GET" | "HEAD", Loc::Libraries) => (StatusCode::NOT_FOUND, "").into_response(),
        // Nothing can be created in, or removed from, the virtual libraries folder.
        (_, Loc::Libraries) => (StatusCode::FORBIDDEN, "").into_response(),
        (_, Loc::Drive(t)) if !t.writable => (StatusCode::FORBIDDEN, "").into_response(),
        ("PUT", Loc::Drive(t)) => dav_put(app, user, &t, headers, &body),
        ("MKCOL", Loc::Drive(t)) => dav_mkcol(app, user, &t),
        ("DELETE", Loc::Drive(t)) => dav_delete(app, user, &t),
        ("MOVE", Loc::Drive(t)) => dav_move(app, user, personal, &t, headers),
        _ => (StatusCode::METHOD_NOT_ALLOWED, "").into_response(),
    }
}

/// Strip the WebDAV prefix and the user segment, returning the DAV-relative
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

/// The ETag of a folder (or a drive root when `folder` is `None`). It folds in
/// the newest change and item count below it, so it changes whenever anything
/// inside the folder does — sync clients rely on that to find changes.
fn collection_tag(app: &Shared, drive: i64, folder: Option<&Item>) -> String {
    let (newest, count) = app.store.subtree_state(drive, folder.map(|f| f.id)).unwrap_or((0, 0));
    match folder {
        Some(f) => format!("\"{}-{}-{count}\"", f.id, newest.max(f.updated)),
        None => format!("\"root-{drive}-{newest}-{count}\""),
    }
}

/// A combined ETag over several drive roots (the home and the libraries).
fn drives_tag(app: &Shared, label: &str, drives: &[i64]) -> String {
    let (mut newest, mut count) = (0, 0);
    for d in drives {
        let (n, c) = app.store.subtree_state(*d, None).unwrap_or((0, 0));
        newest = newest.max(n);
        count += c;
    }
    format!("\"{label}-{newest}-{count}-{}\"", drives.len())
}

fn propfind(app: &Shared, user: &User, t: &Target, headers: &HeaderMap, prefix: &str) -> Response {
    let depth = headers.get("Depth").and_then(|v| v.to_str().ok()).unwrap_or("1");
    let acct = user.email.clone();
    let target = if t.rel.is_empty() {
        None // the drive root
    } else {
        match app.store.resolve_path(t.drive, &t.rel) {
            Ok(Some(it)) => Some(it),
            Ok(None) => return (StatusCode::NOT_FOUND, "").into_response(),
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };
    let libs = if t.is_home() { library_dirs(app, user) } else { Vec::new() };
    let is_collection = target.as_ref().map(|it| it.kind == "folder").unwrap_or(true);
    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">");
    // The target itself.
    let (tag, perms, quota) = match &target {
        Some(it) if it.kind == "folder" => (collection_tag(app, t.drive, Some(it)), folder_perms(t.writable), None),
        Some(it) => (etag(it), file_perms(t.writable), None),
        None if t.is_home() => {
            let mut drives = vec![t.drive];
            drives.extend(libs.iter().map(|(_, d, _)| *d));
            (drives_tag(app, "home", &drives), "RGDNVCK", Some(app.store.drive_total_size(t.drive).unwrap_or(0)))
        }
        None => (collection_tag(app, t.drive, None), if t.writable { "GCKM" } else { "GM" }, None),
    };
    body.push_str(&response_xml(&acct, prefix, &t.href(&t.rel), target.as_ref(), &tag, perms, quota));
    // Its children, when it is a collection and Depth: 1.
    if is_collection && depth != "0" {
        let parent_id = target.as_ref().map(|it| it.id);
        if let Ok(items) = app.store.drive_items(t.drive, parent_id) {
            for it in items.iter().filter(|it| !(t.is_home() && it.name == LIBRARIES_DIR)) {
                let href = t.href(&join(&t.rel, &it.name));
                let (tag, perms) = if it.kind == "folder" {
                    // A restricted subfolder has its own audience and access.
                    let writable = if app.store.is_restricted(it.id).unwrap_or(false) {
                        match app.store.item_role(user.id, t.drive, Some(it.id)).ok().flatten() {
                            Some(r) => r.can_write(),
                            None => continue,
                        }
                    } else {
                        t.writable
                    };
                    (collection_tag(app, t.drive, Some(it)), folder_perms(writable))
                } else {
                    (etag(it), file_perms(t.writable))
                };
                body.push_str(&response_xml(&acct, prefix, &href, Some(it), &tag, perms, None));
            }
        }
        if !libs.is_empty() {
            let drives: Vec<i64> = libs.iter().map(|(_, d, _)| *d).collect();
            body.push_str(&response_xml(&acct, prefix, LIBRARIES_DIR, None, &drives_tag(app, "libs", &drives), "GM", None));
        }
    }
    body.push_str("</d:multistatus>");
    (StatusCode::MULTI_STATUS, [(header::CONTENT_TYPE, "application/xml; charset=utf-8")], body).into_response()
}

/// PROPFIND of the virtual `SharePoint` folder: one entry per library.
fn propfind_libraries(app: &Shared, user: &User, headers: &HeaderMap) -> Response {
    let depth = headers.get("Depth").and_then(|v| v.to_str().ok()).unwrap_or("1");
    let acct = user.email.clone();
    let libs = library_dirs(app, user);
    let drives: Vec<i64> = libs.iter().map(|(_, d, _)| *d).collect();
    let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<d:multistatus xmlns:d=\"DAV:\" xmlns:oc=\"http://owncloud.org/ns\" xmlns:nc=\"http://nextcloud.org/ns\">");
    body.push_str(&response_xml(&acct, DAV_FILES, LIBRARIES_DIR, None, &drives_tag(app, "libs", &drives), "GM", None));
    if depth != "0" {
        for (name, drive, writable) in &libs {
            let href = format!("{LIBRARIES_DIR}/{name}");
            body.push_str(&response_xml(&acct, DAV_FILES, &href, None, &collection_tag(app, *drive, None), if *writable { "GCKM" } else { "GM" }, None));
        }
    }
    body.push_str("</d:multistatus>");
    (StatusCode::MULTI_STATUS, [(header::CONTENT_TYPE, "application/xml; charset=utf-8")], body).into_response()
}

fn folder_perms(writable: bool) -> &'static str {
    if writable { "RGDNVCK" } else { "G" }
}

fn file_perms(writable: bool) -> &'static str {
    if writable { "RGDNVW" } else { "G" }
}

/// One `<d:response>`. `it` is `None` for a drive root or a virtual folder;
/// `quota` (bytes used) is reported on the home only.
fn response_xml(acct: &str, prefix: &str, rel: &str, it: Option<&Item>, tag: &str, perms: &str, quota: Option<i64>) -> String {
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
    let mut props = String::new();
    if collection {
        props.push_str("<d:resourcetype><d:collection/></d:resourcetype>");
        if let Some(used) = quota {
            props.push_str(&format!("<d:quota-used-bytes>{used}</d:quota-used-bytes><d:quota-available-bytes>-3</d:quota-available-bytes>"));
        }
    } else {
        let i = it.unwrap();
        props.push_str("<d:resourcetype/>");
        props.push_str(&format!("<d:getcontentlength>{}</d:getcontentlength>", i.size));
        props.push_str(&format!("<d:getcontenttype>{}</d:getcontenttype>", xml_escape(&i.content_type)));
        props.push_str(&format!("<oc:size>{}</oc:size>", i.size));
    }
    props.push_str(&format!("<oc:permissions>{perms}</oc:permissions>"));
    if let Some(i) = it {
        props.push_str(&format!("<oc:id>{}</oc:id>", i.id));
    }
    format!(
        "<d:response><d:href>{}</d:href><d:propstat><d:prop><d:getlastmodified>{}</d:getlastmodified><d:getetag>{}</d:getetag>{}</d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>",
        xml_escape(&href), modified, xml_escape(tag), props
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

fn dav_put(app: &Shared, user: &User, t: &Target, headers: &HeaderMap, body: &Bytes) -> Response {
    let drive = t.drive;
    if t.rel.is_empty() {
        return (StatusCode::CONFLICT, "").into_response();
    }
    let (dir, name) = split_parent(&t.rel);
    let parent = match app.store.ensure_folders(drive, dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    let existing = app.store.child_by_name(drive, parent, name).ok().flatten();
    if existing.as_ref().map(|e| e.kind != "file").unwrap_or(false) {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    if existing.as_ref().is_some_and(|e| locked(app, user, e.id)) {
        return (StatusCode::LOCKED, "").into_response();
    }
    if over_quota(app, drive, existing.as_ref().map(|e| e.size).unwrap_or(0), body.len() as i64) {
        return (StatusCode::INSUFFICIENT_STORAGE, "").into_response();
    }
    let ct = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).unwrap_or("application/octet-stream");
    let (id, created) = match &existing {
        Some(e) => {
            crate::webmail::drive_api::snapshot_version(app, drive, e.id, user.id);
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
    let file_path = item_file(app, drive, id);
    if std::fs::create_dir_all(file_path.parent().unwrap()).and_then(|_| std::fs::write(&file_path, body)).is_err() {
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

fn dav_mkcol(app: &Shared, user: &User, t: &Target) -> Response {
    if t.rel.is_empty() {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    let (dir, name) = split_parent(&t.rel);
    let parent = match app.store.ensure_folders(t.drive, dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    if app.store.child_by_name(t.drive, parent, name).ok().flatten().is_some() {
        return (StatusCode::METHOD_NOT_ALLOWED, "").into_response();
    }
    match app.store.create_drive_folder(t.drive, parent, name, user.id) {
        Ok(_) => (StatusCode::CREATED, "").into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

/// DELETE moves the item to the drive's recycle bin, like the webmail does, so
/// a deletion made on a synced device can be undone.
fn dav_delete(app: &Shared, user: &User, t: &Target) -> Response {
    if t.rel.is_empty() {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    let it = match app.store.resolve_path(t.drive, &t.rel) {
        Ok(Some(it)) => it,
        Ok(None) => return (StatusCode::NOT_FOUND, "").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if locked(app, user, it.id) {
        return (StatusCode::LOCKED, "").into_response();
    }
    match app.store.trash_item(it.id, user.id) {
        Ok(()) => (StatusCode::NO_CONTENT, "").into_response(),
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

/// MOVE within a drive, or between drives (the personal drive and a library, or
/// two libraries). A replaced destination goes to its drive's recycle bin.
fn dav_move(app: &Shared, user: &User, personal: i64, src: &Target, headers: &HeaderMap) -> Response {
    if src.rel.is_empty() {
        return (StatusCode::FORBIDDEN, "").into_response();
    }
    let item = match app.store.resolve_path(src.drive, &src.rel) {
        Ok(Some(it)) => it,
        Ok(None) => return (StatusCode::NOT_FOUND, "").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    if locked(app, user, item.id) {
        return (StatusCode::LOCKED, "").into_response();
    }
    let Some(dest) = dest_rel(headers) else { return (StatusCode::BAD_REQUEST, "").into_response() };
    let dst = match locate(app, user, personal, &dest) {
        Some(Loc::Drive(t)) if t.writable && !t.rel.is_empty() => t,
        Some(_) => return (StatusCode::FORBIDDEN, "").into_response(),
        None => return (StatusCode::CONFLICT, "").into_response(),
    };
    let (dir, name) = split_parent(&dst.rel);
    let parent = match app.store.ensure_folders(dst.drive, dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    let overwrite = headers.get("Overwrite").and_then(|v| v.to_str().ok()) != Some("F");
    let existing = app.store.child_by_name(dst.drive, parent, name).ok().flatten().filter(|e| e.id != item.id);
    let replaced = existing.is_some();
    if let Some(e) = existing {
        if !overwrite {
            return (StatusCode::PRECONDITION_FAILED, "").into_response();
        }
        if locked(app, user, e.id) {
            return (StatusCode::LOCKED, "").into_response();
        }
        if app.store.trash_item(e.id, user.id).is_err() {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    }
    let moved = if dst.drive == src.drive {
        app.store.relocate_item(item.id, parent, name, user.id)
    } else {
        let size = app.store.subtree_file_size(item.id).unwrap_or(0);
        if over_quota(app, dst.drive, 0, size) {
            return (StatusCode::INSUFFICIENT_STORAGE, "").into_response();
        }
        app.store.move_to_drive(item.id, dst.drive, parent, name, user.id).map(|files| {
            for f in files {
                crate::webmail::drive_api::move_item_disk(app, src.drive, dst.drive, f);
            }
        })
    };
    match moved {
        Ok(()) => (if replaced { StatusCode::NO_CONTENT } else { StatusCode::CREATED }, "").into_response(),
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
            match locate(app, user, drive, &dest) {
                Some(Loc::Drive(t)) if t.writable && !t.rel.is_empty() => assemble_upload(app, user, &t, &dir, headers),
                Some(_) => (StatusCode::FORBIDDEN, "").into_response(),
                None => (StatusCode::CONFLICT, "").into_response(),
            }
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

fn assemble_upload(app: &Shared, user: &User, t: &Target, dir: &std::path::Path, headers: &HeaderMap) -> Response {
    let (drive, dest) = (t.drive, t.rel.as_str());
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
    let assembled: i64 = chunks.iter().map(|(_, p)| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0) as i64).sum();

    let (parent_dir, fname) = split_parent(dest);
    let parent = match app.store.ensure_folders(drive, parent_dir, user.id) {
        Ok(p) => p,
        Err(_) => return (StatusCode::CONFLICT, "").into_response(),
    };
    let existing = app.store.child_by_name(drive, parent, fname).ok().flatten();
    if existing.as_ref().is_some_and(|e| locked(app, user, e.id)) {
        return (StatusCode::LOCKED, "").into_response();
    }
    if over_quota(app, drive, existing.as_ref().map(|e| e.size).unwrap_or(0), assembled) {
        let _ = std::fs::remove_dir_all(dir);
        return (StatusCode::INSUFFICIENT_STORAGE, "").into_response();
    }
    let ct = "application/octet-stream";
    let id = match &existing {
        Some(e) if e.kind == "file" => {
            // Keep the previous contents as a version before overwriting.
            crate::webmail::drive_api::snapshot_version(app, drive, e.id, user.id);
            e.id
        }
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

        // Delete removes it from the tree and puts it in the recycle bin.
        let r = handle(app.clone(), m("DELETE"), &put, "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(app.store.resolve_path(drive, "Photos/a.txt").unwrap().is_none());
        assert_eq!(app.store.trashed_items(drive).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Turn on the Nextcloud interface for a user and return Basic auth headers.
    fn nc_auth(app: &crate::app::Shared, user: &crate::store::User) -> HeaderMap {
        let mut st = app.store.settings(user.id).unwrap();
        st.nextcloud = true;
        app.store.set_settings(user.id, &st).unwrap();
        let pw = app.store.create_app_password(user.id, "t").unwrap();
        basic(&user.email, &pw)
    }

    fn with_depth(h: &HeaderMap, d: &'static str) -> HeaderMap {
        let mut h = h.clone();
        h.insert("Depth", HeaderValue::from_static(d));
        h
    }

    #[tokio::test]
    async fn restricted_folders_and_checkouts_apply_over_webdav() {
        let dir = std::env::temp_dir().join(format!("ondura-nc-{}", token()));
        let (app, owner) = App::test_in(dir.clone());
        let bob = app.store.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
        let lib = app.store.create_library(owner.id, "Team").unwrap();
        app.store.set_drive_member(lib, bob.id, crate::cloud::Role::Editor).unwrap();
        let owner_auth = nc_auth(&app, &owner);
        let auth = nc_auth(&app, &bob);
        let ohome = format!("/remote.php/dav/files/{}/", owner.email);
        let home = format!("/remote.php/dav/files/{}/", bob.email);

        let r = handle(app.clone(), m("PUT"), &format!("{ohome}SharePoint/Team/HR/salaries.txt"), "", &owner_auth, Bytes::from_static(b"x")).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let r = handle(app.clone(), m("PUT"), &format!("{ohome}SharePoint/Team/Open/spec.txt"), "", &owner_auth, Bytes::from_static(b"v1")).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let hr = app.store.resolve_path(lib, "HR").unwrap().unwrap().id;
        app.store.set_restricted(hr, true).unwrap();
        app.store.remove_item_acl(hr, bob.id).unwrap();

        // Bob doesn't see HR at all, but still sees and edits the rest.
        let r = handle(app.clone(), m("PROPFIND"), &format!("{home}SharePoint/Team/"), "", &with_depth(&auth, "1"), Bytes::new()).await;
        let listing = body_string(r).await;
        assert!(listing.contains("/Open/") && !listing.contains("/HR/"));
        let r = handle(app.clone(), Method::GET, &format!("{home}SharePoint/Team/HR/salaries.txt"), "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let r = handle(app.clone(), m("PUT"), &format!("{home}SharePoint/Team/HR/new.txt"), "", &auth, Bytes::from_static(b"x")).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // Read-only on the folder: listed, readable, not writable.
        app.store.set_item_acl(hr, bob.id, crate::cloud::Role::Viewer).unwrap();
        let r = handle(app.clone(), Method::GET, &format!("{home}SharePoint/Team/HR/salaries.txt"), "", &auth, Bytes::new()).await;
        assert_eq!(body_string(r).await, "x");
        let r = handle(app.clone(), m("DELETE"), &format!("{home}SharePoint/Team/HR/salaries.txt"), "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // A file checked out by the owner can't be changed by Bob until it's checked in.
        let spec = app.store.resolve_path(lib, "Open/spec.txt").unwrap().unwrap().id;
        app.store.check_out(spec, owner.id).unwrap();
        let put = format!("{home}SharePoint/Team/Open/spec.txt");
        let r = handle(app.clone(), m("PUT"), &put, "", &auth, Bytes::from_static(b"v2")).await;
        assert_eq!(r.status(), StatusCode::LOCKED);
        let r = handle(app.clone(), m("DELETE"), &format!("{home}SharePoint/Team/Open"), "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::LOCKED);
        let r = handle(app.clone(), m("PUT"), &format!("{ohome}SharePoint/Team/Open/spec.txt"), "", &owner_auth, Bytes::from_static(b"v2")).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        app.store.check_in(spec).unwrap();
        let r = handle(app.clone(), m("PUT"), &put, "", &auth, Bytes::from_static(b"v3")).await;
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn libraries_appear_under_sharepoint_and_are_writable() {
        let dir = std::env::temp_dir().join(format!("ondura-nc-{}", token()));
        let (app, user) = App::test_in(dir.clone());
        let auth = nc_auth(&app, &user);
        let home = format!("/remote.php/dav/files/{}/", user.email);

        // Without libraries there is no SharePoint folder in the home.
        let r = handle(app.clone(), m("PROPFIND"), &home, "", &with_depth(&auth, "1"), Bytes::new()).await;
        assert!(!body_string(r).await.contains("SharePoint"));

        let lib = app.store.create_library(user.id, "Team").unwrap();
        let r = handle(app.clone(), m("PROPFIND"), &home, "", &with_depth(&auth, "1"), Bytes::new()).await;
        assert!(body_string(r).await.contains("/SharePoint/"));
        let libs = format!("{home}SharePoint/");
        let r = handle(app.clone(), m("PROPFIND"), &libs, "", &with_depth(&auth, "1"), Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::MULTI_STATUS);
        assert!(body_string(r).await.contains("/SharePoint/Team/"));

        // Write into the library and read it back; it lands in the library drive.
        let put = format!("{home}SharePoint/Team/Docs/plan.txt");
        let r = handle(app.clone(), m("PUT"), &put, "", &auth, Bytes::from_static(b"plan")).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        assert!(app.store.resolve_path(lib, "Docs/plan.txt").unwrap().is_some());
        let r = handle(app.clone(), Method::GET, &put, "", &auth, Bytes::new()).await;
        assert_eq!(body_string(r).await, "plan");

        // Nothing can be created directly in the virtual folder, and an unknown
        // library is not found.
        let r = handle(app.clone(), m("MKCOL"), &format!("{home}SharePoint/New"), "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
        let r = handle(app.clone(), m("PROPFIND"), &format!("{home}SharePoint/Nope/"), "", &auth, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn viewers_can_read_but_not_write_a_library() {
        let dir = std::env::temp_dir().join(format!("ondura-nc-{}", token()));
        let (app, owner) = App::test_in(dir.clone());
        let bob = app.store.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
        let lib = app.store.create_library(owner.id, "Team").unwrap();
        app.store.set_drive_member(lib, bob.id, crate::cloud::Role::Viewer).unwrap();
        let owner_auth = nc_auth(&app, &owner);
        let bob_auth = nc_auth(&app, &bob);

        let put = format!("/remote.php/dav/files/{}/SharePoint/Team/a.txt", owner.email);
        let r = handle(app.clone(), m("PUT"), &put, "", &owner_auth, Bytes::from_static(b"hi")).await;
        assert_eq!(r.status(), StatusCode::CREATED);

        let bob_file = format!("/remote.php/dav/files/{}/SharePoint/Team/a.txt", bob.email);
        let r = handle(app.clone(), Method::GET, &bob_file, "", &bob_auth, Bytes::new()).await;
        assert_eq!(body_string(r).await, "hi");
        for method in ["PUT", "DELETE", "MKCOL"] {
            let r = handle(app.clone(), m(method), &bob_file, "", &bob_auth, Bytes::from_static(b"x")).await;
            assert_eq!(r.status(), StatusCode::FORBIDDEN, "{method}");
        }
        // The listing advertises read-only permissions.
        let r = handle(app.clone(), m("PROPFIND"), &format!("/remote.php/dav/files/{}/SharePoint/Team/", bob.email), "", &with_depth(&bob_auth, "1"), Bytes::new()).await;
        assert!(body_string(r).await.contains("<oc:permissions>G</oc:permissions>"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn move_between_personal_drive_and_library() {
        let dir = std::env::temp_dir().join(format!("ondura-nc-{}", token()));
        let (app, user) = App::test_in(dir.clone());
        let auth = nc_auth(&app, &user);
        let lib = app.store.create_library(user.id, "Team").unwrap();
        let personal = app.store.ensure_personal_drive(user.id).unwrap();
        let home = format!("/remote.php/dav/files/{}/", user.email);

        let src = format!("{home}Old/report.txt");
        handle(app.clone(), m("PUT"), &src, "", &auth, Bytes::from_static(b"v1")).await;
        // Overwrite once so the file carries a version that has to move too.
        handle(app.clone(), m("PUT"), &src, "", &auth, Bytes::from_static(b"v2")).await;
        let id = app.store.resolve_path(personal, "Old/report.txt").unwrap().unwrap().id;
        assert_eq!(app.store.item_versions(id).unwrap().len(), 1);

        // Move the whole folder into the library under a new name.
        let mut h = auth.clone();
        h.insert("Destination", HeaderValue::from_str(&format!("https://mail.example.com{home}SharePoint/Team/Archive")).unwrap());
        let r = handle(app.clone(), m("MOVE"), &format!("{home}Old"), "", &h, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        assert!(app.store.resolve_path(personal, "Old").unwrap().is_none());
        assert!(app.store.resolve_path(lib, "Archive/report.txt").unwrap().is_some());

        // The contents (and its version) followed the item to the library.
        let r = handle(app.clone(), Method::GET, &format!("{home}SharePoint/Team/Archive/report.txt"), "", &auth, Bytes::new()).await;
        assert_eq!(body_string(r).await, "v2");
        let item = app.store.drive_item(id).unwrap().unwrap();
        assert_eq!(item.drive_id, lib);
        assert_eq!(app.store.item_versions(id).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn folder_etag_changes_when_its_contents_change() {
        let dir = std::env::temp_dir().join(format!("ondura-nc-{}", token()));
        let (app, user) = App::test_in(dir.clone());
        let auth = nc_auth(&app, &user);
        let home = format!("/remote.php/dav/files/{}/", user.email);
        handle(app.clone(), m("MKCOL"), &format!("{home}A"), "", &auth, Bytes::new()).await;
        handle(app.clone(), m("MKCOL"), &format!("{home}A/B"), "", &auth, Bytes::new()).await;

        let tag_of = |body: String| -> String {
            // The first response is the folder itself.
            let s = body.split("<d:getetag>").nth(1).unwrap();
            s.split("</d:getetag>").next().unwrap().to_string()
        };
        let a = format!("{home}A/");
        let before = tag_of(body_string(handle(app.clone(), m("PROPFIND"), &a, "", &with_depth(&auth, "0"), Bytes::new()).await).await);
        // A change two levels down must change A's ETag.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        handle(app.clone(), m("PUT"), &format!("{home}A/B/new.txt"), "", &auth, Bytes::from_static(b"x")).await;
        let after = tag_of(body_string(handle(app.clone(), m("PROPFIND"), &a, "", &with_depth(&auth, "0"), Bytes::new()).await).await);
        assert_ne!(before, after);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn chunked_upload_into_a_library() {
        let dir = std::env::temp_dir().join(format!("ondura-nc-{}", token()));
        let (app, user) = App::test_in(dir.clone());
        let auth = nc_auth(&app, &user);
        let lib = app.store.create_library(user.id, "Team").unwrap();
        let up = format!("/remote.php/dav/uploads/{}/tx1", user.email);
        assert_eq!(handle(app.clone(), m("MKCOL"), &up, "", &auth, Bytes::new()).await.status(), StatusCode::CREATED);
        handle(app.clone(), m("PUT"), &format!("{up}/00001"), "", &auth, Bytes::from_static(b"hello ")).await;
        handle(app.clone(), m("PUT"), &format!("{up}/00002"), "", &auth, Bytes::from_static(b"world")).await;
        let mut h = auth.clone();
        h.insert("Destination", HeaderValue::from_str(&format!("/remote.php/dav/files/{}/SharePoint/Team/big.bin", user.email)).unwrap());
        let r = handle(app.clone(), m("MOVE"), &format!("{up}/.file"), "", &h, Bytes::new()).await;
        assert_eq!(r.status(), StatusCode::CREATED);
        let it = app.store.resolve_path(lib, "big.bin").unwrap().unwrap();
        assert_eq!(it.size, 11);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
