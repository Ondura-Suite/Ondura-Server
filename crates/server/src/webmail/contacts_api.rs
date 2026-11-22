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
