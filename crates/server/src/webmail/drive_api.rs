//! Cloud storage API under `/mail/api/cloud`: personal drives and shared
//! libraries holding folders, uploaded files and collaborative documents.
//!
//! JSON endpoints are dispatched from `route`; binary upload and download are
//! handled in `handle` before the JSON router. The whole surface is gated
//! behind `[cloud] enabled`.

use std::path::PathBuf;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::mail_api::{decode, query_param};
use super::{bad, internal, not_found, parse, ApiResult};
use crate::app::Shared;
use crate::cloud::Role;
use crate::store::User;

/// Largest file accepted in a single upload (held in memory for now).
const MAX_UPLOAD: usize = 100 * 1024 * 1024;

fn forbidden() -> (StatusCode, String) {
    (StatusCode::FORBIDDEN, "You don't have access to this.".into())
}

/// Whether storing `new` bytes (replacing `old`) would push the user's personal
/// drive past their quota. Only personal drives are metered; a quota of 0 means
/// unlimited.
fn over_quota(app: &Shared, user: &User, drive_id: i64, old: i64, new: i64) -> bool {
    if user.quota <= 0 {
        return false;
    }
    if app.store.ensure_personal_drive(user.id).ok() != Some(drive_id) {
        return false;
    }
    let used = app.store.drive_total_size(drive_id).unwrap_or(0);
    used - old + new > user.quota
}

/// The directory that holds a drive's file contents on disk.
fn drive_dir(app: &Shared, drive_id: i64) -> PathBuf {
    app.cfg.cloud.files_dir(&app.cfg.data_dir).join(drive_id.to_string())
}

fn file_path(app: &Shared, drive_id: i64, item_id: i64) -> PathBuf {
    drive_dir(app, drive_id).join(item_id.to_string())
}

/// The caller's role in a drive, or 404 if they can't see it.
fn role(app: &Shared, user: &User, drive_id: i64) -> Result<Role, (StatusCode, String)> {
    app.store.role_in_drive(user.id, drive_id).map_err(internal)?.ok_or_else(not_found)
}

fn item_json(it: &crate::cloud::Item) -> Value {
    json!({
        "id": it.id,
        "drive": it.drive_id,
        "parent": it.parent_id,
        "kind": it.kind,
        "name": it.name,
        "size": it.size,
        "contentType": it.content_type,
        "doc": it.doc_id,
        "updated": it.updated,
    })
}

// -------------------------------------------------------------------- drives

