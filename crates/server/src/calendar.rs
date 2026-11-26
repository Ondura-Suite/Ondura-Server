//! Meetings: invitations, replies and cancellations between mailboxes and
//! with other servers (iTIP over e-mail), room mailboxes, free/busy.

use anyhow::{Result, anyhow};
use tracing::{debug, info};

use crate::app::Shared;
use crate::ical::{self, WriteOptions};
use crate::mime::{self, Outgoing};
use crate::model::{Attendee, AttendeeKind, BusyStatus, Event, FolderClass, ItemKind, Person, Response};
use crate::smtp::submit;
use crate::store::{Folder, Item, User};

pub fn default_calendar(app: &Shared, user_id: i64) -> Result<Folder> {
    app.store.ensure_folder(user_id, "calendar")
}

fn calendar_folders(app: &Shared, user_id: i64) -> Result<Vec<Folder>> {
    Ok(app.store.folders(user_id)?.into_iter().filter(|f| f.class == FolderClass::Calendar).collect())
}

/// The calendar item with this UID, if any.
pub fn find_by_uid(app: &Shared, user_id: i64, uid: &str) -> Result<Option<Item>> {
    Ok(app.store.items_by_uid(user_id, uid)?.into_iter().find(|i| i.kind == ItemKind::Event))
}

fn is_me(app: &Shared, user: &User, email: &str) -> bool {
    email.eq_ignore_ascii_case(&user.email) || app.store.send_as_addresses(user.id).unwrap_or_default().iter().any(|a| a.eq_ignore_ascii_case(email))
}

/// Whether `user` organizes the meeting `e` (no organizer means the user, who is
/// creating it, is the organizer).
pub fn user_is_organizer(app: &Shared, user: &User, e: &Event) -> bool {
    match &e.organizer {
        None => true,
        Some(p) => is_me(app, user, &p.email),
    }
}

/// Apply a meeting message (METHOD REQUEST/CANCEL/REPLY) to the calendar of `user`.
pub async fn process_incoming(app: &Shared, user: &User, ics: &str, from: Option<&Person>) -> Result<()> {
    let cal = ical::parse(ics);
    for incoming in cal.events {
        if incoming.organizer.as_ref().is_some_and(|o| is_me(app, user, &o.email)) && cal.method != "REPLY" {
            continue;
        }
        let existing = find_by_uid(app, user.id, &incoming.uid)?;
        // An update of a single occurrence: no rule, one exception marker.
        let single_occurrence = incoming.recurrence.is_none() && incoming.exceptions.len() == 1 && !incoming.exceptions[0].deleted;
        match cal.method.as_str() {
            "REQUEST" | "PUBLISH" | "" => {
                let stored = match (&existing, single_occurrence) {
                    (Some(item), true) if item.event().is_some_and(|e| e.recurrence.is_some()) => {
                        let mut e = item.event().expect("checked");
                        let original = incoming.exceptions[0].original_start;
                        let ex = e.exception_mut(original);
                        ex.start = Some(incoming.start);
                        ex.end = Some(incoming.end);
                        ex.subject = Some(incoming.subject.clone());
                        ex.location = Some(incoming.location.clone());
                        e.sequence = e.sequence.max(incoming.sequence);
                        app.store.update_pim(item.id, &serde_json::to_string(&e)?, "itip")?;
                        item.id
                    }
                    (Some(item), _) => {
                        let old = item.event().unwrap_or_default();
                        let mut e = incoming.clone();
                        e.exceptions.retain(|x| x.deleted || x.start.is_some());
                        let times_changed = old.start != e.start || old.end != e.end || old.recurrence != e.recurrence;
                        e.response = if times_changed { Response::NotResponded } else { old.response };
                        e.busy = if e.response == Response::Accepted { incoming.busy } else { BusyStatus::Tentative };
                        e.reminder = old.reminder.or(e.reminder);
                        e.categories = old.categories;
                        app.store.update_pim(item.id, &serde_json::to_string(&e)?, "itip")?;
                        item.id
                    }
                    (None, _) => {
                        let mut e = incoming.clone();
                        e.exceptions.retain(|x| x.deleted || x.start.is_some());
                        e.response = Response::NotResponded;
                        e.busy = BusyStatus::Tentative;
                        if e.reminder.is_none() {
                            e.reminder = Some(15);
                        }
                        let folder = default_calendar(app, user.id)?;
                        app.store.insert_pim(user.id, folder.id, ItemKind::Event, &serde_json::to_string(&e)?, Some(&e.uid), "itip")?
                    }
                };
                debug!(user = %user.email, uid = %incoming.uid, "meeting request stored");
                if user.kind == "room" {
                    auto_answer_room(app, user, stored).await?;
                }
            }
            "CANCEL" => {
                let Some(item) = existing else { continue };
                let mut e = item.event().ok_or_else(|| anyhow!("broken calendar item"))?;
                if single_occurrence || (!incoming.exceptions.is_empty() && incoming.recurrence.is_none() && e.recurrence.is_some()) {
                    for x in &incoming.exceptions {
                        e.exception_mut(x.original_start).deleted = true;
                    }
                } else {
                    e.cancelled = true;
                }
                if user.kind == "room" {
                    app.store.delete_item(item.id, "itip")?;
                } else {
                    app.store.update_pim(item.id, &serde_json::to_string(&e)?, "itip")?;
                }
            }
            "REPLY" => {
                let Some(item) = existing else { continue };
                let mut e = item.event().ok_or_else(|| anyhow!("broken calendar item"))?;
                for a in &incoming.attendees {
                    let email = if a.person.email.is_empty() { from.map(|f| f.email.clone()).unwrap_or_default() } else { a.person.email.clone() };
                    match e.attendees.iter_mut().find(|x| x.person.email.eq_ignore_ascii_case(&email)) {
                        Some(x) => x.status = a.status,
                        None => e.attendees.push(Attendee { person: Person::new(a.person.name.clone(), email), kind: AttendeeKind::Optional, status: a.status }),
                    }
                }
                app.store.update_pim(item.id, &serde_json::to_string(&e)?, "itip")?;
            }
            _ => {}
        }
    }
    Ok(())
}

