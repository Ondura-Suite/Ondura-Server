//! Webmail API tests, driving the handler functions directly against an
//! in-memory store (no network, no SMTP submission).

use axum::body::Bytes;

use super::{calendar_api, contacts_api, mail_api, me, pim_api, settings_api};
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
