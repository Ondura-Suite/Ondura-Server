//! Exchange ActiveSync (MS-ASHTTP, MS-ASCMD), protocol versions 12.1–14.1.

pub mod commands;
pub mod convert;
pub mod sync;
#[cfg(test)]
mod device_tests;

use std::collections::HashMap;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ondura_wbxml::Element;
use tracing::{debug, warn};

use crate::app::Shared;
use crate::model::FolderClass;
use crate::store::{Folder, User};

pub const VERSIONS: &str = "12.0,12.1,14.0,14.1";
pub const COMMANDS: &str = "Sync,SendMail,SmartForward,SmartReply,GetAttachment,GetHierarchy,CreateCollection,DeleteCollection,MoveCollection,FolderSync,FolderCreate,FolderDelete,FolderUpdate,MoveItems,GetItemEstimate,MeetingResponse,Search,Settings,Ping,ItemOperations,Provision,ResolveRecipients,ValidateCert";

pub struct Ctx {
    pub app: Shared,
    pub user: User,
    pub device_id: String,
    pub version: f32,
    /// Writer tag of this device (changes made by it are not echoed back).
    pub writer: String,
    /// Addresses of the user (own, aliases, send-as).
    pub me: Vec<String>,
    pub params: HashMap<String, String>,
    pub save_in_sent: bool,
}

impl Ctx {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.get(&name.to_ascii_lowercase()).map(String::as_str)
    }
}

/// Parsed request line: command plus parameters (plain or base64 form).
#[derive(Debug, Default)]
pub struct Query {
    pub cmd: String,
    pub user: String,
    pub device_id: String,
    pub device_type: String,
    pub version: Option<String>,
    pub policy_key: Option<u32>,
    pub params: HashMap<String, String>,
    pub save_in_sent: bool,
    pub accept_multipart: bool,
}

const COMMAND_CODES: &[(u8, &str)] = &[
    (0, "Sync"),
    (1, "SendMail"),
    (2, "SmartForward"),
    (3, "SmartReply"),
    (4, "GetAttachment"),
    (9, "FolderSync"),
    (10, "FolderCreate"),
    (11, "FolderDelete"),
    (12, "FolderUpdate"),
    (13, "MoveItems"),
    (14, "GetItemEstimate"),
    (15, "MeetingResponse"),
    (16, "Search"),
    (17, "Settings"),
    (18, "Ping"),
    (19, "ItemOperations"),
    (20, "Provision"),
    (21, "ResolveRecipients"),
    (22, "ValidateCert"),
];

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn parse_query(q: &str) -> Option<Query> {
    if q.contains('=') {
        let mut out = Query::default();
        for part in q.split('&') {
            let (k, v) = part.split_once('=').unwrap_or((part, ""));
            let v = percent_decode(v);
            match k.to_ascii_lowercase().as_str() {
                "cmd" => out.cmd = v,
                "user" => out.user = v,
                "deviceid" => out.device_id = v,
                "devicetype" => out.device_type = v,
                "saveinsent" => out.save_in_sent = v.eq_ignore_ascii_case("T"),
                other => {
                    out.params.insert(other.to_string(), v);
                }
            }
        }
        return Some(out);
    }
    // Base64-encoded query (MS-ASHTTP 2.2.1.1.1.1).
    let raw = B64.decode(percent_decode(q).trim()).ok()?;
    let mut i = 0;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = raw.get(i..i + n)?;
        i += n;
        Some(s)
    };
    let version = take(1)?[0];
    let cmd = take(1)?[0];
    take(2)?;
    let dev_len = take(1)?[0] as usize;
    let device = take(dev_len)?;
    let device_id = if device.iter().all(|b| b.is_ascii_graphic()) { String::from_utf8_lossy(device).into_owned() } else { hex::encode_upper(device) };
    let pk_len = take(1)?[0] as usize;
    let pk = take(pk_len)?;
    let policy_key = (pk_len == 4).then(|| u32::from_le_bytes([pk[0], pk[1], pk[2], pk[3]]));
    let dt_len = take(1)?[0] as usize;
    let device_type = String::from_utf8_lossy(take(dt_len)?).into_owned();
    let mut out = Query {
        cmd: COMMAND_CODES.iter().find(|(c, _)| *c == cmd).map(|(_, n)| n.to_string()).unwrap_or_default(),
        device_id,
        device_type,
        version: Some(format!("{}.{}", version / 10, version % 10)),
        policy_key,
        ..Default::default()
    };
    while let Some(tag) = take(1).map(|t| t[0]) {
        let Some(len) = take(1).map(|l| l[0] as usize) else { break };
        let Some(value) = take(len) else { break };
        let v = String::from_utf8_lossy(value).into_owned();
        match tag {
            0 => {
                out.params.insert("attachmentname".into(), v);
            }
            1 => {
                out.params.insert("collectionid".into(), v);
            }
            3 => {
                out.params.insert("itemid".into(), v);
            }
            4 => {
                out.params.insert("longid".into(), v);
            }
            6 => {
                out.params.insert("occurrence".into(), v);
            }
            7 => {
                let flags = value.first().copied().unwrap_or(0);
                out.save_in_sent = flags & 1 != 0;
                out.accept_multipart = flags & 2 != 0;
            }
            8 => out.user = v,
            _ => {}
        }
    }
    Some(out)
}

