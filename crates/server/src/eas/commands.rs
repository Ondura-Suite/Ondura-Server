//! Provision, SendMail/SmartReply/SmartForward, MoveItems, ItemOperations,
//! GetAttachment, MeetingResponse, Settings, ResolveRecipients, Search and
//! ValidateCert.

use anyhow::Result;
use axum::body::Bytes;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ondura_wbxml::{Element, Node};
use tracing::{info, warn};

use super::convert::{self, BodyPrefs};
use super::sync::render;
use super::{Ctx, class_name, empty, folder, item_id, server_id, status, wbxml};
use crate::calendar;
use crate::directory;
use crate::mime;
use crate::model::{ItemKind, Response as Answer, flags};
use crate::smtp::submit;
use crate::store::{OofSettings, now};

// -------------------------------------------------------------- Provision

const POLICY_TYPE: &str = "MS-EAS-Provisioning-WBXML";

fn policy_doc(ctx: &Ctx) -> Element {
    let a = &ctx.app.cfg.activesync;
    let pw = a.require_password;
    let mut d = Element::new("Provision:EASProvisionDoc")
        .with_text("Provision:DevicePasswordEnabled", if pw { "1" } else { "0" })
        .with_text("Provision:AlphanumericDevicePasswordRequired", "0")
        .with_text("Provision:PasswordRecoveryEnabled", "0")
        .with_text("Provision:DeviceEncryptionEnabled", "0")
        .with_text("Provision:AttachmentsEnabled", "1");
    if pw {
        d.push_text("Provision:MinDevicePasswordLength", a.min_password_length.to_string());
        d.push_text("Provision:MaxInactivityTimeDeviceLock", "900");
        d.push_text("Provision:MaxDevicePasswordFailedAttempts", "10");
    }
    for (k, v) in [
        ("MaxAttachmentSize", ""),
        ("AllowSimpleDevicePassword", "1"),
        ("DevicePasswordExpiration", ""),
        ("DevicePasswordHistory", "0"),
        ("AllowStorageCard", "1"),
        ("AllowCamera", "1"),
        ("RequireDeviceEncryption", "0"),
        ("AllowUnsignedApplications", "1"),
        ("AllowUnsignedInstallationPackages", "1"),
        ("MinDevicePasswordComplexCharacters", "1"),
        ("AllowWiFi", "1"),
        ("AllowTextMessaging", "1"),
        ("AllowPOPIMAPEmail", "1"),
        ("AllowBluetooth", "2"),
        ("AllowIrDA", "1"),
        ("RequireManualSyncWhenRoaming", "0"),
        ("AllowDesktopSync", "1"),
        ("MaxCalendarAgeFilter", "0"),
        ("AllowHTMLEmail", "1"),
        ("MaxEmailAgeFilter", "0"),
        ("MaxEmailBodyTruncationSize", "-1"),
        ("MaxEmailHTMLBodyTruncationSize", "-1"),
        ("RequireSignedSMIMEMessages", "0"),
        ("RequireEncryptedSMIMEMessages", "0"),
        ("RequireSignedSMIMEAlgorithm", "0"),
        ("RequireEncryptionSMIMEAlgorithm", "0"),
        ("AllowSMIMEEncryptionAlgorithmNegotiation", "2"),
        ("AllowSMIMESoftCerts", "1"),
        ("AllowBrowser", "1"),
        ("AllowConsumerEmail", "1"),
        ("AllowRemoteDesktop", "1"),
        ("AllowInternetSharing", "1"),
    ] {
        d.push(Element::new(&format!("Provision:{k}")).text(v));
    }
    d.push(Element::new("Provision:UnapprovedInROMApplicationList"));
    d.push(Element::new("Provision:ApprovedApplicationList"));
    d
}

fn random_key() -> u32 {
    let mut b = [0u8; 4];
    rand::fill(&mut b);
    (u32::from_le_bytes(b) % 2_000_000_000) + 1
}

fn device_info(d: &Element) -> serde_json::Value {
    let set = d.find("Settings:Set").or_else(|| d.find("Settings:DeviceInformation"));
    let mut map = serde_json::Map::new();
    if let Some(set) = set {
        for e in set.elements() {
            map.insert(e.tag.to_string(), serde_json::Value::String(e.content()));
        }
    }
    serde_json::Value::Object(map)
}

