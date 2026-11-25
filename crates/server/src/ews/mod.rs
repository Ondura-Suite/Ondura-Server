//! Exchange Web Services (SOAP) for mail, calendars, contacts and tasks.
//!
//! Ids are opaque strings: `F.<folder>`, `I.<item>`, `O.<item>.<original
//! start>` for occurrences of a series and `A.<item>.<index>` for
//! attachments. Change keys carry the modseq of the object.

mod items;
mod misc;
mod read;
mod write;

#[cfg(test)]
mod tests;

use std::fmt::Write as _;

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono_tz::Tz;
use tracing::{debug, warn};

use crate::app::Shared;
use crate::model::{FolderClass, default_tz};
use crate::store::{Folder, User};
use crate::tzinfo;
use crate::xml::{Node, escape};

pub const NS_T: &str = "http://schemas.microsoft.com/exchange/services/2006/types";
pub const NS_M: &str = "http://schemas.microsoft.com/exchange/services/2006/messages";

pub struct Ctx {
    pub app: Shared,
    /// Authenticated user.
    pub user: User,
    /// Time zone for times without offset (TimeZoneContext header).
    pub tz: Tz,
    pub version: String,
}

/// Error of a single response message.
#[derive(Debug)]
pub struct EwsError {
    pub code: &'static str,
    pub message: String,
}

impl EwsError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        EwsError { code, message: message.into() }
    }

    pub fn not_found() -> Self {
        Self::new("ErrorItemNotFound", "The specified object was not found in the store.")
    }

    pub fn folder_not_found() -> Self {
        Self::new("ErrorFolderNotFound", "The specified folder could not be found in the store.")
    }

    pub fn denied() -> Self {
        Self::new("ErrorAccessDenied", "Access is denied. Check credentials and try again.")
    }
}

impl From<anyhow::Error> for EwsError {
    fn from(e: anyhow::Error) -> Self {
        warn!("EWS internal error: {e:#}");
        EwsError::new("ErrorInternalServerError", e.to_string())
    }
}

pub type EwsResult<T> = Result<T, EwsError>;

