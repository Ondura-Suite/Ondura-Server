//! Webmail API tests, driving the handler functions directly against an
//! in-memory store (no network, no SMTP submission).

use axum::body::Bytes;

use super::{calendar_api, contacts_api, drive_api, mail_api, me, pim_api, settings_api};
use crate::app::App;

fn body(v: serde_json::Value) -> Bytes {
    Bytes::from(serde_json::to_vec(&v).unwrap())
}

#[test]
fn me_lists_folders() {
    let (app, user) = App::test();
    let r = me(&app, &user).unwrap();
    let folders = r["folders"].as_array().unwrap();
    assert!(folders.iter().any(|f| f["role"] == "inbox"));
    assert!(folders.iter().any(|f| f["role"] == "calendar"));
    assert!(folders.iter().any(|f| f["role"] == "contacts"));
}

#[test]
fn draft_keeps_rich_text_html() {
    // The rich-text composer sends both text and html; the html must survive
    // the round-trip (multipart/alternative) so the editor reloads formatting.
    let (app, user) = App::test();
    let saved = mail_api::draft(&app, &user, &body(serde_json::json!({
        "to": "a@example.com", "subject": "Rich", "text": "Hello bold world", "html": "<p>Hello <b>bold</b> world</p>"
    }))).unwrap();
    let id = saved["id"].as_i64().unwrap();
    let full = mail_api::message(&app, &user, &id.to_string()).unwrap();
    assert!(full["html"].as_str().unwrap().contains("<b>bold</b>"));
    assert!(full["text"].as_str().unwrap().contains("bold"));
}

#[test]
fn draft_keeps_bcc() {
    // Bcc is stored as a header on the draft and re-exposed on read, so the
    // composer can restore the hidden recipients when the draft is reopened.
    let (app, user) = App::test();
    let saved = mail_api::draft(&app, &user, &body(serde_json::json!({
        "to": "a@example.com", "bcc": "secret@example.com", "subject": "Hidden", "text": "hi"
    }))).unwrap();
    let id = saved["id"].as_i64().unwrap();
    let full = mail_api::message(&app, &user, &id.to_string()).unwrap();
    let bcc = full["bcc"].as_array().unwrap();
    assert_eq!(bcc.len(), 1);
    assert_eq!(bcc[0]["email"], "secret@example.com");
}

#[test]
fn categories_assign_rename_and_delete() {
    let (app, user) = App::test();
    // Create a draft to carry a category.
    let saved = mail_api::draft(&app, &user, &body(serde_json::json!({ "to": "a@example.com", "subject": "Tagged", "text": "x" }))).unwrap();
    let id = saved["id"].as_i64().unwrap();
    // Register a colour category.
    let cat = mail_api::save_category(&app, &user, &body(serde_json::json!({ "name": "Work", "color": "red" }))).unwrap();
    assert!(cat["id"].as_i64().unwrap() > 0);
    // Assign it to the message.
    mail_api::categorize(&app, &user, &body(serde_json::json!({ "id": id, "categories": ["Work"] }))).unwrap();
    let full = mail_api::message(&app, &user, &id.to_string()).unwrap();
    assert_eq!(full["categories"].as_array().unwrap(), &vec![serde_json::json!("Work")]);
    // Rename the category — the item's chip name must follow.
    let cid = cat["id"].as_i64().unwrap();
    mail_api::save_category(&app, &user, &body(serde_json::json!({ "id": cid, "name": "Office", "color": "blue" }))).unwrap();
    let full = mail_api::message(&app, &user, &id.to_string()).unwrap();
    assert_eq!(full["categories"].as_array().unwrap(), &vec![serde_json::json!("Office")]);
    // Delete the category — it is stripped from the item.
    mail_api::delete_category(&app, &user, &body(serde_json::json!({ "id": cid }))).unwrap();
    let full = mail_api::message(&app, &user, &id.to_string()).unwrap();
    assert!(full["categories"].as_array().unwrap().is_empty());
    assert!(mail_api::categories(&app, &user).unwrap()["categories"].as_array().unwrap().is_empty());
}