pub fn provision(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let store = &ctx.app.store;
    let device = store.device(ctx.user.id, &ctx.device_id)?;
    let Some(d) = doc else { return Ok(wbxml(&Element::new("Provision:Provision").with_text("Provision:Status", "2"))) };
    // Acknowledged remote wipe.
    if let Some(w) = d.child("Provision:RemoteWipe") {
        let ok = w.text_of("Provision:Status").is_some_and(|s| s.trim() == "1");
        if ok {
            store.set_wipe(ctx.user.id, &ctx.device_id, 2)?;
            store.audit(&ctx.user.email, "device.wiped", &ctx.device_id);
        }
        return Ok(wbxml(&Element::new("Provision:Provision").with_text("Provision:Status", "1")));
    }
    if device.as_ref().is_some_and(|d| d.wipe == 1) {
        info!(user = %ctx.user.email, device = %ctx.device_id, "sending remote wipe");
        return Ok(wbxml(&Element::new("Provision:Provision").with_text("Provision:Status", "1").with(Element::new("Provision:RemoteWipe"))));
    }
    let policy = d.path("Policies/Policy");
    let mut root = Element::new("Provision:Provision");
    if d.has("Settings:DeviceInformation") {
        store.set_device_info(ctx.user.id, &ctx.device_id, &device_info(d))?;
        root.push(Element::new("Settings:DeviceInformation").with_text("Settings:Status", "1"));
    }
    root.push_text("Provision:Status", "1");
    let policy_type = policy.and_then(|p| p.text_of("Provision:PolicyType")).unwrap_or_else(|| POLICY_TYPE.into());
    match policy.and_then(|p| p.text_of("Provision:PolicyKey")) {
        // Phase 2: the device acknowledges the policy.
        Some(key) => {
            let temp = store.device_state(ctx.user.id, &ctx.device_id, "temp_policy_key")?;
            let accepted = policy.and_then(|p| p.text_of("Provision:Status")).is_none_or(|s| s.trim() == "1" || s.trim() == "2");
            let p = if temp.as_deref() == Some(key.trim()) && accepted {
                let final_key = random_key();
                store.set_policy_key(ctx.user.id, &ctx.device_id, final_key as i64)?;
                Element::new("Provision:Policy").with_text("Provision:PolicyType", policy_type).with_text("Provision:Status", "1").with_text("Provision:PolicyKey", final_key.to_string())
            } else {
                Element::new("Provision:Policy").with_text("Provision:PolicyType", policy_type).with_text("Provision:Status", "5")
            };
            root.push(Element::new("Provision:Policies").with(p));
        }
        // Phase 1: send the policy with a temporary key.
        None => {
            let temp = random_key();
            store.set_device_state(ctx.user.id, &ctx.device_id, "temp_policy_key", &temp.to_string())?;
            let p = Element::new("Provision:Policy")
                .with_text("Provision:PolicyType", policy_type)
                .with_text("Provision:Status", "1")
                .with_text("Provision:PolicyKey", temp.to_string())
                .with(Element::new("Provision:Data").with(policy_doc(ctx)));
            root.push(Element::new("Provision:Policies").with(p));
        }
    }
    Ok(wbxml(&root))
}

// --------------------------------------------------------------- SendMail

fn compose_error(tag: &str, code: &str) -> Response {
    wbxml(&Element::new(&format!("ComposeMail:{tag}")).with_text("ComposeMail:Status", code))
}

fn mime_of(doc: Option<&Element>, body: &Bytes) -> Option<Vec<u8>> {
    match doc {
        Some(d) => d.child("ComposeMail:MIME").map(|m| m.bytes()),
        None => (!body.is_empty()).then(|| body.to_vec()),
    }
}

pub async fn send_mail(ctx: &Ctx, doc: Option<&Element>, body: &Bytes) -> Result<Response> {
    let Some(raw) = mime_of(doc, body) else { return Ok(compose_error("SendMail", "102")) };
    let save = doc.map(|d| d.has("ComposeMail:SaveInSentItems")).unwrap_or(ctx.save_in_sent);
    match submit::send(&ctx.app, &ctx.user, raw, submit::Options { save_copy: save, ..Default::default() }).await {
        Ok(()) => Ok(empty()),
        Err(e) => {
            warn!(user = %ctx.user.email, "SendMail failed: {e:#}");
            Ok(compose_error("SendMail", "120"))
        }
    }
}

