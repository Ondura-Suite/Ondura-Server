//! Mail endpoints: list, read, compose/send, draft, flag, move, delete.

use axum::body::Bytes;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{bad, internal, not_found, own_item, parse, role_folder, ApiResult, WRITER};
use crate::app::Shared;
use crate::mime::{self, File, Outgoing};
use crate::model::{flags, Person};
use crate::store::{NewMail, User};

pub(super) fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == key).map(|(_, v)| v)
}

pub(super) fn decode(s: &str) -> String {
    // Minimal percent-decoding for query values.
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() && u8::from_str_radix(&s[i + 1..i + 3], 16).is_ok() => {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).expect("checked"));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn person_json(p: &Person) -> Value {
    json!({ "name": p.name, "email": p.email })
}

fn flags_json(f: i64) -> Value {
    json!({
        "read": f & flags::READ != 0,
        "flagged": f & flags::FLAGGED != 0,
        "answered": f & flags::ANSWERED != 0,
        "draft": f & flags::DRAFT != 0,
    })
}

fn summary(item: &crate::store::Item) -> Value {
    json!({
        "id": item.id,
        "folder": item.folder_id,
        "subject": item.subject,
        "from": item.sender.as_ref().map(person_json),
        "to": item.recipients.to.iter().map(person_json).collect::<Vec<_>>(),
        "date": item.received,
        "preview": item.preview,
        "size": item.size,
        "hasAttachments": item.has_attachments,
        "flags": flags_json(item.flags),
        "categories": item.categories,
        "messageId": item.message_id,
        "inReplyTo": item.in_reply_to,
        "references": item.references,
    })
}

/// GET /messages?folder=<id>&offset=&limit=&search=
pub fn messages(app: &Shared, user: &User, query: &str) -> ApiResult {
    let offset: i64 = query_param(query, "offset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let limit: i64 = query_param(query, "limit").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);
    let search = query_param(query, "search").map(decode).filter(|s| !s.trim().is_empty());

    if let Some(term) = search {
        let folders = app.store.folders(user.id).map_err(internal)?;
        let ids: Vec<i64> = folders.iter().filter(|f| matches!(f.class, crate::model::FolderClass::Mail)).map(|f| f.id).collect();
        let hits = app.store.search(user.id, &ids, &term, limit).map_err(internal)?;
        return Ok(json!({ "messages": hits.iter().map(summary).collect::<Vec<_>>(), "search": true }));
    }

    let folder: i64 = query_param(query, "folder").and_then(|v| v.parse().ok()).ok_or_else(|| bad("folder required"))?;
    // Confirm the folder belongs to the user.
    let owned = app.store.folder(folder).map_err(internal)?.filter(|f| f.user_id == user.id).is_some();
    if !owned {
        return Err(not_found());
    }
    let items = app.store.items(folder, offset, limit).map_err(internal)?;
    Ok(json!({
        "messages": items.iter().map(summary).collect::<Vec<_>>(),
        "offset": offset,
        "limit": limit,
    }))
}

/// GET /message/<id>
pub fn message(app: &Shared, user: &User, id: &str) -> ApiResult {
    let id: i64 = id.parse().map_err(|_| not_found())?;
    let item = own_item(app, user, id)?;
    let raw = app.store.mime(id).map_err(internal)?.unwrap_or_default();
    let (text, html) = mime::bodies(&raw);
    let attachments: Vec<Value> = mime::attachments(&raw)
        .iter()
        .filter(|a| !a.inline)
        .map(|a| json!({ "index": a.index, "name": a.name, "contentType": a.content_type, "size": a.size }))
        .collect();
    Ok(json!({
        "id": item.id,
        "folder": item.folder_id,
        "subject": item.subject,
        "from": item.sender.as_ref().map(person_json),
        "to": item.recipients.to.iter().map(person_json).collect::<Vec<_>>(),
        "cc": item.recipients.cc.iter().map(person_json).collect::<Vec<_>>(),
        "bcc": item.recipients.bcc.iter().map(person_json).collect::<Vec<_>>(),
        "replyTo": item.recipients.reply_to.iter().map(person_json).collect::<Vec<_>>(),
        "date": item.received,
        "messageId": item.message_id,
        "messageClass": item.message_class,
        "flags": flags_json(item.flags),
        "categories": item.categories,
        "text": text,
        "html": html,
        "attachments": attachments,
    }))
}

/// GET /message/<id>/attachment/<idx> — binary download.
pub fn attachment(app: &Shared, user: &User, id: &str, idx: &str) -> Response {
    let (Ok(id), Ok(idx)) = (id.parse::<i64>(), idx.parse::<usize>()) else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };
    if own_item(app, user, id).is_err() {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    }
    let raw = app.store.mime(id).ok().flatten().unwrap_or_default();
    let Some((info, data)) = mime::attachment_data(&raw, idx) else {
        return (StatusCode::NOT_FOUND, "Not found").into_response();
    };
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&info.content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    let safe: String = info.name.chars().filter(|c| !c.is_control() && *c != '"').collect();
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")).unwrap_or(HeaderValue::from_static("attachment")));
    r
}