#[test]
fn draft_then_list_and_read() {
    let (app, user) = App::test();
    let saved = mail_api::draft(&app, &user, &body(serde_json::json!({
        "to": "a@example.com", "subject": "Hello", "text": "Draft body"
    }))).unwrap();
    let id = saved["id"].as_i64().unwrap();

    let drafts = app.store.folder_by_role(user.id, "drafts").unwrap().unwrap();
    let list = mail_api::messages(&app, &user, &format!("folder={}", drafts.id)).unwrap();
    let msgs = list["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["subject"], "Hello");

    let full = mail_api::message(&app, &user, &id.to_string()).unwrap();
    assert_eq!(full["subject"], "Hello");
    assert!(full["text"].as_str().unwrap().contains("Draft body"));
    assert!(full["flags"]["draft"].as_bool().unwrap());
}

#[test]
fn flag_read_and_flagged() {
    let (app, user) = App::test();
    let saved = mail_api::draft(&app, &user, &body(serde_json::json!({ "subject": "x", "text": "y" }))).unwrap();
    let id = saved["id"].as_i64().unwrap();
    mail_api::flag(&app, &user, &body(serde_json::json!({ "id": id, "flagged": true }))).unwrap();
    let full = mail_api::message(&app, &user, &id.to_string()).unwrap();
    assert!(full["flags"]["flagged"].as_bool().unwrap());
}

#[test]
fn foreign_item_is_not_found() {
    let (app, user) = App::test();
    // A high id that does not belong to the user must 404, not leak.
    let err = mail_api::message(&app, &user, "999999").unwrap_err();
    assert_eq!(err.0, axum::http::StatusCode::NOT_FOUND);
}

#[test]
fn empty_folder_removes_all() {
    let (app, user) = App::test();
    // Put two drafts into Deleted Items, then empty it.
    let trash = app.store.ensure_folder(user.id, "deleteditems").unwrap();
    for n in ["a", "b"] {
        let saved = mail_api::draft(&app, &user, &body(serde_json::json!({ "subject": n, "text": n }))).unwrap();
        let id = saved["id"].as_i64().unwrap();
        mail_api::move_message(&app, &user, &body(serde_json::json!({ "id": id, "folder": trash.id }))).unwrap();
    }
    assert_eq!(mail_api::messages(&app, &user, &format!("folder={}", trash.id)).unwrap()["messages"].as_array().unwrap().len(), 2);
    mail_api::empty(&app, &user, &body(serde_json::json!({ "folder": trash.id }))).unwrap();
    assert_eq!(mail_api::messages(&app, &user, &format!("folder={}", trash.id)).unwrap()["messages"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn calendar_save_and_list() {
    let (app, user) = App::test();
    let start = crate::store::now() + 3600;
    let saved = calendar_api::save(&app, &user, &body(serde_json::json!({
        "event": { "uid": "", "subject": "Dentist", "location": "Clinic", "start": start, "end": start + 1800, "tz": "Europe/Berlin" }
    }))).await.unwrap();
    assert!(saved["id"].as_i64().unwrap() > 0);

    let from = crate::store::now();
    let list = calendar_api::list(&app, &user, &format!("from={}&to={}", from, from + 86400)).unwrap();
    let events = list["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["subject"], "Dentist");
    assert_eq!(events[0]["location"], "Clinic");
}

#[tokio::test]
async fn send_as_rejects_foreign_address() {
    let (app, user) = App::test();
    // No alias set up, so only the primary address is allowed.
    let err = mail_api::send(&app, &user, &body(serde_json::json!({
        "from": "foreign@example.com", "to": "x@example.com", "subject": "s", "text": "t"
    }))).await.unwrap_err();
    assert_eq!(err.0, axum::http::StatusCode::FORBIDDEN);
}

#[test]
fn send_as_lists_primary_and_aliases() {
    let (app, user) = App::test();
    app.store.add_alias("sales@example.com", user.id).unwrap();
    let addrs = app.store.send_as_addresses(user.id).unwrap();
    assert!(addrs.contains(&user.email));
    assert!(addrs.iter().any(|a| a == "sales@example.com"));
}

#[test]
fn tasks_crud_and_complete() {
    let (app, user) = App::test();
    let saved = pim_api::save_task(&app, &user, &body(serde_json::json!({
        "task": { "subject": "Pay invoice", "body": "", "complete": false, "percent_complete": 0 }
    }))).unwrap();
    let id = saved["id"].as_i64().unwrap();
    let list = pim_api::tasks(&app, &user).unwrap();
    assert_eq!(list["tasks"][0]["subject"], "Pay invoice");
    assert_eq!(list["tasks"][0]["complete"], false);

    pim_api::save_task(&app, &user, &body(serde_json::json!({
        "id": id, "task": { "subject": "Pay invoice", "body": "", "complete": true, "percent_complete": 100 }
    }))).unwrap();
    assert_eq!(pim_api::tasks(&app, &user).unwrap()["tasks"][0]["complete"], true);

    pim_api::delete_task(&app, &user, &body(serde_json::json!({ "id": id }))).unwrap();
    assert_eq!(pim_api::tasks(&app, &user).unwrap()["tasks"].as_array().unwrap().len(), 0);
}

#[test]
fn notes_crud() {
    let (app, user) = App::test();
    let saved = pim_api::save_note(&app, &user, &body(serde_json::json!({ "note": { "subject": "Idea", "body": "Great", "categories": [] } }))).unwrap();
    let id = saved["id"].as_i64().unwrap();
    assert_eq!(pim_api::notes(&app, &user).unwrap()["notes"][0]["subject"], "Idea");
    pim_api::delete_note(&app, &user, &body(serde_json::json!({ "id": id }))).unwrap();
    assert_eq!(pim_api::notes(&app, &user).unwrap()["notes"].as_array().unwrap().len(), 0);
}

#[test]
fn settings_and_signature() {
    let (app, user) = App::test();
    settings_api::save(&app, &user, &body(serde_json::json!({
        "signature": "Regards", "oof": { "state": "Enabled", "internalReply": "Away", "externalReply": "Away", "externalAudience": "All" }
    }))).unwrap();
    let s = settings_api::get(&app, &user).unwrap();
    assert_eq!(s["signature"], "Regards");
    assert_eq!(s["oof"]["state"], "Enabled");
    // Signature surfaces in /me.
    assert_eq!(me(&app, &user).unwrap()["signature"], "Regards");
}

#[test]
fn password_change_requires_current() {
    let (app, user) = App::test();
    // Wrong current password is refused.
    let err = settings_api::password(&app, &user, &body(serde_json::json!({ "current": "wrong", "new": "longenough1" }))).unwrap_err();
    assert_eq!(err.0, axum::http::StatusCode::FORBIDDEN);
}

#[test]
fn custom_folder_lifecycle() {
    let (app, user) = App::test();
    let created = pim_api::create_folder(&app, &user, &body(serde_json::json!({ "name": "Projects" }))).unwrap();
    let fid = created["id"].as_i64().unwrap();
    pim_api::rename_folder(&app, &user, &body(serde_json::json!({ "id": fid, "name": "Work" }))).unwrap();
    assert_eq!(app.store.folder(fid).unwrap().unwrap().name, "Work");
    pim_api::delete_folder(&app, &user, &body(serde_json::json!({ "id": fid }))).unwrap();
    assert!(app.store.folder(fid).unwrap().is_none());
}

#[test]
fn system_folder_cannot_be_deleted() {
    let (app, user) = App::test();
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    let err = pim_api::delete_folder(&app, &user, &body(serde_json::json!({ "id": inbox.id }))).unwrap_err();
    assert_eq!(err.0, axum::http::StatusCode::BAD_REQUEST);
}

#[test]
fn stores_and_exposes_threading_headers() {
    use crate::store::{NewMail, now};
    let (app, user) = App::test();
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    let raw = b"From: a@example.com\r\nSubject: Re: Hi\r\nMessage-ID: <child@x>\r\nIn-Reply-To: <parent@x>\r\nReferences: <root@x> <parent@x>\r\n\r\nbody";
    app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: raw, flags: 0, received: now(), writer: "smtp" }).unwrap();
    let list = mail_api::messages(&app, &user, &format!("folder={}", inbox.id)).unwrap();
    let m = &list["messages"].as_array().unwrap()[0];
    assert_eq!(m["messageId"], "<child@x>");
    assert_eq!(m["inReplyTo"], "<parent@x>");
    assert_eq!(m["references"], "<root@x> <parent@x>");
}

#[test]
fn rule_moves_matching_mail() {
    use crate::store::{NewMail, RuleAction, RuleCondition, now};
    let (app, user) = App::test();
    let archive = app.store.ensure_folder(user.id, "archive").unwrap();
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    let raw = b"From: Boss <boss@corp.example>\r\nTo: me@example.com\r\nSubject: Weekly report\r\n\r\nhi";
    let id = app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: raw, flags: 0, received: now(), writer: "smtp" }).unwrap();
    app.store
        .save_rule(
            user.id,
            None,
            "boss to archive",
            true,
            true,
            false,
            &[RuleCondition { field: "from".into(), value: "boss@corp".into() }],
            &[RuleAction { kind: "move".into(), folder: Some(archive.id), category: None }, RuleAction { kind: "markRead".into(), folder: None, category: None }],
        )
        .unwrap();
    app.store.apply_rules(user.id, id).unwrap();
    let item = app.store.item(id).unwrap().unwrap();
    assert_eq!(item.folder_id, archive.id);
    assert!(item.flags & crate::model::flags::READ != 0);
}

#[test]
fn rule_skips_non_matching_mail() {
    use crate::store::{NewMail, RuleAction, RuleCondition, now};
    let (app, user) = App::test();
    let archive = app.store.ensure_folder(user.id, "archive").unwrap();
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    let raw = b"From: Friend <friend@example.com>\r\nSubject: Lunch\r\n\r\nhi";
    let id = app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: raw, flags: 0, received: now(), writer: "smtp" }).unwrap();
    app.store
        .save_rule(user.id, None, "boss", true, true, false, &[RuleCondition { field: "from".into(), value: "boss@corp".into() }], &[RuleAction { kind: "move".into(), folder: Some(archive.id), category: None }])
        .unwrap();
    app.store.apply_rules(user.id, id).unwrap();
    assert_eq!(app.store.item(id).unwrap().unwrap().folder_id, inbox.id);
}