pub async fn smart(ctx: &Ctx, doc: Option<&Element>, body: &Bytes, forward: bool) -> Result<Response> {
    let tag = if forward { "SmartForward" } else { "SmartReply" };
    let Some(raw) = mime_of(doc, body) else { return Ok(compose_error(tag, "102")) };
    let save = doc.map(|d| d.has("ComposeMail:SaveInSentItems")).unwrap_or(ctx.save_in_sent);
    let source = doc.and_then(|d| d.child("ComposeMail:Source"));
    let original_id = source
        .and_then(|s| s.text_of("ComposeMail:ItemId").or_else(|| s.text_of("ComposeMail:LongId")))
        .or_else(|| ctx.param("ItemId").map(str::to_string))
        .and_then(|id| item_id(&id));
    let original = original_id.and_then(|id| ctx.app.store.item(id).ok().flatten()).filter(|i| i.user_id == ctx.user.id);
    let replace = doc.is_some_and(|d| d.has("ComposeMail:ReplaceMime"));
    let message = match (&original, replace) {
        (Some(o), false) if o.kind == ItemKind::Mail => {
            let orig_raw = ctx.app.store.mime(o.id)?.unwrap_or_default();
            mime::combine_with_original(&raw, &orig_raw, forward)
        }
        _ => raw,
    };
    match submit::send(&ctx.app, &ctx.user, message, submit::Options { save_copy: save, ..Default::default() }).await {
        Ok(()) => {
            if let Some(o) = original {
                let flag = if forward { flags::FORWARDED } else { flags::ANSWERED };
                let _ = ctx.app.store.update_flags(o.id, flag, 0, None, "eas:smart");
            }
            Ok(empty())
        }
        Err(e) => {
            warn!(user = %ctx.user.email, "{tag} failed: {e:#}");
            Ok(compose_error(tag, "120"))
        }
    }
}

// -------------------------------------------------------------- MoveItems

pub fn move_items(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let mut root = Element::new("Move:Moves");
    let Some(d) = doc else { return Ok(wbxml(&root)) };
    for m in d.children_named("Move:Move") {
        let src = m.text_of("Move:SrcMsgId").unwrap_or_default();
        let dst_folder = m.text_of("Move:DstFldId").and_then(|f| folder(ctx, &f));
        let mut r = Element::new("Move:Response").with_text("Move:SrcMsgId", src.clone());
        let item = item_id(&src).and_then(|id| ctx.app.store.item(id).ok().flatten()).filter(|i| i.user_id == ctx.user.id);
        match (item, dst_folder) {
            (None, _) => r.push_text("Move:Status", "1"),
            (_, None) => r.push_text("Move:Status", "2"),
            (Some(i), Some(f)) if i.folder_id == f.id => r.push_text("Move:Status", "4"),
            (Some(i), Some(f)) => match ctx.app.store.move_item(i.id, f.id, &ctx.writer) {
                Ok(()) => {
                    r.push_text("Move:Status", "3");
                    r.push_text("Move:DstMsgId", server_id(f.id, i.id));
                }
                Err(_) => r.push_text("Move:Status", "5"),
            },
        }
        root.push(r);
    }
    Ok(wbxml(&root))
}

// --------------------------------------------------------- ItemOperations

fn attachment(ctx: &Ctx, reference: &str) -> Option<(mime::AttachmentInfo, Vec<u8>)> {
    let (item, index) = reference.rsplit_once(':')?;
    let item: i64 = item.rsplit(':').next()?.parse().ok()?;
    let index: usize = index.parse().ok()?;
    let it = ctx.app.store.item(item).ok().flatten().filter(|i| i.user_id == ctx.user.id)?;
    let raw = ctx.app.store.mime(it.id).ok().flatten()?;
    mime::attachment_data(&raw, index)
}

