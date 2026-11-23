//! Tests for collaborative-document storage and server-side reconciliation.

use super::ot::{to_u16, TextOp};
use crate::app::App;
use serde_json::json;

#[test]
fn create_apply_and_share() {
    let (app, user) = App::test();
    let store = &app.store;
    let doc = store.create_doc(user.id, "Notes").unwrap();

    // Insert "hello" at revision 0.
    let (rev, _t) = store.apply_op(doc, user.id, 0, &json!(["hello"])).unwrap();
    assert_eq!(rev, 1);
    assert_eq!(store.doc(doc).unwrap().unwrap().content, "hello");

    // Append " world" at revision 1 (retain 5 + insert).
    let (rev, _t) = store.apply_op(doc, user.id, 1, &json!([5, " world"])).unwrap();
    assert_eq!(rev, 2);
    assert_eq!(store.doc(doc).unwrap().unwrap().content, "hello world");

    // Share with a second user (read-only) and check access + listing.
    let other = store.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
    store.share_doc(doc, other.id, false).unwrap();
    let acc = store.doc_access(other.id, doc).unwrap().unwrap();
    assert!(acc.read && !acc.write);
    assert!(store.docs_for(other.id).unwrap().iter().any(|d| d.id == doc && !d.owned && !d.can_write));
    // Owner has write.
    assert!(store.doc_access(user.id, doc).unwrap().unwrap().write);
    // A stranger has no access.
    let stranger = store.create_user("eve@example.com", "Eve", "pw", "user").unwrap();
    assert!(store.doc_access(stranger.id, doc).unwrap().is_none());
}

#[test]
fn concurrent_edits_converge_on_server() {
    let (app, user) = App::test();
    let store = &app.store;
    let bob = store.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
    let doc = store.create_doc(user.id, "Shared").unwrap();
    store.share_doc(doc, bob.id, true).unwrap();

    // Base content "abc" at rev 1.
    store.apply_op(doc, user.id, 0, &json!(["abc"])).unwrap();
    assert_eq!(store.doc(doc).unwrap().unwrap().revision, 1);

    // Both clients branched from revision 1. Alice inserts "X" at start, Bob
    // inserts "Y" at end — submitted against the same base revision.
    let (rev_a, _) = store.apply_op(doc, user.id, 1, &json!(["X", 3])).unwrap();
    assert_eq!(rev_a, 2);
    // Bob still thinks the base is revision 1; the server reconciles.
    let (rev_b, transformed) = store.apply_op(doc, bob.id, 1, &json!([3, "Y"])).unwrap();
    assert_eq!(rev_b, 3);

    // The server document must contain both edits.
    assert_eq!(store.doc(doc).unwrap().unwrap().content, "XabcY");

    // The broadcast op Bob's insert was transformed to must now target position 4.
    let t = TextOp::from_json(&transformed, to_u16("Xabc").len() as u32).unwrap();
    assert_eq!(t.target_len(), 5);
}

#[test]
fn rejects_out_of_range_base() {
    let (app, user) = App::test();
    let store = &app.store;
    let doc = store.create_doc(user.id, "x").unwrap();
    // base revision ahead of the document
    assert!(store.apply_op(doc, user.id, 5, &json!(["hi"])).is_err());
}

#[test]
fn ops_since_returns_tail() {
    let (app, user) = App::test();
    let store = &app.store;
    let doc = store.create_doc(user.id, "x").unwrap();
    store.apply_op(doc, user.id, 0, &json!(["a"])).unwrap();
    store.apply_op(doc, user.id, 1, &json!([1, "b"])).unwrap();
    store.apply_op(doc, user.id, 2, &json!([2, "c"])).unwrap();
    let tail = store.ops_since(doc, 1).unwrap();
    assert_eq!(tail.len(), 2);
    assert_eq!(tail[0].0, 2);
    assert_eq!(tail[1].0, 3);
}
