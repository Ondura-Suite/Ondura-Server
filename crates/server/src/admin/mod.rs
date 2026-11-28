//! Admin Center: web administration at `/admin` (login with an admin
//! mailbox) and its JSON API under `/admin/api`.

use std::net::IpAddr;
use std::sync::OnceLock;
use std::time::Instant;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use crate::app::Shared;
use crate::auth::{self, LoginError};
use crate::dns;
use crate::smtp::dkim;
use crate::store::{User, normalize_domain};

const INDEX: &str = include_str!("index.html");
const LOGO: &str = include_str!("logo.svg");
const COOKIE: &str = "ondura_admin";
const SESSION_LIFETIME: i64 = 8 * 3600;

pub fn started() -> Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

type ApiResult = Result<Value, (StatusCode, String)>;

fn bad(msg: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn not_found() -> (StatusCode, String) {
    (StatusCode::NOT_FOUND, "Not found".into())
}

fn internal(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn json_response(r: ApiResult) -> Response {
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
        HeaderValue::from_static("default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'"),
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

fn session_user(app: &Shared, headers: &HeaderMap) -> Option<User> {
    let token = session_token(headers)?;
    app.store.session_user(&token).ok().flatten().filter(|u| u.is_admin && u.enabled)
}

pub async fn handle(app: Shared, method: Method, path: &str, query: &str, headers: &HeaderMap, body: Bytes, ip: IpAddr) -> Response {
    let _ = started();
    let rest = path.trim_start_matches("/admin");
    if !rest.starts_with("/api") {
        if rest.is_empty() {
            return security_headers(axum::response::Redirect::temporary("/admin/").into_response());
        }
        if rest == "/logo.svg" {
            let mut r = (StatusCode::OK, LOGO).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"));
            return r;
        }
        let mut r = (StatusCode::OK, INDEX).into_response();
        r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        return security_headers(r);
    }
    let api = rest.trim_start_matches("/api");
    // Mutating requests must come from the Admin Center itself.
    if method != Method::GET && headers.get("X-Ondura-Admin").is_none() {
        return json_response(Err((StatusCode::FORBIDDEN, "missing request header".into())));
    }
    if api == "/login" && method == Method::POST {
        return security_headers(login(&app, &body, ip, headers).await);
    }
    let Some(admin) = session_user(&app, headers) else {
        return json_response(Err((StatusCode::UNAUTHORIZED, "Not signed in".into())));
    };
    let _ = query;
    // A mailbox avatar: binary download, raw upload or removal.
    if let Some(rest) = api.strip_prefix("/mailboxes/")
        && let Some(id) = rest.strip_suffix("/avatar")
    {
        return match find_user(&app, id) {
            Ok(u) => match method {
                Method::GET => security_headers(crate::api::serve_avatar(&app, &u.id.to_string())),
                Method::PUT | Method::POST => security_headers(crate::api::set_avatar(&app, u.id, headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()), &body)),
                Method::DELETE => security_headers(crate::api::delete_avatar(&app, u.id)),
                _ => json_response(Err(not_found())),
            },
            Err(e) => json_response(Err(e)),
        };
    }
    let result = route(&app, &admin, &method, api, query, &body, headers).await;
    security_headers(json_response(result))
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
    #[serde(default)]
    code: Option<String>,
}

async fn login(app: &Shared, body: &Bytes, ip: IpAddr, headers: &HeaderMap) -> Response {
    let Ok(req) = serde_json::from_slice::<LoginBody>(body) else { return json_response(Err(bad("invalid request"))) };
    let store = app.store.clone();
    let email = req.email.clone();
    let res = tokio::task::spawn_blocking(move || auth::login(&store, &req.email, &req.password, Some(ip))).await;
    let user = match res {
        Ok(Ok(u)) => u,
        Ok(Err(LoginError::Locked)) => {
            crate::audit::failed_sign_in(app, &email, "admin", ip, true);
            return json_response(Err((StatusCode::TOO_MANY_REQUESTS, "Too many failed sign-ins. Try again later.".into())));
        }
        _ => {
            crate::audit::failed_sign_in(app, &email, "admin", ip, false);
            return json_response(Err((StatusCode::UNAUTHORIZED, "The e-mail address or password is incorrect.".into())));
        }
    };
    if !user.is_admin {
        return json_response(Err((StatusCode::FORBIDDEN, "This account has no administrator rights.".into())));
    }
    if let Some(r) = crate::webmail::second_step(app, &user, req.code.as_deref()) {
        return security_headers(r);
    }
    let token = match app.store.create_session(user.id, SESSION_LIFETIME) {
        Ok(t) => t,
        Err(e) => return json_response(Err(internal(e))),
    };
    app.store.audit(&user.email, "admin.login", &ip.to_string());
    info!(user = %user.email, %ip, "Admin Center sign-in");
    let plain = headers.get("X-Forwarded-Proto").is_some_and(|v| v.as_bytes() == b"http") || (!app.cfg.http.behind_proxy && app.https_tls.is_none());
    let secure = if plain { "" } else { " Secure;" };
    let mut r = axum::Json(json!({ "email": user.email, "name": user.name })).into_response();
    r.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!("{COOKIE}={token}; Path=/admin; HttpOnly;{secure} SameSite=Strict; Max-Age={SESSION_LIFETIME}")).expect("cookie"),
    );
    r
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> Result<T, (StatusCode, String)> {
    serde_json::from_slice(body).map_err(|e| bad(format!("invalid request: {e}")))
}

fn user_json(app: &Shared, u: &User, stats: Option<(i64, i64)>) -> Value {
    let (items, bytes) = stats.unwrap_or_else(|| (0, app.store.used_bytes(u.id).unwrap_or(0)));
    json!({
        "id": u.id, "email": u.email, "name": u.name, "kind": u.kind, "isAdmin": u.is_admin, "enabled": u.enabled,
        "mailbox": u.mailbox,
        "twoStep": app.store.totp_enabled(u.id).unwrap_or(false),
        "quota": u.quota, "items": items, "used": bytes, "created": u.created,
        "aliases": app.store.aliases_of(u.id).unwrap_or_default(),
    })
}

fn find_user(app: &Shared, id: &str) -> Result<User, (StatusCode, String)> {
    let id: i64 = id.parse().map_err(|_| not_found())?;
    app.store.user(id).map_err(internal)?.ok_or_else(not_found)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewMailbox {
    email: String,
    #[serde(default)]
    name: String,
    password: Option<String>,
    #[serde(default = "default_kind")]
    kind: String,
    #[serde(default)]
    quota: i64,
    #[serde(default)]
    is_admin: bool,
    /// Provision a mailbox with the account. Defaults to true.
    #[serde(default = "crate::serde_true")]
    mailbox: bool,
}

fn default_kind() -> String {
    "user".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MailboxUpdate {
    name: Option<String>,
    quota: Option<i64>,
    is_admin: Option<bool>,
    enabled: Option<bool>,
    mailbox: Option<bool>,
    forward_to: Option<String>,
    keep_forwarded_copy: Option<bool>,
    time_zone: Option<String>,
}

#[derive(Deserialize)]
struct PasswordBody {
    password: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AliasBody {
    address: String,
    mailbox_id: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PermissionBody {
    owner_id: i64,
    grantee_id: i64,
    full_access: bool,
    send_as: bool,
}

#[derive(Deserialize)]
struct DomainBody {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceBody {
    user_id: i64,
    device_id: String,
}

async fn route(app: &Shared, admin: &User, method: &Method, api: &str, query: &str, body: &Bytes, headers: &HeaderMap) -> ApiResult {
    let store = &app.store;
    let parts: Vec<&str> = api.trim_matches('/').split('/').filter(|p| !p.is_empty()).collect();
    let audit = |action: &str, detail: &str| store.audit(&admin.email, action, detail);
    match (method.as_str(), parts.as_slice()) {
        ("GET", ["me"]) => Ok(json!({ "email": admin.email, "name": admin.name, "hostname": app.cfg.hostname, "version": env!("CARGO_PKG_VERSION") })),
        ("POST", ["logout"]) => {
            if let Some(t) = session_token(headers) {
                let _ = store.delete_session(&t);
            }
            Ok(json!({}))
        }
        ("GET", ["overview"]) => {
            let users = store.users().map_err(internal)?;
            let stats = store.user_stats().map_err(internal)?;
            let used: i64 = stats.values().map(|s| s.1).sum();
            let devices = store.devices(None).map_err(internal)?;
            let queue = store.queue().map_err(internal)?;
            let active_devices = devices.iter().filter(|d| crate::store::now() - d.last_seen < 7 * 86_400).count();
            Ok(json!({
                "hostname": app.cfg.hostname,
                "version": env!("CARGO_PKG_VERSION"),
                "uptime": started().elapsed().as_secs(),
                "tls": app.https_tls.is_some() || app.cfg.http.behind_proxy,
                "mailboxes": users.iter().filter(|u| u.kind == "user").count(),
                "shared": users.iter().filter(|u| u.kind == "shared").count(),
                "rooms": users.iter().filter(|u| u.kind == "room").count(),
                "domains": store.domains().map_err(internal)?.len(),
                "storage": used,
                "devices": devices.len(),
                "activeDevices": active_devices,
                "queue": queue.len(),
                "queueDeferred": queue.iter().filter(|q| q.attempts > 0).count(),
                "spamFilter": app.cfg.spam.rspamd.is_some(),
                "endpoints": {
                    "activesync": format!("https://{}/Microsoft-Server-ActiveSync", app.cfg.hostname),
                    "ews": format!("https://{}/EWS/Exchange.asmx", app.cfg.hostname),
                    "autodiscover": format!("https://{}/autodiscover/autodiscover.xml", app.cfg.hostname),
                    "smtp": format!("{}:587 (STARTTLS), {}:465 (TLS)", app.cfg.hostname, app.cfg.hostname),
                }
            }))
        }

        // -------------------------------------------------------- mailboxes
        ("GET", ["mailboxes"]) => {
            let stats = store.user_stats().map_err(internal)?;
            let drives = store.drive_usage_by_owner().map_err(internal)?;
            let list: Vec<Value> = store
                .users()
                .map_err(internal)?
                .iter()
                .map(|u| {
                    let mut v = user_json(app, u, Some(stats.get(&u.id).copied().unwrap_or((0, 0))));
                    v["driveUsed"] = json!(drives.get(&u.id).copied().unwrap_or(0));
                    v
                })
                .collect();
            Ok(json!(list))
        }
        ("POST", ["mailboxes"]) => {
            let req: NewMailbox = parse(body)?;
            if !matches!(req.kind.as_str(), "user" | "shared" | "room") {
                return Err(bad("kind must be user, shared or room"));
            }
            let generated = req.password.as_deref().is_none_or(str::is_empty);
            let password = req.password.filter(|p| !p.is_empty()).unwrap_or_else(auth::random_password);
            let u = store.create_user(&req.email, &req.name, &password, &req.kind).map_err(|e| bad(e.to_string()))?;
            store.update_user(u.id, None, Some(req.quota), Some(req.is_admin && req.kind == "user"), None).map_err(internal)?;
            if !req.mailbox {
                store.set_mailbox(u.id, false).map_err(internal)?;
            }
            audit("mailbox.create", &u.email);
            let u = store.user(u.id).map_err(internal)?.ok_or_else(not_found)?;
            let mut v = user_json(app, &u, None);
            if generated && req.kind == "user" {
                v["password"] = json!(password);
            }
            Ok(v)
        }
        ("GET", ["mailboxes", id]) => {
            let u = find_user(app, id)?;
            let mut v = user_json(app, &u, None);
            let settings = store.settings(u.id).map_err(internal)?;
            v["forwardTo"] = json!(settings.forward_to);
            v["keepForwardedCopy"] = json!(settings.keep_forwarded_copy);
            v["timeZone"] = json!(settings.time_zone);
            v["oof"] = json!({ "state": settings.oof.state, "internal": settings.oof.internal_reply, "external": settings.oof.external_reply });
            v["sendAs"] = json!(store.send_as_addresses(u.id).map_err(internal)?);
            v["profile"] = json!(store.profile(u.id).map_err(internal)?);
            v["storage"] = json!(store.storage_usage(u.id, u.quota).map_err(internal)?);
            let users = store.users().map_err(internal)?;
            let name_of = |id: i64| users.iter().find(|x| x.id == id).map(|x| x.email.clone()).unwrap_or_default();
            let perms: Vec<Value> = store
                .permissions()
                .map_err(internal)?
                .into_iter()
                .filter(|(o, g, _, _)| *o == u.id || *g == u.id)
                .map(|(o, g, full, send)| json!({ "ownerId": o, "owner": name_of(o), "granteeId": g, "grantee": name_of(g), "fullAccess": full, "sendAs": send }))
                .collect();
            v["permissions"] = json!(perms);
            let folders: Vec<Value> = store
                .folders(u.id)
                .map_err(internal)?
                .into_iter()
                .filter(|f| f.class != crate::model::FolderClass::Root)
                .map(|f| {
                    let (total, unread) = store.unread_count(f.id).unwrap_or((0, 0));
                    json!({ "id": f.id, "name": f.name, "class": f.class.as_str(), "total": total, "unread": unread, "size": store.folder_size(f.id).unwrap_or(0) })
                })
                .collect();
            v["folders"] = json!(folders);
            v["devices"] = json!(store.devices(Some(u.id)).map_err(internal)?);
            v["avatar"] = json!(store.avatar_updated(u.id).map_err(internal)?);
            Ok(v)
        }
        ("PATCH", ["mailboxes", id]) => {
            let u = find_user(app, id)?;
            let req: MailboxUpdate = parse(body)?;
            if u.id == admin.id && (req.is_admin == Some(false) || req.enabled == Some(false)) {
                return Err(bad("You cannot remove your own administrator rights or disable your own mailbox."));
            }
            store.update_user(u.id, req.name.as_deref(), req.quota, req.is_admin.filter(|_| u.kind == "user"), req.enabled).map_err(internal)?;
            if let Some(m) = req.mailbox {
                store.set_mailbox(u.id, m).map_err(internal)?;
            }
            if req.forward_to.is_some() || req.keep_forwarded_copy.is_some() || req.time_zone.is_some() {
                let mut s = store.settings(u.id).map_err(internal)?;
                if let Some(f) = req.forward_to {
                    let f = f.trim().to_ascii_lowercase();
                    if !f.is_empty() && !f.contains('@') {
                        return Err(bad("invalid forwarding address"));
                    }
                    s.forward_to = (!f.is_empty()).then_some(f);
                }
                if let Some(k) = req.keep_forwarded_copy {
                    s.keep_forwarded_copy = k;
                }
                if let Some(tz) = req.time_zone {
                    if !tz.is_empty() && tz.parse::<chrono_tz::Tz>().is_err() {
                        return Err(bad("unknown time zone"));
                    }
                    s.time_zone = (!tz.is_empty()).then_some(tz);
                }
                store.set_settings(u.id, &s).map_err(internal)?;
            }
            audit("mailbox.update", &u.email);
            Ok(user_json(app, &store.user(u.id).map_err(internal)?.ok_or_else(not_found)?, None))
        }
        ("POST", ["mailboxes", id, "password"]) => {
            let u = find_user(app, id)?;
            let req: PasswordBody = parse(body)?;
            let generated = req.password.as_deref().is_none_or(str::is_empty);
            let password = req.password.filter(|p| !p.is_empty()).unwrap_or_else(auth::random_password);
            if password.chars().count() < 8 && !generated {
                return Err(bad("The password must have at least 8 characters."));
            }
            store.set_password(u.id, &password).map_err(internal)?;
            audit("mailbox.password", &u.email);
            Ok(if generated { json!({ "password": password }) } else { json!({}) })
        }
        ("POST", ["mailboxes", id, "profile"]) => {
            let u = find_user(app, id)?;
            let p: crate::directory::Profile = parse(body)?;
            store.set_profile(u.id, &p.cleaned()).map_err(internal)?;
            audit("mailbox.profile", &u.email);
            Ok(json!(store.profile(u.id).map_err(internal)?))
        }
        ("POST", ["mailboxes", id, "2fa", "reset"]) => {
            // For a lost phone: turns two-step sign-in off so the person can
            // sign in with the password and set it up again.
            let u = find_user(app, id)?;
            store.totp_disable(u.id).map_err(internal)?;
            audit("mailbox.2fa.reset", &u.email);
            Ok(json!({}))
        }
        ("DELETE", ["mailboxes", id]) => {
            let u = find_user(app, id)?;
            if u.id == admin.id {
                return Err(bad("You cannot delete your own mailbox."));
            }
            store.delete_user(u.id).map_err(internal)?;
            audit("mailbox.delete", &u.email);
            Ok(json!({}))
        }

        // ---------------------------------------------- aliases, permissions
        ("POST", ["aliases"]) => {
            let req: AliasBody = parse(body)?;
            store.user(req.mailbox_id).map_err(internal)?.ok_or_else(not_found)?;
            store.add_alias(&req.address, req.mailbox_id).map_err(|e| bad(e.to_string()))?;
            audit("alias.add", &req.address);
            Ok(json!({}))
        }
        ("DELETE", ["aliases", address]) => {
            let address = urldecode(address);
            store.remove_alias(&address).map_err(internal)?;
            audit("alias.remove", &address);
            Ok(json!({}))
        }
        ("POST", ["aliases", "primary"]) => {
            let req: AliasBody = parse(body)?;
            store.user(req.mailbox_id).map_err(internal)?.ok_or_else(not_found)?;
            store.set_primary_address(req.mailbox_id, &req.address).map_err(|e| bad(e.to_string()))?;
            audit("alias.primary", &req.address);
            Ok(json!({}))
        }
        ("GET", ["aliases"]) => {
            let users = store.users().map_err(internal)?;
            let list: Vec<Value> = store
                .aliases()
                .map_err(internal)?
                .into_iter()
                .map(|(a, id)| json!({ "address": a, "mailboxId": id, "mailbox": users.iter().find(|u| u.id == id).map(|u| u.email.clone()).unwrap_or_default() }))
                .collect();
            Ok(json!(list))
        }
        ("POST", ["permissions"]) => {
            let req: PermissionBody = parse(body)?;
            if req.owner_id == req.grantee_id {
                return Err(bad("A mailbox cannot be granted rights on itself."));
            }
            store.grant(req.owner_id, req.grantee_id, req.full_access, req.send_as).map_err(internal)?;
            audit("permission.set", &format!("{} -> {} full={} sendas={}", req.grantee_id, req.owner_id, req.full_access, req.send_as));
            Ok(json!({}))
        }

        // --------------------------------------------------------- domains
        ("GET", ["domains"]) => {
            let users = store.users().map_err(internal)?;
            let list: Vec<Value> = store
                .domains()
                .map_err(internal)?
                .into_iter()
                .map(|d| {
                    let count = users.iter().filter(|u| u.domain() == d).count();
                    json!({ "name": d, "mailboxes": count, "dkim": store.dkim(&d).ok().flatten(), "records": dns::records(store, &app.cfg.hostname, &d) })
                })
                .collect();
            Ok(json!({ "domains": list, "hostHint": dns::host_hint(&app.cfg.hostname) }))
        }
        ("POST", ["domains"]) => {
            let req: DomainBody = parse(body)?;
            let name = normalize_domain(&req.name).map_err(|e| bad(e.to_string()))?;
            store.add_domain(&name).map_err(internal)?;
            if store.dkim(&name).map_err(internal)?.is_none() {
                dkim::generate(store, &name).map_err(internal)?;
            }
            audit("domain.add", &name);
            Ok(json!({ "name": name }))
        }
        ("POST", ["domains", name, "dkim"]) => {
            let name = urldecode(name);
            dkim::generate(store, &name).map_err(internal)?;
            audit("domain.dkim", &name);
            Ok(json!({}))
        }
        ("DELETE", ["domains", name]) => {
            let name = urldecode(name);
            store.remove_domain(&name).map_err(|e| bad(e.to_string()))?;
            audit("domain.remove", &name);
            Ok(json!({}))
        }

        // ----------------------------------------------------------- queue
        ("GET", ["queue"]) => {
            let list: Vec<Value> = store
                .queue()
                .map_err(internal)?
                .into_iter()
                .map(|m| {
                    let meta = crate::mime::parse_meta(&m.mime);
                    json!({ "id": m.id, "sender": m.sender, "recipients": m.recipients, "subject": meta.subject, "size": m.mime.len(), "attempts": m.attempts, "nextAttempt": m.next_attempt, "created": m.created, "lastError": m.last_error })
                })
                .collect();
            Ok(json!(list))
        }
        ("POST", ["queue", id, "retry"]) => {
            store.retry_now(id.parse().map_err(|_| not_found())?).map_err(internal)?;
            app.queue_wake.notify_one();
            Ok(json!({}))
        }
        ("DELETE", ["queue", id]) => {
            store.dequeue(id.parse().map_err(|_| not_found())?).map_err(internal)?;
            audit("queue.delete", id);
            Ok(json!({}))
        }

        // --------------------------------------------------------- devices
        ("GET", ["devices"]) => {
            let users = store.users().map_err(internal)?;
            let list: Vec<Value> = store
                .devices(None)
                .map_err(internal)?
                .into_iter()
                .map(|d| {
                    let mut v = json!(d);
                    v["user"] = json!(users.iter().find(|u| u.id == d.user_id).map(|u| u.email.clone()).unwrap_or_default());
                    v
                })
                .collect();
            Ok(json!(list))
        }
        ("POST", ["devices", "wipe"]) => {
            let req: DeviceBody = parse(body)?;
            store.set_wipe(req.user_id, &req.device_id, 1).map_err(internal)?;
            audit("device.wipe", &req.device_id);
            Ok(json!({}))
        }
        ("POST", ["devices", "block"]) => {
            let req: DeviceBody = parse(body)?;
            let d = store.device(req.user_id, &req.device_id).map_err(internal)?.ok_or_else(not_found)?;
            store.set_blocked(req.user_id, &req.device_id, !d.blocked).map_err(internal)?;
            audit(if d.blocked { "device.unblock" } else { "device.block" }, &req.device_id);
            Ok(json!({ "blocked": !d.blocked }))
        }
        ("POST", ["devices", "remove"]) => {
            let req: DeviceBody = parse(body)?;
            store.remove_device(req.user_id, &req.device_id).map_err(internal)?;
            audit("device.remove", &req.device_id);
            Ok(json!({}))
        }

        ("GET", ["settings"]) => Ok(app.settings_json()),
        ("PATCH", ["settings"]) => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Patch {
                activesync_provisioning: Option<bool>,
                activesync_require_password: Option<bool>,
                collab_enabled: Option<bool>,
                audit_retention_days: Option<u32>,
            }
            let p: Patch = parse(body)?;
            if p.audit_retention_days.is_some_and(|d| d > 3650) {
                return Err(bad("Keep audit entries for at most 3650 days."));
            }
            let patch = crate::app::SettingsOverrides {
                activesync_provisioning: p.activesync_provisioning,
                activesync_require_password: p.activesync_require_password,
                collab_enabled: p.collab_enabled,
                audit_retention_days: p.audit_retention_days,
            };
            audit("settings.change", &String::from_utf8_lossy(body));
            app.update_settings(&patch).map_err(internal)?;
            Ok(app.settings_json())
        }
        ("GET", ["audit"]) => {
            let f = crate::audit::Filter::from_query(query);
            let list = store.audit_search(&f).map_err(internal)?;
            Ok(json!({ "entries": list, "actions": store.audit_actions().map_err(internal)? }))
        }
        ("GET", ["audit", "export.csv"]) => {
            let mut f = crate::audit::Filter::from_query(query);
            f.limit = 50_000;
            let list = store.audit_search(&f).map_err(internal)?;
            audit("audit.export", &list.len().to_string());
            Ok(json!({ "csv": crate::audit::to_csv(&list) }))
        }
        _ => Err(not_found()),
    }
}

fn urldecode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&b[i + 1..(i + 3).min(b.len())]).unwrap_or("zz"), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn req(app: &Shared, method: &str, path: &str, cookie: Option<&str>, body: Value) -> (StatusCode, Value, Option<String>) {
        let mut r = Request::builder().method(method).uri(path).header("X-Ondura-Admin", "1").header("Content-Type", "application/json");
        if let Some(c) = cookie {
            r = r.header("Cookie", c);
        }
        let resp = crate::http::router(app.clone()).oneshot(r.body(Body::from(body.to_string())).unwrap()).await.unwrap();
        let status = resp.status();
        let cookie = resp.headers().get("set-cookie").map(|c| c.to_str().unwrap().split(';').next().unwrap().to_string());
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null), cookie)
    }

    #[tokio::test]
    async fn admin_sign_in_needs_the_code_and_admins_can_reset_it() {
        let (app, user) = crate::app::App::test();
        app.store.update_user(user.id, None, None, Some(true), None).unwrap();
        let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
        let secret = app.store.totp_begin(user.id).unwrap();
        let key = crate::totp::base32_decode(&secret).unwrap();
        let step = crate::store::now() / 30;
        app.store.totp_enable(user.id, &format!("{:06}", crate::totp::code_at(&key, step))).unwrap();
        let (s, v, _) = req(&app, "POST", "/admin/api/login", None, json!({ "email": "niklas@example.com", "password": "secret" })).await;
        assert_eq!((s, v["mfa"].clone()), (StatusCode::UNAUTHORIZED, json!(true)));
        let code = format!("{:06}", crate::totp::code_at(&key, step + 1));
        let (s, _, cookie) = req(&app, "POST", "/admin/api/login", None, json!({ "email": "niklas@example.com", "password": "secret", "code": code })).await;
        assert_eq!(s, StatusCode::OK);
        let c = cookie.unwrap();
        // Another account's two-step sign-in is shown and can be reset.
        let s2 = app.store.totp_begin(anna.id).unwrap();
        let k2 = crate::totp::base32_decode(&s2).unwrap();
        app.store.totp_enable(anna.id, &format!("{:06}", crate::totp::code_at(&k2, step))).unwrap();
        let (_, v, _) = req(&app, "GET", &format!("/admin/api/mailboxes/{}", anna.id), Some(&c), json!(null)).await;
        assert_eq!(v["twoStep"], true);
        let (s, _, _) = req(&app, "POST", &format!("/admin/api/mailboxes/{}/2fa/reset", anna.id), Some(&c), json!({})).await;
        assert_eq!(s, StatusCode::OK);
        assert!(!app.store.totp_enabled(anna.id).unwrap());
    }

    #[tokio::test]
    async fn admin_api() {
        let (app, user) = crate::app::App::test();
        let (s, _, _) = req(&app, "POST", "/admin/api/login", None, json!({ "email": "niklas@example.com", "password": "secret" })).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        app.store.update_user(user.id, None, None, Some(true), None).unwrap();
        let (s, _, cookie) = req(&app, "POST", "/admin/api/login", None, json!({ "email": "niklas@example.com", "password": "secret" })).await;
        assert_eq!(s, StatusCode::OK);
        let c = cookie.unwrap();
        let (_, v, _) = req(&app, "POST", "/admin/api/mailboxes", Some(&c), json!({ "email": "anna@example.com", "name": "Anna" })).await;
        assert_eq!(v["email"], "anna@example.com");
        assert!(v["password"].as_str().unwrap().len() >= 12);
        let id = v["id"].as_i64().unwrap();
        let (s, _, _) = req(&app, "POST", "/admin/api/aliases", Some(&c), json!({ "address": "info@example.com", "mailboxId": id })).await;
        assert_eq!(s, StatusCode::OK);
        let (_, v, _) = req(&app, "GET", &format!("/admin/api/mailboxes/{id}"), Some(&c), Value::Null).await;
        assert_eq!(v["aliases"][0], "info@example.com");
        assert!(v["folders"].as_array().unwrap().len() >= 10);
        let (_, v, _) = req(&app, "GET", "/admin/api/domains", Some(&c), Value::Null).await;
        assert_eq!(v["domains"][0]["name"], "example.com");
        let (_, v, _) = req(&app, "GET", "/admin/api/overview", Some(&c), Value::Null).await;
        assert_eq!(v["mailboxes"], 2);
        let (s, _, _) = req(&app, "DELETE", &format!("/admin/api/mailboxes/{}", user.id), Some(&c), Value::Null).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let (s, _, _) = req(&app, "GET", "/admin/api/overview", None, Value::Null).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
}
