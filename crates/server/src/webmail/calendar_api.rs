//! Calendar endpoints: list a date range, create/update/delete, respond.

use axum::body::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{bad, internal, not_found, own_item, parse, ApiResult, WRITER};
use crate::app::Shared;
use crate::calendar::CalendarAccess;
use crate::model::{Event, ItemKind, Person, Response as MeetingResponse};
use crate::store::User;

fn calendar_folders(app: &Shared, owner: i64) -> Result<Vec<i64>, (axum::http::StatusCode, String)> {
    let folders = app.store.folders(owner).map_err(internal)?;
    Ok(folders.iter().filter(|f| matches!(f.class, crate::model::FolderClass::Calendar)).map(|f| f.id).collect())
}

/// The calendar owner a request is about: `owner=<id>` or the caller, with the
/// caller's access to that calendar. Only enabled accounts with a mailbox
/// have a calendar.
fn calendar_owner(app: &Shared, user: &User, owner: Option<i64>) -> Result<(i64, CalendarAccess), (axum::http::StatusCode, String)> {
    let owner = owner.unwrap_or(user.id);
    if owner != user.id {
        app.store.user(owner).map_err(internal)?.filter(|u| u.enabled && u.mailbox).ok_or_else(not_found)?;
    }
    Ok((owner, crate::calendar::calendar_access(app, user.id, owner)))
}

/// A calendar item the caller may read (own, or in a calendar shared at least
/// for reading; another person's private events need write access).
fn readable_event(app: &Shared, user: &User, id: i64) -> Result<(crate::store::Item, Event, CalendarAccess), (axum::http::StatusCode, String)> {
    let item = app.store.item(id).map_err(internal)?.filter(|i| i.kind == ItemKind::Event).ok_or_else(not_found)?;
    let e = item.event().ok_or_else(|| bad("not a calendar item"))?;
    let access = crate::calendar::calendar_access(app, user.id, item.user_id);
    let private = matches!(e.sensitivity, crate::model::Sensitivity::Private | crate::model::Sensitivity::Confidential);
    if access < CalendarAccess::Read || (private && access < CalendarAccess::Write) {
        return Err(not_found());
    }
    Ok((item, e, access))
}

/// Whether the caller may change this event: always in their own calendar; in
/// another person's calendar with write access, except meetings, whose mail
/// would go out in the owner's name.
fn may_edit(user: &User, item: &crate::store::Item, e: &Event, access: CalendarAccess) -> bool {
    item.user_id == user.id || (access == CalendarAccess::Write && e.attendees.is_empty())
}

