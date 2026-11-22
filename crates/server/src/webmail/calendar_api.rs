//! Calendar endpoints: list a date range, create/update/delete, respond.

use axum::body::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{bad, internal, not_found, own_item, parse, ApiResult, WRITER};
use crate::app::Shared;
use crate::model::{Event, ItemKind, Response as MeetingResponse};
use crate::store::User;

fn calendar_folders(app: &Shared, user: &User) -> Result<Vec<i64>, (axum::http::StatusCode, String)> {
    let folders = app.store.folders(user.id).map_err(internal)?;
    Ok(folders.iter().filter(|f| matches!(f.class, crate::model::FolderClass::Calendar)).map(|f| f.id).collect())
}

/// `times` is (start, end, original_start) in UTC seconds.
fn occurrence_json(item_id: i64, folder: i64, e: &Event, times: (i64, i64, i64), is_recurring: bool, ex: Option<&crate::model::Exception>) -> Value {
    let (start, end, original) = times;
    let subject = ex.and_then(|x| x.subject.clone()).unwrap_or_else(|| e.subject.clone());
    let location = ex.and_then(|x| x.location.clone()).unwrap_or_else(|| e.location.clone());
    let all_day = ex.and_then(|x| x.all_day).unwrap_or(e.all_day);
    json!({
        "id": item_id,
        "folder": folder,
        "uid": e.uid,
        "subject": subject,
        "location": location,
        "start": start,
        "end": end,
        "allDay": all_day,
        "tz": e.tz,
        "isRecurring": is_recurring,
        "occurrenceStart": original,
        "busy": format!("{:?}", e.busy).to_ascii_lowercase(),
        "organizer": e.organizer.as_ref().map(|p| json!({ "name": p.name, "email": p.email })),
        "hasAttendees": !e.attendees.is_empty(),
        "response": format!("{:?}", e.response).to_ascii_lowercase(),
        "cancelled": e.cancelled,
    })
}

/// GET /calendar?from=<ts>&to=<ts>
pub fn list(app: &Shared, user: &User, query: &str) -> ApiResult {
    let get = |k: &str| query.split('&').filter_map(|p| p.split_once('=')).find(|(x, _)| *x == k).and_then(|(_, v)| v.parse::<i64>().ok());
    let from = get("from").ok_or_else(|| bad("from required"))?;
    let to = get("to").ok_or_else(|| bad("to required"))?;
    if to <= from {
        return Err(bad("to must be after from"));
    }
    let mut out = Vec::new();
    for folder in calendar_folders(app, user)? {
        for item in app.store.events_in_range(folder, from, to).map_err(internal)? {
            let Some(e) = item.event() else { continue };
            let recurring = e.recurrence.is_some();
            if recurring {
                for occ in e.occurrences(from, to) {
                    out.push(occurrence_json(item.id, folder, &e, (occ.start, occ.end, occ.original_start), true, occ.exception.as_ref()));
                }
            } else {
                out.push(occurrence_json(item.id, folder, &e, (e.start, e.end, e.start), false, None));
            }
        }
    }
    Ok(json!({ "events": out }))
}

/// POST /event — create or update an event. Body: the Event plus optional id.
pub fn save(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct SaveBody {
        #[serde(default)]
        id: Option<i64>,
        event: Event,
    }
    let mut req: SaveBody = parse(body)?;
    if req.event.uid.is_empty() {
        req.event.uid = uuid::Uuid::new_v4().simple().to_string();
    }
    let data = serde_json::to_string(&req.event).map_err(|e| internal(e.into()))?;

    if let Some(id) = req.id {
        let item = own_item(app, user, id)?;
        if item.kind != ItemKind::Event {
            return Err(bad("not a calendar item"));
        }
        app.store.update_pim(item.id, &data, WRITER).map_err(internal)?;
        return Ok(json!({ "ok": true, "id": item.id }));
    }

    let folder = app.store.folder_by_role(user.id, "calendar").map_err(internal)?.ok_or_else(not_found)?;
    let id = app.store.insert_pim(user.id, folder.id, ItemKind::Event, &data, Some(&req.event.uid), WRITER).map_err(internal)?;
    Ok(json!({ "ok": true, "id": id }))
}

/// POST /event/delete
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

/// POST /event/respond — accept/decline/tentative a meeting invitation.
pub async fn respond(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RespondBody {
        id: i64,
        /// "accepted" | "declined" | "tentative"
        response: String,
        #[serde(default)]
        occurrence: Option<i64>,
        #[serde(default = "yes")]
        send: bool,
        #[serde(default)]
        comment: String,
    }
    fn yes() -> bool {
        true
    }
    let r: RespondBody = parse(body)?;
    let response = match r.response.as_str() {
        "accepted" | "accept" => MeetingResponse::Accepted,
        "declined" | "decline" => MeetingResponse::Declined,
        "tentative" => MeetingResponse::Tentative,
        _ => return Err(bad("invalid response")),
    };
    // Ownership check before acting.
    own_item(app, user, r.id)?;
    crate::calendar::respond(app, user, r.id, response, r.occurrence, r.send, &r.comment).await.map_err(internal)?;
    Ok(json!({ "ok": true }))
}