pub fn item_operations(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let mut response = Element::new("ItemOperations:Response");
    let Some(d) = doc else { return Ok(wbxml(&Element::new("ItemOperations:ItemOperations").with_text("ItemOperations:Status", "2"))) };
    for op in d.elements() {
        match op.tag {
            "Fetch" => {
                let mut f = Element::new("ItemOperations:Fetch");
                let options = op.child("ItemOperations:Options");
                let prefs = BodyPrefs::parse(options);
                if let Some(reference) = op.text_of("AirSyncBase:FileReference") {
                    match attachment(ctx, &reference) {
                        Some((info, data)) => {
                            f.push_text("ItemOperations:Status", "1");
                            f.push_text("AirSyncBase:FileReference", reference);
                            f.push(
                                Element::new("ItemOperations:Properties")
                                    .with_text("AirSyncBase:ContentType", info.content_type)
                                    .with_text("ItemOperations:Data", B64.encode(&data)),
                            );
                        }
                        None => {
                            f.push_text("ItemOperations:Status", "15");
                            f.push_text("AirSyncBase:FileReference", reference);
                        }
                    }
                    response.push(f);
                    continue;
                }
                let sid = op.text_of("AirSync:ServerId").or_else(|| op.text_of("Search:LongId")).unwrap_or_default();
                let item = item_id(&sid).and_then(|id| ctx.app.store.item(id).ok().flatten()).filter(|i| i.user_id == ctx.user.id);
                match item {
                    Some(item) => {
                        let folder = ctx.app.store.folder(item.folder_id)?;
                        f.push_text("ItemOperations:Status", "1");
                        if let Some(c) = op.text_of("AirSync:CollectionId") {
                            f.push_text("AirSync:CollectionId", c);
                        }
                        f.push_text("AirSync:ServerId", sid.clone());
                        f.push_text("AirSync:Class", folder.map(|x| class_name(x.class)).unwrap_or("Email"));
                        let data = render(ctx, &item, &prefs, true);
                        let mut props = Element::new("ItemOperations:Properties");
                        props.children = data.children;
                        f.push(props);
                    }
                    None => {
                        f.push_text("ItemOperations:Status", "6");
                        f.push_text("AirSync:ServerId", sid);
                    }
                }
                response.push(f);
            }
            "EmptyFolderContents" => {
                let id = op.text_of("AirSync:CollectionId").unwrap_or_default();
                let sub = op.path("Options/DeleteSubFolders").is_some();
                let mut r = Element::new("ItemOperations:EmptyFolderContents");
                match folder(ctx, &id) {
                    Some(f) => {
                        ctx.app.store.empty_folder(f.id, sub, &ctx.writer)?;
                        r.push_text("ItemOperations:Status", "1");
                    }
                    None => r.push_text("ItemOperations:Status", "6"),
                }
                r.push_text("AirSync:CollectionId", id);
                response.push(r);
            }
            "Move" => {
                let mut r = Element::new("ItemOperations:Move").with_text("ItemOperations:Status", "2");
                if let Some(c) = op.text_of("ItemOperations:ConversationId") {
                    r.push_text("ItemOperations:ConversationId", c);
                }
                response.push(r);
            }
            _ => {}
        }
    }
    Ok(wbxml(&Element::new("ItemOperations:ItemOperations").with_text("ItemOperations:Status", "1").with(response)))
}

