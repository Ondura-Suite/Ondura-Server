//! Calendar endpoints: list a date range, create/update/delete, respond.

use axum::body::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{bad, internal, not_found, own_item, parse, ApiResult, WRITER};
use crate::app::Shared;
use crate::model::{Event, ItemKind, Person, Response as MeetingResponse};
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
pub async fn save(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct SaveBody {
        #[serde(default)]
        id: Option<i64>,
        /// Send meeting invitations to attendees (default true).
        #[serde(default = "crate::serde_true")]
        send: bool,
        event: Event,
    }
    let mut req: SaveBody = parse(body)?;
    // On an update, keep what identifies the event: its UID, its organizer and
    // the meeting sequence (raised so attendees' clients accept the change).
    let existing = match req.id {
        Some(id) => {
            let item = own_item(app, user, id)?;
            if item.kind != ItemKind::Event {
                return Err(bad("not a calendar item"));
            }
            Some((item.id, item.event().unwrap_or_default()))
        }
        None => None,
    };
    if let Some((_, old)) = &existing {
        if req.event.uid.is_empty() || req.event.uid != old.uid {
            req.event.uid = old.uid.clone();
        }
        if req.event.organizer.is_none() {
            req.event.organizer = old.organizer.clone();
        }
        // Changing a series' time or pattern resets its changed occurrences,
        // since their original start times no longer line up (as Outlook does).
        if old.recurrence.is_some() && (old.recurrence != req.event.recurrence || old.start != req.event.start || old.end != req.event.end) {
            req.event.exceptions.clear();
        }
        req.event.sequence = old.sequence.max(req.event.sequence);
        if !req.event.attendees.is_empty() || !old.attendees.is_empty() {
            req.event.sequence += 1;
        }
    }
    if req.event.uid.is_empty() {
        req.event.uid = uuid::Uuid::new_v4().simple().to_string();
    }
    // A meeting we organize carries us as the organizer so invitations are valid.
    if !req.event.attendees.is_empty() && req.event.organizer.is_none() {
        req.event.organizer = Some(Person::new(&user.name, &user.email));
    }
    req.event.dtstamp = crate::store::now();
    let data = serde_json::to_string(&req.event).map_err(|e| internal(e.into()))?;

    // Attendees taken off a meeting we organize get a cancellation.
    let removed: Vec<Person> = match &existing {
        Some((_, old)) if crate::calendar::user_is_organizer(app, user, old) => old
            .attendees
            .iter()
            .filter(|a| !req.event.attendees.iter().any(|n| n.person.email.eq_ignore_ascii_case(&a.person.email)))
            .map(|a| a.person.clone())
            .collect(),
        _ => Vec::new(),
    };
    let id = if let Some((id, _)) = existing {
        app.store.update_pim(id, &data, WRITER).map_err(internal)?;
        id
    } else {
        let folder = app.store.folder_by_role(user.id, "calendar").map_err(internal)?.ok_or_else(not_found)?;
        app.store.insert_pim(user.id, folder.id, ItemKind::Event, &data, Some(&req.event.uid), WRITER).map_err(internal)?
    };
    if req.send
        && !removed.is_empty()
        && let Err(e) = crate::calendar::send_invitations(app, user, &req.event, "CANCEL", Some(&removed), None).await
    {
        tracing::warn!("meeting cancellation failed: {e:#}");
    }

    // Send a meeting request to the attendees of a meeting we organize.
    if req.send
        && !req.event.attendees.is_empty()
        && crate::calendar::user_is_organizer(app, user, &req.event)
        && let Err(e) = crate::calendar::send_invitations(app, user, &req.event, "REQUEST", None, None).await
    {
        tracing::warn!("meeting request failed: {e:#}");
    }
    Ok(json!({ "ok": true, "id": id }))
}

/// GET /calendar/freebusy?emails=a,b&from=&to= — when each person is busy, for
/// scheduling. Accounts on this server report their busy blocks (free/busy is
/// visible across the organisation, as in Exchange); subjects are included only
/// where the caller may see that calendar's details. Other addresses are
/// reported as unknown.
pub fn freebusy(app: &Shared, user: &User, query: &str) -> ApiResult {
    let param = |k: &str| query.split('&').filter_map(|p| p.split_once('=')).find(|(x, _)| *x == k).map(|(_, v)| v.to_string());
    let num = |k: &str| param(k).and_then(|v| v.parse::<i64>().ok());
    let from = num("from").ok_or_else(|| bad("from required"))?;
    let to = num("to").ok_or_else(|| bad("to required"))?;
    if to <= from || to - from > 62 * 86_400 {
        return Err(bad("invalid range"));
    }
    let emails = param("emails").map(|v| super::mail_api::decode(&v)).unwrap_or_default();
    let mut out = Vec::new();
    for email in emails.split(',').map(str::trim).filter(|e| !e.is_empty()).take(50) {
        let owner = app.store.resolve_address(email).ok().flatten().and_then(|(id, _)| app.store.user(id).ok().flatten());
        let Some(owner) = owner.filter(|u| u.enabled && u.mailbox) else {
            out.push(json!({ "email": email, "known": false }));
            continue;
        };
        let details = crate::calendar::can_see_details(app, user.id, owner.id);
        let busy = crate::calendar::busy_times(app, owner.id, from, to).map_err(internal)?;
        out.push(json!({
            "email": email,
            "known": true,
            "name": owner.name,
            "busy": busy.iter().map(|b| json!({
                "start": b.start, "end": b.end,
                "status": format!("{:?}", b.status).to_ascii_lowercase(),
                "subject": if details && !b.private { Some(b.subject.clone()) } else { None },
                "id": if details { Some(b.item_id) } else { None },
            })).collect::<Vec<_>>(),
        }));
    }
    Ok(json!({ "people": out }))
}