/// Room mailboxes accept requests when free and decline conflicts.
async fn auto_answer_room(app: &Shared, room: &User, item_id: i64) -> Result<()> {
    let item = app.store.item(item_id)?.ok_or_else(|| anyhow!("item vanished"))?;
    let e = item.event().ok_or_else(|| anyhow!("broken calendar item"))?;
    let horizon = e.start + 366 * 86_400;
    let conflict = e.occurrences(e.start, horizon).iter().any(|o| {
        busy_times(app, room.id, o.start, o.end).map(|b| b.iter().any(|x| x.item_id != item_id)).unwrap_or(false)
    });
    let answer = if conflict { Response::Declined } else { Response::Accepted };
    info!(room = %room.email, uid = %e.uid, ?answer, "room booking");
    respond(app, room, item_id, answer, None, true, "").await
}

pub fn method_subject(method: &str, subject: &str, response: Option<Response>) -> String {
    match (method, response) {
        ("CANCEL", _) => format!("Canceled: {subject}"),
        ("REPLY", Some(Response::Accepted)) => format!("Accepted: {subject}"),
        ("REPLY", Some(Response::Tentative)) => format!("Tentative: {subject}"),
        ("REPLY", Some(Response::Declined)) => format!("Declined: {subject}"),
        _ => subject.to_string(),
    }
}

/// Send invitations (REQUEST) or cancellations (CANCEL) for an event
/// organised by `user` to its attendees (or `only` these people).
pub async fn send_invitations(app: &Shared, user: &User, e: &Event, method: &str, only: Option<&[Person]>, occurrence: Option<i64>) -> Result<()> {
    let to: Vec<Person> = match only {
        Some(list) => list.to_vec(),
        None => e.attendees.iter().map(|a| a.person.clone()).collect(),
    };
    let to: Vec<Person> = to.into_iter().filter(|p| !p.email.is_empty() && !is_me(app, user, &p.email)).collect();
    if to.is_empty() {
        return Ok(());
    }
    let mut ev = e.clone();
    if ev.organizer.is_none() {
        ev.organizer = Some(user.person());
    }
    let ics = ical::write(&ev, &WriteOptions { method, reply_from: None, occurrence });
    let text = ical::summary_text(&ev);
    let out = Outgoing {
        from: user.person(),
        to,
        subject: method_subject(method, &ev.subject, None),
        html: Some(mime::text_to_html(&text)),
        text,
        calendar: Some((method.to_string(), ics)),
        ..Default::default()
    };
    submit::send_generated(app, user, &out, true).await
}

