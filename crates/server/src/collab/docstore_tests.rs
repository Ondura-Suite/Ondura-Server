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
fn history_and_content_at_revision() {
    let (app, user) = App::test();
    let store = &app.store;
    let doc = store.create_doc(user.id, "x").unwrap();
    store.apply_op(doc, user.id, 0, &json!(["hello"])).unwrap(); // +5
    store.apply_op(doc, user.id, 1, &json!([5, " world"])).unwrap(); // +6
    store.apply_op(doc, user.id, 2, &json!([-6, 5])).unwrap(); // delete "hello ", -6

    let hist = store.doc_history(doc, 100).unwrap();
    assert_eq!(hist.len(), 3);
    // newest first
    assert_eq!(hist[0].0, 3);
    assert_eq!(hist[0].5, 6); // removed 6
    assert_eq!(hist[2].0, 1);
    assert_eq!(hist[2].4, 5); // added 5

    // content at each revision via replay
    assert_eq!(store.doc_content_at(doc, 1).unwrap(), "hello");
    assert_eq!(store.doc_content_at(doc, 2).unwrap(), "hello world");
    assert_eq!(store.doc_content_at(doc, 3).unwrap(), "world");
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

#[test]
fn share_links_grant_and_revoke() {
    let (app, user) = App::test();
    let store = &app.store;
    let doc = store.create_doc(user.id, "Plan").unwrap();

    // An edit link resolves to the doc with write access.
    let token = store.create_link(doc, true).unwrap();
    assert_eq!(store.link(&token).unwrap(), Some((doc, true)));
    assert_eq!(store.link_owner(&token).unwrap(), Some(user.id));
    assert_eq!(store.doc_links(doc).unwrap().len(), 1);

    // Downgrade to view-only, then revoke.
    store.set_link_write(&token, false).unwrap();
    assert_eq!(store.link(&token).unwrap(), Some((doc, false)));
    store.revoke_link(&token).unwrap();
    assert_eq!(store.link(&token).unwrap(), None);
    assert!(store.doc_links(doc).unwrap().is_empty());
}

#[test]
fn comment_anchors_follow_concurrent_edits() {
    let (app, user) = App::test();
    let store = &app.store;
    let doc = store.create_doc(user.id, "Spec").unwrap();
    store.apply_op(doc, user.id, 0, &json!(["The quick brown fox"])).unwrap();
    // Comment on "quick" (4..9).
    let c = store.add_comment(doc, user.id, None, 4, 9, "Too vague?").unwrap();
    assert_eq!(store.doc_comment(doc, c).unwrap().unwrap().quote, "quick");

    // Someone inserts "very " in front of it, based on the revision before.
    store.apply_op(doc, user.id, 1, &json!([4, "very ", 15])).unwrap();
    let a = store.doc_comment(doc, c).unwrap().unwrap();
    assert_eq!((a.start, a.end), (9, 14));
    let text = store.doc(doc).unwrap().unwrap().content;
    assert_eq!(&text[a.start as usize..a.end as usize], "quick");

    // Typing right at the end of the range does not extend it.
    store.apply_op(doc, user.id, 2, &json!([14, "!", 10])).unwrap();
    let a = store.doc_comment(doc, c).unwrap().unwrap();
    assert_eq!((a.start, a.end), (9, 14));

    // Deleting the quoted word collapses the anchor but keeps the thread.
    store.apply_op(doc, user.id, 3, &json!([9, -5, 11])).unwrap();
    let a = store.doc_comment(doc, c).unwrap().unwrap();
    assert_eq!((a.start, a.end), (9, 9));
    assert_eq!(a.quote, "quick");
}

#[test]
fn comment_replies_resolve_and_delete() {
    let (app, user) = App::test();
    let store = &app.store;
    let doc = store.create_doc(user.id, "Spec").unwrap();
    store.apply_op(doc, user.id, 0, &json!(["abc"])).unwrap();
    let thread = store.add_comment(doc, user.id, None, 0, 3, "Check this").unwrap();
    let reply = store.add_comment(doc, user.id, Some(thread), 0, 0, "Done").unwrap();
    // A reply to a reply is refused; so is an empty comment.
    assert!(store.add_comment(doc, user.id, Some(reply), 0, 0, "x").is_err());
    assert!(store.add_comment(doc, user.id, None, 0, 1, "   ").is_err());
    assert_eq!(store.doc_comments(doc).unwrap().len(), 2);

    store.set_comment_resolved(thread, true).unwrap();
    assert!(store.doc_comment(doc, thread).unwrap().unwrap().resolved);
    // Deleting the thread removes its replies too.
    store.delete_comment(thread).unwrap();
    assert!(store.doc_comments(doc).unwrap().is_empty());
}
