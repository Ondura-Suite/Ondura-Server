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

/// Where a file's previous versions are stored, one file per version id.
fn versions_dir(app: &Shared, drive_id: i64, item_id: i64) -> PathBuf {
    drive_dir(app, drive_id).join(format!("{item_id}.versions"))
}

/// How many previous versions of a file are kept.
const KEEP_VERSIONS: i64 = 50;

/// Remove a deleted file's contents and its version history from disk.
pub(crate) fn purge_item_disk(app: &Shared, drive_id: i64, item_id: i64) {
    let _ = std::fs::remove_file(file_path(app, drive_id, item_id));
    let _ = std::fs::remove_dir_all(versions_dir(app, drive_id, item_id));
}

/// Move a file's contents and its version history to another drive's directory
/// after the item itself was moved there.
pub(crate) fn move_item_disk(app: &Shared, from_drive: i64, to_drive: i64, item_id: i64) {
    let _ = std::fs::create_dir_all(drive_dir(app, to_drive));
    for (from, to) in [
        (file_path(app, from_drive, item_id), file_path(app, to_drive, item_id)),
        (versions_dir(app, from_drive, item_id), versions_dir(app, to_drive, item_id)),
    ] {
        if from.exists() && std::fs::rename(&from, &to).is_err() {
            // Different filesystems: fall back to copy and remove.
            if from.is_dir() {
                let _ = std::fs::create_dir_all(&to);
                if let Ok(entries) = std::fs::read_dir(&from) {
                    for e in entries.flatten() {
                        let _ = std::fs::copy(e.path(), to.join(e.file_name()));
                    }
                }
                let _ = std::fs::remove_dir_all(&from);
            } else if std::fs::copy(&from, &to).is_ok() {
                let _ = std::fs::remove_file(&from);
            }
        }
    }
}

/// Snapshot a file's current contents as a new version before it is overwritten
/// or restored over. Best-effort: a snapshot failure never blocks the write.
pub(crate) fn snapshot_version(app: &Shared, drive_id: i64, item_id: i64, by: i64) {
    let Ok(Some(vid)) = app.store.snapshot_file_version(item_id, by) else { return };
    let dir = versions_dir(app, drive_id, item_id);
    if std::fs::create_dir_all(&dir).and_then(|_| std::fs::copy(file_path(app, drive_id, item_id), dir.join(vid.to_string()))).is_err() {
        let _ = app.store.delete_file_version(vid);
        return;
    }
    if let Ok(gone) = app.store.prune_file_versions(item_id, KEEP_VERSIONS) {
        for g in gone {
            let _ = std::fs::remove_file(dir.join(g.to_string()));
        }
    }
}

/// The caller's role in a drive, or 404 if they can't see it.
fn role(app: &Shared, user: &User, drive_id: i64) -> Result<Role, (StatusCode, String)> {
    app.store.role_in_drive(user.id, drive_id).map_err(internal)?.ok_or_else(not_found)
}

/// The caller's effective role on an item of a drive (or on the drive root when
/// `item` is `None`), taking restricted folders into account; 404 if they can't
/// see it or the item isn't a live item of that drive.
fn role_at(app: &Shared, user: &User, drive_id: i64, item: Option<i64>) -> Result<Role, (StatusCode, String)> {
    if let Some(id) = item {
        let it = app.store.drive_item(id).map_err(internal)?.ok_or_else(not_found)?;
        if it.drive_id != drive_id {
            return Err(not_found());
        }
    }
    app.store.item_role(user.id, drive_id, item).map_err(internal)?.ok_or_else(not_found)
}

/// Whether the caller can see an item at all: they have access, and it isn't
/// a file waiting for approval that their read-only access doesn't show.
fn visible(app: &Shared, user: &User, it: &crate::cloud::Item) -> bool {
    match app.store.item_role(user.id, it.drive_id, Some(it.id)).ok().flatten() {
        Some(r) => !app.store.hidden_until_approved(it, r).unwrap_or(true),
        None => false,
    }
}

/// After a file's contents changed (`by` is `None` for a guest): in a library
/// that requires approval the file waits for an owner, and the owners hear
/// about it.
pub(crate) fn file_changed(app: &Shared, item: i64, by: Option<&User>) {
    let Ok(Some("pending")) = app.store.file_changed_for_approval(item, by.map(|u| u.id).unwrap_or(0)) else { return };
    let Ok(Some(it)) = app.store.drive_item(item) else { return };
    let actor = by.map(crate::activity::display_name).unwrap_or_else(|| "Guest".into());
    let link = format!("/sharepoint/{}{}", it.drive_id, it.parent_id.map(|p| format!("/{p}")).unwrap_or_default());
    for m in app.store.drive_members(it.drive_id).unwrap_or_default().into_iter().filter(|m| m.role == "owner") {
        let _ = app.store.add_activity(m.user_id, (by.map(|u| u.id).unwrap_or(0), &actor), "approval_requested", &it.name, "", &link);
    }
}