/// Answer a meeting request: update the own calendar item and (optionally)
/// send a reply to the organiser. Declined meetings are removed.
pub async fn respond(app: &Shared, user: &User, item_id: i64, response: Response, occurrence: Option<i64>, send: bool, comment: &str) -> Result<()> {
    let item = app.store.item(item_id)?.ok_or_else(|| anyhow!("calendar item not found"))?;
    let mut e = item.event().ok_or_else(|| anyhow!("not a calendar item"))?;
    let organizer = e.organizer.clone();
    match occurrence {
        Some(original) if e.recurrence.is_some() => {
            let ex = e.exception_mut(original);
            if response == Response::Declined {
                ex.deleted = true;
            } else {
                ex.busy = Some(if response == Response::Tentative { BusyStatus::Tentative } else { BusyStatus::Busy });
            }
            app.store.update_pim(item_id, &serde_json::to_string(&e)?, "respond")?;
        }
        _ => {
            e.response = response;
            e.busy = match response {
                Response::Tentative => BusyStatus::Tentative,
                Response::Declined => BusyStatus::Free,
                _ => BusyStatus::Busy,
            };
            if let Some(me) = e.attendees.iter_mut().find(|a| is_me(app, user, &a.person.email)) {
                me.status = response;
            }
            if response == Response::Declined {
                app.store.delete_item(item_id, "respond")?;
            } else {
                app.store.update_pim(item_id, &serde_json::to_string(&e)?, "respond")?;
            }
        }
    }
    if send
        && let Some(org) = organizer.filter(|o| !is_me(app, user, &o.email) && !o.email.is_empty())
    {
        let me = e.attendees.iter().find(|a| is_me(app, user, &a.person.email)).map(|a| a.person.clone()).unwrap_or_else(|| user.person());
        let ics = ical::write(&e, &WriteOptions { method: "REPLY", reply_from: Some((&me, response)), occurrence });
        let text = if comment.trim().is_empty() { ical::summary_text(&e) } else { comment.to_string() };
        let out = Outgoing {
            from: user.person(),
            to: vec![org],
            subject: method_subject("REPLY", &e.subject, Some(response)),
            html: Some(mime::text_to_html(&text)),
            text,
            calendar: Some(("REPLY".into(), ics)),
            ..Default::default()
        };
        submit::send_generated(app, user, &out, false).await?;
    }
    Ok(())
}

/// Calendar item for the meeting request in a mail item (creating it from
/// the attached iCalendar data if needed).
pub fn event_for_request(app: &Shared, user: &User, mail: &Item) -> Result<Item> {
    let ics = mail.data.as_deref().ok_or_else(|| anyhow!("the message is not a meeting request"))?;
    let cal = ical::parse(ics);
    let e = cal.events.into_iter().next().ok_or_else(|| anyhow!("meeting request without event"))?;
    if let Some(item) = find_by_uid(app, user.id, &e.uid)? {
        return Ok(item);
    }
    let mut e = e;
    e.response = Response::NotResponded;
    e.busy = BusyStatus::Tentative;
    let folder = default_calendar(app, user.id)?;
    let id = app.store.insert_pim(user.id, folder.id, ItemKind::Event, &serde_json::to_string(&e)?, Some(&e.uid), "respond")?;
    app.store.item(id)?.ok_or_else(|| anyhow!("item vanished"))
}

#[derive(Debug, Clone)]
pub struct Busy {
    pub item_id: i64,
    pub start: i64,
    pub end: i64,
    pub status: BusyStatus,
    pub subject: String,
    pub location: String,
    pub private: bool,
    pub recurring: bool,
}

/// Busy times of a mailbox in `[from, to)` (all calendars, free and
/// cancelled items left out).
pub fn busy_times(app: &Shared, user_id: i64, from: i64, to: i64) -> Result<Vec<Busy>> {
    let mut out = Vec::new();
    for f in calendar_folders(app, user_id)? {
        for item in app.store.events_in_range(f.id, from, to)? {
            let Some(e) = item.event() else { continue };
            if e.cancelled {
                continue;
            }
            for o in e.occurrences(from, to) {
                let status = o.exception.as_ref().and_then(|x| x.busy).unwrap_or(e.busy);
                if status == BusyStatus::Free {
                    continue;
                }
                out.push(Busy {
                    item_id: item.id,
                    start: o.start,
                    end: o.end,
                    status,
                    subject: o.exception.as_ref().and_then(|x| x.subject.clone()).unwrap_or_else(|| e.subject.clone()),
                    location: o.exception.as_ref().and_then(|x| x.location.clone()).unwrap_or_else(|| e.location.clone()),
                    private: matches!(e.sensitivity, crate::model::Sensitivity::Private | crate::model::Sensitivity::Confidential),
                    recurring: e.recurrence.is_some(),
                });
            }
        }
    }
    out.sort_by_key(|b| b.start);
    Ok(out)
}