#[test]
fn sweep_keeps_latest_from_sender() {
    use crate::store::{NewMail, now};
    let (app, user) = App::test();
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    let trash = app.store.folder_by_role(user.id, "deleteditems").unwrap().unwrap();
    for n in 0..3 {
        let raw = format!("From: Spammer <spam@ads.example>\r\nSubject: Deal {n}\r\n\r\nbuy");
        app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: raw.as_bytes(), flags: 0, received: now() + n, writer: "smtp" }).unwrap();
    }
    let r = mail_api::sweep(&app, &user, &body(serde_json::json!({ "from": "spam@ads.example", "folder": inbox.id, "action": "keepLatest" }))).unwrap();
    assert_eq!(r["count"].as_i64().unwrap(), 2);
    assert_eq!(mail_api::messages(&app, &user, &format!("folder={}", inbox.id)).unwrap()["messages"].as_array().unwrap().len(), 1);
    assert_eq!(mail_api::messages(&app, &user, &format!("folder={}", trash.id)).unwrap()["messages"].as_array().unwrap().len(), 2);
}

#[test]
fn bulk_mark_read_and_move() {
    let (app, user) = App::test();
    let archive = app.store.ensure_folder(user.id, "archive").unwrap();
    let mut ids = Vec::new();
    for n in ["a", "b", "c"] {
        ids.push(mail_api::draft(&app, &user, &body(serde_json::json!({ "subject": n, "text": n }))).unwrap()["id"].as_i64().unwrap());
    }
    pim_api::bulk(&app, &user, &body(serde_json::json!({ "ids": ids, "action": "move", "folder": archive.id }))).unwrap();
    assert_eq!(mail_api::messages(&app, &user, &format!("folder={}", archive.id)).unwrap()["messages"].as_array().unwrap().len(), 3);
}