/// A calendar item the caller may change.
fn writable_event(app: &Shared, user: &User, id: i64) -> Result<(crate::store::Item, Event), (axum::http::StatusCode, String)> {
    let (item, e, access) = readable_event(app, user, id)?;
    if !may_edit(user, &item, &e, access) {
        return Err((axum::http::StatusCode::FORBIDDEN, "You can't change this event.".into()));
    }
    Ok((item, e))
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

/// GET /calendar?from=<ts>&to=<ts>[&owner=<id>] — the caller's calendar, or a
/// calendar shared with them. With free/busy access only the busy blocks are
/// returned, and private events show as blocks unless the caller may edit.
pub fn list(app: &Shared, user: &User, query: &str) -> ApiResult {
    let get = |k: &str| query.split('&').filter_map(|p| p.split_once('=')).find(|(x, _)| *x == k).and_then(|(_, v)| v.parse::<i64>().ok());
    let from = get("from").ok_or_else(|| bad("from required"))?;
    let to = get("to").ok_or_else(|| bad("to required"))?;
    if to <= from {
        return Err(bad("to must be after from"));
    }
    let (owner, access) = calendar_owner(app, user, get("owner"))?;
    let mut out = Vec::new();
    for folder in calendar_folders(app, owner)? {
        for item in app.store.events_in_range(folder, from, to).map_err(internal)? {
            let Some(e) = item.event() else { continue };
            let private = matches!(e.sensitivity, crate::model::Sensitivity::Private | crate::model::Sensitivity::Confidential);
            let blocks_only = access < CalendarAccess::Read || (private && access < CalendarAccess::Write);
            if blocks_only && (e.cancelled || e.busy == crate::model::BusyStatus::Free) {
                continue;
            }
            let editable = may_edit(user, &item, &e, access);
            let recurring = e.recurrence.is_some();
            let occurrences: Vec<_> = if recurring {
                e.occurrences(from, to).into_iter().map(|o| (o.start, o.end, o.original_start, o.exception)).collect()
            } else {
                vec![(e.start, e.end, e.start, None)]
            };
            for (start, end, original, ex) in occurrences {
                let mut v = occurrence_json(item.id, folder, &e, (start, end, original), recurring, ex.as_ref());
                v["owner"] = json!(owner);
                v["canEdit"] = json!(editable);
                if blocks_only {
                    // Only that the time is taken, not what for.
                    for k in ["subject", "location", "organizer", "uid"] {
                        v[k] = Value::Null;
                    }
                    v["blocksOnly"] = json!(true);
                    v["id"] = Value::Null;
                }
                out.push(v);
            }
        }
    }
    Ok(json!({ "events": out, "owner": owner, "access": access.as_str() }))
}

/// POST /event — create or update an event. Body: the Event plus optional id.
pub async fn save(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct SaveBody {
        #[serde(default)]
        id: Option<i64>,
        /// Calendar owner for a new event (default: the caller).
        #[serde(default)]
        owner: Option<i64>,
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
            let (item, e) = writable_event(app, user, id)?;
            if item.user_id != user.id && !req.event.attendees.is_empty() {
                return Err(bad("Meetings can only be planned in your own calendar."));
            }
            Some((item.id, e))
        }
        None => None,
    };
    // A new event goes into the caller's calendar, or into a calendar shared
    // with them for writing.
    let target_owner = match (&existing, req.owner) {
        (None, Some(owner)) if owner != user.id => {
            let (owner, access) = calendar_owner(app, user, Some(owner))?;
            if access < CalendarAccess::Write {
                return Err((axum::http::StatusCode::FORBIDDEN, "You can't add events to this calendar.".into()));
            }
            if !req.event.attendees.is_empty() {
                return Err(bad("Meetings can only be planned in your own calendar."));
            }
            owner
        }
        _ => user.id,
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
        let folder = app.store.folder_by_role(target_owner, "calendar").map_err(internal)?.ok_or_else(not_found)?;
        app.store.insert_pim(target_owner, folder.id, ItemKind::Event, &data, Some(&req.event.uid), WRITER).map_err(internal)?
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

/// GET /calendar/shares — who the caller shares their calendar with, and the
/// calendars the caller can open (shared with them, or mailboxes they have full
/// access to).
pub fn shares(app: &Shared, user: &User) -> ApiResult {
    let person = |id: i64| app.store.user(id).ok().flatten().filter(|u| u.enabled && u.mailbox);
    let given: Vec<Value> = app
        .store
        .calendar_shares_of(user.id)
        .map_err(internal)?
        .into_iter()
        .filter_map(|(id, level)| person(id).map(|u| json!({ "id": u.id, "email": u.email, "name": u.name, "level": level })))
        .collect();
    let mut owners: Vec<i64> = app.store.calendars_shared_with(user.id).map_err(internal)?.into_iter().map(|(id, _)| id).collect();
    owners.extend(app.store.full_access_owners(user.id).map_err(internal)?);
    owners.sort_unstable();
    owners.dedup();
    let received: Vec<Value> = owners
        .into_iter()
        .filter(|id| *id != user.id)
        .filter_map(|id| person(id).map(|u| json!({ "id": u.id, "email": u.email, "name": u.name, "access": crate::calendar::calendar_access(app, user.id, u.id).as_str() })))
        .collect();
    Ok(json!({ "given": given, "received": received }))
}

/// POST /calendar/share { email, level } — share the caller's calendar with an
/// account on this server at `freebusy`, `read` or `write`, or stop sharing it
/// with `none`.
pub fn share(app: &Shared, user: &User, body: &Bytes) -> ApiResult {
    #[derive(Deserialize)]
    struct ShareBody {
        email: String,
        level: String,
    }
    let b: ShareBody = parse(body)?;
    let grantee = app
        .store
        .resolve_address(&b.email)
        .map_err(internal)?
        .and_then(|(id, _)| app.store.user(id).ok().flatten())
        .filter(|u| u.enabled && u.kind == "user")
        .ok_or_else(|| bad("No account with that address on this server."))?;
    if grantee.id == user.id {
        return Err(bad("That is your own calendar."));
    }
    let level = match b.level.as_str() {
        "none" => None,
        l @ ("freebusy" | "read" | "write") => Some(l),
        _ => return Err(bad("invalid level")),
    };
    app.store.set_calendar_share(user.id, grantee.id, level).map_err(internal)?;
    Ok(json!({ "ok": true }))
}

/// GET /event?id= — the full stored event, so an editor can change it without
/// losing fields it does not show.
pub fn get(app: &Shared, user: &User, query: &str) -> ApiResult {
    let id: i64 = query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == "id").and_then(|(_, v)| v.parse().ok()).ok_or_else(|| bad("id required"))?;
    let (item, e, access) = readable_event(app, user, id)?;
    let own = item.user_id == user.id;
    Ok(json!({
        "id": item.id,
        "folder": item.folder_id,
        "owner": item.user_id,
        "canEdit": may_edit(user, &item, &e, access),
        "isOrganizer": !own || e.attendees.is_empty() || crate::calendar::user_is_organizer(app, user, &e),
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
    let (item, mut ev) = writable_event(app, user, b.id)?;
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
    let (item, _) = writable_event(app, user, d.id)?;
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
