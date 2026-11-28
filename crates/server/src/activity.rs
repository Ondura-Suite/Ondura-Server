//! The activity feed: short notices for an account about things other people
//! did that concern it — a mention in a comment, a document, folder, library or
//! calendar shared with it, an approval to decide or a decision on its file.
//! The web app shows them behind the bell in the top bar.
//!
//! Each notice stores who did it, what it is about and an in-app link; the web
//! app writes the sentence in the reader's language from `kind`.

use anyhow::Result;
use serde::Serialize;

use crate::db::params;
use crate::store::{now, Store};

/// How many notices are kept per account; older ones are dropped.
const KEEP: i64 = 200;

#[derive(Debug, Clone, Serialize)]
pub struct Activity {
    pub id: i64,
    /// `mention`, `doc_shared`, `folder_access`, `library_member`,
    /// `calendar_shared`, `approval_requested`, `approved` or `rejected`.
    pub kind: String,
    /// Display name of the person who caused it.
    pub actor: String,
    /// What it is about (document, folder, library or file name).
    pub subject: String,
    /// Extra text, e.g. the comment or the reviewer's note.
    pub detail: String,
    /// Where it leads inside the web app.
    pub link: String,
    pub created: i64,
    pub read: bool,
}

impl Store {
    /// Record a notice for `user_id`. `actor_id` is never notified about their
    /// own actions.
    /// `by` is the acting account's id and display name.
    pub fn add_activity(&self, user_id: i64, by: (i64, &str), kind: &str, subject: &str, detail: &str, link: &str) -> Result<()> {
        let (actor_id, actor) = by;
        if user_id == actor_id {
            return Ok(());
        }
        let detail: String = detail.chars().take(300).collect();
        let db = self.db();
        db.execute(
            "INSERT INTO activity (user_id, kind, actor, subject, detail, link, created, seen) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
            params![user_id, kind, actor, subject, detail, link, now()],
        )?;
        db.execute(
            "DELETE FROM activity WHERE user_id = ?1 AND id NOT IN (SELECT id FROM activity WHERE user_id = ?1 ORDER BY id DESC LIMIT ?2)",
            params![user_id, KEEP],
        )?;
        Ok(())
    }

    /// The newest notices for an account.
    pub fn activities(&self, user_id: i64, limit: i64) -> Result<Vec<Activity>> {
        let db = self.db();
        let st = db.prepare("SELECT id, kind, actor, subject, detail, link, created, seen FROM activity WHERE user_id = ?1 ORDER BY id DESC LIMIT ?2")?;
        st.query_map(params![user_id, limit], |r| {
            Ok(Activity {
                id: r.get(0)?,
                kind: r.get(1)?,
                actor: r.get(2)?,
                subject: r.get(3)?,
                detail: r.get(4)?,
                link: r.get(5)?,
                created: r.get(6)?,
                read: r.get::<_, i64>(7)? != 0,
            })
        })
    }

    pub fn unread_activities(&self, user_id: i64) -> Result<i64> {
        self.db().query_row("SELECT COUNT(*) FROM activity WHERE user_id = ?1 AND seen = 0", [user_id], |r| r.get(0))
    }

    /// Mark one notice (or, with `None`, all of them) as read.
    pub fn mark_activity_read(&self, user_id: i64, id: Option<i64>) -> Result<()> {
        match id {
            Some(id) => self.db().execute("UPDATE activity SET seen = 1 WHERE user_id = ?1 AND id = ?2", params![user_id, id])?,
            None => self.db().execute("UPDATE activity SET seen = 1 WHERE user_id = ?1", [user_id])?,
        };
        Ok(())
    }
}

/// A person's display name for notices.
pub fn display_name(u: &crate::store::User) -> String {
    if u.name.trim().is_empty() { u.email.clone() } else { u.name.clone() }
}

#[cfg(test)]
mod tests {
    use crate::app::App;

    #[test]
    fn notices_are_listed_newest_first_and_marked_read() {
        let (app, user) = App::test();
        let s = &app.store;
        let anna = s.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
        s.add_activity(user.id, (anna.id, "Anna"), "doc_shared", "Plan", "", "/files#doc=1").unwrap();
        s.add_activity(user.id, (anna.id, "Anna"), "mention", "Plan", "@niklas look", "/files#doc=1").unwrap();
        // Own actions don't notify.
        s.add_activity(user.id, (user.id, "Me"), "mention", "Plan", "", "").unwrap();
        let list = s.activities(user.id, 10).unwrap();
        assert_eq!(list.iter().map(|a| a.kind.as_str()).collect::<Vec<_>>(), ["mention", "doc_shared"]);
        assert_eq!(s.unread_activities(user.id).unwrap(), 2);
        s.mark_activity_read(user.id, Some(list[0].id)).unwrap();
        assert_eq!(s.unread_activities(user.id).unwrap(), 1);
        // Someone else's id doesn't touch this account's notices.
        s.mark_activity_read(anna.id, None).unwrap();
        assert_eq!(s.unread_activities(user.id).unwrap(), 1);
        s.mark_activity_read(user.id, None).unwrap();
        assert_eq!(s.unread_activities(user.id).unwrap(), 0);
    }

    #[test]
    fn only_the_newest_are_kept() {
        let (app, user) = App::test();
        let s = &app.store;
        let anna = s.create_user("anna@example.com", "Anna", "pw", "user").unwrap();
        for i in 0..(super::KEEP + 5) {
            s.add_activity(user.id, (anna.id, "Anna"), "doc_shared", &format!("Doc {i}"), "", "").unwrap();
        }
        let list = s.activities(user.id, 1000).unwrap();
        assert_eq!(list.len() as i64, super::KEEP);
        assert_eq!(list[0].subject, format!("Doc {}", super::KEEP + 4));
    }
}
