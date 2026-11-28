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
    json!({ "id": id, "folder": folder, "subject": tk.subject, "body": tk.body, "due": tk.due, "complete": tk.complete, "percentComplete": tk.percent_complete, "reminder": tk.reminder, "importance": tk.importance, "task": tk })
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
    let mut task = b.task;
    let prev = match b.id {
        Some(id) => {
            let item = own_item(app, user, id)?;
            if item.kind != ItemKind::Task {
                return Err(bad("not a task"));
            }
            Some((item.id, item.task().unwrap_or_default()))
        }
        None => None,
    };
    // Fields the server keeps track of can't be set from outside.
    let old = prev.as_ref().map(|(_, t)| t.clone()).unwrap_or_default();
    // A linked e-mail must be one of the account's own messages.
    if let Some(mid) = task.source_message
        && old.source_message != Some(mid)
        && own_item(app, user, mid).is_err()
    {
        task.source_message = None;
    }
    task.assigned_copy = old.assigned_copy;
    task.assigned_by = old.assigned_by.clone();
    task.assigned_origin = old.assigned_origin;
    if task.assigned_by.is_some() {
        // An assigned copy keeps its assigner and can't be passed on.
        task.assigned_to = None;
    }
    task.assigned_to = task.assigned_to.map(|e| e.trim().to_ascii_lowercase()).filter(|e| !e.is_empty());
    let completed_now = task.complete && !old.complete;
    if completed_now {
        task.completed_at = Some(crate::store::now());
        task.percent_complete = 100;
    } else if !task.complete {
        task.completed_at = None;
    }
    // Completing a repeating task finishes this one and starts the next.
    let next = if completed_now { next_occurrence(&task) } else { None };
    if next.is_some() {
        task.recurrence = None;
    }
    let assignee = match task.assigned_to.as_deref() {
        Some(e) if task.assigned_by.is_none() => {
            let u = app.store.resolve_address(e).map_err(internal)?.and_then(|(uid, _)| app.store.user(uid).ok().flatten()).filter(|u| u.enabled && u.kind == "user");
            let Some(u) = u else { return Err(bad("No account with that address on this server.")) };
            (u.id != user.id).then_some(u)
        }
        _ => None,
    };
    if assignee.is_none() {
        task.assigned_to = None;
    }

    let id = match prev {
        Some((id, _)) => {
            write_task(app, id, &task)?;
            id
        }
        None => {
            let folder = app.store.folder_by_role(user.id, "tasks").map_err(internal)?.ok_or_else(not_found)?;
            let uid = uuid::Uuid::new_v4().simple().to_string();
            let data = serde_json::to_string(&task).map_err(|e| internal(e.into()))?;
            app.store.insert_pim(user.id, folder.id, ItemKind::Task, &data, Some(&uid), WRITER).map_err(internal)?
        }
    };
    let actor = crate::activity::display_name(user);

    // Assignment: keep a copy in the assignee's task list in step.
    if task.assigned_by.is_none() {
        let copy = task.assigned_copy.and_then(|c| app.store.item(c).ok().flatten()).filter(|i| i.kind == ItemKind::Task);
        match (&assignee, copy) {
            // Same person: refresh their copy, keeping their progress.
            (Some(a), Some(item)) if item.user_id == a.id => {
                if let Some(mut c) = item.task() {
                    sync_copy(&mut c, &task);
                    write_task(app, item.id, &c)?;
                }
            }
            (_, copy) => {
                if let Some(item) = copy {
                    let _ = app.store.delete_item(item.id, WRITER);
                }
                let had = task.assigned_copy.take().is_some();
                if let Some(a) = &assignee {
                    let folder = app.store.folder_by_role(a.id, "tasks").map_err(internal)?.ok_or_else(|| bad("That account has no task list."))?;
                    let mut c = Task { assigned_by: Some(user.email.clone()), assigned_origin: Some(id), ..Default::default() };
                    sync_copy(&mut c, &task);
                    let uid = uuid::Uuid::new_v4().simple().to_string();
                    let data = serde_json::to_string(&c).map_err(|e| internal(e.into()))?;
                    task.assigned_copy = Some(app.store.insert_pim(a.id, folder.id, ItemKind::Task, &data, Some(&uid), WRITER).map_err(internal)?);
                    let _ = app.store.add_activity(a.id, (user.id, &actor), "task_assigned", &task.subject, "", "/tasks");
                }
                if had || task.assigned_copy.is_some() {
                    write_task(app, id, &task)?;
                }
            }
        }
    }
    // Progress on an assigned copy flows back to the assigner.
    if let Some(origin) = task.assigned_origin
        && let Some(item) = app.store.item(origin).map_err(internal)?.filter(|i| i.kind == ItemKind::Task)
        && let Some(mut orig) = item.task()
        && orig.assigned_copy == Some(id)
        && (orig.complete != task.complete || orig.percent_complete != task.percent_complete || orig.steps != task.steps)
    {
        orig.complete = task.complete;
        orig.percent_complete = task.percent_complete;
        orig.completed_at = task.completed_at;
        orig.steps = task.steps.clone();
        write_task(app, origin, &orig)?;
        if completed_now {
            let _ = app.store.add_activity(item.user_id, (user.id, &actor), "task_completed", &task.subject, "", "/tasks");
        }
    }
    if let Some(n) = next {
        let folder = app.store.folder_by_role(user.id, "tasks").map_err(internal)?.ok_or_else(not_found)?;
        let uid = uuid::Uuid::new_v4().simple().to_string();
        let data = serde_json::to_string(&n).map_err(|e| internal(e.into()))?;
        app.store.insert_pim(user.id, folder.id, ItemKind::Task, &data, Some(&uid), WRITER).map_err(internal)?;
    }
    Ok(json!({ "ok": true, "id": id }))
}