/// Refuse a change to an item (or anything below it) that someone else has
/// checked out.
fn ensure_unlocked(app: &Shared, user: &User, item: i64) -> Result<(), (StatusCode, String)> {
    if app.store.locked_for(item, Some(user.id)).map_err(internal)? {
        return Err((StatusCode::LOCKED, "Checked out by someone else.".into()));
    }
    Ok(())
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
    let parent: Option<i64> = query_param(query, "parent").and_then(|v| v.parse().ok());
    let r = role_at(app, user, drive_id, parent)?;
    let drive_role = app.store.role_in_drive(user.id, drive_id).map_err(internal)?;
    // Restricted subfolders the caller has no access to are left out.
    let items: Vec<_> = app
        .store
        .drive_items(drive_id, parent)
        .map_err(internal)?
        .into_iter()
        .filter(|it| it.kind != "folder" || !app.store.is_restricted(it.id).unwrap_or(false) || visible(app, user, it))
        .filter(|it| !app.store.hidden_until_approved(it, r).unwrap_or(true))
        .collect();
    // The breadcrumb starts at the first folder the caller can see, so someone
    // given access to one folder doesn't see the names above it.
    let path: Vec<(i64, String)> = match parent {
        Some(p) => {
            let full = app.store.drive_item_path(p).map_err(internal)?;
            if drive_role.is_some() {
                full
            } else {
                let first = full.iter().position(|(id, _)| app.store.item_role(user.id, drive_id, Some(*id)).ok().flatten().is_some()).unwrap_or(full.len());
                full[first..].to_vec()
            }
        }
        None => Vec::new(),
    };
    let checkouts = app.store.checkouts_in_folder(drive_id, parent).map_err(internal)?;
    let fields = app.store.fields_in_folder(drive_id, parent).map_err(internal)?;
    let columns = app.store.drive_columns(drive_id).map_err(internal)?;
    let approvals = app.store.approvals_in_folder(drive_id, parent).map_err(internal)?;
    let require_approval = app.store.requires_approval(drive_id).map_err(internal)?;
    Ok(json!({
        "requireApproval": require_approval,
        "drive": drive_id,
        "role": r.as_str(),
        "canWrite": r.can_write(),
        "canManage": r.can_admin(),
        "member": drive_role.is_some(),
        "libraryOwner": drive_role.is_some_and(|d| d.can_admin()),
        "parent": parent,
        "path": path.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        "columns": columns,
        "items": items.iter().map(|it| {
            let mut v = item_json(it);
            if it.kind == "folder" {
                v["restricted"] = json!(app.store.is_restricted(it.id).unwrap_or(false));
            }
            if let Some((_, c)) = checkouts.iter().find(|(id, _)| *id == it.id) {
                v["checkedOut"] = json!({ "id": c.user_id, "name": c.name, "email": c.email, "since": c.since, "mine": c.user_id == user.id });
            }
            if require_approval && let Some((_, a)) = approvals.iter().find(|(id, _)| *id == it.id) {
                v["approval"] = json!({ "status": a.status, "note": a.note, "by": a.submitted_by, "at": a.submitted_at, "mine": a.submitted_by == user.id });
            }
            let f: serde_json::Map<String, Value> = fields.iter().filter(|(i, _, _)| *i == it.id).map(|(_, c, val)| (c.to_string(), json!(val))).collect();
            if !f.is_empty() {
                v["fields"] = Value::Object(f);
            }
            v
        }).collect::<Vec<_>>(),
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
    if !role_at(app, user, b.drive, b.parent)?.can_write() {
        return Err(forbidden());
    }
    let id = app.store.create_drive_folder(b.drive, b.parent, &b.name, user.id).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "id": id }))
}