/// One `<m:XxxResponseMessage>`.
pub fn message(name: &str, result: EwsResult<String>) -> String {
    match result {
        Ok(inner) => format!(r#"<m:{name}ResponseMessage ResponseClass="Success"><m:ResponseCode>NoError</m:ResponseCode>{inner}</m:{name}ResponseMessage>"#),
        Err(e) => format!(
            r#"<m:{name}ResponseMessage ResponseClass="Error"><m:MessageText>{}</m:MessageText><m:ResponseCode>{}</m:ResponseCode><m:DescriptiveLinkKey>0</m:DescriptiveLinkKey></m:{name}ResponseMessage>"#,
            escape(&e.message),
            e.code
        ),
    }
}

/// `<m:XxxResponse><m:ResponseMessages>…</m:ResponseMessages></m:XxxResponse>`
pub fn wrap(name: &str, messages: &str) -> String {
    format!(r#"<m:{name}Response xmlns:m="{NS_M}" xmlns:t="{NS_T}"><m:ResponseMessages>{messages}</m:ResponseMessages></m:{name}Response>"#)
}

pub fn envelope(body: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Header><h:ServerVersionInfo MajorVersion="15" MinorVersion="1" MajorBuildNumber="2507" MinorBuildNumber="6" Version="V2017_07_11" xmlns:h="{NS_T}" xmlns="{NS_T}"/></s:Header><s:Body xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema">{body}</s:Body></s:Envelope>"#
    )
}

fn soap_response(body: String) -> Response {
    let mut r = (StatusCode::OK, envelope(&body)).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml; charset=utf-8"));
    r
}

pub fn fault(code: &str, text: &str) -> Response {
    let body = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode xmlns:a="{NS_T}">a:{code}</faultcode><faultstring xml:lang="en-US">{}</faultstring><detail><e:ResponseCode xmlns:e="{NS_M}">{code}</e:ResponseCode><e:Message xmlns:e="{NS_M}">{}</e:Message></detail></s:Fault></s:Body></s:Envelope>"#,
        escape(text),
        escape(text)
    );
    let mut r = (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/xml; charset=utf-8"));
    r
}

pub async fn handle(app: Shared, user: User, _headers: &HeaderMap, body: Bytes) -> Response {
    let text = String::from_utf8_lossy(&body);
    let doc = match Node::parse(&text) {
        Ok(d) => d,
        Err(e) => return fault("ErrorSchemaValidation", &format!("The request is not valid XML: {e}")),
    };
    let Some(op) = doc.find("Body").and_then(|b| b.children.first()).cloned() else {
        return fault("ErrorInvalidRequest", "The request has no SOAP body.");
    };
    let header = doc.find("Header");
    let tz = header
        .and_then(|h| h.find("TimeZoneDefinition"))
        .and_then(|d| d.attr("Id"))
        .and_then(tzinfo::from_windows_name)
        .or_else(|| app.store.settings(user.id).ok().and_then(|s| s.time_zone).and_then(|t| t.parse().ok()))
        .unwrap_or_else(default_tz);
    let version = header.and_then(|h| h.find("RequestServerVersion")).and_then(|v| v.attr("Version")).unwrap_or("Exchange2013_SP1").to_string();
    let ctx = Ctx { app, user, tz, version };
    debug!(op = %op.name, user = %ctx.user.email, "EWS request");
    if op.name == "GetStreamingEvents" {
        return misc::get_streaming_events(&ctx, &op).await;
    }
    let result = match op.name.as_str() {
        "GetFolder" => items::get_folder(&ctx, &op),
        "FindFolder" => items::find_folder(&ctx, &op),
        "CreateFolder" => items::create_folder(&ctx, &op),
        "DeleteFolder" => items::delete_folder(&ctx, &op),
        "UpdateFolder" => items::update_folder(&ctx, &op),
        "MoveFolder" => items::move_folder(&ctx, &op),
        "EmptyFolder" => items::empty_folder(&ctx, &op),
        "SyncFolderHierarchy" => items::sync_folder_hierarchy(&ctx, &op),
        "SyncFolderItems" => items::sync_folder_items(&ctx, &op),
        "FindItem" => items::find_item(&ctx, &op),
        "GetItem" => items::get_item(&ctx, &op),
        "CreateItem" => items::create_item(&ctx, &op).await,
        "UpdateItem" => items::update_item(&ctx, &op).await,
        "DeleteItem" => items::delete_item(&ctx, &op).await,
        "MoveItem" => items::move_or_copy(&ctx, &op, false),
        "CopyItem" => items::move_or_copy(&ctx, &op, true),
        "SendItem" => items::send_item(&ctx, &op).await,
        "MarkAllItemsAsRead" => items::mark_all_read(&ctx, &op),
        "GetAttachment" => misc::get_attachment(&ctx, &op),
        "CreateAttachment" => misc::create_attachment(&ctx, &op),
        "DeleteAttachment" => misc::delete_attachment(&ctx, &op),
        "ResolveNames" => misc::resolve_names(&ctx, &op),
        "ExpandDL" => Ok(wrap("ExpandDL", &message("ExpandDL", Err(EwsError::new("ErrorNameResolutionNoResults", "No results were found."))))),
        "GetUserOofSettingsRequest" => misc::get_oof(&ctx, &op),
        "SetUserOofSettingsRequest" => misc::set_oof(&ctx, &op),
        "GetUserAvailabilityRequest" => misc::availability(&ctx, &op),
        "GetRoomLists" => misc::room_lists(&ctx),
        "GetRooms" => misc::rooms(&ctx, &op),
        "Subscribe" => misc::subscribe(&ctx, &op),
        "Unsubscribe" => misc::unsubscribe(&ctx, &op),
        "GetEvents" => misc::get_events(&ctx, &op),
        "GetServerTimeZones" => misc::server_time_zones(&ctx, &op),
        "ConvertId" => misc::convert_id(&ctx, &op),
        "GetInboxRules" => misc::get_inbox_rules(&ctx),
        "GetMailTips" => misc::mail_tips(&ctx, &op),
        other => {
            warn!(op = other, "unsupported EWS operation");
            return fault("ErrorInvalidRequest", &format!("The operation {other} is not supported by this server."));
        }
    };
    match result {
        Ok(body) => soap_response(body),
        Err(e) => fault(e.code, &e.message),
    }
}

// --------------------------------------------------------------------- ids

pub fn folder_id(f: &Folder) -> String {
    format!(r#"<t:FolderId Id="F.{}" ChangeKey="C{}"/>"#, f.id, f.modseq)
}

pub fn item_id_xml(id: i64, modseq: i64) -> String {
    format!(r#"<t:ItemId Id="I.{id}" ChangeKey="C{modseq}"/>"#)
}

/// Which object an item id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemRef {
    Item(i64),
    Occurrence(i64, i64),
}

impl ItemRef {
    pub fn item(self) -> i64 {
        match self {
            ItemRef::Item(i) | ItemRef::Occurrence(i, _) => i,
        }
    }
}

pub fn parse_item_id(id: &str) -> Option<ItemRef> {
    let mut parts = id.split('.');
    match (parts.next()?, parts.next(), parts.next()) {
        ("I", Some(i), None) => i.parse().ok().map(ItemRef::Item),
        ("O", Some(i), Some(t)) => Some(ItemRef::Occurrence(i.parse().ok()?, t.parse().ok()?)),
        _ => None,
    }
}

/// Item reference from `ItemId`, `OccurrenceItemId` or `RecurringMasterItemId`.
pub fn item_ref(ctx: &Ctx, n: &Node) -> EwsResult<ItemRef> {
    match n.name.as_str() {
        "ItemId" | "ReferenceItemId" | "ParentItemId" => n.attr("Id").and_then(parse_item_id).ok_or_else(EwsError::not_found),
        "RecurringMasterItemId" => {
            let r = n.attr("OccurrenceId").and_then(parse_item_id).ok_or_else(EwsError::not_found)?;
            Ok(ItemRef::Item(r.item()))
        }
        "OccurrenceItemId" => {
            let master = n.attr("RecurringMasterId").and_then(parse_item_id).ok_or_else(EwsError::not_found)?.item();
            let index: usize = n.attr("InstanceIndex").and_then(|i| i.parse().ok()).filter(|i| *i >= 1).ok_or_else(EwsError::not_found)?;
            let item = ctx.app.store.item(master)?.ok_or_else(EwsError::not_found)?;
            let e = item.event().ok_or_else(EwsError::not_found)?;
            let occ = e.occurrences(e.start, e.start + 400 * 366 * 86_400);
            let o = occ.get(index - 1).ok_or_else(|| EwsError::new("ErrorCalendarOccurrenceIndexIsOutOfRecurrenceRange", "The occurrence index is out of range."))?;
            Ok(ItemRef::Occurrence(master, o.original_start))
        }
        _ => Err(EwsError::new("ErrorInvalidIdMalformed", "Id is malformed.")),
    }
}

/// Load an item the user may access.
pub fn load_item(ctx: &Ctx, r: ItemRef) -> EwsResult<crate::store::Item> {
    let item = ctx.app.store.item(r.item())?.ok_or_else(EwsError::not_found)?;
    if !ctx.app.store.can_access(ctx.user.id, item.user_id) {
        return Err(EwsError::denied());
    }
    Ok(item)
}

/// Mailbox owner named in a `Mailbox` element (shared mailboxes), else the user.
fn mailbox_owner(ctx: &Ctx, n: &Node) -> EwsResult<i64> {
    let Some(addr) = n.child("Mailbox").and_then(|m| m.text_of("EmailAddress")) else { return Ok(ctx.user.id) };
    let (owner, _) = ctx.app.store.resolve_address(addr)?.ok_or_else(|| EwsError::new("ErrorNonExistentMailbox", format!("No mailbox with such guid. ({addr})")))?;
    if !ctx.app.store.can_access(ctx.user.id, owner) {
        return Err(EwsError::new("ErrorAccessDenied", "The account does not have permission to access the mailbox."));
    }
    Ok(owner)
}

/// Folder from `FolderId` or `DistinguishedFolderId`.
pub fn resolve_folder(ctx: &Ctx, n: &Node) -> EwsResult<Folder> {
    match n.name.as_str() {
        "FolderId" => {
            let id: i64 = n.attr("Id").and_then(|i| i.strip_prefix("F.")).and_then(|i| i.parse().ok()).ok_or_else(EwsError::folder_not_found)?;
            let f = ctx.app.store.folder(id)?.ok_or_else(EwsError::folder_not_found)?;
            if !ctx.app.store.can_access(ctx.user.id, f.user_id) {
                return Err(EwsError::denied());
            }
            Ok(f)
        }
        "DistinguishedFolderId" => {
            let owner = mailbox_owner(ctx, n)?;
            let name = n.attr("Id").unwrap_or_default().to_ascii_lowercase();
            let role = match name.as_str() {
                "root" | "msgfolderroot" | "publicfoldersroot" => "msgfolderroot",
                "inbox" | "drafts" | "sentitems" | "deleteditems" | "junkemail" | "outbox" | "calendar" | "contacts" | "tasks" | "notes" | "archive" => name.as_str(),
                _ => return Err(EwsError::folder_not_found()),
            };
            ctx.app.store.folder_by_role(owner, role)?.ok_or_else(EwsError::folder_not_found)
        }
        _ => Err(EwsError::folder_not_found()),
    }
}

/// First folder reference inside `n` (e.g. `ParentFolderId`, `SavedItemFolderId`).
pub fn folder_in(ctx: &Ctx, n: Option<&Node>) -> Option<EwsResult<Folder>> {
    let n = n?;
    let f = n.children.iter().find(|c| c.name == "FolderId" || c.name == "DistinguishedFolderId")?;
    Some(resolve_folder(ctx, f))
}

pub fn class_element(c: FolderClass) -> &'static str {
    match c {
        FolderClass::Calendar => "CalendarFolder",
        FolderClass::Contacts => "ContactsFolder",
        FolderClass::Tasks => "TasksFolder",
        FolderClass::Notes => "Folder",
        _ => "Folder",
    }
}

/// Requested shape of items or folders.
#[derive(Debug, Clone, Default)]
pub struct Shape {
    /// `IdOnly`, `Default` or `AllProperties`.
    pub base: String,
    pub fields: Vec<String>,
    /// Extended properties requested (tag, type).
    pub extended: Vec<(String, String)>,
    pub include_mime: bool,
    /// `Best`, `HTML` or `Text`.
    pub body_type: String,
}

impl Shape {
    pub fn parse(n: Option<&Node>) -> Shape {
        let Some(n) = n else { return Shape { base: "Default".into(), body_type: "Best".into(), ..Default::default() } };
        let mut s = Shape {
            base: n.text_of("BaseShape").unwrap_or("Default").to_string(),
            include_mime: n.text_of("IncludeMimeContent").is_some_and(|v| v.eq_ignore_ascii_case("true")),
            body_type: n.text_of("BodyType").unwrap_or("Best").to_string(),
            ..Default::default()
        };
        if let Some(props) = n.child("AdditionalProperties") {
            for p in &props.children {
                match p.name.as_str() {
                    "FieldURI" | "IndexedFieldURI" => {
                        if let Some(f) = p.attr("FieldURI") {
                            s.fields.push(f.to_string());
                        }
                    }
                    "ExtendedFieldURI" => {
                        let tag = p.attr("PropertyTag").or(p.attr("PropertyName")).or(p.attr("PropertyId")).unwrap_or_default().to_string();
                        s.extended.push((tag, p.attr("PropertyType").unwrap_or("String").to_string()));
                    }
                    _ => {}
                }
            }
        }
        s
    }

    /// Whether a field (by FieldURI or local name) is part of the shape.
    pub fn wants(&self, uri: &str, default: bool) -> bool {
        match self.base.as_str() {
            "AllProperties" => true,
            "Default" if default => true,
            _ => {
                let local = uri.rsplit(':').next().unwrap_or(uri);
                self.fields.iter().any(|f| f == uri || f.rsplit(':').next() == Some(local))
            }
        }
    }
}

pub fn bool_str(b: bool) -> &'static str {
    if b { "true" } else { "false" }
}

/// Append `<t:Name>value</t:Name>`.
pub fn push(s: &mut String, name: &str, value: &str) {
    let _ = write!(s, "<t:{name}>{}</t:{name}>", escape(value));
}