pub fn drives(app: &Shared, user: &User) -> ApiResult {
    let drives = app.store.list_drives(user.id).map_err(internal)?;
    Ok(json!({
        "drives": drives.iter().map(|d| json!({
            "id": d.id, "kind": d.kind, "name": d.name, "role": d.role,
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct NameBody {
    name: String,
}

pub fn create_library(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: NameBody = parse(body)?;
    let id = app.store.create_library(user.id, &b.name).map_err(internal)?;
    Ok(json!({ "id": id }))
}

#[derive(Deserialize)]
struct RenameDrive {
    drive: i64,
    name: String,
}

pub fn rename_drive(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: RenameDrive = parse(body)?;
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    app.store.rename_drive(b.drive, &b.name).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct DriveRef {
    drive: i64,
}

pub fn delete_library(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: DriveRef = parse(body)?;
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    let files = app.store.delete_library(b.drive).map_err(internal)?;
    for id in files {
        let _ = std::fs::remove_file(file_path(app, b.drive, id));
    }
    let _ = std::fs::remove_dir_all(drive_dir(app, b.drive));
    Ok(json!({ "ok": true }))
}

// --------------------------------------------------------------------- items

pub fn items(app: &Shared, user: &User, query: &str) -> ApiResult {
    let drive_id: i64 = query_param(query, "drive").and_then(|v| v.parse().ok()).ok_or_else(|| bad("drive required"))?;
    let r = role(app, user, drive_id)?;
    let parent: Option<i64> = query_param(query, "parent").and_then(|v| v.parse().ok());
    let items = app.store.drive_items(drive_id, parent).map_err(internal)?;
    let path = match parent {
        Some(p) => app.store.drive_item_path(p).map_err(internal)?,
        None => Vec::new(),
    };
    Ok(json!({
        "drive": drive_id,
        "role": r.as_str(),
        "canWrite": r.can_write(),
        "parent": parent,
        "path": path.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        "items": items.iter().map(item_json).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct NewFolder {
    drive: i64,
    parent: Option<i64>,
    name: String,
}

pub fn create_folder(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: NewFolder = parse(body)?;
    if !role(app, user, b.drive)?.can_write() {
        return Err(forbidden());
    }
    let id = app.store.create_drive_folder(b.drive, b.parent, &b.name, user.id).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "id": id }))
}

pub fn create_doc(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: NewFolder = parse(body)?;
    if !role(app, user, b.drive)?.can_write() {
        return Err(forbidden());
    }
    let (item, doc) = app.store.create_drive_doc(b.drive, b.parent, &b.name, user.id).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "id": item, "doc": doc }))
}

#[derive(Deserialize)]
struct ItemRename {
    item: i64,
    name: String,
}

pub fn rename_item(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRename = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    app.store.rename_drive_item(b.item, &b.name, user.id).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct ItemMove {
    item: i64,
    parent: Option<i64>,
}

pub fn move_item(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemMove = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    // A destination folder must be in the same drive.
    if let Some(p) = b.parent {
        let dest = app.store.drive_item(p).map_err(internal)?.ok_or_else(not_found)?;
        if dest.drive_id != it.drive_id || dest.kind != "folder" {
            return Err(bad("invalid destination"));
        }
    }
    app.store.move_drive_item(b.item, b.parent, user.id).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct ItemRef {
    item: i64,
}

pub fn delete_item(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    // Soft delete: the item (and its subtree) moves to the drive's recycle bin
    // and keeps counting toward quota until the bin is emptied, matching O365.
    app.store.trash_item(b.item, user.id).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

// ------------------------------------------------------------------- recycle bin

pub fn trash(app: &Shared, user: &User, query: &str) -> ApiResult {
    let drive_id: i64 = query_param(query, "drive").and_then(|v| v.parse().ok()).ok_or_else(|| bad("drive required"))?;
    role(app, user, drive_id)?;
    let items = app.store.trashed_items(drive_id).map_err(internal)?;
    Ok(json!({ "items": items.iter().map(item_json).collect::<Vec<_>>() }))
}

pub fn restore_item(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    app.store.restore_item(b.item).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

pub fn trash_delete(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    let files = app.store.delete_drive_item(b.item).map_err(internal)?;
    for id in files {
        let _ = std::fs::remove_file(file_path(app, it.drive_id, id));
    }
    Ok(json!({ "ok": true }))
}

pub fn trash_empty(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: DriveRef = parse(body)?;
    if !role(app, user, b.drive)?.can_write() {
        return Err(forbidden());
    }
    let files = app.store.empty_trash(b.drive).map_err(internal)?;
    for id in files {
        let _ = std::fs::remove_file(file_path(app, b.drive, id));
    }
    Ok(json!({ "ok": true }))
}

// ------------------------------------------------------------------- members

pub fn members(app: &Shared, user: &User, query: &str) -> ApiResult {
    let drive_id: i64 = query_param(query, "drive").and_then(|v| v.parse().ok()).ok_or_else(|| bad("drive required"))?;
    role(app, user, drive_id)?;
    let members = app.store.drive_members(drive_id).map_err(internal)?;
    Ok(json!({
        "members": members.iter().map(|m| json!({
            "id": m.user_id, "email": m.email, "name": m.name, "role": m.role,
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct SetMember {
    drive: i64,
    email: String,
    role: String,
}

pub fn set_member(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: SetMember = parse(body)?;
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    let target = app.store.user_by_email(b.email.trim()).map_err(internal)?.ok_or_else(|| bad("no such user"))?;
    app.store.set_drive_member(b.drive, target.id, Role::parse(&b.role)).map_err(internal)?;
    Ok(json!({ "id": target.id }))
}

#[derive(Deserialize)]
struct RemoveMember {
    drive: i64,
    user: i64,
}

pub fn remove_member(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: RemoveMember = parse(body)?;
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    app.store.remove_drive_member(b.drive, b.user).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

// ------------------------------------------------------------ binary up/down

/// `POST /mail/api/cloud/upload?drive=&parent=&name=` with the file as the raw
/// body. Returns the new item as JSON.
pub fn upload(app: &Shared, user: &User, query: &str, headers: &HeaderMap, body: &Bytes) -> Response {
    let json_err = |code: StatusCode, msg: &str| (code, axum::Json(json!({ "error": msg }))).into_response();
    let Some(drive_id) = query_param(query, "drive").and_then(|v| v.parse::<i64>().ok()) else {
        return json_err(StatusCode::BAD_REQUEST, "drive required");
    };
    match app.store.role_in_drive(user.id, drive_id) {
        Ok(Some(r)) if r.can_write() => {}
        Ok(Some(_)) => return json_err(StatusCode::FORBIDDEN, "read-only"),
        _ => return json_err(StatusCode::NOT_FOUND, "not found"),
    }
    if body.len() > MAX_UPLOAD {
        return json_err(StatusCode::PAYLOAD_TOO_LARGE, "file too large");
    }
    if over_quota(app, user, drive_id, 0, body.len() as i64) {
        return json_err(StatusCode::INSUFFICIENT_STORAGE, "not enough storage");
    }
    let parent: Option<i64> = query_param(query, "parent").and_then(|v| v.parse().ok());
    let name = query_param(query, "name").map(decode).unwrap_or_default();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("application/octet-stream");
    let item = match app.store.create_drive_file(drive_id, parent, &name, body.len() as i64, content_type, user.id) {
        Ok(id) => id,
        Err(e) => return json_err(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let dir = drive_dir(app, drive_id);
    if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(item.to_string()), body)).is_err() {
        let _ = app.store.delete_drive_item(item);
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not store file");
    }
    (StatusCode::OK, axum::Json(json!({ "id": item }))).into_response()
}

/// `GET /mail/api/cloud/download/{item}` — stream a stored file to the browser.
pub fn download(app: &Shared, user: &User, item_id: &str) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "Not found").into_response();
    let Ok(id) = item_id.parse::<i64>() else { return not_found() };
    let Ok(Some(it)) = app.store.drive_item(id) else { return not_found() };
    if it.kind != "file" {
        return not_found();
    }
    match app.store.role_in_drive(user.id, it.drive_id) {
        Ok(Some(_)) => {}
        _ => return not_found(),
    }
    serve_file(app, &it, inline_wanted(&it))
}

/// True for types a browser can safely render in place (images, PDFs); these are
/// served inline so a preview can show them, everything else as a download.
fn inline_wanted(it: &crate::cloud::Item) -> bool {
    let ct = it.content_type.as_str();
    ct.starts_with("image/") || ct == "application/pdf" || ct.starts_with("text/")
}

/// Stream a stored file's bytes with the right type and disposition.
fn serve_file(app: &Shared, it: &crate::cloud::Item, inline: bool) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "Not found").into_response();
    let Ok(data) = std::fs::read(file_path(app, it.drive_id, it.id)) else { return not_found() };
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&it.content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    let safe: String = it.name.chars().filter(|c| !c.is_control() && *c != '"').collect();
    let disp = if inline { "inline" } else { "attachment" };
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(&format!("{disp}; filename=\"{safe}\"")).unwrap_or(HeaderValue::from_static("attachment")));
    r
}

// ------------------------------------------------------------- item share links

#[derive(Deserialize)]
struct LinkCreate {
    item: i64,
    #[serde(default)]
    write: bool,
}

#[derive(Deserialize)]
struct LinkRef {
    token: String,
    #[serde(default)]
    write: bool,
}

/// List an item's share links (anyone with write access to the drive).
pub fn item_links(app: &Shared, user: &User, query: &str) -> ApiResult {
    let item_id: i64 = query_param(query, "item").and_then(|v| v.parse().ok()).ok_or_else(|| bad("item required"))?;
    let it = app.store.drive_item(item_id).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    let links = app.store.item_links(item_id).map_err(internal)?;
    Ok(json!({
        "links": links.iter().map(|(t, w, c)| json!({ "token": t, "canWrite": w, "created": c })).collect::<Vec<_>>(),
    }))
}

pub fn create_item_link(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LinkCreate = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if it.kind == "doc" {
        // Documents are shared through their own collaborative-link mechanism.
        return Err(bad("documents use document share links"));
    }
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    let token = app.store.create_item_link(b.item, b.write, user.id).map_err(internal)?;
    Ok(json!({ "token": token, "canWrite": b.write }))
}

pub fn update_item_link(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LinkRef = parse(body)?;
    let item_id = app.store.item_link_item(&b.token).map_err(internal)?.ok_or_else(not_found)?;
    let it = app.store.drive_item(item_id).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    app.store.set_item_link_write(&b.token, b.write).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

pub fn revoke_item_link(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LinkRef = parse(body)?;
    let item_id = app.store.item_link_item(&b.token).map_err(internal)?.ok_or_else(not_found)?;
    let it = app.store.drive_item(item_id).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_write() {
        return Err(forbidden());
    }
    app.store.revoke_item_link(&b.token).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

// --------------------------------------------------------- public (guest) access

/// Metadata for a shared link, with no account required. For a folder the guest
/// can browse the subtree; for a file it can be previewed or downloaded.
pub fn shared_meta(app: &Shared, token: &str) -> ApiResult {
    let (it, can_write) = app.store.item_by_link(token).map_err(internal)?.ok_or_else(not_found)?;
    Ok(json!({
        "kind": it.kind,
        "name": it.name,
        "size": it.size,
        "contentType": it.content_type,
        "canWrite": can_write,
        "root": it.id,
    }))
}

/// List a folder share's children. `parent` must be the shared folder itself or
/// a descendant of it, so a link can't reach the rest of the drive.
pub fn shared_items(app: &Shared, token: &str, query: &str) -> ApiResult {
    let (root, _) = app.store.item_by_link(token).map_err(internal)?.ok_or_else(not_found)?;
    if root.kind != "folder" {
        return Err(not_found());
    }
    let parent: i64 = query_param(query, "parent").and_then(|v| v.parse().ok()).unwrap_or(root.id);
    if parent != root.id && !app.store.is_within(root.id, Some(parent)).map_err(internal)? {
        return Err(not_found());
    }
    let items = app.store.drive_items(root.drive_id, Some(parent)).map_err(internal)?;
    // Breadcrumb within the share only (root-relative).
    let full = app.store.drive_item_path(parent).map_err(internal)?;
    let path: Vec<_> = full.into_iter().skip_while(|(id, _)| *id != root.id).collect();
    Ok(json!({
        "items": items.iter().map(item_json).collect::<Vec<_>>(),
        "path": path.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        "canWrite": false,
    }))
}

/// Download (or preview) a file reached through a share link. For a folder share
/// an `item` query names a descendant file; for a file share it is the file.
pub fn shared_download(app: &Shared, token: &str, query: &str) -> Response {
    let nf = || (StatusCode::NOT_FOUND, "Not found").into_response();
    let Ok(Some((root, _))) = app.store.item_by_link(token) else { return nf() };
    let target = if root.kind == "folder" {
        let Some(id) = query_param(query, "item").and_then(|v| v.parse::<i64>().ok()) else { return nf() };
        let Ok(within) = app.store.is_within(root.id, Some(id)) else { return nf() };
        if !within {
            return nf();
        }
        match app.store.drive_item(id) {
            Ok(Some(it)) => it,
            _ => return nf(),
        }
    } else {
        root
    };
    if target.kind != "file" {
        return nf();
    }
    let inline = query_param(query, "inline").is_some();
    serve_file(app, &target, inline && inline_wanted(&target))
}
