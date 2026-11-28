//! The audit log: who did what and when — Admin Center changes, sign-ins
//! (including failed ones), two-step and app-password changes, sharing, and
//! work in mailboxes opened with full access. Entries are kept for the
//! retention set in the Admin Center (a year by default).

use std::net::IpAddr;

use anyhow::Result;
use serde::Serialize;

use crate::app::Shared;
use crate::db::params;
use crate::store::{now, Store};

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    pub time: i64,
    pub actor: String,
    pub action: String,
    pub detail: String,
}

/// A search over the log, newest first.
#[derive(Debug, Clone)]
pub struct Filter {
    /// Words that must all appear in the actor or the detail.
    pub q: String,
    /// An action or an action prefix (`mailbox` matches `mailbox.create`).
    pub action: String,
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// Only entries older than this id (for paging).
    pub before: Option<i64>,
    pub limit: i64,
}

impl Default for Filter {
    fn default() -> Self {
        Filter { q: String::new(), action: String::new(), from: None, to: None, before: None, limit: 200 }
    }
}

impl Filter {
    /// `?q=&action=&from=&to=&before=&limit=` (times in Unix seconds).
    pub fn from_query(query: &str) -> Filter {
        let mut f = Filter::default();
        for (k, v) in query.split('&').filter_map(|p| p.split_once('=')) {
            let v = crate::webmail::mail_api::decode(v);
            match k {
                "q" => f.q = v.trim().to_lowercase(),
                "action" => f.action = v.trim().to_lowercase(),
                "from" => f.from = v.parse().ok(),
                "to" => f.to = v.parse().ok(),
                "before" => f.before = v.parse().ok(),
                "limit" => f.limit = v.parse::<i64>().unwrap_or(200).clamp(1, 1000),
                _ => {}
            }
        }
        f
    }
}

/// Escape `%`, `_` and `\` for a LIKE pattern.
fn like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

impl Store {
    pub fn audit_search(&self, f: &Filter) -> Result<Vec<Entry>> {
        let mut sql = String::from("SELECT id, time, actor, action, detail FROM audit WHERE 1 = 1");
        let mut args: Vec<crate::db::Val> = Vec::new();
        let mut push = |sql: &mut String, cond: &str, v: crate::db::Val| {
            args.push(v);
            sql.push_str(&cond.replace("?", &format!("?{}", args.len())));
        };
        for w in f.q.split_whitespace().take(5) {
            push(&mut sql, " AND LOWER(actor || ' ' || detail) LIKE ? ESCAPE '\\'", format!("%{}%", like(w)).as_str().into());
        }
        if !f.action.is_empty() {
            push(&mut sql, " AND (action = ? ", f.action.as_str().into());
            push(&mut sql, "OR action LIKE ? ESCAPE '\\')", format!("{}.%", like(&f.action)).as_str().into());
        }
        if let Some(t) = f.from {
            push(&mut sql, " AND time >= ?", t.into());
        }
        if let Some(t) = f.to {
            push(&mut sql, " AND time < ?", t.into());
        }
        if let Some(b) = f.before {
            push(&mut sql, " AND id < ?", b.into());
        }
        push(&mut sql, " ORDER BY id DESC LIMIT ?", f.limit.clamp(1, 50_000).into());
        let db = self.db();
        let st = db.prepare(&sql)?;
        st.query_map(args, |r| Ok(Entry { id: r.get(0)?, time: r.get(1)?, actor: r.get(2)?, action: r.get(3)?, detail: r.get(4)? }))
    }

    /// The distinct action names in the log, for the filter list.
    pub fn audit_actions(&self) -> Result<Vec<String>> {
        let db = self.db();
        let st = db.prepare("SELECT DISTINCT action FROM audit ORDER BY action")?;
        st.query_map(params![], |r| r.get(0))
    }

    /// Drop entries older than `days`; returns how many were removed.
    pub fn purge_audit(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = now() - i64::from(days) * 86_400;
        self.db().execute("DELETE FROM audit WHERE time < ?1", [cutoff])
    }
}

/// Record a failed or locked-out sign-in. The typed address is kept short, as
/// it comes from whoever tried.
pub fn failed_sign_in(app: &Shared, email: &str, surface: &str, ip: IpAddr, locked: bool) {
    let who: String = email.trim().chars().filter(|c| !c.is_control()).take(120).collect();
    let action = if locked { "login.locked" } else { "login.failed" };
    app.store.audit(&who, action, &format!("{surface} {ip}"));
}

/// The entries as CSV (time in UTC, RFC 3339).
pub fn to_csv(list: &[Entry]) -> String {
    let cell = |s: &str| {
        // Keep spreadsheet programs from treating a cell as a formula.
        let s = if s.starts_with(['=', '+', '-', '@']) { format!("'{s}") } else { s.to_string() };
        format!("\"{}\"", s.replace('"', "\"\""))
    };
    let mut out = String::from("time,actor,action,detail\r\n");
    for e in list {
        let t = chrono::DateTime::from_timestamp(e.time, 0).map(|d| d.to_rfc3339()).unwrap_or_default();
        out.push_str(&format!("{},{},{},{}\r\n", cell(&t), cell(&e.actor), cell(&e.action), cell(&e.detail)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    #[test]
    fn search_filters_pages_and_purges() {
        let (app, _) = App::test();
        let s = &app.store;
        s.audit("admin@example.com", "mailbox.create", "anna@example.com");
        s.audit("admin@example.com", "mailbox.delete", "old@example.com");
        s.audit("anna@example.com", "webmail.login", "10.0.0.1");
        s.audit("x_y%@example.com", "login.failed", "webmail 10.0.0.9");
        let all = s.audit_search(&Filter::default()).unwrap();
        assert_eq!(all.len(), 4);
        assert_eq!(all[0].action, "login.failed");
        let f = Filter::from_query("action=mailbox&q=ANNA");
        let hits = s.audit_search(&f).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action, "mailbox.create");
        // `%` and `_` are literal.
        assert_eq!(s.audit_search(&Filter { q: "x_y%".into(), ..Default::default() }).unwrap().len(), 1);
        assert_eq!(s.audit_search(&Filter { q: "_".into(), ..Default::default() }).unwrap().len(), 1);
        // Exact action, and paging.
        assert_eq!(s.audit_search(&Filter { action: "webmail.login".into(), ..Default::default() }).unwrap().len(), 1);
        let page = s.audit_search(&Filter { before: Some(all[1].id), ..Default::default() }).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(s.audit_actions().unwrap().len(), 4);

        let csv = to_csv(&all[..1]);
        assert!(csv.contains("\"login.failed\""));
        assert!(to_csv(&[Entry { id: 1, time: 0, actor: "=cmd".into(), action: "a".into(), detail: "say \"hi\"".into() }]).contains("\"'=cmd\""));

        s.db().execute("UPDATE audit SET time = 0 WHERE action = 'mailbox.delete'", params![]).unwrap();
        assert_eq!(s.purge_audit(30).unwrap(), 1);
        assert_eq!(s.purge_audit(0).unwrap(), 0);
        assert_eq!(s.audit_search(&Filter::default()).unwrap().len(), 3);
    }
}
