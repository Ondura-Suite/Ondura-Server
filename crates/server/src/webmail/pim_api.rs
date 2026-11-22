//! Tasks, notes, folder management and bulk message actions.

use axum::body::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{bad, internal, not_found, own_item, parse, ApiResult, WRITER};
use crate::app::Shared;
use crate::model::{flags, FolderClass, ItemKind, Note, Task};
use crate::store::User;

// ----------------------------------------------------------------- tasks

fn task_json(id: i64, folder: i64, tk: &Task) -> Value {
    json!({ "id": id, "folder": folder, "subject": tk.subject, "body": tk.body, "due": tk.due, "complete": tk.complete, "percentComplete": tk.percent_complete, "task": tk })
}

pub fn tasks(app: &Shared, user: &User) -> ApiResult {
    let folders = app.store.folders(user.id).map_err(internal)?;
    let mut out = Vec::new();
    for f in folders.iter().filter(|f| matches!(f.class, FolderClass::Tasks)) {
        for item in app.store.items(f.id, 0, 5000).map_err(internal)? {
            if let Some(tk) = item.task() {
                out.push(task_json(item.id, f.id, &tk));
            }
        }
    }
    out.sort_by_key(|v| (v["complete"].as_bool().unwrap_or(false), v["due"].as_i64().unwrap_or(i64::MAX)));
    Ok(json!({ "tasks": out }))
}

pub fn save_task(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        #[serde(default)]
        id: Option<i64>,
        task: Task,
    }
    let b: B = parse(body)?;
    let data = serde_json::to_string(&b.task).map_err(|e| internal(e.into()))?;
    if let Some(id) = b.id {
        let item = own_item(app, user, id)?;
        if item.kind != ItemKind::Task {
            return Err(bad("not a task"));
        }
        app.store.update_pim(item.id, &data, WRITER).map_err(internal)?;
        return Ok(json!({ "ok": true, "id": item.id }));
    }
    let folder = app.store.folder_by_role(user.id, "tasks").map_err(internal)?.ok_or_else(not_found)?;
    let uid = uuid::Uuid::new_v4().simple().to_string();
    let id = app.store.insert_pim(user.id, folder.id, ItemKind::Task, &data, Some(&uid), WRITER).map_err(internal)?;
    Ok(json!({ "ok": true, "id": id }))
}

pub fn delete_task(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    delete_pim(app, user, body)
}

// ----------------------------------------------------------------- notes

pub fn notes(app: &Shared, user: &User) -> ApiResult {
    let folders = app.store.folders(user.id).map_err(internal)?;
    let mut out = Vec::new();
    for f in folders.iter().filter(|f| matches!(f.class, FolderClass::Notes)) {
        for item in app.store.items(f.id, 0, 5000).map_err(internal)? {
            if let Some(n) = item.note() {
                out.push(json!({ "id": item.id, "folder": f.id, "subject": n.subject, "body": n.body, "date": item.received }));
            }
        }
    }
    out.sort_by_key(|v| std::cmp::Reverse(v["date"].as_i64().unwrap_or(0)));
    Ok(json!({ "notes": out }))
}

pub fn save_note(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        #[serde(default)]
        id: Option<i64>,
        note: Note,
    }
    let b: B = parse(body)?;
    let data = serde_json::to_string(&b.note).map_err(|e| internal(e.into()))?;
    if let Some(id) = b.id {
        let item = own_item(app, user, id)?;
        if item.kind != ItemKind::Note {
            return Err(bad("not a note"));
        }
        app.store.update_pim(item.id, &data, WRITER).map_err(internal)?;
        return Ok(json!({ "ok": true, "id": item.id }));
    }
    let folder = app.store.folder_by_role(user.id, "notes").map_err(internal)?.ok_or_else(not_found)?;
    let uid = uuid::Uuid::new_v4().simple().to_string();
    let id = app.store.insert_pim(user.id, folder.id, ItemKind::Note, &data, Some(&uid), WRITER).map_err(internal)?;
    Ok(json!({ "ok": true, "id": id }))
}