#[test]
fn contacts_save_list_delete() {
    let (app, user) = App::test();
    let saved = contacts_api::save(&app, &user, &body(serde_json::json!({
        "contact": { "first_name": "Max", "last_name": "Mustermann", "emails": ["max@example.com"], "company": "ACME" }
    }))).unwrap();
    let id = saved["id"].as_i64().unwrap();

    let list = contacts_api::list(&app, &user).unwrap();
    let contacts = list["contacts"].as_array().unwrap();
    assert_eq!(contacts.len(), 1);
    assert_eq!(contacts[0]["displayName"], "Max Mustermann");
    assert_eq!(contacts[0]["emails"][0], "max@example.com");

    contacts_api::delete(&app, &user, &body(serde_json::json!({ "id": id }))).unwrap();
    let list = contacts_api::list(&app, &user).unwrap();
    assert_eq!(list["contacts"].as_array().unwrap().len(), 0);
}

#[test]
fn search_matches_body_and_attachment_name() {
    use crate::store::{NewMail, now};
    let (app, user) = App::test();
    let inbox = app.store.folder_by_role(user.id, "inbox").unwrap().unwrap();
    // Distinctive word only in the body.
    let body = b"From: a@example.com\r\nSubject: Hallo\r\nContent-Type: text/plain\r\n\r\nDies enthaelt das Zauberwort Flux im Text.";
    app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: body, flags: 0, received: now(), writer: "smtp" }).unwrap();
    // Distinctive word only in an attachment filename.
    let att = concat!(
        "From: b@example.com\r\nSubject: Rechnung\r\n",
        "Content-Type: multipart/mixed; boundary=bd\r\n\r\n",
        "--bd\r\nContent-Type: text/plain\r\n\r\nsiehe Anhang\r\n",
        "--bd\r\nContent-Type: application/pdf; name=\"Quartalsbericht.pdf\"\r\n",
        "Content-Disposition: attachment; filename=\"Quartalsbericht.pdf\"\r\n",
        "Content-Transfer-Encoding: base64\r\n\r\nSGVsbG8=\r\n--bd--\r\n"
    );
    app.store.insert_mail(NewMail { user_id: user.id, folder_id: inbox.id, mime: att.as_bytes(), flags: 0, received: now(), writer: "smtp" }).unwrap();

    assert_eq!(app.store.search(user.id, &[inbox.id], "Flux", 20).unwrap().len(), 1);
    assert_eq!(app.store.search(user.id, &[inbox.id], "Quartalsbericht", 20).unwrap().len(), 1);
    assert!(app.store.search(user.id, &[inbox.id], "Nichtvorhandenes", 20).unwrap().is_empty());
}

#[tokio::test]
async fn meeting_save_sends_request_and_cancel() {
    let (app, user) = App::test();
    let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
    let event = serde_json::json!({
        "uid": "mtg-1",
        "subject": "Planning",
        "start": 1_800_000_000i64,
        "end": 1_800_003_600i64,
        "tz": "Europe/Berlin",
        "attendees": [
            { "email": "anna@example.com", "name": "Anna" },
            { "email": "max@other.org", "name": "Max" }
        ]
    });
    let r = calendar_api::save(&app, &user, &body(serde_json::json!({ "event": event }))).await.unwrap();
    let id = r["id"].as_i64().unwrap();

    // The local attendee received the invitation.
    let anna_inbox = app.store.folder_by_role(anna.id, "inbox").unwrap().unwrap();
    assert_eq!(app.store.items(anna_inbox.id, 0, 10).unwrap().len(), 1);
    // The remote attendee was queued for outbound delivery.
    assert!(app.store.queue().unwrap().iter().any(|q| q.recipients.iter().any(|a| a == "max@other.org")));
    // The organizer kept a Sent copy.
    let sent = app.store.folder_by_role(user.id, "sentitems").unwrap().unwrap();
    assert_eq!(app.store.items(sent.id, 0, 10).unwrap().len(), 1);

    // Deleting the meeting cancels it for the attendees.
    calendar_api::delete(&app, &user, &body(serde_json::json!({ "id": id }))).await.unwrap();
    assert_eq!(app.store.items(anna_inbox.id, 0, 10).unwrap().len(), 2);
}