pub fn wbxml(el: &Element) -> Response {
    debug!(response = %el.to_xml(), "EAS response");
    let mut r = (StatusCode::OK, el.encode()).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.ms-sync.wbxml"));
    r
}

pub fn empty() -> Response {
    (StatusCode::OK, Body::empty()).into_response()
}

pub fn status(code: StatusCode) -> Response {
    (code, Body::empty()).into_response()
}

fn with_server_headers(mut r: Response) -> Response {
    let h = r.headers_mut();
    h.insert("MS-Server-ActiveSync", HeaderValue::from_static("14.1"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
    r
}

pub fn options_response() -> Response {
    let mut r = (StatusCode::OK, Body::empty()).into_response();
    let h = r.headers_mut();
    h.insert("MS-ASProtocolVersions", HeaderValue::from_static(VERSIONS));
    h.insert("MS-ASProtocolCommands", HeaderValue::from_static(COMMANDS));
    h.insert(header::ALLOW, HeaderValue::from_static("OPTIONS,POST"));
    h.insert("Public", HeaderValue::from_static("OPTIONS,POST"));
    with_server_headers(r)
}

/// Entry point for `/Microsoft-Server-ActiveSync`.
pub async fn handle(app: Shared, user: User, method: Method, query: &str, headers: &HeaderMap, body: Bytes) -> Response {
    if method == Method::OPTIONS {
        return options_response();
    }
    if method != Method::POST {
        return status(StatusCode::METHOD_NOT_ALLOWED);
    }
    let Some(q) = parse_query(query) else { return status(StatusCode::BAD_REQUEST) };
    if q.device_id.is_empty() || q.cmd.is_empty() {
        return status(StatusCode::BAD_REQUEST);
    }
    let version_str = headers.get("MS-ASProtocolVersion").and_then(|v| v.to_str().ok()).map(str::to_string).or(q.version.clone()).unwrap_or_else(|| "14.1".into());
    let version: f32 = version_str.parse().unwrap_or(14.1);
    let policy_key = headers.get("X-MS-PolicyKey").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u32>().ok()).or(q.policy_key);
    let user_agent = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let device = match app.store.touch_device(user.id, &q.device_id, &q.device_type, user_agent) {
        Ok(d) => d,
        Err(e) => {
            warn!("device registry: {e:#}");
            return status(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    if device.blocked {
        return status(StatusCode::FORBIDDEN);
    }
    let me = app.store.send_as_addresses(user.id).unwrap_or_default();
    let ctx = Ctx {
        writer: format!("eas:{}", q.device_id),
        app: app.clone(),
        user,
        device_id: q.device_id.clone(),
        version,
        me,
        params: q.params,
        save_in_sent: q.save_in_sent,
    };

    // Provisioning: a device must accept the policy before it may sync.
    let needs_policy = !matches!(q.cmd.as_str(), "Provision" | "Ping" | "GetAttachment");
    let wipe_pending = device.wipe == 1;
    if (app.activesync_provisioning() || wipe_pending) && needs_policy && (policy_key.is_none_or(|k| k as i64 != device.policy_key) || device.policy_key == 0 || wipe_pending) {
        debug!(device = %q.device_id, "device must provision");
        return with_server_headers(if version >= 14.0 { not_provisioned(&q.cmd) } else { status(StatusCode::from_u16(449).expect("valid status")) });
    }

    let is_wbxml = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_none_or(|c| c.contains("wbxml"));
    let doc = if body.is_empty() || !is_wbxml {
        None
    } else {
        match Element::decode(&body) {
            Ok(d) => {
                debug!(cmd = %q.cmd, request = %d.to_xml(), "EAS request");
                Some(d)
            }
            Err(e) => {
                warn!(cmd = %q.cmd, "invalid WBXML: {e}");
                return status(StatusCode::BAD_REQUEST);
            }
        }
    };
    let result = match q.cmd.as_str() {
        "FolderSync" => sync::folder_sync(&ctx, doc.as_ref()),
        "FolderCreate" => sync::folder_create(&ctx, doc.as_ref()),
        "FolderUpdate" => sync::folder_update(&ctx, doc.as_ref()),
        "FolderDelete" => sync::folder_delete(&ctx, doc.as_ref()),
        "Sync" => sync::sync(&ctx, doc.as_ref()).await,
        "GetItemEstimate" => sync::get_item_estimate(&ctx, doc.as_ref()),
        "Ping" => sync::ping(&ctx, doc.as_ref()).await,
        "Provision" => commands::provision(&ctx, doc.as_ref()),
        "SendMail" => commands::send_mail(&ctx, doc.as_ref(), &body).await,
        "SmartReply" => commands::smart(&ctx, doc.as_ref(), &body, false).await,
        "SmartForward" => commands::smart(&ctx, doc.as_ref(), &body, true).await,
        "MoveItems" => commands::move_items(&ctx, doc.as_ref()),
        "ItemOperations" => commands::item_operations(&ctx, doc.as_ref()),
        "GetAttachment" => commands::get_attachment(&ctx),
        "MeetingResponse" => commands::meeting_response(&ctx, doc.as_ref()).await,
        "Settings" => commands::settings(&ctx, doc.as_ref()),
        "ResolveRecipients" => commands::resolve_recipients(&ctx, doc.as_ref()),
        "Search" => commands::search(&ctx, doc.as_ref()),
        "ValidateCert" => commands::validate_cert(&ctx, doc.as_ref()),
        other => {
            warn!(cmd = other, "unsupported ActiveSync command");
            Ok(status(StatusCode::NOT_IMPLEMENTED))
        }
    };
    with_server_headers(match result {
        Ok(r) => r,
        Err(e) => {
            warn!(cmd = %q.cmd, user = %ctx.user.email, "ActiveSync error: {e:#}");
            status(StatusCode::INTERNAL_SERVER_ERROR)
        }
    })
}

/// Status 142 (DeviceNotProvisioned) in the shape of the command response.
fn not_provisioned(cmd: &str) -> Response {
    let root = match cmd {
        "FolderSync" => "FolderHierarchy:FolderSync",
        "FolderCreate" => "FolderHierarchy:FolderCreate",
        "FolderUpdate" => "FolderHierarchy:FolderUpdate",
        "FolderDelete" => "FolderHierarchy:FolderDelete",
        "Sync" => "AirSync:Sync",
        "GetItemEstimate" => "GetItemEstimate:GetItemEstimate",
        "MoveItems" => "Move:Moves",
        "ItemOperations" => "ItemOperations:ItemOperations",
        "MeetingResponse" => "MeetingResponse:MeetingResponse",
        "Settings" => "Settings:Settings",
        "ResolveRecipients" => "ResolveRecipients:ResolveRecipients",
        "Search" => "Search:Search",
        "SendMail" | "SmartReply" | "SmartForward" => "ComposeMail:SendMail",
        "ValidateCert" => "ValidateCert:ValidateCert",
        _ => "FolderHierarchy:FolderSync",
    };
    let ns = root.split(':').next().unwrap_or("AirSync");
    wbxml(&Element::new(root).with_text(&format!("{ns}:Status"), "142"))
}

// ------------------------------------------------------------- folder ids

/// ActiveSync folder type (MS-ASCMD 2.2.3.186.3).
pub fn folder_type(f: &Folder) -> u8 {
    match (f.role.as_deref(), f.class) {
        (Some("inbox"), _) => 2,
        (Some("drafts"), _) => 3,
        (Some("deleteditems"), _) => 4,
        (Some("sentitems"), _) => 5,
        (Some("outbox"), _) => 6,
        (Some("tasks"), _) => 7,
        (Some("calendar"), _) => 8,
        (Some("contacts"), _) => 9,
        (Some("notes"), _) => 10,
        (_, FolderClass::Mail) => 12,
        (_, FolderClass::Calendar) => 13,
        (_, FolderClass::Contacts) => 14,
        (_, FolderClass::Tasks) => 15,
        (_, FolderClass::Notes) => 17,
        (_, FolderClass::Root) => 1,
    }
}

pub fn class_name(c: FolderClass) -> &'static str {
    match c {
        FolderClass::Calendar => "Calendar",
        FolderClass::Contacts => "Contacts",
        FolderClass::Tasks => "Tasks",
        FolderClass::Notes => "Notes",
        _ => "Email",
    }
}

/// Folder of the user by ActiveSync id.
pub fn folder(ctx: &Ctx, id: &str) -> Option<Folder> {
    let id: i64 = id.trim().parse().ok()?;
    ctx.app.store.folder(id).ok().flatten().filter(|f| f.user_id == ctx.user.id)
}

/// Item id from an ActiveSync ServerId (`folder:item`) or LongId.
pub fn item_id(server_id: &str) -> Option<i64> {
    server_id.rsplit(':').next()?.trim().parse().ok()
}

pub fn server_id(folder_id: i64, item_id: i64) -> String {
    format!("{folder_id}:{item_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_and_base64_queries() {
        let q = parse_query("Cmd=Sync&User=niklas%40example.com&DeviceId=ABC123&DeviceType=iPhone").unwrap();
        assert_eq!((q.cmd.as_str(), q.user.as_str(), q.device_id.as_str()), ("Sync", "niklas@example.com", "ABC123"));
        // Version 14.1, Ping, locale, device id "ab", policy key 7, type "SP", SaveInSent flag.
        let raw = [141u8, 18, 0x07, 0x04, 2, b'a', b'b', 4, 7, 0, 0, 0, 2, b'S', b'P', 7, 1, 1];
        let q = parse_query(&B64.encode(raw)).unwrap();
        assert_eq!(q.cmd, "Ping");
        assert_eq!(q.version.as_deref(), Some("14.1"));
        assert_eq!(q.policy_key, Some(7));
        assert_eq!(q.device_type, "SP");
        assert!(q.save_in_sent);
    }
}
