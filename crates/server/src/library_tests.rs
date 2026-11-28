use crate::app::App;
use crate::cloud::Role;

#[test]
fn restricted_folder_overrides_library_roles() {
    let (app, owner) = App::test();
    let s = &app.store;
    let bob = s.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
    let eve = s.create_user("eve@example.com", "Eve", "pw", "user").unwrap();
    let lib = s.create_library(owner.id, "Team").unwrap();
    s.set_drive_member(lib, bob.id, Role::Editor).unwrap();
    let hr = s.create_drive_folder(lib, None, "HR", owner.id).unwrap();
    let inner = s.create_drive_folder(lib, Some(hr), "Contracts", owner.id).unwrap();
    let file = s.create_drive_file(lib, Some(inner), "a.pdf", 1, "application/pdf", owner.id).unwrap();

    // Inherited: Bob edits everywhere, Eve sees nothing.
    assert_eq!(s.item_role(bob.id, lib, Some(file)).unwrap(), Some(Role::Editor));
    assert_eq!(s.item_role(eve.id, lib, Some(file)).unwrap(), None);

    // Restricting copies the members (Bob stays an editor) until changed.
    s.set_restricted(hr, true).unwrap();
    assert!(s.is_restricted(hr).unwrap());
    assert_eq!(s.restriction_of(file).unwrap(), Some(hr));
    assert_eq!(s.item_role(bob.id, lib, Some(file)).unwrap(), Some(Role::Editor));

    s.set_item_acl(hr, bob.id, Role::Viewer).unwrap();
    s.set_item_acl(hr, eve.id, Role::Editor).unwrap();
    assert_eq!(s.item_role(bob.id, lib, Some(inner)).unwrap(), Some(Role::Viewer));
    assert_eq!(s.item_role(eve.id, lib, Some(file)).unwrap(), Some(Role::Editor));
    // Outside the folder nothing changed, and the owner keeps full access.
    assert_eq!(s.item_role(eve.id, lib, None).unwrap(), None);
    assert_eq!(s.item_role(owner.id, lib, Some(file)).unwrap(), Some(Role::Owner));

    // Eve is not a member, so the folder shows under "Shared with me".
    let granted = s.folders_granted_to(eve.id).unwrap();
    assert_eq!(granted.len(), 1);
    assert_eq!(granted[0].item_id, hr);
    assert!(s.folders_granted_to(bob.id).unwrap().is_empty());

    s.remove_item_acl(hr, bob.id).unwrap();
    assert_eq!(s.item_role(bob.id, lib, Some(file)).unwrap(), None);

    // Inheriting again drops the folder's own list.
    s.set_restricted(hr, false).unwrap();
    assert_eq!(s.item_role(bob.id, lib, Some(file)).unwrap(), Some(Role::Editor));
    assert_eq!(s.item_role(eve.id, lib, Some(file)).unwrap(), None);
    assert!(s.item_acl(hr).unwrap().is_empty());
    // Files can't be restricted.
    assert!(s.set_restricted(file, true).is_err());
}

#[test]
fn documents_follow_folder_permissions() {
    let (app, owner) = App::test();
    let s = &app.store;
    let bob = s.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
    let lib = s.create_library(owner.id, "Team").unwrap();
    s.set_drive_member(lib, bob.id, Role::Editor).unwrap();
    let f = s.create_drive_folder(lib, None, "Board", owner.id).unwrap();
    let (_, doc) = s.create_drive_doc(lib, Some(f), "Minutes", owner.id).unwrap();
    assert!(s.doc_access(bob.id, doc).unwrap().unwrap().write);
    s.set_restricted(f, true).unwrap();
    s.set_item_acl(f, bob.id, Role::Viewer).unwrap();
    let a = s.doc_access(bob.id, doc).unwrap().unwrap();
    assert!(a.read && !a.write);
    s.remove_item_acl(f, bob.id).unwrap();
    assert!(s.doc_access(bob.id, doc).unwrap().is_none());
}