#[tokio::test]
async fn editing_an_event_keeps_its_identity_and_series() {
    let (app, user) = App::test();
    let event = serde_json::json!({
        "uid": "weekly-1", "subject": "Standup", "body": "Daily notes",
        "start": 1_800_000_000i64, "end": 1_800_001_800i64, "tz": "Europe/Berlin",
        "reminder": 15,
        "recurrence": { "freq": "weekly", "interval": 1, "days": 2 }
    });
    let id = calendar_api::save(&app, &user, &body(serde_json::json!({ "event": event }))).await.unwrap()["id"].as_i64().unwrap();
    let full = calendar_api::get(&app, &user, &format!("id={id}")).unwrap();
    assert_eq!(full["event"]["body"], "Daily notes");

    // Saving the edited full event (empty uid, as a careless client might send)
    // keeps the UID and does not touch the pattern.
    let mut ev = full["event"].clone();
    ev["subject"] = "Standup (team)".into();
    ev["uid"] = "".into();
    calendar_api::save(&app, &user, &body(serde_json::json!({ "id": id, "event": ev }))).await.unwrap();
    let after = calendar_api::get(&app, &user, &format!("id={id}")).unwrap();
    assert_eq!(after["event"]["uid"], "weekly-1");
    assert_eq!(after["event"]["subject"], "Standup (team)");
    assert_eq!(after["event"]["reminder"], 15);
    assert!(after["event"]["recurrence"].is_object());
}

#[tokio::test]
async fn single_occurrence_can_be_changed_and_deleted() {
    let (app, user) = App::test();
    let start = 1_800_000_000i64;
    let event = serde_json::json!({
        "uid": "daily-1", "subject": "Sync", "start": start, "end": start + 1800, "tz": "UTC",
        "recurrence": { "freq": "daily", "interval": 1, "count": 5 }
    });
    let id = calendar_api::save(&app, &user, &body(serde_json::json!({ "event": event }))).await.unwrap()["id"].as_i64().unwrap();
    let day = 86_400;
    let range = format!("from={}&to={}", start - 1, start + 6 * day);
    assert_eq!(calendar_api::list(&app, &user, &range).unwrap()["events"].as_array().unwrap().len(), 5);

    // Move the second occurrence by an hour and rename it.
    calendar_api::occurrence(&app, &user, &body(serde_json::json!({
        "id": id, "occurrence": start + day, "subject": "Sync (moved)", "start": start + day + 3600, "end": start + day + 5400
    }))).await.unwrap();
    // Delete the third.
    calendar_api::occurrence(&app, &user, &body(serde_json::json!({ "id": id, "occurrence": start + 2 * day, "delete": true }))).await.unwrap();

    let events = calendar_api::list(&app, &user, &range).unwrap()["events"].as_array().unwrap().clone();
    assert_eq!(events.len(), 4);
    let moved = events.iter().find(|e| e["occurrenceStart"] == start + day).unwrap();
    assert_eq!(moved["subject"], "Sync (moved)");
    assert_eq!(moved["start"], start + day + 3600);
    assert!(!events.iter().any(|e| e["occurrenceStart"] == start + 2 * day));
    // Other occurrences are untouched.
    assert!(events.iter().any(|e| e["occurrenceStart"] == start && e["subject"] == "Sync"));

    // An occurrence the pattern does not produce is refused.
    assert!(calendar_api::occurrence(&app, &user, &body(serde_json::json!({ "id": id, "occurrence": start + 7, "delete": true }))).await.is_err());
}

#[tokio::test]
async fn removing_an_attendee_sends_them_a_cancellation() {
    let (app, user) = App::test();
    let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
    let bert = app.store.create_user("bert@example.com", "Bert", "pw", "user").unwrap();
    let event = serde_json::json!({
        "uid": "mtg-2", "subject": "Review", "start": 1_800_000_000i64, "end": 1_800_003_600i64, "tz": "UTC",
        "attendees": [ { "email": "anna@example.com" }, { "email": "bert@example.com" } ]
    });
    let id = calendar_api::save(&app, &user, &body(serde_json::json!({ "event": event }))).await.unwrap()["id"].as_i64().unwrap();
    let full = calendar_api::get(&app, &user, &format!("id={id}")).unwrap();
    let seq = full["event"]["sequence"].as_u64().unwrap_or(0);

    let mut ev = full["event"].clone();
    ev["attendees"] = serde_json::json!([ { "email": "anna@example.com" } ]);
    calendar_api::save(&app, &user, &body(serde_json::json!({ "id": id, "event": ev }))).await.unwrap();

    // Bert got the invitation and then a cancellation; Anna got two requests.
    let inbox = |id| app.store.folder_by_role(id, "inbox").unwrap().unwrap().id;
    let bert_mail = app.store.items(inbox(bert.id), 0, 10).unwrap();
    assert_eq!(bert_mail.len(), 2);
    assert!(bert_mail.iter().any(|m| m.subject.starts_with("Canceled:")));
    assert_eq!(app.store.items(inbox(anna.id), 0, 10).unwrap().len(), 2);
    // The update raised the meeting sequence.
    let after = calendar_api::get(&app, &user, &format!("id={id}")).unwrap();
    assert!(after["event"]["sequence"].as_u64().unwrap() > seq);
}