fn parse_addresses(s: &str) -> Vec<Person> {
    // Split on commas and semicolons; accept "Name <addr>" or bare addresses.
    s.split([',', ';'])
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(|p| {
            if let (Some(lt), Some(gt)) = (p.find('<'), p.find('>'))
                && lt < gt
            {
                let name = p[..lt].trim().trim_matches('"').to_string();
                let email = p[lt + 1..gt].trim().to_string();
                return Person { name, email };
            }
            Person { name: String::new(), email: p.to_string() }
        })
        .filter(|p| p.email.contains('@'))
        .collect()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attachment {
    name: String,
    #[serde(default = "octet")]
    content_type: String,
    /// Base64-encoded content.
    data: String,
}

fn octet() -> String {
    "application/octet-stream".into()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Compose {
    #[serde(default)]
    to: String,
    #[serde(default)]
    cc: String,
    #[serde(default)]
    bcc: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    html: Option<String>,
    #[serde(default)]
    in_reply_to: Option<String>,
    /// Address to send from; must be the user's own or one of their aliases.
    #[serde(default)]
    from: Option<String>,
    /// Id of a message being answered, so it can be flagged.
    #[serde(default)]
    answering: Option<i64>,
    /// Id of a draft being sent, removed from Drafts once the mail is on its way.
    #[serde(default)]
    draft_id: Option<i64>,
    #[serde(default)]
    attachments: Vec<Attachment>,
}

fn build_outgoing(app: &Shared, user: &User, c: &Compose) -> Result<Outgoing, (StatusCode, String)> {
    let to = parse_addresses(&c.to);
    let cc = parse_addresses(&c.cc);
    let bcc = parse_addresses(&c.bcc);
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err(bad("at least one recipient is required"));
    }
    // The From address defaults to the primary address; any other value must be
    // one the user is allowed to send as (their own aliases or a delegation).
    let from_email = match &c.from {
        Some(f) if !f.trim().is_empty() => {
            let f = f.trim().to_ascii_lowercase();
            let allowed = app.store.send_as_addresses(user.id).map_err(internal)?;
            if !allowed.iter().any(|a| a.to_ascii_lowercase() == f) {
                return Err((StatusCode::FORBIDDEN, "You are not allowed to send from that address.".into()));
            }
            f
        }
        _ => user.email.clone(),
    };
    let mut files = Vec::new();
    for a in &c.attachments {
        let data = B64.decode(a.data.as_bytes()).map_err(|_| bad("invalid attachment encoding"))?;
        files.push(File { name: a.name.clone(), content_type: a.content_type.clone(), data, content_id: None, inline: false });
    }
    // Bcc goes in a header; it is included in the envelope recipients and then
    // stripped before the message is transmitted, so it stays hidden.
    let mut headers = Vec::new();
    if !bcc.is_empty() {
        let v = bcc.iter().map(|p| if p.name.is_empty() { p.email.clone() } else { format!("{} <{}>", p.name, p.email) }).collect::<Vec<_>>().join(", ");
        headers.push(("Bcc".to_string(), v));
    }
    Ok(Outgoing {
        from: Person { name: user.name.clone(), email: from_email },
        to,
        cc,
        subject: c.subject.clone(),
        text: c.text.clone(),
        html: c.html.clone(),
        calendar: None,
        attachment: None,
        headers,
        in_reply_to: c.in_reply_to.clone(),
        multipart_report: None,
        files,
    })
}

/// POST /send
pub async fn send(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let c: Compose = parse(body)?;
    let out = build_outgoing(app, user, &c)?;
    crate::smtp::submit::send_generated(app, user, &out, true).await.map_err(internal)?;
    if let Some(aid) = c.answering
        && let Ok(item) = own_item(app, user, aid)
    {
        let _ = app.store.update_flags(item.id, flags::ANSWERED, 0, None, WRITER);
    }
    // The mail is on its way, so drop the draft it was composed from.
    if let Some(did) = c.draft_id
        && let Ok(item) = own_item(app, user, did)
    {
        let _ = app.store.delete_item(item.id, WRITER);
    }
    Ok(json!({ "ok": true }))
}

/// POST /empty — permanently remove every message in one of the user's folders
/// (used for emptying Deleted Items).
pub fn empty(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct EmptyBody {
        folder: i64,
    }
    let b: EmptyBody = parse(body)?;
    let folder = app.store.folder(b.folder).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?;
    app.store.empty_folder(folder.id, false, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// POST /draft — save (or replace) a draft in the Drafts folder.
pub fn draft(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct DraftBody {
        #[serde(flatten)]
        compose: Compose,
        /// Existing draft id to replace.
        #[serde(default)]
        id: Option<i64>,
    }
    let d: DraftBody = parse(body)?;
    let out = {
        // Drafts may have no recipient yet, so build directly.
        let to = parse_addresses(&d.compose.to);
        let cc = parse_addresses(&d.compose.cc);
        let bcc = parse_addresses(&d.compose.bcc);
        // Keep Bcc on the stored draft so it is restored when the draft is reopened.
        let mut headers = Vec::new();
        if !bcc.is_empty() {
            let v = bcc.iter().map(|p| if p.name.is_empty() { p.email.clone() } else { format!("{} <{}>", p.name, p.email) }).collect::<Vec<_>>().join(", ");
            headers.push(("Bcc".to_string(), v));
        }
        Outgoing {
            from: Person { name: user.name.clone(), email: user.email.clone() },
            to,
            cc,
            subject: d.compose.subject.clone(),
            text: d.compose.text.clone(),
            html: d.compose.html.clone(),
            calendar: None,
            attachment: None,
            headers,
            in_reply_to: d.compose.in_reply_to.clone(),
            multipart_report: None,
            files: Vec::new(),
        }
    };
    let raw = out.build(app.hostname());
    let drafts = role_folder(app, user, "drafts")?;
    if let Some(id) = d.id
        && own_item(app, user, id).is_ok()
    {
        app.store.replace_mime(id, &raw, WRITER).map_err(internal)?;
        return Ok(json!({ "ok": true, "id": id }));
    }
    let id = app
        .store
        .insert_mail(NewMail { user_id: user.id, folder_id: drafts.id, mime: &raw, flags: flags::DRAFT, received: crate::store::now(), writer: WRITER })
        .map_err(internal)?;
    Ok(json!({ "ok": true, "id": id }))
}

/// POST /flag — set read/flagged state.
pub fn flag(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct FlagBody {
        id: i64,
        #[serde(default)]
        read: Option<bool>,
        #[serde(default)]
        flagged: Option<bool>,
    }
    let f: FlagBody = parse(body)?;
    let item = own_item(app, user, f.id)?;
    let (set, clear) = super::flag_bits(f.read, f.flagged);
    app.store.update_flags(item.id, set, clear, None, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// GET /categories — the user's colour-category registry.
pub fn categories(app: &Shared, user: &User) -> ApiResult {
    let cats = app.store.categories(user.id).map_err(internal)?;
    Ok(json!({
        "categories": cats.iter().map(|(id, name, color)| json!({ "id": id, "name": name, "color": color })).collect::<Vec<_>>()
    }))
}

/// POST /category — create a colour category, or update one when `id` is given.
pub fn save_category(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct Body {
        #[serde(default)]
        id: Option<i64>,
        name: String,
        #[serde(default)]
        color: String,
    }
    let b: Body = parse(body)?;
    let name = b.name.trim();
    if name.is_empty() {
        return Err(bad("category name is required"));
    }
    let color = if b.color.trim().is_empty() { "blue".to_string() } else { b.color.trim().to_string() };
    let id = match b.id {
        Some(id) => {
            app.store.update_category(user.id, id, name, &color).map_err(internal)?;
            id
        }
        None => app.store.create_category(user.id, name, &color).map_err(internal)?,
    };
    Ok(json!({ "ok": true, "id": id }))
}

/// POST /category/delete — remove a category and strip it from every message.
pub fn delete_category(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct Body {
        id: i64,
    }
    let b: Body = parse(body)?;
    app.store.delete_category(user.id, b.id).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// POST /categorize — set the exact category list on one message.
pub fn categorize(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct Body {
        id: i64,
        #[serde(default)]
        categories: Vec<String>,
    }
    let b: Body = parse(body)?;
    let item = own_item(app, user, b.id)?;
    app.store.update_flags(item.id, 0, 0, Some(&b.categories), WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// GET /rules — the user's inbox rules.
pub fn rules(app: &Shared, user: &User) -> ApiResult {
    let rules = app.store.rules(user.id).map_err(internal)?;
    Ok(json!({ "rules": rules }))
}

/// POST /rule — create or update an inbox rule.
pub fn save_rule(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    use crate::store::{RuleAction, RuleCondition};
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Body {
        #[serde(default)]
        id: Option<i64>,
        name: String,
        #[serde(default = "crate::serde_true")]
        enabled: bool,
        #[serde(default = "crate::serde_true")]
        match_all: bool,
        #[serde(default)]
        stop: bool,
        #[serde(default)]
        conditions: Vec<RuleCondition>,
        #[serde(default)]
        actions: Vec<RuleAction>,
    }
    let b: Body = parse(body)?;
    if b.name.trim().is_empty() {
        return Err(bad("rule name is required"));
    }
    let id = app
        .store
        .save_rule(user.id, b.id, b.name.trim(), b.enabled, b.match_all, b.stop, &b.conditions, &b.actions)
        .map_err(internal)?;
    Ok(json!({ "ok": true, "id": id }))
}

/// POST /rule/delete — remove an inbox rule.
pub fn delete_rule(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct Body {
        id: i64,
    }
    let b: Body = parse(body)?;
    app.store.delete_rule(user.id, b.id).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// POST /sweep — bulk-act on all mail from a sender within a folder.
pub fn sweep(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct Body {
        from: String,
        folder: i64,
        #[serde(default)]
        action: String,
        #[serde(default)]
        target: Option<i64>,
    }
    let b: Body = parse(body)?;
    let folder = app.store.folder(b.folder).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?;
    let from = b.from.trim().to_ascii_lowercase();
    if from.is_empty() {
        return Err(bad("sender is required"));
    }
    let mut items: Vec<_> = app
        .store
        .items(folder.id, 0, 10_000)
        .map_err(internal)?
        .into_iter()
        .filter(|it| it.sender.as_ref().is_some_and(|p| p.email.eq_ignore_ascii_case(&from)))
        .collect();
    items.sort_by(|a, c| c.received.cmp(&a.received).then(c.id.cmp(&a.id)));
    let ids: Vec<i64> = match b.action.as_str() {
        "keepLatest" => items.iter().skip(1).map(|i| i.id).collect(),
        _ => items.iter().map(|i| i.id).collect(),
    };
    let target = match b.action.as_str() {
        "move" => Some(app.store.folder(b.target.ok_or_else(|| bad("target folder required"))?).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?.id),
        _ => None,
    };
    let trash = app.store.folder_by_role(user.id, "deleteditems").map_err(internal)?;
    let mut done = 0;
    for id in ids {
        let r = match (b.action.as_str(), target) {
            ("move", Some(t)) => app.store.move_item(id, t, WRITER),
            _ => match &trash {
                Some(tr) if folder.id != tr.id => app.store.move_item(id, tr.id, WRITER),
                _ => app.store.delete_item(id, WRITER),
            },
        };
        if r.is_ok() {
            done += 1;
        }
    }
    Ok(json!({ "ok": true, "count": done }))
}

/// POST /move — move a message to another folder.
pub fn move_message(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct MoveBody {
        id: i64,
        folder: i64,
    }
    let m: MoveBody = parse(body)?;
    let item = own_item(app, user, m.id)?;
    let target = app.store.folder(m.folder).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?;
    app.store.move_item(item.id, target.id, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// POST /delete — move to Deleted Items, or delete permanently if already there.
pub fn delete(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct DeleteBody {
        id: i64,
    }
    let d: DeleteBody = parse(body)?;
    let item = own_item(app, user, d.id)?;
    let trash = role_folder(app, user, "deleteditems")?;
    if item.folder_id == trash.id {
        app.store.delete_item(item.id, WRITER).map_err(internal)?;
        Ok(json!({ "ok": true, "deleted": true }))
    } else {
        app.store.move_item(item.id, trash.id, WRITER).map_err(internal)?;
        Ok(json!({ "ok": true, "deleted": false }))
    }
}
