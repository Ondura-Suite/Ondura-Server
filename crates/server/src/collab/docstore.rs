//! Storage and server-side reconciliation for collaborative documents.

use anyhow::{bail, Result};
use serde_json::Value;

use crate::db::{params, OptionalExtension};

use super::ot::{to_u16, TextOp};
use crate::store::{now, Store};

/// Upper bound on a document's size, in UTF-16 units, to bound memory.
const MAX_DOC_LEN: u32 = 2_000_000;

/// One revision in a document's history: (revision, editor email, editor name,
/// timestamp, units added, units removed).
pub type HistoryEntry = (i64, String, String, i64, i64, i64);

/// Count the UTF-16 units inserted and deleted by a stored operation.
fn op_deltas(op_json: &str) -> (i64, i64) {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(op_json) else { return (0, 0) };
    let mut added = 0i64;
    let mut removed = 0i64;
    for it in items {
        if let Some(n) = it.as_i64() {
            if n < 0 {
                removed += -n;
            }
        } else if let Some(s) = it.as_str() {
            added += s.encode_utf16().count() as i64;
        }
    }
    (added, removed)
}

/// Metadata about a collaborative document (no content).
#[derive(Debug, Clone)]
pub struct DocMeta {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub revision: i64,
    pub updated: i64,
    pub can_write: bool,
    pub owned: bool,
}

/// Full document state.
#[derive(Debug, Clone)]
pub struct Doc {
    pub id: i64,
    pub owner_id: i64,
    pub name: String,
    pub content: String,
    pub revision: i64,
}

/// A comment on a document. A thread has an anchor (`start..end`, UTF-16
/// offsets kept up to date as the text changes) and the text it quotes;
/// replies point at their thread through `parent_id`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Comment {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub user_id: i64,
    pub author: String,
    pub email: String,
    pub start: i64,
    pub end: i64,
    pub quote: String,
    pub body: String,
    pub created: i64,
    pub resolved: bool,
}

/// A user's access to a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub read: bool,
    pub write: bool,
}