pub fn get_attachment(ctx: &Ctx) -> Result<Response> {
    let Some(name) = ctx.param("AttachmentName") else { return Ok(status(StatusCode::BAD_REQUEST)) };
    match attachment(ctx, name) {
        Some((info, data)) => {
            let mut r = (StatusCode::OK, data).into_response();
            r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_str(&info.content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")));
            Ok(r)
        }
        None => Ok(status(StatusCode::NOT_FOUND)),
    }
}

// -------------------------------------------------------- MeetingResponse

pub async fn meeting_response(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let mut root = Element::new("MeetingResponse:MeetingResponse");
    let Some(d) = doc else { return Ok(wbxml(&root)) };
    for req in d.children_named("MeetingResponse:Request") {
        let request_id = req.text_of("MeetingResponse:RequestId").or_else(|| req.text_of("Search:LongId")).unwrap_or_default();
        let mut result = Element::new("MeetingResponse:Result").with_text("MeetingResponse:RequestId", request_id.clone());
        let answer = match req.text_of("MeetingResponse:UserResponse").as_deref().map(str::trim) {
            Some("1") => Answer::Accepted,
            Some("2") => Answer::Tentative,
            Some("3") => Answer::Declined,
            _ => {
                root.push(result.with_text("MeetingResponse:Status", "2"));
                continue;
            }
        };
        let item = item_id(&request_id).and_then(|id| ctx.app.store.item(id).ok().flatten()).filter(|i| i.user_id == ctx.user.id);
        let event = match item {
            Some(i) if i.kind == ItemKind::Event => Some(i),
            Some(i) if i.kind == ItemKind::Mail => calendar::event_for_request(&ctx.app, &ctx.user, &i).ok(),
            _ => None,
        };
        let Some(event) = event else {
            root.push(result.with_text("MeetingResponse:Status", "2"));
            continue;
        };
        let occurrence = req.text_of("MeetingResponse:InstanceId").and_then(|v| convert::parse_date(&v));
        // ActiveSync clients send the reply mail themselves.
        match calendar::respond(&ctx.app, &ctx.user, event.id, answer, occurrence, false, "").await {
            Ok(()) => {
                result.push_text("MeetingResponse:Status", "1");
                if answer != Answer::Declined {
                    result.push_text("MeetingResponse:CalendarId", server_id(event.folder_id, event.id));
                }
            }
            Err(e) => {
                warn!("MeetingResponse: {e:#}");
                result.push_text("MeetingResponse:Status", "2");
            }
        }
        root.push(result);
    }
    Ok(wbxml(&root))
}

// --------------------------------------------------------------- Settings

fn oof_message(name: &str, enabled: bool, text: &str, body_type: &str) -> Element {
    let mut m = Element::new("Settings:OofMessage").with_empty(&format!("Settings:{name}")).with_text("Settings:Enabled", if enabled { "1" } else { "0" });
    if enabled || !text.is_empty() {
        let text = if body_type.eq_ignore_ascii_case("HTML") { mime::text_to_html(text) } else { mime::html_to_text(text) };
        m.push_text("Settings:ReplyMessage", text);
        m.push_text("Settings:BodyType", body_type.to_string());
    }
    m
}

pub fn settings(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let store = &ctx.app.store;
    let mut root = Element::new("Settings:Settings").with_text("Settings:Status", "1");
    let Some(d) = doc else { return Ok(wbxml(&root)) };
    for part in d.elements() {
        match part.tag {
            "Oof" => {
                let mut settings = store.settings(ctx.user.id)?;
                if let Some(get) = part.child("Settings:Get") {
                    let body_type = get.text_of("Settings:BodyType").unwrap_or_else(|| "Text".into());
                    let o = &settings.oof;
                    let state = match o.state.as_str() {
                        "Enabled" => "1",
                        "Scheduled" => "2",
                        _ => "0",
                    };
                    let mut g = Element::new("Settings:Get").with_text("Settings:OofState", state);
                    if let (Some(s), Some(e)) = (o.start, o.end) {
                        g.push_text("Settings:StartTime", convert::iso(s));
                        g.push_text("Settings:EndTime", convert::iso(e));
                    }
                    g.push(oof_message("AppliesToInternal", true, &o.internal_reply, &body_type));
                    g.push(oof_message("AppliesToExternalKnown", o.external_audience != "None", &o.external_reply, &body_type));
                    g.push(oof_message("AppliesToExternalUnknown", o.external_audience == "All", &o.external_reply, &body_type));
                    root.push(Element::new("Settings:Oof").with_text("Settings:Status", "1").with(g));
                } else if let Some(set) = part.child("Settings:Set") {
                    let mut o = OofSettings {
                        state: match set.text_of("Settings:OofState").as_deref().map(str::trim) {
                            Some("1") => "Enabled",
                            Some("2") => "Scheduled",
                            _ => "Disabled",
                        }
                        .into(),
                        start: set.text_of("Settings:StartTime").and_then(|v| convert::parse_date(&v)),
                        end: set.text_of("Settings:EndTime").and_then(|v| convert::parse_date(&v)),
                        external_audience: "None".into(),
                        ..settings.oof.clone()
                    };
                    let mut known = false;
                    let mut unknown = false;
                    for m in set.children_named("Settings:OofMessage") {
                        let enabled = m.text_of("Settings:Enabled").is_some_and(|v| v.trim() == "1");
                        let text = m.text_of("Settings:ReplyMessage").unwrap_or_default();
                        if m.has("Settings:AppliesToInternal") {
                            o.internal_reply = text;
                        } else if m.has("Settings:AppliesToExternalKnown") {
                            known = enabled;
                            if enabled {
                                o.external_reply = text;
                            }
                        } else if m.has("Settings:AppliesToExternalUnknown") {
                            unknown = enabled;
                            if enabled {
                                o.external_reply = text;
                            }
                        }
                    }
                    o.external_audience = if unknown { "All" } else if known { "Known" } else { "None" }.into();
                    if o.state != settings.oof.state {
                        settings.oof_sent.clear();
                    }
                    settings.oof = o;
                    store.set_settings(ctx.user.id, &settings)?;
                    root.push(Element::new("Settings:Oof").with_text("Settings:Status", "1"));
                }
            }
            "DeviceInformation" => {
                store.set_device_info(ctx.user.id, &ctx.device_id, &device_info(part))?;
                root.push(Element::new("Settings:DeviceInformation").with_text("Settings:Status", "1"));
            }
            "DevicePassword" => root.push(Element::new("Settings:DevicePassword").with_text("Settings:Status", "1")),
            "UserInformation" => {
                let addresses = ctx.me.clone();
                let mut emails = Element::new("Settings:EmailAddresses");
                if ctx.version >= 14.1 {
                    emails.push_text("Settings:PrimarySmtpAddress", ctx.user.email.clone());
                }
                for a in &addresses {
                    emails.push_text("Settings:SmtpAddress", a.clone());
                }
                let get = if ctx.version >= 14.1 {
                    Element::new("Settings:Get").with(
                        Element::new("Settings:Accounts").with(
                            Element::new("Settings:Account")
                                .with_text("Settings:UserDisplayName", if ctx.user.name.is_empty() { ctx.user.email.clone() } else { ctx.user.name.clone() })
                                .with(emails),
                        ),
                    )
                } else {
                    Element::new("Settings:Get").with(emails)
                };
                root.push(Element::new("Settings:UserInformation").with_text("Settings:Status", "1").with(get));
            }
            "RightsManagementInformation" => {
                root.push(Element::new("Settings:RightsManagementInformation").with_text("Settings:Status", "1").with(Element::new("Settings:Get")));
            }
            _ => {}
        }
    }
    Ok(wbxml(&root))
}

// ------------------------------------------------------ ResolveRecipients

pub fn resolve_recipients(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let mut root = Element::new("ResolveRecipients:ResolveRecipients").with_text("ResolveRecipients:Status", "1");
    let Some(d) = doc else { return Ok(wbxml(&root)) };
    let options = d.child("ResolveRecipients:Options");
    let max = options.and_then(|o| o.text_of("ResolveRecipients:MaxAmbiguousRecipients")).and_then(|v| v.trim().parse().ok()).unwrap_or(20usize).max(1);
    let availability = options.and_then(|o| o.child("ResolveRecipients:Availability")).and_then(|a| {
        let s = convert::parse_date(&a.text_of("ResolveRecipients:StartTime")?)?;
        let e = convert::parse_date(&a.text_of("ResolveRecipients:EndTime")?)?;
        Some((s, e))
    });
    for to in d.children_named("ResolveRecipients:To") {
        let query = to.content();
        let mut users = match directory::resolve(&ctx.app, &query) {
            Some(u) => vec![u],
            None => directory::search(&ctx.app, &query, max),
        };
        users.truncate(max);
        let mut r = Element::new("ResolveRecipients:Response").with_text("ResolveRecipients:To", query.clone());
        let status = match users.len() {
            0 => "4",
            1 => "1",
            _ => "2",
        };
        r.push_text("ResolveRecipients:Status", status);
        r.push_text("ResolveRecipients:RecipientCount", users.len().to_string());
        for u in &users {
            let mut rec = Element::new("ResolveRecipients:Recipient")
                .with_text("ResolveRecipients:Type", "1")
                .with_text("ResolveRecipients:DisplayName", if u.name.is_empty() { u.email.clone() } else { u.name.clone() })
                .with_text("ResolveRecipients:EmailAddress", u.email.clone());
            if let Some((s, e)) = availability {
                let busy = calendar::busy_times(&ctx.app, u.id, s, e).unwrap_or_default();
                rec.push(
                    Element::new("ResolveRecipients:Availability")
                        .with_text("ResolveRecipients:Status", "1")
                        .with_text("ResolveRecipients:MergedFreeBusy", calendar::merged_free_busy(&busy, s, e, 30)),
                );
            }
            r.push(rec);
        }
        root.push(r);
    }
    Ok(wbxml(&root))
}

// ----------------------------------------------------------------- Search

fn parse_range(r: Option<String>) -> (usize, usize) {
    r.and_then(|r| {
        let (a, b) = r.split_once('-')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    })
    .unwrap_or((0, 99))
}

fn free_text(q: &Element) -> String {
    let mut out = Vec::new();
    fn walk(e: &Element, out: &mut Vec<String>) {
        if e.tag == "FreeText" {
            out.push(e.content());
        }
        for c in e.elements() {
            walk(c, out);
        }
    }
    walk(q, &mut out);
    if out.is_empty() { q.content() } else { out.join(" ") }
}

pub fn search(ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let Some(store_el) = doc.and_then(|d| d.child("Search:Store")) else {
        return Ok(wbxml(&Element::new("Search:Search").with_text("Search:Status", "2")));
    };
    let name = store_el.text_of("Search:Name").unwrap_or_default();
    let query = store_el.child("Search:Query");
    let options = store_el.child("Search:Options");
    let (from, to) = parse_range(options.and_then(|o| o.text_of("Search:Range")));
    let mut result_store = Element::new("Search:Store").with_text("Search:Status", "1");
    let total;
    if name.eq_ignore_ascii_case("GAL") {
        let q = query.map(|q| q.content()).unwrap_or_default();
        let users = directory::search(&ctx.app, &q, 1000);
        total = users.len();
        for u in users.iter().skip(from).take(to.saturating_sub(from) + 1) {
            let (first, last) = directory::split_name(&u.name);
            let props = Element::new("Search:Properties")
                .with_text("GAL:DisplayName", if u.name.is_empty() { u.email.clone() } else { u.name.clone() })
                .with_text("GAL:EmailAddress", u.email.clone())
                .with_text("GAL:FirstName", first)
                .with_text("GAL:LastName", last)
                .with_text("GAL:Alias", u.email.split('@').next().unwrap_or_default().to_string());
            result_store.push(Element::new("Search:Result").with(props));
        }
    } else if name.eq_ignore_ascii_case("Mailbox") {
        let q = query.map(free_text).unwrap_or_default();
        let collection = query.and_then(|q| q.find("AirSync:CollectionId")).map(|c| c.content());
        let deep = options.is_some_and(|o| o.has("Search:DeepTraversal"));
        let mut folders: Vec<i64> = Vec::new();
        if let Some(c) = collection.and_then(|c| folder(ctx, &c)) {
            folders.push(c.id);
            if deep {
                let all = ctx.app.store.folders(ctx.user.id)?;
                let mut i = 0;
                while i < folders.len() {
                    let cur = folders[i];
                    folders.extend(all.iter().filter(|f| f.parent_id == Some(cur)).map(|f| f.id));
                    i += 1;
                }
            }
        }
        let prefs = BodyPrefs::parse(options);
        let items: Vec<_> = ctx.app.store.search(ctx.user.id, &folders, &q, 1000)?.into_iter().filter(|i| i.kind == ItemKind::Mail).collect();
        total = items.len();
        for item in items.iter().skip(from).take(to.saturating_sub(from) + 1) {
            let data = render(ctx, item, &prefs, true);
            let mut props = Element::new("Search:Properties");
            props.children = data.children;
            result_store.push(
                Element::new("Search:Result")
                    .with_text("AirSync:Class", "Email")
                    .with_text("Search:LongId", server_id(item.folder_id, item.id))
                    .with_text("AirSync:CollectionId", item.folder_id.to_string())
                    .with(props),
            );
        }
    } else {
        return Ok(wbxml(&Element::new("Search:Search").with_text("Search:Status", "1").with(Element::new("Search:Response").with(Element::new("Search:Store").with_text("Search:Status", "3")))));
    }
    if total > 0 {
        let last = (to.min(total.saturating_sub(1))).max(from);
        result_store.push_text("Search:Range", format!("{from}-{last}"));
    } else {
        // An empty result still needs one empty Result element.
        result_store.push(Element::new("Search:Result"));
    }
    result_store.push_text("Search:Total", total.to_string());
    Ok(wbxml(&Element::new("Search:Search").with_text("Search:Status", "1").with(Element::new("Search:Response").with(result_store))))
}

// ----------------------------------------------------------- ValidateCert

pub fn validate_cert(_ctx: &Ctx, doc: Option<&Element>) -> Result<Response> {
    let mut root = Element::new("ValidateCert:ValidateCert").with_text("ValidateCert:Status", "1");
    if let Some(certs) = doc.and_then(|d| d.child("ValidateCert:Certificates")) {
        for _ in certs.children.iter().filter(|c| matches!(c, Node::Element(_))) {
            root.push(Element::new("ValidateCert:Certificate").with_text("ValidateCert:Status", "1"));
        }
    }
    let _ = now();
    Ok(wbxml(&root))
}