/// GET /event?id= — the full stored event, so an editor can change it without
/// losing fields it does not show.
pub fn get(app: &Shared, user: &User, query: &str) -> ApiResult {
    let id: i64 = query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == "id").and_then(|(_, v)| v.parse().ok()).ok_or_else(|| bad("id required"))?;
    let item = own_item(app, user, id)?;
    let e = item.event().ok_or_else(|| bad("not a calendar item"))?;
    Ok(json!({
        "id": item.id,
        "folder": item.folder_id,
        "isOrganizer": e.attendees.is_empty() || crate::calendar::user_is_organizer(app, user, &e),
        "event": e,
    }))
}

/// POST /event/occurrence — change or delete one occurrence of a series. The
/// change is stored as an exception of the series; for a meeting we organize
/// the attendees get an update (or a cancellation) for that occurrence.
pub async fn occurrence(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct OccBody {
        id: i64,
        /// Original start of the occurrence, as given by the pattern.
        occurrence: i64,
        #[serde(default)]
        delete: bool,
        subject: Option<String>,
        location: Option<String>,
        body: Option<String>,
        start: Option<i64>,
        end: Option<i64>,
        all_day: Option<bool>,
        busy: Option<crate::model::BusyStatus>,
        #[serde(default = "crate::serde_true")]
        send: bool,
    }
    let b: OccBody = parse(body)?;
    let item = own_item(app, user, b.id)?;
    let mut ev = item.event().ok_or_else(|| bad("not a calendar item"))?;
    if ev.recurrence.is_none() {
        return Err(bad("not a recurring event"));
    }
    // The occurrence has to be one the pattern produces.
    let mut pattern = ev.clone();
    pattern.exceptions.clear();
    if !pattern.occurrences(b.occurrence - 1, b.occurrence + 1).iter().any(|o| o.original_start == b.occurrence) {
        return Err(bad("no such occurrence"));
    }
    if let (Some(s), Some(e)) = (b.start, b.end)
        && e < s
    {
        return Err(bad("end before start"));
    }
    let mut ex = ev.exceptions.iter().find(|x| x.original_start == b.occurrence).cloned().unwrap_or(crate::model::Exception { original_start: b.occurrence, ..Default::default() });
    if b.delete {
        ex.deleted = true;
    } else {
        if b.subject.is_some() {
            ex.subject = b.subject;
        }
        if b.location.is_some() {
            ex.location = b.location;
        }
        if b.body.is_some() {
            ex.body = b.body;
        }
        if b.start.is_some() {
            ex.start = b.start;
        }
        if b.end.is_some() {
            ex.end = b.end;
        }
        if b.all_day.is_some() {
            ex.all_day = b.all_day;
        }
        if b.busy.is_some() {
            ex.busy = b.busy;
        }
    }
    ev.exceptions.retain(|x| x.original_start != b.occurrence);
    ev.exceptions.push(ex);
    ev.exceptions.sort_by_key(|x| x.original_start);
    let organizer = !ev.attendees.is_empty() && crate::calendar::user_is_organizer(app, user, &ev);
    if organizer {
        ev.sequence += 1;
    }
    ev.dtstamp = crate::store::now();
    let data = serde_json::to_string(&ev).map_err(|e| internal(e.into()))?;
    app.store.update_pim(item.id, &data, WRITER).map_err(internal)?;
    if b.send
        && organizer
        && let Err(e) = crate::calendar::send_invitations(app, user, &ev, if b.delete { "CANCEL" } else { "REQUEST" }, None, Some(b.occurrence)).await
    {
        tracing::warn!("occurrence update failed: {e:#}");
    }
    Ok(json!({ "ok": true }))
}

/// POST /event/delete
pub async fn delete(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct DelBody {
        id: i64,
        #[serde(default = "crate::serde_true")]
        send: bool,
    }
    let d: DelBody = parse(body)?;
    let item = own_item(app, user, d.id)?;
    // Cancel the meeting for its attendees when we organize it.
    if d.send
        && let Some(data) = &item.data
        && let Ok(ev) = serde_json::from_str::<Event>(data)
        && !ev.attendees.is_empty()
        && crate::calendar::user_is_organizer(app, user, &ev)
        && let Err(e) = crate::calendar::send_invitations(app, user, &ev, "CANCEL", None, None).await
    {
        tracing::warn!("meeting cancellation failed: {e:#}");
    }
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
        #[serde(default = "crate::serde_true")]
        send: bool,
        #[serde(default)]
        comment: String,
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
