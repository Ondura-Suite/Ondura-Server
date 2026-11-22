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

#[test]
fn calendar_save_and_list() {
    let (app, user) = App::test();
    let start = crate::store::now() + 3600;
    let saved = calendar_api::save(&app, &user, &body(serde_json::json!({
        "event": { "uid": "", "subject": "Dentist", "location": "Clinic", "start": start, "end": start + 1800, "tz": "Europe/Berlin" }
    }))).unwrap();
    assert!(saved["id"].as_i64().unwrap() > 0);

    let from = crate::store::now();
    let list = calendar_api::list(&app, &user, &format!("from={}&to={}", from, from + 86400)).unwrap();
    let events = list["events"].as_array().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["subject"], "Dentist");
    assert_eq!(events[0]["location"], "Clinic");
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
