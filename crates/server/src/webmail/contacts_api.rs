//! Contact endpoints: list, create/update, delete.

use axum::body::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{bad, internal, not_found, own_item, parse, ApiResult, WRITER};
use crate::app::Shared;
use crate::model::{Contact, ItemKind};
use crate::store::User;

fn contact_json(id: i64, folder: i64, c: &Contact) -> Value {
    let display = if !c.display_name.is_empty() {
        c.display_name.clone()
    } else if !c.file_as.is_empty() {
        c.file_as.clone()
    } else {
        format!("{} {}", c.first_name, c.last_name).trim().to_string()
    };
    json!({
        "id": id,
        "folder": folder,
        "displayName": display,
        "emails": c.emails,
        "company": c.company,
        "jobTitle": c.job_title,
        "mobilePhone": c.mobile_phone,
        "businessPhone": c.business_phone,
        "contact": c,
    })
}

/// GET /contacts
pub fn list(app: &Shared, user: &User) -> ApiResult {
    let folders = app.store.folders(user.id).map_err(internal)?;
    let mut out = Vec::new();
    for f in folders.iter().filter(|f| matches!(f.class, crate::model::FolderClass::Contacts)) {
        for item in app.store.items(f.id, 0, 5000).map_err(internal)? {
            if let Some(c) = item.contact() {
                out.push(contact_json(item.id, f.id, &c));
            }
        }
    }
    out.sort_by(|a, b| a["displayName"].as_str().unwrap_or("").to_lowercase().cmp(&b["displayName"].as_str().unwrap_or("").to_lowercase()));
    Ok(json!({ "contacts": out }))
}

/// POST /contact — create or update.
pub fn save(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct SaveBody {
        #[serde(default)]
        id: Option<i64>,
        contact: Contact,
    }
    let req: SaveBody = parse(body)?;
    let data = serde_json::to_string(&req.contact).map_err(|e| internal(e.into()))?;
    if let Some(id) = req.id {
        let item = own_item(app, user, id)?;
        if item.kind != ItemKind::Contact {
            return Err(bad("not a contact"));
        }
        app.store.update_pim(item.id, &data, WRITER).map_err(internal)?;
        return Ok(json!({ "ok": true, "id": item.id }));
    }
    let folder = app.store.folder_by_role(user.id, "contacts").map_err(internal)?.ok_or_else(not_found)?;
    let uid = uuid::Uuid::new_v4().simple().to_string();
    let id = app.store.insert_pim(user.id, folder.id, ItemKind::Contact, &data, Some(&uid), WRITER).map_err(internal)?;
    Ok(json!({ "ok": true, "id": id }))
}

/// POST /contact/delete
pub fn delete(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct DelBody {
        id: i64,
    }
    let d: DelBody = parse(body)?;
    let item = own_item(app, user, d.id)?;
    app.store.delete_item(item.id, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// GET /contacts/export.vcf — every contact as one vCard file.
pub fn export(app: &Shared, user: &User) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    let Ok(folders) = app.store.folders(user.id) else { return StatusCode::INTERNAL_SERVER_ERROR.into_response() };
    let mut out = String::new();
    for f in folders.iter().filter(|f| matches!(f.class, crate::model::FolderClass::Contacts)) {
        for item in app.store.items(f.id, 0, 5000).unwrap_or_default() {
            if let Some(c) = item.contact() {
                out.push_str(&crate::contacts::to_vcard(&c, item.uid.as_deref()));
            }
        }
    }
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/vcard; charset=utf-8"), (header::CONTENT_DISPOSITION, "attachment; filename=\"contacts.vcf\"")],
        out,
    )
        .into_response()
}

/// POST /contacts/import { vcf } — add the contacts of a vCard file. A card
/// whose e-mail address (or, without one, whose name) is already in the
/// address book is skipped.
pub fn import(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        vcf: String,
    }
    let b: B = parse(body)?;
    let cards = crate::contacts::parse_vcards(&b.vcf);
    if cards.is_empty() {
        return Err(bad("No contacts found in the file."));
    }
    let folder = app.store.folder_by_role(user.id, "contacts").map_err(internal)?.ok_or_else(not_found)?;
    let mut emails: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for item in app.store.items(folder.id, 0, 5000).map_err(internal)? {
        if let Some(c) = item.contact() {
            emails.extend(c.emails.iter().map(|e| e.to_ascii_lowercase()));
            names.push(c.effective_name().to_lowercase());
        }
    }
    let (mut added, mut skipped) = (0, 0);
    for c in cards {
        let dup = if c.emails.is_empty() { names.contains(&c.effective_name().to_lowercase()) } else { c.emails.iter().any(|e| emails.contains(&e.to_ascii_lowercase())) };
        if dup {
            skipped += 1;
            continue;
        }
        emails.extend(c.emails.iter().map(|e| e.to_ascii_lowercase()));
        names.push(c.effective_name().to_lowercase());
        let data = serde_json::to_string(&c).map_err(|e| internal(e.into()))?;
        let uid = uuid::Uuid::new_v4().simple().to_string();
        app.store.insert_pim(user.id, folder.id, ItemKind::Contact, &data, Some(&uid), WRITER).map_err(internal)?;
        added += 1;
    }
    Ok(json!({ "added": added, "skipped": skipped }))
}

/// GET /contact-groups
pub fn groups(app: &Shared, user: &User) -> ApiResult {
    Ok(json!({ "groups": app.store.contact_groups(user.id).map_err(internal)? }))
}

/// POST /contact-group { id?, name, members: [{ name, email }] }
pub fn save_group(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        #[serde(default)]
        id: Option<i64>,
        name: String,
        #[serde(default)]
        members: Vec<crate::contacts::GroupMember>,
    }
    let b: B = parse(body)?;
    let id = app.store.save_contact_group(user.id, b.id, &b.name, &b.members).map_err(|e| bad(e.to_string()))?;
    Ok(json!({ "id": id }))
}

/// POST /contact-group/delete { id }
pub fn delete_group(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: i64,
    }
    let b: B = parse(body)?;
    app.store.delete_contact_group(user.id, b.id).map_err(internal)?;
    Ok(json!({ "ok": true }))
}