#[tokio::test]
async fn freebusy_shows_times_and_details_only_when_shared() {
    let (app, user) = App::test();
    let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
    let start = 1_800_000_000i64;
    let event = serde_json::json!({ "uid": "a1", "subject": "Dentist", "start": start, "end": start + 3600, "tz": "UTC" });
    calendar_api::save(&app, &anna, &body(serde_json::json!({ "event": event }))).await.unwrap();

    let q = format!("from={}&to={}&emails=anna%40example.com,nobody%40else.org", start - 3600, start + 7200);
    let r = calendar_api::freebusy(&app, &user, &q).unwrap();
    let people = r["people"].as_array().unwrap();
    let a = &people[0];
    assert_eq!(a["known"], true);
    assert_eq!(a["busy"][0]["start"], start);
    // Free/busy only: no subject.
    assert!(a["busy"][0]["subject"].is_null());
    assert_eq!(people[1]["known"], false);

    // Once Anna shares her calendar for reading, the subject shows.
    app.store.set_calendar_share(anna.id, user.id, Some("read")).unwrap();
    let r = calendar_api::freebusy(&app, &user, &q).unwrap();
    assert_eq!(r["people"][0]["busy"][0]["subject"], "Dentist");
}

#[tokio::test]
async fn shared_calendar_levels_control_what_is_seen_and_changed() {
    let (app, user) = App::test();
    let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
    let start = 1_800_000_000i64;
    let ev = |uid: &str, subject: &str, extra: serde_json::Value| {
        let mut e = serde_json::json!({ "uid": uid, "subject": subject, "start": start, "end": start + 3600, "tz": "UTC" });
        e.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        e
    };
    let plain = calendar_api::save(&app, &anna, &body(serde_json::json!({ "event": ev("p", "Lunch", serde_json::json!({})) }))).await.unwrap()["id"].as_i64().unwrap();
    calendar_api::save(&app, &anna, &body(serde_json::json!({ "event": ev("s", "Therapy", serde_json::json!({ "sensitivity": "private" })) }))).await.unwrap();
    let range = format!("from={}&to={}&owner={}", start - 1, start + 7200, anna.id);

    // Without a share: blocks only, nothing to open or change.
    let r = calendar_api::list(&app, &user, &range).unwrap();
    assert_eq!(r["access"], "freebusy");
    assert!(r["events"].as_array().unwrap().iter().all(|e| e["subject"].is_null() && e["id"].is_null()));
    assert!(calendar_api::get(&app, &user, &format!("id={plain}")).is_err());

    // Read: details show, except the private event; still not changeable.
    calendar_api::share(&app, &anna, &body(serde_json::json!({ "email": "niklas@example.com", "level": "read" }))).unwrap();
    let r = calendar_api::list(&app, &user, &range).unwrap();
    let evs = r["events"].as_array().unwrap();
    assert!(evs.iter().any(|e| e["subject"] == "Lunch"));
    assert!(!evs.iter().any(|e| e["subject"] == "Therapy"));
    assert_eq!(calendar_api::get(&app, &user, &format!("id={plain}")).unwrap()["canEdit"], false);
    let mut changed = calendar_api::get(&app, &user, &format!("id={plain}")).unwrap()["event"].clone();
    changed["subject"] = "Hacked".into();
    assert!(calendar_api::save(&app, &user, &body(serde_json::json!({ "id": plain, "event": changed.clone() }))).await.is_err());

    // Write: the event can be changed, and a new one added to Anna's calendar.
    calendar_api::share(&app, &anna, &body(serde_json::json!({ "email": "niklas@example.com", "level": "write" }))).unwrap();
    calendar_api::save(&app, &user, &body(serde_json::json!({ "id": plain, "event": changed }))).await.unwrap();
    assert_eq!(calendar_api::get(&app, &anna, &format!("id={plain}")).unwrap()["event"]["subject"], "Hacked");
    let added = calendar_api::save(&app, &user, &body(serde_json::json!({ "owner": anna.id, "event": ev("n", "Added", serde_json::json!({})) }))).await.unwrap()["id"].as_i64().unwrap();
    assert_eq!(app.store.item(added).unwrap().unwrap().user_id, anna.id);
    // But not a meeting in Anna's name.
    let mtg = ev("m", "Mtg", serde_json::json!({ "attendees": [ { "email": "x@other.org" } ] }));
    assert!(calendar_api::save(&app, &user, &body(serde_json::json!({ "owner": anna.id, "event": mtg }))).await.is_err());

    // The share shows up on both sides, and can be withdrawn.
    assert_eq!(calendar_api::shares(&app, &anna).unwrap()["given"][0]["level"], "write");
    assert_eq!(calendar_api::shares(&app, &user).unwrap()["received"][0]["access"], "write");
    calendar_api::share(&app, &anna, &body(serde_json::json!({ "email": "niklas@example.com", "level": "none" }))).unwrap();
    assert!(calendar_api::shares(&app, &user).unwrap()["received"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn calendar_export_and_import_round_trip() {
    let (app, user) = App::test();
    let anna = app.store.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
    let start = 1_800_000_000i64;
    let series = serde_json::json!({
        "uid": "ser-1", "subject": "Weekly", "start": start, "end": start + 3600, "tz": "Europe/Berlin",
        // 1_800_000_000 falls on a Friday (day mask 32).
        "recurrence": { "freq": "weekly", "interval": 1, "days": 32, "count": 4 }
    });
    let id = calendar_api::save(&app, &user, &body(serde_json::json!({ "event": series }))).await.unwrap()["id"].as_i64().unwrap();
    calendar_api::occurrence(&app, &user, &body(serde_json::json!({ "id": id, "occurrence": start + 7 * 86_400, "subject": "Weekly (moved)", "start": start + 7 * 86_400 + 7200, "end": start + 7 * 86_400 + 10_800 }))).await.unwrap();
    calendar_api::save(&app, &user, &body(serde_json::json!({ "event": { "uid": "one-1", "subject": "Once", "start": start + 3600, "end": start + 5400, "tz": "UTC" } }))).await.unwrap();

    let r = calendar_api::export(&app, &user, "");
    let bytes = axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap();
    let ics = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(ics.starts_with("BEGIN:VCALENDAR") && ics.contains("METHOD:PUBLISH"));
    // Each time zone definition is included once, however many events use it.
    let zones: Vec<&str> = ics.split("BEGIN:VTIMEZONE").skip(1).map(|b| b.split("END:VTIMEZONE").next().unwrap()).collect();
    assert!(!zones.is_empty());
    assert!(zones.iter().enumerate().all(|(i, z)| !zones[..i].contains(z)));

    let r = calendar_api::import(&app, &anna, &Bytes::from(ics.clone())).unwrap();
    assert_eq!(r["added"], 2);
    let range = format!("from={}&to={}", start - 1, start + 30 * 86_400);
    let events = calendar_api::list(&app, &anna, &range).unwrap()["events"].as_array().unwrap().clone();
    // Four weekly occurrences plus the single event, with the moved one intact.
    assert_eq!(events.len(), 5);
    assert!(events.iter().any(|e| e["subject"] == "Weekly (moved)" && e["start"] == start + 7 * 86_400 + 7200));

    // Importing the same file again replaces rather than duplicates.
    let r = calendar_api::import(&app, &anna, &Bytes::from(ics)).unwrap();
    assert_eq!(r["added"], 0);
    assert_eq!(r["updated"], 2);
    assert_eq!(calendar_api::list(&app, &anna, &range).unwrap()["events"].as_array().unwrap().len(), 5);
    assert!(calendar_api::import(&app, &anna, &Bytes::from_static(b"hello")).is_err());
}

#[test]
fn library_folder_permissions_checkout_and_columns() {
    let (app, owner) = App::test();
    let bob = app.store.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
    let eve = app.store.create_user("eve@example.com", "Eve", "pw", "user").unwrap();
    let lib = app.store.create_library(owner.id, "Team").unwrap();
    app.store.set_drive_member(lib, bob.id, crate::cloud::Role::Editor).unwrap();
    let hr = app.store.create_drive_folder(lib, None, "HR", owner.id).unwrap();
    let open = app.store.create_drive_folder(lib, None, "Open", owner.id).unwrap();
    let file = app.store.create_drive_file(lib, Some(open), "spec.txt", 1, "text/plain", owner.id).unwrap();

    // Restrict HR to the owner and Eve (who isn't a library member).
    drive_api::set_restricted(&app, &owner, &body(serde_json::json!({ "item": hr, "restricted": true }))).unwrap();
    // Bob may not manage it.
    assert!(drive_api::set_permission(&app, &bob, &body(serde_json::json!({ "item": hr, "email": "bob@example.com", "role": "owner" }))).is_err());
    drive_api::remove_permission(&app, &owner, &body(serde_json::json!({ "item": hr, "user": bob.id }))).unwrap();
    drive_api::set_permission(&app, &owner, &body(serde_json::json!({ "item": hr, "email": "eve@example.com", "role": "editor" }))).unwrap();
    let p = drive_api::permissions(&app, &owner, &format!("item={hr}")).unwrap();
    assert_eq!(p["restricted"], true);
    assert_eq!(p["entries"].as_array().unwrap().len(), 1);

    let names = |v: &serde_json::Value| v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let root = drive_api::items(&app, &bob, &format!("drive={lib}")).unwrap();
    assert_eq!(names(&root), ["Open"]);
    assert!(drive_api::items(&app, &bob, &format!("drive={lib}&parent={hr}")).is_err());
    assert!(drive_api::create_folder(&app, &bob, &body(serde_json::json!({ "drive": lib, "parent": hr, "name": "x" }))).is_err());

    // Eve reaches HR from "Shared with me", can work in it, but not elsewhere.
    let shared = drive_api::shared_with_me(&app, &eve).unwrap();
    assert!(shared["items"].as_array().unwrap().iter().any(|i| i["kind"] == "folder" && i["item"] == hr));
    let inside = drive_api::items(&app, &eve, &format!("drive={lib}&parent={hr}")).unwrap();
    assert_eq!(inside["canWrite"], true);
    assert_eq!(inside["path"].as_array().unwrap().len(), 1);
    drive_api::create_folder(&app, &eve, &body(serde_json::json!({ "drive": lib, "parent": hr, "name": "2026" }))).unwrap();
    assert!(drive_api::items(&app, &eve, &format!("drive={lib}")).is_err());
    assert!(drive_api::items(&app, &eve, &format!("drive={lib}&parent={open}")).is_err());

    // Check-out: Bob holds the file; the owner can't rename it until it's back.
    drive_api::check_out(&app, &bob, &body(serde_json::json!({ "item": file }))).unwrap();
    let listing = drive_api::items(&app, &owner, &format!("drive={lib}&parent={open}")).unwrap();
    assert_eq!(listing["items"][0]["checkedOut"]["name"], "Bob");
    let err = drive_api::rename_item(&app, &owner, &body(serde_json::json!({ "item": file, "name": "x.txt" }))).unwrap_err();
    assert_eq!(err.0, axum::http::StatusCode::LOCKED);
    // The library owner can discard the check-out.
    drive_api::check_in(&app, &owner, &body(serde_json::json!({ "item": file }))).unwrap();
    drive_api::rename_item(&app, &owner, &body(serde_json::json!({ "item": file, "name": "x.txt" }))).unwrap();

    // Columns: owners define them, editors fill them in.
    assert!(drive_api::add_column(&app, &bob, &body(serde_json::json!({ "drive": lib, "name": "Status", "kind": "text" }))).is_err());
    let col = drive_api::add_column(&app, &owner, &body(serde_json::json!({ "drive": lib, "name": "Status", "kind": "choice", "choices": ["Draft", "Final"] }))).unwrap()["id"].as_i64().unwrap();
    drive_api::set_field(&app, &bob, &body(serde_json::json!({ "item": file, "column": col, "value": "Final" }))).unwrap();
    assert!(drive_api::set_field(&app, &bob, &body(serde_json::json!({ "item": file, "column": col, "value": "Nope" }))).is_err());
    let listing = drive_api::items(&app, &bob, &format!("drive={lib}&parent={open}")).unwrap();
    assert_eq!(listing["columns"][0]["name"], "Status");
    assert_eq!(listing["items"][0]["fields"][col.to_string()], "Final");
}

#[test]
fn library_link_lets_guests_in_but_not_into_restricted_folders() {
    let (app, owner) = App::test();
    let lib = app.store.create_library(owner.id, "Team").unwrap();
    let hr = app.store.create_drive_folder(lib, None, "HR", owner.id).unwrap();
    let docs = app.store.create_drive_folder(lib, None, "Docs", owner.id).unwrap();
    let secret = app.store.create_drive_file(lib, Some(hr), "s.txt", 1, "text/plain", owner.id).unwrap();
    app.store.set_restricted(hr, true).unwrap();
    let token = drive_api::create_library_link(&app, &owner, &body(serde_json::json!({ "drive": lib, "write": true }))).unwrap()["token"].as_str().unwrap().to_string();

    let meta = drive_api::shared_meta(&app, &token).unwrap();
    assert_eq!(meta["kind"], "folder");
    assert_eq!(meta["library"], true);
    assert_eq!(meta["name"], "Team");
    let root = drive_api::shared_items(&app, &token, "").unwrap();
    let names: Vec<_> = root["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Docs"]);
    assert!(drive_api::shared_items(&app, &token, &format!("parent={hr}")).is_err());
    assert!(drive_api::shared_items(&app, &token, &format!("parent={docs}")).is_ok());
    let r = drive_api::shared_download(&app, &token, &format!("item={secret}"));
    assert_eq!(r.status(), axum::http::StatusCode::NOT_FOUND);

    // The link is listed for the owner and can be revoked like an item link.
    let links = drive_api::library_links(&app, &owner, &format!("drive={lib}")).unwrap();
    assert_eq!(links["links"].as_array().unwrap().len(), 1);
    drive_api::revoke_item_link(&app, &owner, &body(serde_json::json!({ "token": token }))).unwrap();
    assert!(drive_api::shared_meta(&app, &token).is_err());
}