pub fn create_doc(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: NewFolder = parse(body)?;
    if !role_at(app, user, b.drive, b.parent)?.can_write() {
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
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    ensure_unlocked(app, user, it.id)?;
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
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() || !role_at(app, user, it.drive_id, b.parent)?.can_write() {
        return Err(forbidden());
    }
    ensure_unlocked(app, user, it.id)?;
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
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    ensure_unlocked(app, user, it.id)?;
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
    Ok(json!({ "items": items.iter().filter(|it| visible(app, user, it)).map(item_json).collect::<Vec<_>>() }))
}

pub fn restore_item(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    app.store.restore_item(b.item).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

pub fn trash_delete(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    let files = app.store.delete_drive_item(b.item).map_err(internal)?;
    for id in files {
        purge_item_disk(app, it.drive_id, id);
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
        purge_item_disk(app, b.drive, id);
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
    let was_member = app.store.role_in_drive(target.id, b.drive).map_err(internal)?.is_some();
    app.store.set_drive_member(b.drive, target.id, Role::parse(&b.role)).map_err(internal)?;
    if !was_member {
        let name = app.store.list_drives(target.id).map_err(internal)?.into_iter().find(|d| d.id == b.drive).map(|d| d.name).unwrap_or_default();
        let _ = app.store.add_activity(target.id, (user.id, &crate::activity::display_name(user)), "library_member", &name, "", &format!("/sharepoint/{}", b.drive));
    }
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
    let parent: Option<i64> = query_param(query, "parent").and_then(|v| v.parse().ok());
    match role_at(app, user, drive_id, parent) {
        Ok(r) if r.can_write() => {}
        Ok(_) => return json_err(StatusCode::FORBIDDEN, "read-only"),
        Err(_) => return json_err(StatusCode::NOT_FOUND, "not found"),
    }
    if body.len() > MAX_UPLOAD {
        return json_err(StatusCode::PAYLOAD_TOO_LARGE, "file too large");
    }
    let name = query_param(query, "name").map(decode).unwrap_or_default();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .unwrap_or("application/octet-stream");
    // Uploading over an existing file of the same name overwrites it and keeps
    // the previous contents as a version, the way OneDrive does.
    let existing = app.store.child_by_name(drive_id, parent, &name).ok().flatten().filter(|e| e.kind == "file");
    if existing.as_ref().is_some_and(|e| app.store.locked_for(e.id, Some(user.id)).unwrap_or(true)) {
        return json_err(StatusCode::LOCKED, "checked out by someone else");
    }
    let old_size = existing.as_ref().map(|e| e.size).unwrap_or(0);
    if over_quota(app, user, drive_id, old_size, body.len() as i64) {
        return json_err(StatusCode::INSUFFICIENT_STORAGE, "not enough storage");
    }
    let item = match &existing {
        Some(e) => {
            snapshot_version(app, drive_id, e.id, user.id);
            if app.store.set_file_meta(e.id, body.len() as i64, content_type, user.id).is_err() {
                return json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not store file");
            }
            e.id
        }
        None => match app.store.create_drive_file(drive_id, parent, &name, body.len() as i64, content_type, user.id) {
            Ok(id) => id,
            Err(e) => return json_err(StatusCode::BAD_REQUEST, &e.to_string()),
        },
    };
    let dir = drive_dir(app, drive_id);
    if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(item.to_string()), body)).is_err() {
        if existing.is_none() {
            let _ = app.store.delete_drive_item(item);
        }
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not store file");
    }
    file_changed(app, item, Some(user));
    (StatusCode::OK, axum::Json(json!({ "id": item }))).into_response()
}

/// `GET /mail/api/cloud/download/{item}` — stream a stored file to the browser.
pub fn download(app: &Shared, user: &User, item_id: &str) -> Response {
    let not_found = || (StatusCode::NOT_FOUND, "Not found").into_response();
    let Ok(id) = item_id.parse::<i64>() else { return not_found() };
    let Ok(Some(it)) = app.store.drive_item(id) else { return not_found() };
    if it.kind != "file" || !visible(app, user, &it) {
        return not_found();
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

/// Recently updated files and documents across the user's drives.
pub fn recent(app: &Shared, user: &User, query: &str) -> ApiResult {
    let limit: i64 = query_param(query, "limit").and_then(|v| v.parse().ok()).unwrap_or(50).clamp(1, 200);
    let items = app.store.recent_items(user.id, limit).map_err(internal)?;
    Ok(json!({
        "items": items.iter().filter(|r| visible(app, user, &r.item)).map(|r| {
            let mut v = item_json(&r.item);
            v["driveId"] = json!(r.drive_id);
            v["driveName"] = json!(r.drive_name);
            v["driveKind"] = json!(r.drive_kind);
            v
        }).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------- file versions

/// List a file's stored versions.
pub fn versions(app: &Shared, user: &User, query: &str) -> ApiResult {
    let item_id: i64 = query_param(query, "item").and_then(|v| v.parse().ok()).ok_or_else(|| bad("item required"))?;
    let it = app.store.drive_item(item_id).map_err(internal)?.ok_or_else(not_found)?;
    role_at(app, user, it.drive_id, Some(it.id))?;
    let versions = app.store.item_versions(item_id).map_err(internal)?;
    Ok(json!({
        "versions": versions.iter().map(|v| json!({
            "id": v.id, "size": v.size, "contentType": v.content_type, "created": v.created, "by": v.created_by,
        })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct VersionRef {
    item: i64,
    version: i64,
}

/// Make a stored version the file's current contents. The contents being
/// replaced are themselves kept as a new version first, so a restore is
/// reversible.
pub fn restore_version(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: VersionRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    ensure_unlocked(app, user, it.id)?;
    let v = app.store.file_version(b.version).map_err(internal)?.filter(|v| v.item_id == b.item).ok_or_else(not_found)?;
    let vpath = versions_dir(app, it.drive_id, it.id).join(v.id.to_string());
    if !vpath.exists() {
        return Err(not_found());
    }
    if over_quota(app, user, it.drive_id, it.size, v.size) {
        return Err((StatusCode::INSUFFICIENT_STORAGE, "not enough storage".into()));
    }
    // Keep the current contents as a version, then copy the chosen one in.
    snapshot_version(app, it.drive_id, it.id, user.id);
    if std::fs::copy(&vpath, file_path(app, it.drive_id, it.id)).is_err() {
        return Err(internal(anyhow::anyhow!("could not restore version")));
    }
    app.store.set_file_meta(it.id, v.size, &v.content_type, user.id).map_err(internal)?;
    file_changed(app, it.id, Some(user));
    Ok(json!({ "ok": true }))
}

/// Download one stored version of a file.
pub fn download_version(app: &Shared, user: &User, query: &str) -> Response {
    let nf = || (StatusCode::NOT_FOUND, "Not found").into_response();
    let Some(version_id) = query_param(query, "version").and_then(|v| v.parse::<i64>().ok()) else { return nf() };
    let Ok(Some(v)) = app.store.file_version(version_id) else { return nf() };
    let Ok(Some(it)) = app.store.drive_item(v.item_id) else { return nf() };
    if !visible(app, user, &it) {
        return nf();
    }
    let path = versions_dir(app, it.drive_id, it.id).join(v.id.to_string());
    let Ok(data) = std::fs::read(&path) else { return nf() };
    let mut r = data.into_response();
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&v.content_type).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    let safe: String = it.name.chars().filter(|c| !c.is_control() && *c != '"').collect();
    h.insert(header::CONTENT_DISPOSITION, HeaderValue::from_str(&format!("attachment; filename=\"{safe}\"")).unwrap_or(HeaderValue::from_static("attachment")));
    r
}

/// Search files, folders and documents by name across the user's drives.
pub fn search(app: &Shared, user: &User, query: &str) -> ApiResult {
    let q = query_param(query, "q").map(decode).unwrap_or_default();
    let q = q.trim();
    if q.is_empty() {
        return Ok(json!({ "items": [] }));
    }
    let items = app.store.search_items(user.id, q, 50).map_err(internal)?;
    Ok(json!({
        "items": items.iter().filter(|r| visible(app, user, &r.item)).map(|r| {
            let mut v = item_json(&r.item);
            v["driveId"] = json!(r.drive_id);
            v["driveName"] = json!(r.drive_name);
            v["driveKind"] = json!(r.drive_kind);
            v
        }).collect::<Vec<_>>(),
    }))
}

/// Documents shared directly with the signed-in user (not ones they own), and
/// library folders they were given access to without being a library member.
pub fn shared_with_me(app: &Shared, user: &User) -> ApiResult {
    let docs = app.store.docs_shared_with(user.id).map_err(internal)?;
    let folders = app.store.folders_granted_to(user.id).map_err(internal)?;
    let mut items: Vec<Value> = docs
        .iter()
        .map(|d| json!({ "kind": "doc", "doc": d.doc_id, "item": d.item_id, "name": d.name, "owner": d.owner, "updated": d.updated }))
        .collect();
    items.extend(folders.iter().map(|f| {
        json!({ "kind": "folder", "item": f.item_id, "drive": f.drive_id, "name": f.name, "owner": f.drive_name, "role": f.role, "updated": f.updated })
    }));
    items.sort_by_key(|v| std::cmp::Reverse(v["updated"].as_i64().unwrap_or(0)));
    Ok(json!({ "items": items }))
}

// ------------------------------------------------------------- item share links

#[derive(Deserialize)]
struct LinkCreate {
    item: i64,
    #[serde(default)]
    write: bool,
    /// Days from now until the link expires; `0`/absent means it never expires.
    #[serde(default)]
    expires_days: i64,
}

#[derive(Deserialize)]
struct LinkRef {
    token: String,
    #[serde(default)]
    write: bool,
    #[serde(default)]
    expires_days: i64,
}

/// A number of days (from now) turned into an absolute expiry time, or `None`.
fn expiry_from_days(days: i64) -> Option<i64> {
    (days > 0).then(|| crate::store::now() + days * 86_400)
}

/// List an item's share links (anyone with write access to the drive).
pub fn item_links(app: &Shared, user: &User, query: &str) -> ApiResult {
    let item_id: i64 = query_param(query, "item").and_then(|v| v.parse().ok()).ok_or_else(|| bad("item required"))?;
    let it = app.store.drive_item(item_id).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    let links = app.store.item_links(item_id).map_err(internal)?;
    Ok(json!({
        "links": links.iter().map(|l| json!({ "token": l.token, "canWrite": l.can_write, "created": l.created, "expires": l.expires })).collect::<Vec<_>>(),
    }))
}

pub fn create_item_link(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LinkCreate = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if it.kind == "doc" {
        // Documents are shared through their own collaborative-link mechanism.
        return Err(bad("documents use document share links"));
    }
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    let token = app.store.create_item_link(b.item, b.write, expiry_from_days(b.expires_days), user.id).map_err(internal)?;
    Ok(json!({ "token": token, "canWrite": b.write }))
}

pub fn update_item_link(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LinkRef = parse(body)?;
    if let Some(drive) = app.store.library_link_drive(&b.token).map_err(internal)? {
        if !role(app, user, drive)?.can_admin() {
            return Err(forbidden());
        }
        app.store.set_library_link(&b.token, b.write, expiry_from_days(b.expires_days)).map_err(internal)?;
        return Ok(json!({ "ok": true }));
    }
    let item_id = app.store.item_link_item(&b.token).map_err(internal)?.ok_or_else(not_found)?;
    let it = app.store.drive_item(item_id).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    app.store.set_item_link(&b.token, b.write, expiry_from_days(b.expires_days)).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

pub fn revoke_item_link(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LinkRef = parse(body)?;
    if let Some(drive) = app.store.library_link_drive(&b.token).map_err(internal)? {
        if !role(app, user, drive)?.can_admin() {
            return Err(forbidden());
        }
        app.store.revoke_library_link(&b.token).map_err(internal)?;
        return Ok(json!({ "ok": true }));
    }
    let item_id = app.store.item_link_item(&b.token).map_err(internal)?.ok_or_else(not_found)?;
    let it = app.store.drive_item(item_id).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    app.store.revoke_item_link(&b.token).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

// --------------------------------------------------------- public (guest) access

/// What a share link opens: one file or folder, or a whole library.
struct LinkScope {
    drive: i64,
    /// The shared item, or `None` for a library link (the library root).
    root: Option<crate::cloud::Item>,
    /// Display name: the item's or the library's.
    name: String,
    can_write: bool,
    /// The account that created the link; a guest's changes are attributed to it.
    by: i64,
}

fn link_scope(app: &Shared, token: &str) -> Option<LinkScope> {
    if let Some((it, can_write)) = app.store.item_by_link(token).ok().flatten() {
        let by = app.store.item_link_creator(token).ok().flatten().unwrap_or(0);
        return Some(LinkScope { drive: it.drive_id, name: it.name.clone(), root: Some(it), can_write, by });
    }
    let (drive, name, can_write, by) = app.store.library_by_link(token).ok().flatten()?;
    Some(LinkScope { drive, root: None, name, can_write, by })
}

impl LinkScope {
    fn is_folder(&self) -> bool {
        self.root.as_ref().is_none_or(|r| r.kind == "folder")
    }
    fn root_id(&self) -> Option<i64> {
        self.root.as_ref().map(|r| r.id)
    }
    /// Whether `item` is reachable through this link: inside the shared folder
    /// (or library), live, and not below a restricted folder under the link's
    /// root — restricted folders keep their own audience even from links.
    fn reaches(&self, app: &Shared, item: i64) -> bool {
        let Ok(Some(it)) = app.store.drive_item(item) else { return false };
        if it.drive_id != self.drive {
            return false;
        }
        let path = app.store.drive_item_path(item).unwrap_or_default();
        let below: Vec<i64> = match self.root_id() {
            Some(r) => match path.iter().position(|(id, _)| *id == r) {
                Some(pos) => path[pos + 1..].iter().map(|(id, _)| *id).collect(),
                None => return false,
            },
            None => path.iter().map(|(id, _)| *id).collect(),
        };
        if below.iter().any(|id| app.store.is_restricted(*id).unwrap_or(true)) {
            return false;
        }
        // Guests only see approved files in a library that requires approval.
        if app.store.hidden_until_approved(&it, Role::Viewer).unwrap_or(true) {
            return false;
        }
        // Nothing on the way down may be in the recycle bin.
        let trashed = app.store.trashed_items(self.drive).unwrap_or_default();
        !path.iter().any(|(id, _)| trashed.iter().any(|t| t.id == *id))
    }
    /// A folder (or the root, for `None`) the guest may list or upload into.
    fn folder_ok(&self, app: &Shared, folder: Option<i64>) -> bool {
        match folder {
            None => self.root.is_none(),
            Some(f) if Some(f) == self.root_id() => true,
            Some(f) => self.reaches(app, f) && app.store.drive_item(f).ok().flatten().is_some_and(|it| it.kind == "folder"),
        }
    }
}

/// Metadata for a shared link, with no account required. For a folder or a
/// library the guest can browse the contents; a file can be previewed or
/// downloaded.
pub fn shared_meta(app: &Shared, token: &str) -> ApiResult {
    let sc = link_scope(app, token).ok_or_else(not_found)?;
    let (kind, size, ct) = match &sc.root {
        Some(it) => (it.kind.as_str(), it.size, it.content_type.as_str()),
        None => ("folder", 0, ""),
    };
    Ok(json!({
        "kind": kind,
        "name": sc.name,
        "size": size,
        "contentType": ct,
        "canWrite": sc.can_write,
        "root": sc.root_id(),
        "library": sc.root.is_none(),
    }))
}

/// List a folder share's children. `parent` must be the shared folder itself or
/// a folder below it, so a link can't reach the rest of the drive.
pub fn shared_items(app: &Shared, token: &str, query: &str) -> ApiResult {
    let sc = link_scope(app, token).ok_or_else(not_found)?;
    if !sc.is_folder() {
        return Err(not_found());
    }
    let parent: Option<i64> = query_param(query, "parent").and_then(|v| v.parse().ok()).or(sc.root_id());
    if !sc.folder_ok(app, parent) {
        return Err(not_found());
    }
    let items: Vec<_> = app
        .store
        .drive_items(sc.drive, parent)
        .map_err(internal)?
        .into_iter()
        .filter(|it| !(it.kind == "folder" && app.store.is_restricted(it.id).unwrap_or(true)))
        .filter(|it| !app.store.hidden_until_approved(it, Role::Viewer).unwrap_or(true))
        .collect();
    // Breadcrumb within the share only (root-relative).
    let path: Vec<(i64, String)> = match parent {
        None => Vec::new(),
        Some(p) => {
            let full = app.store.drive_item_path(p).map_err(internal)?;
            match sc.root_id() {
                Some(r) => full.into_iter().skip_while(|(id, _)| *id != r).collect(),
                None => full,
            }
        }
    };
    Ok(json!({
        "items": items.iter().map(item_json).collect::<Vec<_>>(),
        "path": path.iter().map(|(id, name)| json!({ "id": id, "name": name })).collect::<Vec<_>>(),
        "canWrite": sc.can_write,
        "parent": parent,
    }))
}

/// Would storing `new` bytes (replacing `old`) push a metered drive over quota?
fn guest_over_quota(app: &Shared, drive_id: i64, old: i64, new: i64) -> bool {
    match app.store.drive_quota(drive_id) {
        Ok(Some(q)) if q > 0 => app.store.drive_total_size(drive_id).unwrap_or(0) - old + new > q,
        _ => false,
    }
}

/// Upload through an edit share link, with no account. A folder or library
/// link drops the file into the shared folder (or one below it); a file link
/// replaces that file, keeping the old contents as a version. Checked-out
/// files can't be replaced.
pub fn shared_upload(app: &Shared, token: &str, query: &str, headers: &HeaderMap, body: &Bytes) -> Response {
    let json_err = |code: StatusCode, msg: &str| (code, axum::Json(json!({ "error": msg }))).into_response();
    let Some(sc) = link_scope(app, token) else {
        return json_err(StatusCode::NOT_FOUND, "not found");
    };
    if !sc.can_write {
        return json_err(StatusCode::FORBIDDEN, "read-only");
    }
    if body.len() > MAX_UPLOAD {
        return json_err(StatusCode::PAYLOAD_TOO_LARGE, "file too large");
    }
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).filter(|v| !v.is_empty()).unwrap_or("application/octet-stream");
    // Resolve where the upload lands and which file (if any) it overwrites.
    let (parent, name, existing) = if sc.is_folder() {
        let parent: Option<i64> = query_param(query, "parent").and_then(|v| v.parse().ok()).or(sc.root_id());
        if !sc.folder_ok(app, parent) {
            return json_err(StatusCode::NOT_FOUND, "not found");
        }
        let name = query_param(query, "name").map(decode).unwrap_or_default();
        let existing = app.store.child_by_name(sc.drive, parent, &name).ok().flatten().filter(|e| e.kind == "file");
        (parent, name, existing)
    } else {
        let Some(root) = sc.root.clone().filter(|r| r.kind == "file") else {
            return json_err(StatusCode::NOT_FOUND, "not found");
        };
        (root.parent_id, root.name.clone(), Some(root))
    };
    if existing.as_ref().is_some_and(|e| app.store.locked_for(e.id, None).unwrap_or(true)) {
        return json_err(StatusCode::LOCKED, "checked out");
    }
    let old_size = existing.as_ref().map(|e| e.size).unwrap_or(0);
    if guest_over_quota(app, sc.drive, old_size, body.len() as i64) {
        return json_err(StatusCode::INSUFFICIENT_STORAGE, "not enough storage");
    }
    let by = sc.by;
    let item = match &existing {
        Some(e) => {
            snapshot_version(app, sc.drive, e.id, by);
            if app.store.set_file_meta(e.id, body.len() as i64, content_type, by).is_err() {
                return json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not store file");
            }
            e.id
        }
        None => match app.store.create_drive_file(sc.drive, parent, &name, body.len() as i64, content_type, by) {
            Ok(id) => id,
            Err(e) => return json_err(StatusCode::BAD_REQUEST, &e.to_string()),
        },
    };
    let dir = drive_dir(app, sc.drive);
    if std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(item.to_string()), body)).is_err() {
        if existing.is_none() {
            let _ = app.store.delete_drive_item(item);
        }
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "could not store file");
    }
    file_changed(app, item, None);
    (StatusCode::OK, axum::Json(json!({ "id": item }))).into_response()
}

/// Download (or preview) a file reached through a share link. For a folder or
/// library share an `item` query names a file below it; for a file share it is
/// the file.
pub fn shared_download(app: &Shared, token: &str, query: &str) -> Response {
    let nf = || (StatusCode::NOT_FOUND, "Not found").into_response();
    let Some(sc) = link_scope(app, token) else { return nf() };
    let target = if sc.is_folder() {
        let Some(id) = query_param(query, "item").and_then(|v| v.parse::<i64>().ok()) else { return nf() };
        if !sc.reaches(app, id) {
            return nf();
        }
        match app.store.drive_item(id) {
            Ok(Some(it)) => it,
            _ => return nf(),
        }
    } else {
        match sc.root {
            Some(r) => r,
            None => return nf(),
        }
    };
    if target.kind != "file" {
        return nf();
    }
    let inline = query_param(query, "inline").is_some();
    serve_file(app, &target, inline && inline_wanted(&target))
}

// ------------------------------------------------------------- library extras

/// The library's own share links (owners only).
pub fn library_links(app: &Shared, user: &User, query: &str) -> ApiResult {
    let drive: i64 = query_param(query, "drive").and_then(|v| v.parse().ok()).ok_or_else(|| bad("drive required"))?;
    if !role(app, user, drive)?.can_admin() {
        return Err(forbidden());
    }
    let links = app.store.library_links(drive).map_err(internal)?;
    Ok(json!({
        "links": links.iter().map(|l| json!({ "token": l.token, "canWrite": l.can_write, "created": l.created, "expires": l.expires })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct LibraryLinkCreate {
    drive: i64,
    #[serde(default)]
    write: bool,
    #[serde(default)]
    expires_days: i64,
}

pub fn create_library_link(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LibraryLinkCreate = parse(body)?;
    if app.store.drive_kind(b.drive).map_err(internal)?.as_deref() != Some("library") {
        return Err(bad("only libraries have library links"));
    }
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    let token = app.store.create_library_link(b.drive, b.write, expiry_from_days(b.expires_days), user.id).map_err(internal)?;
    Ok(json!({ "token": token, "canWrite": b.write }))
}

/// A folder's permissions: whether it has its own, and its access list.
pub fn permissions(app: &Shared, user: &User, query: &str) -> ApiResult {
    let item: i64 = query_param(query, "item").and_then(|v| v.parse().ok()).ok_or_else(|| bad("item required"))?;
    let it = app.store.drive_item(item).map_err(internal)?.ok_or_else(not_found)?;
    let r = role_at(app, user, it.drive_id, Some(item))?;
    let restricted = app.store.is_restricted(item).map_err(internal)?;
    let from = app.store.restriction_of(item).map_err(internal)?.filter(|f| *f != item);
    let entries = if restricted { app.store.item_acl(item).map_err(internal)? } else { Vec::new() };
    Ok(json!({
        "restricted": restricted,
        "inheritsFrom": from.and_then(|f| app.store.drive_item(f).ok().flatten()).map(|f| json!({ "id": f.id, "name": f.name })),
        "canManage": r.can_admin(),
        "entries": entries.iter().map(|m| json!({ "id": m.user_id, "email": m.email, "name": m.name, "role": m.role })).collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
struct Restrict {
    item: i64,
    restricted: bool,
}

/// Give a folder its own permissions or let it inherit again. Only library
/// owners and owners of the folder's access list may do this; library drives
/// only (a personal drive is shared through links).
pub fn set_restricted(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: Restrict = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if app.store.drive_kind(it.drive_id).map_err(internal)?.as_deref() != Some("library") {
        return Err(bad("only library folders have their own permissions"));
    }
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_admin() {
        return Err(forbidden());
    }
    app.store.set_restricted(b.item, b.restricted).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct AclSet {
    item: i64,
    email: String,
    role: String,
}

pub fn set_permission(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: AclSet = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_admin() {
        return Err(forbidden());
    }
    if !app.store.is_restricted(b.item).map_err(internal)? {
        return Err(bad("the folder inherits its permissions"));
    }
    let target = app.store.user_by_email(b.email.trim()).map_err(internal)?.ok_or_else(|| bad("no such user"))?;
    let had_access = app.store.item_role(target.id, it.drive_id, Some(it.id)).map_err(internal)?.is_some();
    app.store.set_item_acl(b.item, target.id, Role::parse(&b.role)).map_err(internal)?;
    if !had_access {
        let link = format!("/sharepoint/{}/{}", it.drive_id, it.id);
        let _ = app.store.add_activity(target.id, (user.id, &crate::activity::display_name(user)), "folder_access", &it.name, "", &link);
    }
    Ok(json!({ "id": target.id }))
}

#[derive(Deserialize)]
struct AclRemove {
    item: i64,
    user: i64,
}

pub fn remove_permission(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: AclRemove = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_admin() {
        return Err(forbidden());
    }
    app.store.remove_item_acl(b.item, b.user).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// Check a file out: until it is checked in, only this account can change it.
pub fn check_out(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if it.kind != "file" {
        return Err(bad("only files can be checked out"));
    }
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    app.store.check_out(it.id, user.id).map_err(|e| (StatusCode::LOCKED, e.to_string()))?;
    Ok(json!({ "ok": true }))
}

/// Check a file back in. The holder can always; whoever may manage the item
/// can also discard someone else's check-out.
pub fn check_in(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ItemRef = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    let r = role_at(app, user, it.drive_id, Some(it.id))?;
    let Some(c) = app.store.checkout_of(it.id).map_err(internal)? else { return Ok(json!({ "ok": true })) };
    if c.user_id != user.id && !r.can_admin() {
        return Err(forbidden());
    }
    app.store.check_in(it.id).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct NewColumn {
    drive: i64,
    name: String,
    kind: String,
    #[serde(default)]
    choices: Vec<String>,
}

/// Add a metadata column to a library (library owners).
pub fn add_column(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: NewColumn = parse(body)?;
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    let id = app.store.add_drive_column(b.drive, &b.name, &b.kind, &b.choices).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "id": id }))
}

#[derive(Deserialize)]
struct ColumnRef {
    drive: i64,
    id: i64,
}

pub fn delete_column(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: ColumnRef = parse(body)?;
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    app.store.delete_drive_column(b.drive, b.id).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct FieldSet {
    item: i64,
    column: i64,
    #[serde(default)]
    value: String,
}

/// Set one metadata value on an item (anyone who can edit it).
pub fn set_field(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: FieldSet = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role_at(app, user, it.drive_id, Some(it.id))?.can_write() {
        return Err(forbidden());
    }
    ensure_unlocked(app, user, it.id)?;
    app.store.set_item_field(it.id, b.column, &b.value).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct LibrarySettings {
    drive: i64,
    require_approval: bool,
}

/// Library settings (owners): whether changes need an owner's approval.
pub fn library_settings(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: LibrarySettings = parse(body)?;
    if app.store.drive_kind(b.drive).map_err(internal)?.as_deref() != Some("library") {
        return Err(bad("only libraries have settings"));
    }
    if !role(app, user, b.drive)?.can_admin() {
        return Err(forbidden());
    }
    app.store.set_requires_approval(b.drive, b.require_approval).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

#[derive(Deserialize)]
struct Review {
    item: i64,
    approve: bool,
    #[serde(default)]
    note: String,
}

/// Approve or reject a waiting file (library owners). The person who submitted
/// it is told the outcome.
pub fn review(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    let b: Review = parse(body)?;
    let it = app.store.drive_item(b.item).map_err(internal)?.ok_or_else(not_found)?;
    if !role(app, user, it.drive_id)?.can_admin() {
        return Err(forbidden());
    }
    let before = app.store.approval_of(it.id).map_err(internal)?.ok_or_else(|| bad("nothing to approve"))?;
    app.store.review_file(it.id, user.id, b.approve, &b.note).map_err(|e| bad(e.to_string()))?;
    let link = format!("/sharepoint/{}{}", it.drive_id, it.parent_id.map(|p| format!("/{p}")).unwrap_or_default());
    let kind = if b.approve { "approved" } else { "rejected" };
    let _ = app.store.add_activity(before.submitted_by, (user.id, &crate::activity::display_name(user)), kind, &it.name, b.note.trim(), &link);
    Ok(json!({ "ok": true }))
}