#[test]
fn checkout_locks_a_file_for_others() {
    let (app, owner) = App::test();
    let s = &app.store;
    let bob = s.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
    let lib = s.create_library(owner.id, "Team").unwrap();
    let folder = s.create_drive_folder(lib, None, "Specs", owner.id).unwrap();
    let file = s.create_drive_file(lib, Some(folder), "spec.docx", 1, "", owner.id).unwrap();

    s.check_out(file, bob.id).unwrap();
    assert!(s.check_out(file, owner.id).is_err());
    assert!(s.check_out(file, bob.id).is_ok());
    assert_eq!(s.checkout_of(file).unwrap().unwrap().user_id, bob.id);
    assert_eq!(s.checkouts_in_folder(lib, Some(folder)).unwrap().len(), 1);
    // Locked for the owner (file and the folder above it), not for Bob.
    assert!(s.locked_for(file, Some(owner.id)).unwrap());
    assert!(s.locked_for(folder, Some(owner.id)).unwrap());
    assert!(!s.locked_for(file, Some(bob.id)).unwrap());
    assert!(s.locked_for(file, None).unwrap());

    s.check_in(file).unwrap();
    assert!(s.checkout_of(file).unwrap().is_none());
    assert!(!s.locked_for(folder, Some(owner.id)).unwrap());
}

#[test]
fn columns_validate_values() {
    let (app, owner) = App::test();
    let s = &app.store;
    let lib = s.create_library(owner.id, "Team").unwrap();
    let other = s.create_library(owner.id, "Other").unwrap();
    let file = s.create_drive_file(lib, None, "a.txt", 1, "", owner.id).unwrap();
    let status = s.add_drive_column(lib, "Status", "choice", &["Draft".into(), "Final".into()]).unwrap();
    let due = s.add_drive_column(lib, "Due", "date", &[]).unwrap();
    let amount = s.add_drive_column(lib, "Amount", "number", &[]).unwrap();
    let foreign = s.add_drive_column(other, "X", "text", &[]).unwrap();
    assert!(s.add_drive_column(lib, "Bad", "color", &[]).is_err());
    assert!(s.add_drive_column(lib, "Empty", "choice", &[]).is_err());
    assert_eq!(s.drive_columns(lib).unwrap().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Status", "Due", "Amount"]);

    s.set_item_field(file, status, "Final").unwrap();
    s.set_item_field(file, due, "2026-12-01").unwrap();
    s.set_item_field(file, amount, "12.5").unwrap();
    assert!(s.set_item_field(file, status, "Other").is_err());
    assert!(s.set_item_field(file, due, "1.12.2026").is_err());
    assert!(s.set_item_field(file, amount, "abc").is_err());
    // A column of another library doesn't apply.
    assert!(s.set_item_field(file, foreign, "x").is_err());
    assert_eq!(s.fields_in_folder(lib, None).unwrap().len(), 3);

    s.set_item_field(file, amount, "").unwrap();
    s.delete_drive_column(lib, status).unwrap();
    assert_eq!(s.fields_in_folder(lib, None).unwrap(), vec![(file, due, "2026-12-01".to_string())]);
}

#[test]
fn library_links_resolve_until_expired() {
    let (app, owner) = App::test();
    let s = &app.store;
    let lib = s.create_library(owner.id, "Team").unwrap();
    let t = s.create_library_link(lib, true, None, owner.id).unwrap();
    assert_eq!(s.library_by_link(&t).unwrap(), Some((lib, "Team".to_string(), true, owner.id)));
    assert_eq!(s.library_links(lib).unwrap().len(), 1);
    s.set_library_link(&t, false, Some(crate::store::now() - 1)).unwrap();
    assert!(s.library_by_link(&t).unwrap().is_none());
    assert_eq!(s.library_link_drive(&t).unwrap(), Some(lib));
    s.revoke_library_link(&t).unwrap();
    assert!(s.library_link_drive(&t).unwrap().is_none());
}

#[test]
fn moving_to_another_library_drops_its_state() {
    let (app, owner) = App::test();
    let s = &app.store;
    let a = s.create_library(owner.id, "A").unwrap();
    let b = s.create_library(owner.id, "B").unwrap();
    let f = s.create_drive_folder(a, None, "F", owner.id).unwrap();
    let col = s.add_drive_column(a, "Note", "text", &[]).unwrap();
    s.set_item_field(f, col, "hi").unwrap();
    s.set_restricted(f, true).unwrap();
    s.move_to_drive(f, b, None, "F", owner.id).unwrap();
    assert!(!s.is_restricted(f).unwrap());
    assert!(s.fields_in_folder(a, None).unwrap().is_empty());
    assert!(s.fields_in_folder(b, None).unwrap().is_empty());
}