pub fn delete_note(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    delete_pim(app, user, body)
}

fn delete_pim(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: i64,
    }
    let b: B = parse(body)?;
    let item = own_item(app, user, b.id)?;
    app.store.delete_item(item.id, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

// ----------------------------------------------------------------- folders

/// POST /folder — create a mail folder. Body: { name, parent? }
pub fn create_folder(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        name: String,
        #[serde(default)]
        parent: Option<i64>,
    }
    let b: B = parse(body)?;
    let name = b.name.trim();
    if name.is_empty() {
        return Err(bad("name required"));
    }
    let parent = match b.parent {
        Some(p) => Some(app.store.folder(p).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?.id),
        None => app.store.folder_by_role(user.id, "msgfolderroot").map_err(internal)?.map(|f| f.id),
    };
    let f = app.store.create_folder(user.id, parent, name, FolderClass::Mail, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true, "id": f.id }))
}

/// POST /folder/rename — { id, name }
pub fn rename_folder(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: i64,
        name: String,
    }
    let b: B = parse(body)?;
    let folder = app.store.folder(b.id).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?;
    if folder.role.is_some() {
        return Err(bad("system folders cannot be renamed"));
    }
    app.store.update_folder(folder.id, Some(b.name.trim()), None).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// POST /folder/delete — { id }
pub fn delete_folder(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: i64,
    }
    let b: B = parse(body)?;
    let folder = app.store.folder(b.id).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?;
    if folder.role.is_some() {
        return Err(bad("system folders cannot be deleted"));
    }
    app.store.delete_folder(folder.id, WRITER).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

// ----------------------------------------------------------------- bulk

/// POST /bulk — act on several messages at once.
/// Body: { ids: [..], action: "read"|"unread"|"flag"|"unflag"|"delete"|"move", folder? }
pub fn bulk(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        ids: Vec<i64>,
        action: String,
        #[serde(default)]
        folder: Option<i64>,
    }
    let b: B = parse(body)?;
    let target = match (b.action.as_str(), b.folder) {
        ("move", Some(f)) => Some(app.store.folder(f).map_err(internal)?.filter(|x| x.user_id == user.id).ok_or_else(not_found)?.id),
        ("move", None) => return Err(bad("folder required")),
        _ => None,
    };
    let trash = app.store.folder_by_role(user.id, "deleteditems").map_err(internal)?;
    let mut done = 0;
    for id in b.ids {
        let Ok(item) = own_item(app, user, id) else { continue };
        let r = match b.action.as_str() {
            "read" => app.store.update_flags(item.id, flags::READ, 0, None, WRITER),
            "unread" => app.store.update_flags(item.id, 0, flags::READ, None, WRITER),
            "flag" => app.store.update_flags(item.id, flags::FLAGGED, 0, None, WRITER),
            "unflag" => app.store.update_flags(item.id, 0, flags::FLAGGED, None, WRITER),
            "move" => app.store.move_item(item.id, target.unwrap(), WRITER),
            "delete" => match &trash {
                Some(tr) if item.folder_id != tr.id => app.store.move_item(item.id, tr.id, WRITER),
                _ => app.store.delete_item(item.id, WRITER),
            },
            _ => return Err(bad("invalid action")),
        };
        if r.is_ok() {
            done += 1;
        }
    }
    Ok(json!({ "ok": true, "count": done }))
}

/// POST /folder/read — mark every message in a folder read. { folder }
pub fn mark_folder_read(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        folder: i64,
    }
    let b: B = parse(body)?;
    let folder = app.store.folder(b.folder).map_err(internal)?.filter(|f| f.user_id == user.id).ok_or_else(not_found)?;
    for item in app.store.items(folder.id, 0, 100000).map_err(internal)? {
        if item.flags & flags::READ == 0 {
            let _ = app.store.update_flags(item.id, flags::READ, 0, None, WRITER);
        }
    }
    Ok(json!({ "ok": true }))
}