/// Merged free/busy string (one digit per interval: 0 free, 1 tentative,
/// 2 busy, 3 OOF, 4 working elsewhere) as used by EWS and ActiveSync.
pub fn merged_free_busy(busy: &[Busy], from: i64, to: i64, interval_minutes: i64) -> String {
    let step = interval_minutes.max(1) * 60;
    let mut s = String::new();
    let mut t = from;
    while t < to {
        let end = (t + step).min(to);
        let level = busy
            .iter()
            .filter(|b| b.start < end && b.end > t)
            .map(|b| match b.status {
                BusyStatus::Free => 0,
                BusyStatus::Tentative => 1,
                BusyStatus::Busy => 2,
                BusyStatus::Oof => 3,
                BusyStatus::WorkingElsewhere => 4,
            })
            .max()
            .unwrap_or(0);
        s.push(char::from(b'0' + level));
        t = end;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smtp::delivery;

    #[tokio::test]
    async fn invitation_round_trip_between_mailboxes() {
        let (app, niklas) = crate::app::App::test();
        let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
        let start = 1_795_000_000;
        let e = Event {
            uid: "meet-1".into(),
            subject: "Planung".into(),
            start,
            end: start + 3600,
            tz: "Europe/Berlin".into(),
            organizer: Some(niklas.person()),
            attendees: vec![Attendee { person: Person::new("Anna", "anna@example.com"), kind: AttendeeKind::Required, status: Response::NotResponded }],
            ..Default::default()
        };
        let cal = default_calendar(&app, niklas.id).unwrap();
        let own = app.store.insert_pim(niklas.id, cal.id, ItemKind::Event, &serde_json::to_string(&e).unwrap(), Some("meet-1"), "test").unwrap();
        send_invitations(&app, &niklas, &e, "REQUEST", None, None).await.unwrap();

        let item = find_by_uid(&app, anna.id, "meet-1").unwrap().expect("event created at attendee");
        assert_eq!(item.event().unwrap().response, Response::NotResponded);
        assert_eq!(busy_times(&app, anna.id, start - 10, start + 10).unwrap()[0].status, BusyStatus::Tentative);
        let inbox = app.store.folder_by_role(anna.id, "inbox").unwrap().unwrap();
        let mails = app.store.items(inbox.id, 0, 10).unwrap();
        assert_eq!(mails[0].message_class, "IPM.Schedule.Meeting.Request");

        respond(&app, &anna, item.id, Response::Accepted, None, true, "").await.unwrap();
        let organizer_view = app.store.item(own).unwrap().unwrap().event().unwrap();
        assert_eq!(organizer_view.attendees[0].status, Response::Accepted);

        send_invitations(&app, &niklas, &e, "CANCEL", None, None).await.unwrap();
        assert!(find_by_uid(&app, anna.id, "meet-1").unwrap().unwrap().event().unwrap().cancelled);
        let _ = delivery::Options::default();
    }

    #[tokio::test]
    async fn rooms_decline_conflicts() {
        let (app, niklas) = crate::app::App::test();
        let room = app.store.create_user("raum@example.com", "Raum Saar", "pw", "room").unwrap();
        let mk = |uid: &str| Event {
            uid: uid.into(),
            subject: "Termin".into(),
            start: 1_795_000_000,
            end: 1_795_003_600,
            organizer: Some(niklas.person()),
            attendees: vec![Attendee { person: room.person(), kind: AttendeeKind::Resource, status: Response::NotResponded }],
            ..Default::default()
        };
        send_invitations(&app, &niklas, &mk("a"), "REQUEST", None, None).await.unwrap();
        send_invitations(&app, &niklas, &mk("b"), "REQUEST", None, None).await.unwrap();
        assert!(find_by_uid(&app, room.id, "a").unwrap().is_some());
        assert!(find_by_uid(&app, room.id, "b").unwrap().is_none());
        assert_eq!(merged_free_busy(&busy_times(&app, room.id, 1_795_000_000 - 1800, 1_795_003_600).unwrap(), 1_795_000_000 - 1800, 1_795_003_600, 30), "022");
    }
}