impl Store {
    pub fn create_doc(&self, owner_id: i64, name: &str) -> Result<i64> {
        let name = name.trim();
        let name = if name.is_empty() { "Untitled" } else { name };
        let ts = now();
        let db = self.db();
        let id: i64 = db.query_row(
            "INSERT INTO collab_docs (owner_id, name, content, revision, created, updated) VALUES (?1, ?2, '', 0, ?3, ?3) RETURNING id",
            params![owner_id, name, ts],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn doc(&self, id: i64) -> Result<Option<Doc>> {
        self
            .db()
            .query_row(
                "SELECT id, owner_id, name, content, revision FROM collab_docs WHERE id = ?1",
                [id],
                |r| {
                    Ok(Doc {
                        id: r.get(0)?,
                        owner_id: r.get(1)?,
                        name: r.get(2)?,
                        content: r.get(3)?,
                        revision: r.get(4)?,
                    })
                },
            )
            .optional()
    }

    /// Documents the user owns or that are shared with them, most recent first.
    pub fn docs_for(&self, user_id: i64) -> Result<Vec<DocMeta>> {
        let db = self.db();
        let stmt = db.prepare(
            "SELECT d.id, d.owner_id, d.name, d.revision, d.updated,
                    CASE WHEN d.owner_id = ?1 THEN 1 ELSE COALESCE(s.can_write, 0) END AS can_write,
                    CASE WHEN d.owner_id = ?1 THEN 1 ELSE 0 END AS owned
             FROM collab_docs d
             LEFT JOIN collab_shares s ON s.doc_id = d.id AND s.user_id = ?1
             WHERE d.owner_id = ?1 OR s.user_id = ?1
             ORDER BY d.updated DESC",
        )?;
        let rows = stmt
            .query_map([user_id], |r| {
                Ok(DocMeta {
                    id: r.get(0)?,
                    owner_id: r.get(1)?,
                    name: r.get(2)?,
                    revision: r.get(3)?,
                    updated: r.get(4)?,
                    can_write: r.get::<_, i64>(5)? != 0,
                    owned: r.get::<_, i64>(6)? != 0,
                })
            })?
;
        Ok(rows)
    }

    /// What access `user_id` has to document `doc_id`.
    pub fn doc_access(&self, user_id: i64, doc_id: i64) -> Result<Option<Access>> {
        let db = self.db();
        let owner: Option<i64> = db.query_row("SELECT owner_id FROM collab_docs WHERE id = ?1", [doc_id], |r| r.get(0)).optional()?;
        let Some(owner) = owner else { return Ok(None) };
        if owner == user_id {
            return Ok(Some(Access { read: true, write: true }));
        }
        let share: Option<i64> = db
            .query_row("SELECT can_write FROM collab_shares WHERE doc_id = ?1 AND user_id = ?2", params![doc_id, user_id], |r| r.get(0))
            .optional()?;
        if let Some(w) = share {
            return Ok(Some(Access { read: true, write: w != 0 }));
        }
        // A document stored in a drive is accessible to that drive's members:
        // a viewer may read, an editor or owner may write. Evaluated inline so
        // the storage lock held above is not taken a second time.
        let level: Option<i64> = db
            .query_row(
                "SELECT CASE \
                        WHEN d.kind = 'personal' AND d.owner_id = ?2 THEN 2 \
                        WHEN m.role IN ('owner', 'editor') THEN 2 \
                        WHEN m.role = 'viewer' THEN 1 ELSE 0 END \
                 FROM drive_items it JOIN drives d ON d.id = it.drive_id \
                 LEFT JOIN drive_members m ON m.drive_id = d.id AND m.user_id = ?2 \
                 WHERE it.doc_id = ?1 AND ((d.kind = 'personal' AND d.owner_id = ?2) OR m.user_id = ?2)",
                params![doc_id, user_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(level.map(|l| Access { read: true, write: l >= 2 }))
    }

    pub fn rename_doc(&self, doc_id: i64, name: &str) -> Result<()> {
        self.db().execute("UPDATE collab_docs SET name = ?1, updated = ?2 WHERE id = ?3", params![name.trim(), now(), doc_id])?;
        Ok(())
    }

    pub fn delete_doc(&self, doc_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM collab_docs WHERE id = ?1", [doc_id])?;
        Ok(())
    }

    pub fn share_doc(&self, doc_id: i64, user_id: i64, can_write: bool) -> Result<()> {
        self.db().execute(
            "INSERT INTO collab_shares (doc_id, user_id, can_write) VALUES (?1, ?2, ?3)
             ON CONFLICT(doc_id, user_id) DO UPDATE SET can_write = excluded.can_write",
            params![doc_id, user_id, can_write as i64],
        )?;
        Ok(())
    }

    pub fn unshare_doc(&self, doc_id: i64, user_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM collab_shares WHERE doc_id = ?1 AND user_id = ?2", params![doc_id, user_id])?;
        Ok(())
    }

    /// Users a document is shared with: (user_id, email, name, can_write).
    /// Documents shared directly with a user (not ones they own), newest first.
    /// Returns, per document, its id, name, the drive item wrapping it, the
    /// owner's display name and the last-updated time.
    pub fn docs_shared_with(&self, user_id: i64) -> Result<Vec<crate::cloud::SharedDoc>> {
        let db = self.db();
        let stmt = db.prepare(
            "SELECT d.id, d.name, i.id, COALESCE(NULLIF(u.name, ''), u.email), d.updated \
             FROM collab_shares s \
             JOIN collab_docs d ON d.id = s.doc_id \
             JOIN users u ON u.id = d.owner_id \
             JOIN drive_items i ON i.doc_id = d.id AND i.trashed = 0 \
             WHERE s.user_id = ?1 AND d.owner_id <> ?1 \
             ORDER BY d.updated DESC, d.id DESC",
        )?;
        stmt.query_map([user_id], |r| {
            Ok(crate::cloud::SharedDoc { doc_id: r.get(0)?, name: r.get(1)?, item_id: r.get(2)?, owner: r.get(3)?, updated: r.get(4)? })
        })
    }

    pub fn doc_shares(&self, doc_id: i64) -> Result<Vec<(i64, String, String, bool)>> {
        let db = self.db();
        let stmt = db.prepare(
            "SELECT u.id, u.email, u.name, s.can_write FROM collab_shares s JOIN users u ON u.id = s.user_id WHERE s.doc_id = ?1 ORDER BY u.email",
        )?;
        let rows = stmt
            .query_map([doc_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0)))?
;
        Ok(rows)
    }

    // ---------------------------------------------------------------- comments

    /// Add a comment to a document: a new thread anchored at `start..end` (UTF-16
    /// offsets) with the text it was made on, or a reply to `parent`.
    pub fn add_comment(&self, doc_id: i64, user_id: i64, parent: Option<i64>, start: u32, end: u32, body: &str) -> Result<i64> {
        let body = body.trim();
        if body.is_empty() {
            bail!("empty comment");
        }
        if body.chars().count() > 10_000 {
            bail!("comment too long");
        }
        let db = self.db();
        if let Some(p) = parent {
            let ok: Option<i64> = db.query_row("SELECT 1 FROM collab_comments WHERE id = ?1 AND doc_id = ?2 AND parent_id IS NULL", params![p, doc_id], |r| r.get(0)).optional()?;
            if ok.is_none() {
                bail!("no such comment");
            }
            return db.query_row(
                "INSERT INTO collab_comments (doc_id, parent_id, user_id, body, created) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
                params![doc_id, p, user_id, body, now()],
                |r| r.get(0),
            );
        }
        let content: String = db.query_row("SELECT content FROM collab_docs WHERE id = ?1", [doc_id], |r| r.get(0))?;
        let text = to_u16(&content);
        let (start, end) = (start.min(text.len() as u32), end.min(text.len() as u32));
        if end < start {
            bail!("invalid range");
        }
        let quote: String = super::ot::from_u16(&text[start as usize..end as usize]).chars().take(500).collect();
        db.query_row(
            "INSERT INTO collab_comments (doc_id, user_id, start_pos, end_pos, quote, body, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING id",
            params![doc_id, user_id, start as i64, end as i64, quote, body, now()],
            |r| r.get(0),
        )
    }

    /// A document's comments, threads first and replies after, oldest first.
    pub fn doc_comments(&self, doc_id: i64) -> Result<Vec<Comment>> {
        let db = self.db();
        let st = db.prepare(
            "SELECT c.id, c.parent_id, c.user_id, COALESCE(NULLIF(u.name, ''), u.email), u.email, c.start_pos, c.end_pos, c.quote, c.body, c.created, c.resolved \
             FROM collab_comments c JOIN users u ON u.id = c.user_id WHERE c.doc_id = ?1 ORDER BY c.created, c.id",
        )?;
        st.query_map([doc_id], |r| {
            Ok(Comment {
                id: r.get(0)?,
                parent_id: r.get(1)?,
                user_id: r.get(2)?,
                author: r.get(3)?,
                email: r.get(4)?,
                start: r.get(5)?,
                end: r.get(6)?,
                quote: r.get(7)?,
                body: r.get(8)?,
                created: r.get(9)?,
                resolved: r.get::<_, i64>(10)? != 0,
            })
        })
    }

    /// One comment, by id, within a document.
    pub fn doc_comment(&self, doc_id: i64, id: i64) -> Result<Option<Comment>> {
        Ok(self.doc_comments(doc_id)?.into_iter().find(|c| c.id == id))
    }

    pub fn set_comment_resolved(&self, id: i64, resolved: bool) -> Result<()> {
        self.db().execute("UPDATE collab_comments SET resolved = ?1 WHERE id = ?2", params![resolved as i64, id])?;
        Ok(())
    }

    /// Delete a comment; deleting a thread deletes its replies.
    pub fn delete_comment(&self, id: i64) -> Result<()> {
        self.db().execute("DELETE FROM collab_comments WHERE id = ?1 OR parent_id = ?1", [id])?;
        Ok(())
    }

    // ------------------------------------------------------------ share links

    /// Create a shareable link for a document and return its token. The link
    /// grants view or edit access to anyone who holds it, signed in or not.
    pub fn create_link(&self, doc_id: i64, can_write: bool) -> Result<String> {
        let mut bytes = [0u8; 24];
        rand::fill(&mut bytes);
        let token = hex::encode(bytes);
        self.db().execute(
            "INSERT INTO collab_links (token, doc_id, can_write, created) VALUES (?1, ?2, ?3, ?4)",
            params![&token, doc_id, can_write as i64, now()],
        )?;
        Ok(token)
    }

    /// Resolve a link token to (doc_id, can_write), if it exists.
    pub fn link(&self, token: &str) -> Result<Option<(i64, bool)>> {
        self.db()
            .query_row("SELECT doc_id, can_write FROM collab_links WHERE token = ?1", [token], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0)))
            .optional()
    }

    /// The owner of the document a link points to (for authorizing changes).
    pub fn link_owner(&self, token: &str) -> Result<Option<i64>> {
        self.db()
            .query_row(
                "SELECT d.owner_id FROM collab_links l JOIN collab_docs d ON d.id = l.doc_id WHERE l.token = ?1",
                [token],
                |r| r.get(0),
            )
            .optional()
    }

    /// A document's links: (token, can_write, created).
    pub fn doc_links(&self, doc_id: i64) -> Result<Vec<(String, bool, i64)>> {
        let db = self.db();
        let stmt = db.prepare("SELECT token, can_write, created FROM collab_links WHERE doc_id = ?1 ORDER BY created")?;
        stmt.query_map([doc_id], |r| Ok((r.get(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?)))
    }

    pub fn set_link_write(&self, token: &str, can_write: bool) -> Result<()> {
        self.db().execute("UPDATE collab_links SET can_write = ?1 WHERE token = ?2", params![can_write as i64, token])?;
        Ok(())
    }

    pub fn revoke_link(&self, token: &str) -> Result<()> {
        self.db().execute("DELETE FROM collab_links WHERE token = ?1", [token])?;
        Ok(())
    }

    /// Revision history, newest first: (revision, editor email, editor name,
    /// timestamp, units added, units deleted).
    pub fn doc_history(&self, doc_id: i64, limit: i64) -> Result<Vec<HistoryEntry>> {
        let db = self.db();
        let stmt = db.prepare(
            "SELECT o.revision, COALESCE(u.email, '?'), COALESCE(u.name, ''), o.ts, o.op
             FROM collab_ops o LEFT JOIN users u ON u.id = o.user_id
             WHERE o.doc_id = ?1 ORDER BY o.revision DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![doc_id, limit.clamp(1, 1000)], |r| {
                let rev: i64 = r.get(0)?;
                let email: String = r.get(1)?;
                let name: String = r.get(2)?;
                let ts: i64 = r.get(3)?;
                let op: String = r.get(4)?;
                Ok((rev, email, name, ts, op))
            })?
;
        rows.into_iter()
            .map(|(rev, email, name, ts, op)| {
                let (added, removed) = op_deltas(&op);
                Ok((rev, email, name, ts, added, removed))
            })
            .collect()
    }

    /// Reconstruct the document content as it stood at `revision` by replaying
    /// the operation log from the start.
    pub fn doc_content_at(&self, doc_id: i64, revision: i64) -> Result<String> {
        let ops = {
            let db = self.db();
            let stmt = db.prepare("SELECT op, base_len FROM collab_ops WHERE doc_id = ?1 AND revision <= ?2 ORDER BY revision")?;
            stmt.query_map(params![doc_id, revision], |r| {
                let op: String = r.get(0)?;
                let bl: u32 = r.get(1)?;
                Ok((op, bl))
            })?

        };
        let mut doc: Vec<u16> = Vec::new();
        for (op_json, base_len) in ops {
            let v: Value = serde_json::from_str(&op_json)?;
            let op = TextOp::from_json(&v, base_len).map_err(|e| anyhow::anyhow!(e))?;
            doc = op.apply(&doc).map_err(|e| anyhow::anyhow!(e))?;
        }
        Ok(super::ot::from_u16(&doc))
    }

    /// Operations with revision greater than `since`, for client catch-up.
    pub fn ops_since(&self, doc_id: i64, since: i64) -> Result<Vec<(i64, Value)>> {
        let db = self.db();
        let stmt = db.prepare("SELECT revision, op FROM collab_ops WHERE doc_id = ?1 AND revision > ?2 ORDER BY revision")?;
        let rows = stmt
            .query_map(params![doc_id, since], |r| {
                let rev: i64 = r.get(0)?;
                let op: String = r.get(1)?;
                Ok((rev, op))
            })?
;
        rows.into_iter()
            .map(|(rev, op)| Ok((rev, serde_json::from_str(&op)?)))
            .collect()
    }

    /// Apply a client operation made against `base_rev`. The server transforms it
    /// over any operations committed since `base_rev`, applies it, bumps the
    /// revision and logs it. Returns `(new_revision, transformed_op)` to
    /// broadcast to the other participants. Runs under the store lock, so
    /// concurrent submissions are serialised and can't race.
    pub fn apply_op(&self, doc_id: i64, user_id: i64, base_rev: i64, op_json: &Value) -> Result<(i64, Value)> {
        let mut db = self.db();
        let tx = db.transaction()?;
        let (mut content, cur_rev): (String, i64) =
            tx.query_row("SELECT content, revision FROM collab_docs WHERE id = ?1", [doc_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if base_rev > cur_rev || base_rev < 0 {
            bail!("base revision {base_rev} out of range (current {cur_rev})");
        }

        // The document length the client built its operation against.
        let base_len: u32 = if base_rev == cur_rev {
            to_u16(&content).len() as u32
        } else {
            // The first concurrent op records the length it applied to.
            tx.query_row(
                "SELECT base_len FROM collab_ops WHERE doc_id = ?1 AND revision = ?2",
                params![doc_id, base_rev + 1],
                |r| r.get(0),
            )?
        };

        let mut incoming = TextOp::from_json(op_json, base_len).map_err(|e| anyhow::anyhow!(e))?;

        // Transform the incoming op forward over each already-committed op.
        let concurrent: Vec<(Value, u32)> = tx
            .query_all("SELECT revision, op, base_len FROM collab_ops WHERE doc_id = ?1 AND revision > ?2 ORDER BY revision", params![doc_id, base_rev], |r| {
                let op: String = r.get(1)?;
                let bl: u32 = r.get(2)?;
                Ok((op, bl))
            })?
            .into_iter()
            .map(|(op, bl)| Ok::<_, anyhow::Error>((serde_json::from_str(&op)?, bl)))
            .collect::<Result<Vec<_>>>()?;

        for (server_op_json, server_base_len) in concurrent {
            let server_op = TextOp::from_json(&server_op_json, server_base_len).map_err(|e| anyhow::anyhow!(e))?;
            // incoming and server_op share the same base here.
            let (incoming_prime, _server_prime) = TextOp::transform(&incoming, &server_op).map_err(|e| anyhow::anyhow!(e))?;
            incoming = incoming_prime;
        }

        // Now incoming applies to the current content.
        let doc_u16 = to_u16(&content);
        if incoming.base_len() != doc_u16.len() as u32 {
            bail!("reconciled operation does not fit the current document");
        }
        if incoming.target_len() > MAX_DOC_LEN {
            bail!("document would exceed the size limit");
        }
        let new_u16 = incoming.apply(&doc_u16).map_err(|e| anyhow::anyhow!(e))?;
        content = super::ot::from_u16(&new_u16);
        let new_rev = cur_rev + 1;
        let transformed = incoming.to_json();

        tx.execute("UPDATE collab_docs SET content = ?1, revision = ?2, updated = ?3 WHERE id = ?4", params![content, new_rev, now(), doc_id])?;
        tx.execute(
            "INSERT INTO collab_ops (doc_id, revision, user_id, op, base_len, ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![doc_id, new_rev, user_id, transformed.to_string(), doc_u16.len() as u32, now()],
        )?;
        // Comment anchors move with the text: text typed at the start of a
        // commented range stays outside it, and so does text typed at its end.
        let anchors: Vec<(i64, u32, u32)> = tx.query_all(
            "SELECT id, start_pos, end_pos FROM collab_comments WHERE doc_id = ?1 AND parent_id IS NULL",
            [doc_id],
            |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u32, r.get::<_, i64>(2)? as u32)),
        )?;
        for (cid, start, end) in anchors {
            let s2 = incoming.transform_index(start, true);
            let e2 = incoming.transform_index(end, false).max(s2);
            if (s2, e2) != (start, end) {
                tx.execute("UPDATE collab_comments SET start_pos = ?1, end_pos = ?2 WHERE id = ?3", params![s2 as i64, e2 as i64, cid])?;
            }
        }
        tx.commit()?;
        Ok((new_rev, transformed))
    }
}