fn write_task(app: &Shared, id: i64, task: &Task) -> Result<(), (axum::http::StatusCode, String)> {
    let data = serde_json::to_string(task).map_err(|e| internal(e.into()))?;
    app.store.update_pim(id, &data, WRITER).map_err(internal)
}

/// Copy what the assigner decides (what, when, how important) onto the
/// assignee's copy; progress stays the assignee's.
fn sync_copy(copy: &mut Task, from: &Task) {
    copy.subject = from.subject.clone();
    copy.body = from.body.clone();
    copy.start = from.start;
    copy.due = from.due;
    copy.importance = from.importance;
    if copy.steps.len() != from.steps.len() {
        copy.steps = from.steps.clone();
    } else {
        for (c, f) in copy.steps.iter_mut().zip(&from.steps) {
            c.text = f.text.clone();
        }
    }
}

/// The next task of a repeating series, or `None` once it has ended. Its due
/// date (and start and reminder) move by the same amount.
fn next_occurrence(t: &Task) -> Option<Task> {
    let r = t.recurrence.clone()?;
    let due = t.due?;
    if r.count.is_some_and(|c| c <= 1) {
        return None;
    }
    // Due dates are the local midnight of the person's day. Twelve hours later
    // is still that day in UTC for any time zone, so the pattern (weekday,
    // day of month) is evaluated on the right date.
    let anchor = due + 12 * 3600;
    let ev = crate::model::Event { start: anchor, end: anchor, tz: "UTC".into(), recurrence: Some(r.clone()), ..Default::default() };
    let next = ev.occurrences(anchor + 1, anchor + 5 * 366 * 86_400).into_iter().map(|o| o.start).find(|s| *s > anchor)?;
    let shift = next - anchor;
    let mut n = t.clone();
    n.due = Some(due + shift);
    n.start = t.start.map(|s| s + shift);
    n.reminder = t.reminder.map(|s| s + shift);
    n.complete = false;
    n.completed_at = None;
    n.percent_complete = 0;
    n.steps.iter_mut().for_each(|s| s.done = false);
    n.assigned_copy = None;
    n.assigned_to = None;
    let mut r = r;
    r.count = r.count.map(|c| c - 1);
    n.recurrence = Some(r);
    Some(n)
}

pub fn delete_task(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    // Deleting an assigned task removes the assignee's copy too.
    #[derive(Deserialize)]
    struct D {
        id: i64,
    }
    if let Ok(d) = serde_json::from_slice::<D>(body)
        && let Ok(item) = own_item(app, user, d.id)
        && let Some(cid) = item.task().and_then(|t| t.assigned_copy)
        && app.store.item(cid).ok().flatten().is_some_and(|c| c.kind == ItemKind::Task)
    {
        let _ = app.store.delete_item(cid, WRITER);
    }
    delete_pim(app, user, body)
}

// ----------------------------------------------------------------- notes

pub fn notes(app: &Shared, user: &User) -> ApiResult {
    let folders = app.store.folders(user.id).map_err(internal)?;
    let mut out = Vec::new();
    for f in folders.iter().filter(|f| matches!(f.class, FolderClass::Notes)) {
        for item in app.store.items(f.id, 0, 5000).map_err(internal)? {
            if let Some(n) = item.note() {
                out.push(json!({ "id": item.id, "folder": f.id, "subject": n.subject, "body": n.body, "notebook": n.notebook, "pinned": n.pinned, "date": item.received, "note": n }));
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

/// POST /note/to-document { id } — turn a note into a collaborative document
/// in the personal drive (to share it and edit it together); the note is
/// removed. Returns the document id.
pub fn note_to_document(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct B {
        id: i64,
    }
    if !app.cfg.cloud.enabled || !app.collab_enabled() {
        return Err(not_found());
    }
    let b: B = parse(body)?;
    let item = own_item(app, user, b.id)?;
    let note = item.note().filter(|_| item.kind == ItemKind::Note).ok_or_else(|| bad("not a note"))?;
    let drive = app.store.ensure_personal_drive(user.id).map_err(internal)?;
    let name = if note.subject.trim().is_empty() { "Note".to_string() } else { note.subject.trim().to_string() };
    let (_, doc) = app.store.create_drive_doc(drive, None, &name, user.id).map_err(|e| bad(e.to_string()))?;
    if !note.body.is_empty() {
        app.store.apply_op(doc, user.id, 0, &json!([note.body])).map_err(internal)?;
    }
    app.store.delete_item(item.id, WRITER).map_err(internal)?;
    Ok(json!({ "doc": doc }))
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
        #[serde(default)]
        categories: Vec<String>,
    }
    let b: B = parse(body)?;
    let target = match (b.action.as_str(), b.folder) {
        ("move", Some(f)) => Some(app.store.folder(f).map_err(internal)?.filter(|x| x.user_id == user.id).ok_or_else(not_found)?.id),
        ("move", None) => return Err(bad("folder required")),
        ("archive", _) => Some(app.store.folder_by_role(user.id, "archive").map_err(internal)?.ok_or_else(|| bad("no archive folder"))?.id),
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
            "move" | "archive" => app.store.move_item(item.id, target.unwrap(), WRITER),
            "categorize" => {
                // Additive: union the requested categories onto the item.
                let mut cats = item.categories.clone();
                for c in &b.categories {
                    if !cats.iter().any(|x| x == c) {
                        cats.push(c.clone());
                    }
                }
                app.store.update_flags(item.id, 0, 0, Some(&cats), WRITER)
            }
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
