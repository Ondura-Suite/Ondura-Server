//! Document-library features on top of the cloud drives (`cloud.rs`):
//! folder permissions that differ from the library's, check-out / check-in of
//! files, custom metadata columns, and share links for a whole library.
//!
//! Permissions: by default everything in a drive inherits the member roles of
//! the drive. A folder can be *restricted*; from then on its subtree uses the
//! folder's own access list instead. Library owners always keep full access, so
//! a library can never lock its owners out.

use anyhow::{bail, Result};
use serde::Serialize;

use crate::cloud::{ItemLink, Member, Role};
use crate::db::{params, OptionalExtension};
use crate::store::{now, Store};

/// A metadata column defined on a library.
#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub id: i64,
    pub name: String,
    /// `text`, `number`, `date`, `choice` or `yesno`.
    pub kind: String,
    pub choices: Vec<String>,
}

/// Who has a file checked out, and since when.
#[derive(Debug, Clone, Serialize)]
pub struct Checkout {
    pub user_id: i64,
    pub name: String,
    pub email: String,
    pub since: i64,
}

/// The approval state of a file in a library that requires approval.
#[derive(Debug, Clone, Serialize)]
pub struct Approval {
    /// `pending`, `approved` or `rejected`.
    pub status: String,
    pub submitted_by: i64,
    pub submitted_at: i64,
    pub reviewed_by: Option<i64>,
    pub reviewed_at: Option<i64>,
    pub note: String,
}

/// A folder someone outside its library was given access to.
#[derive(Debug, Clone, Serialize)]
pub struct GrantedFolder {
    pub item_id: i64,
    pub drive_id: i64,
    pub drive_name: String,
    pub name: String,
    pub role: String,
    pub updated: i64,
}

pub const COLUMN_KINDS: [&str; 5] = ["text", "number", "date", "choice", "yesno"];

impl Store {
    /// `personal` or `library`, or `None` for an unknown drive.
    pub fn drive_kind(&self, drive_id: i64) -> Result<Option<String>> {
        self.db().query_row("SELECT kind FROM drives WHERE id = ?1", [drive_id], |r| r.get(0)).optional()
    }

    // ---------------------------------------------------------- permissions

    /// The nearest restricted folder at or above `item`, if any.
    pub fn restriction_of(&self, item: i64) -> Result<Option<i64>> {
        self.db()
            .query_row(
                "WITH RECURSIVE up(id, parent_id, depth) AS ( \
                     SELECT id, parent_id, 0 FROM drive_items WHERE id = ?1 \
                     UNION ALL SELECT d.id, d.parent_id, up.depth + 1 FROM drive_items d JOIN up ON d.id = up.parent_id WHERE up.depth < 256) \
                 SELECT up.id FROM up JOIN drive_restricted r ON r.item_id = up.id ORDER BY up.depth LIMIT 1",
                [item],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn is_restricted(&self, item: i64) -> Result<bool> {
        Ok(self.db().query_row("SELECT 1 FROM drive_restricted WHERE item_id = ?1", [item], |r| r.get::<_, i64>(0)).optional()?.is_some())
    }

    /// The user's effective role on `item` (or the drive root when `None`):
    /// the drive role, unless a restricted folder above decides otherwise.
    pub fn item_role(&self, user_id: i64, drive_id: i64, item: Option<i64>) -> Result<Option<Role>> {
        let drive_role = self.role_in_drive(user_id, drive_id)?;
        if drive_role.is_some_and(|r| r.can_admin()) {
            return Ok(drive_role);
        }
        let Some(item) = item else { return Ok(drive_role) };
        let Some(folder) = self.restriction_of(item)? else { return Ok(drive_role) };
        let role: Option<String> = self
            .db()
            .query_row("SELECT role FROM drive_item_acl WHERE item_id = ?1 AND user_id = ?2", params![folder, user_id], |r| r.get(0))
            .optional()?;
        Ok(role.map(|r| Role::parse(&r)))
    }

    /// Give a folder its own permissions (`on`), starting from a copy of the
    /// library's members, or make it inherit from the library again.
    pub fn set_restricted(&self, item: i64, on: bool) -> Result<()> {
        let db = self.db();
        if !on {
            db.execute("DELETE FROM drive_item_acl WHERE item_id = ?1", [item])?;
            db.execute("DELETE FROM drive_restricted WHERE item_id = ?1", [item])?;
            return Ok(());
        }
        let kind: Option<String> = db.query_row("SELECT kind FROM drive_items WHERE id = ?1", [item], |r| r.get(0)).optional()?;
        if kind.as_deref() != Some("folder") {
            bail!("only folders can have their own permissions");
        }
        if db.query_row("SELECT 1 FROM drive_restricted WHERE item_id = ?1", [item], |r| r.get::<_, i64>(0)).optional()?.is_some() {
            return Ok(());
        }
        db.execute("INSERT INTO drive_restricted (item_id) VALUES (?1)", [item])?;
        db.execute(
            "INSERT INTO drive_item_acl (item_id, user_id, role) \
             SELECT ?1, m.user_id, m.role FROM drive_members m JOIN drive_items i ON i.drive_id = m.drive_id \
             WHERE i.id = ?1 AND m.role <> 'owner'",
            [item],
        )?;
        Ok(())
    }

    /// The access list of a restricted folder.
    pub fn item_acl(&self, item: i64) -> Result<Vec<Member>> {
        let db = self.db();
        let st = db.prepare(
            "SELECT u.id, u.email, u.name, a.role FROM drive_item_acl a JOIN users u ON u.id = a.user_id \
             WHERE a.item_id = ?1 ORDER BY CASE a.role WHEN 'owner' THEN 0 WHEN 'editor' THEN 1 ELSE 2 END, LOWER(u.email)",
        )?;
        st.query_map([item], |r| Ok(Member { user_id: r.get(0)?, email: r.get(1)?, name: r.get(2)?, role: r.get(3)? }))
    }

    pub fn set_item_acl(&self, item: i64, user_id: i64, role: Role) -> Result<()> {
        self.db().execute(
            "INSERT INTO drive_item_acl (item_id, user_id, role) VALUES (?1, ?2, ?3) \
             ON CONFLICT (item_id, user_id) DO UPDATE SET role = excluded.role",
            params![item, user_id, role.as_str()],
        )?;
        Ok(())
    }

    pub fn remove_item_acl(&self, item: i64, user_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM drive_item_acl WHERE item_id = ?1 AND user_id = ?2", params![item, user_id])?;
        Ok(())
    }

    /// Restricted folders the user was added to without being a member of the
    /// library, for "Shared with me".
    pub fn folders_granted_to(&self, user_id: i64) -> Result<Vec<GrantedFolder>> {
        let db = self.db();
        let st = db.prepare(
            "SELECT i.id, i.drive_id, d.name, i.name, a.role, i.updated FROM drive_item_acl a \
             JOIN drive_restricted r ON r.item_id = a.item_id \
             JOIN drive_items i ON i.id = a.item_id \
             JOIN drives d ON d.id = i.drive_id \
             WHERE a.user_id = ?1 AND i.trashed = 0 \
               AND NOT EXISTS (SELECT 1 FROM drive_members m WHERE m.drive_id = i.drive_id AND m.user_id = ?1) \
             ORDER BY LOWER(i.name), i.id",
        )?;
        st.query_map([user_id], |r| {
            Ok(GrantedFolder { item_id: r.get(0)?, drive_id: r.get(1)?, drive_name: r.get(2)?, name: r.get(3)?, role: r.get(4)?, updated: r.get(5)? })
        })
    }

    // ------------------------------------------------------------ check-out

    /// Check a file out to `user_id`. Fails if someone else has it.
    pub fn check_out(&self, item: i64, user_id: i64) -> Result<()> {
        let db = self.db();
        let holder: Option<i64> = db.query_row("SELECT user_id FROM drive_checkouts WHERE item_id = ?1", [item], |r| r.get(0)).optional()?;
        match holder {
            Some(h) if h == user_id => Ok(()),
            Some(_) => bail!("checked out by someone else"),
            None => {
                db.execute("INSERT INTO drive_checkouts (item_id, user_id, since) VALUES (?1, ?2, ?3)", params![item, user_id, now()])?;
                Ok(())
            }
        }
    }

    pub fn check_in(&self, item: i64) -> Result<()> {
        self.db().execute("DELETE FROM drive_checkouts WHERE item_id = ?1", [item])?;
        Ok(())
    }

    pub fn checkout_of(&self, item: i64) -> Result<Option<Checkout>> {
        self.db()
            .query_row(
                "SELECT c.user_id, u.name, u.email, c.since FROM drive_checkouts c JOIN users u ON u.id = c.user_id WHERE c.item_id = ?1",
                [item],
                |r| Ok(Checkout { user_id: r.get(0)?, name: r.get(1)?, email: r.get(2)?, since: r.get(3)? }),
            )
            .optional()
    }

    /// Check-outs of the direct children of a folder (or the drive root).
    pub fn checkouts_in_folder(&self, drive_id: i64, parent: Option<i64>) -> Result<Vec<(i64, Checkout)>> {
        let db = self.db();
        let sql = "SELECT c.item_id, c.user_id, u.name, u.email, c.since FROM drive_checkouts c \
                   JOIN users u ON u.id = c.user_id JOIN drive_items i ON i.id = c.item_id WHERE i.drive_id = ?1 AND ";
        let row = |r: &crate::db::Row| Ok((r.get(0)?, Checkout { user_id: r.get(1)?, name: r.get(2)?, email: r.get(3)?, since: r.get(4)? }));
        match parent {
            None => db.prepare(&format!("{sql} i.parent_id IS NULL"))?.query_map([drive_id], row),
            Some(p) => db.prepare(&format!("{sql} i.parent_id = ?2"))?.query_map(params![drive_id, p], row),
        }
    }

    /// Whether `item` or anything below it is checked out by someone other than
    /// `user_id` (`None`: by anyone at all, for guests).
    pub fn locked_for(&self, item: i64, user_id: Option<i64>) -> Result<bool> {
        let found: Option<i64> = self
            .db()
            .query_row(
                "WITH RECURSIVE sub(id) AS (SELECT id FROM drive_items WHERE id = ?1 UNION ALL SELECT d.id FROM drive_items d JOIN sub ON d.parent_id = sub.id) \
                 SELECT 1 FROM drive_checkouts c JOIN sub ON c.item_id = sub.id WHERE c.user_id <> ?2 LIMIT 1",
                params![item, user_id.unwrap_or(-1)],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    // -------------------------------------------------------------- columns

    pub fn drive_columns(&self, drive_id: i64) -> Result<Vec<Column>> {
        let db = self.db();
        let st = db.prepare("SELECT id, name, kind, choices FROM drive_columns WHERE drive_id = ?1 ORDER BY position, id")?;
        st.query_map([drive_id], |r| {
            let choices: String = r.get(3)?;
            Ok(Column { id: r.get(0)?, name: r.get(1)?, kind: r.get(2)?, choices: choices.lines().map(str::to_string).collect() })
        })
    }

    pub fn add_drive_column(&self, drive_id: i64, name: &str, kind: &str, choices: &[String]) -> Result<i64> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 64 {
            bail!("invalid column name");
        }
        if !COLUMN_KINDS.contains(&kind) {
            bail!("unknown column type");
        }
        let choices: Vec<&str> = choices.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).collect();
        if kind == "choice" && choices.is_empty() {
            bail!("a choice column needs choices");
        }
        let db = self.db();
        let pos: i64 = db.query_row("SELECT COALESCE(MAX(position), 0) + 1 FROM drive_columns WHERE drive_id = ?1", [drive_id], |r| r.get(0))?;
        db.query_row(
            "INSERT INTO drive_columns (drive_id, name, kind, choices, position) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            params![drive_id, name, kind, choices.join("\n"), pos],
            |r| r.get(0),
        )
    }

    pub fn delete_drive_column(&self, drive_id: i64, column_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM drive_columns WHERE id = ?1 AND drive_id = ?2", params![column_id, drive_id])?;
        Ok(())
    }

    /// Metadata values of the direct children of a folder: `(item, column, value)`.
    pub fn fields_in_folder(&self, drive_id: i64, parent: Option<i64>) -> Result<Vec<(i64, i64, String)>> {
        let db = self.db();
        let sql = "SELECT f.item_id, f.column_id, f.value FROM drive_item_fields f JOIN drive_items i ON i.id = f.item_id WHERE i.drive_id = ?1 AND ";
        let row = |r: &crate::db::Row| Ok((r.get(0)?, r.get(1)?, r.get(2)?));
        match parent {
            None => db.prepare(&format!("{sql} i.parent_id IS NULL"))?.query_map([drive_id], row),
            Some(p) => db.prepare(&format!("{sql} i.parent_id = ?2"))?.query_map(params![drive_id, p], row),
        }
    }

    /// Set one metadata value on an item, checked against the column type. An
    /// empty value clears it.
    pub fn set_item_field(&self, item: i64, column_id: i64, value: &str) -> Result<()> {
        let db = self.db();
        let col: Option<(String, String)> = db
            .query_row(
                "SELECT c.kind, c.choices FROM drive_columns c JOIN drive_items i ON i.drive_id = c.drive_id WHERE c.id = ?1 AND i.id = ?2",
                params![column_id, item],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((kind, choices)) = col else { bail!("unknown column") };
        let value = value.trim();
        if value.is_empty() {
            db.execute("DELETE FROM drive_item_fields WHERE item_id = ?1 AND column_id = ?2", params![item, column_id])?;
            return Ok(());
        }
        let valid = match kind.as_str() {
            "number" => value.parse::<f64>().is_ok_and(f64::is_finite),
            "date" => valid_date(value),
            "choice" => choices.lines().any(|c| c == value),
            "yesno" => value == "1" || value == "0",
            _ => value.chars().count() <= 1000,
        };
        if !valid {
            bail!("invalid value");
        }
        db.execute(
            "INSERT INTO drive_item_fields (item_id, column_id, value) VALUES (?1, ?2, ?3) \
             ON CONFLICT (item_id, column_id) DO UPDATE SET value = excluded.value",
            params![item, column_id, value],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ approval

    /// Whether new and changed files in a library wait for an owner's approval.
    pub fn requires_approval(&self, drive_id: i64) -> Result<bool> {
        let v: Option<i64> = self.db().query_row("SELECT require_approval FROM drive_settings WHERE drive_id = ?1", [drive_id], |r| r.get(0)).optional()?;
        Ok(v.unwrap_or(0) != 0)
    }

    pub fn set_requires_approval(&self, drive_id: i64, on: bool) -> Result<()> {
        self.db().execute(
            "INSERT INTO drive_settings (drive_id, require_approval) VALUES (?1, ?2) \
             ON CONFLICT (drive_id) DO UPDATE SET require_approval = excluded.require_approval",
            params![drive_id, on as i64],
        )?;
        Ok(())
    }

    pub fn approval_of(&self, item: i64) -> Result<Option<Approval>> {
        self.db()
            .query_row(
                "SELECT status, submitted_by, submitted_at, reviewed_by, reviewed_at, note FROM drive_approvals WHERE item_id = ?1",
                [item],
                |r| Ok(Approval { status: r.get(0)?, submitted_by: r.get(1)?, submitted_at: r.get(2)?, reviewed_by: r.get(3)?, reviewed_at: r.get(4)?, note: r.get(5)? }),
            )
            .optional()
    }

    /// Approval states of the direct children of a folder (or the drive root).
    pub fn approvals_in_folder(&self, drive_id: i64, parent: Option<i64>) -> Result<Vec<(i64, Approval)>> {
        let db = self.db();
        let sql = "SELECT a.item_id, a.status, a.submitted_by, a.submitted_at, a.reviewed_by, a.reviewed_at, a.note FROM drive_approvals a \
                   JOIN drive_items i ON i.id = a.item_id WHERE i.drive_id = ?1 AND ";
        let row = |r: &crate::db::Row| {
            Ok((r.get(0)?, Approval { status: r.get(1)?, submitted_by: r.get(2)?, submitted_at: r.get(3)?, reviewed_by: r.get(4)?, reviewed_at: r.get(5)?, note: r.get(6)? }))
        };
        match parent {
            None => db.prepare(&format!("{sql} i.parent_id IS NULL"))?.query_map([drive_id], row),
            Some(p) => db.prepare(&format!("{sql} i.parent_id = ?2"))?.query_map(params![drive_id, p], row),
        }
    }

    /// Record that `by` changed a file. In a library that requires approval the
    /// file waits for approval (`Some("pending")`), unless `by` owns the
    /// library, whose changes count as approved (`Some("approved")`). `None`
    /// when the library doesn't require approval.
    pub fn file_changed_for_approval(&self, item: i64, by: i64) -> Result<Option<&'static str>> {
        let Some(it) = self.drive_item(item)? else { return Ok(None) };
        if it.kind != "file" || !self.requires_approval(it.drive_id)? {
            return Ok(None);
        }
        let owner = self.role_in_drive(by, it.drive_id)?.is_some_and(|r| r.can_admin());
        let status = if owner { "approved" } else { "pending" };
        let ts = now();
        let (reviewed_by, reviewed_at) = if owner { (Some(by), Some(ts)) } else { (None, None) };
        self.db().execute(
            "INSERT INTO drive_approvals (item_id, status, submitted_by, submitted_at, reviewed_by, reviewed_at, note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, '') \
             ON CONFLICT (item_id) DO UPDATE SET status = excluded.status, submitted_by = excluded.submitted_by, submitted_at = excluded.submitted_at, \
             reviewed_by = excluded.reviewed_by, reviewed_at = excluded.reviewed_at, note = ''",
            params![item, status, by, ts, reviewed_by, reviewed_at],
        )?;
        Ok(Some(status))
    }

    /// An owner's decision on a waiting file.
    pub fn review_file(&self, item: i64, by: i64, approve: bool, note: &str) -> Result<()> {
        let n = self.db().execute(
            "UPDATE drive_approvals SET status = ?1, reviewed_by = ?2, reviewed_at = ?3, note = ?4 WHERE item_id = ?5",
            params![if approve { "approved" } else { "rejected" }, by, now(), note.trim(), item],
        )?;
        if n == 0 {
            bail!("nothing to approve");
        }
        Ok(())
    }

    /// Whether a file is hidden from someone with read-only access because it
    /// is waiting for approval or was rejected. Files changed before approval
    /// was turned on (no record) count as approved.
    pub fn hidden_until_approved(&self, item: &crate::cloud::Item, role: Role) -> Result<bool> {
        if item.kind != "file" || role.can_write() || !self.requires_approval(item.drive_id)? {
            return Ok(false);
        }
        Ok(self.approval_of(item.id)?.is_some_and(|a| a.status != "approved"))
    }

    // ------------------------------------------------------- link passwords

    /// Protect a share link (item or library) with a password, or remove it
    /// with `None`. Stored as an Argon2 hash.
    pub fn set_link_password(&self, token: &str, password: Option<&str>) -> Result<()> {
        match password.filter(|p| !p.is_empty()) {
            Some(p) => {
                let hash = crate::auth::hash_password(p)?;
                self.db().execute(
                    "INSERT INTO link_passwords (token, hash) VALUES (?1, ?2) ON CONFLICT (token) DO UPDATE SET hash = excluded.hash",
                    params![token, hash],
                )?;
            }
            None => {
                self.db().execute("DELETE FROM link_passwords WHERE token = ?1", [token])?;
            }
        }
        Ok(())
    }

    pub fn link_password_hash(&self, token: &str) -> Result<Option<String>> {
        self.db().query_row("SELECT hash FROM link_passwords WHERE token = ?1", [token], |r| r.get(0)).optional()
    }

    // ------------------------------------------------------------ favorites

    pub fn set_favorite(&self, user_id: i64, item: i64, on: bool) -> Result<()> {
        if on {
            self.db().execute(
                "INSERT INTO drive_favorites (user_id, item_id, created) VALUES (?1, ?2, ?3) ON CONFLICT (user_id, item_id) DO NOTHING",
                params![user_id, item, now()],
            )?;
        } else {
            self.db().execute("DELETE FROM drive_favorites WHERE user_id = ?1 AND item_id = ?2", params![user_id, item])?;
        }
        Ok(())
    }

    /// The user's favorite items (live ones), most recently added first.
    pub fn favorites(&self, user_id: i64) -> Result<Vec<crate::cloud::Item>> {
        let ids: Vec<i64> = {
            let db = self.db();
            let st = db.prepare("SELECT f.item_id FROM drive_favorites f JOIN drive_items i ON i.id = f.item_id WHERE f.user_id = ?1 AND i.trashed = 0 ORDER BY f.created DESC")?;
            st.query_map([user_id], |r| r.get(0))?
        };
        Ok(ids.into_iter().filter_map(|id| self.drive_item(id).ok().flatten()).collect())
    }

    pub fn favorite_ids(&self, user_id: i64) -> Result<Vec<i64>> {
        let db = self.db();
        let st = db.prepare("SELECT item_id FROM drive_favorites WHERE user_id = ?1")?;
        st.query_map([user_id], |r| r.get(0))
    }

    // -------------------------------------------------------- library links

    /// A share link for a whole library: anyone with it can browse (and, for
    /// an edit link, upload to) the library, except restricted folders.
    pub fn create_library_link(&self, drive_id: i64, can_write: bool, expires: Option<i64>, by: i64) -> Result<String> {
        let mut bytes = [0u8; 24];
        rand::fill(&mut bytes);
        let token = hex::encode(bytes);
        self.db().execute(
            "INSERT INTO library_links (token, drive_id, can_write, created, created_by, expires) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![&token, drive_id, can_write as i64, now(), by, expires],
        )?;
        Ok(token)
    }

    /// Resolve a library link: `(drive, library name, can_write, created_by)`,
    /// unless it has expired.
    pub fn library_by_link(&self, token: &str) -> Result<Option<(i64, String, bool, i64)>> {
        let row: Option<(i64, String, bool, i64, Option<i64>)> = self
            .db()
            .query_row(
                "SELECT l.drive_id, d.name, l.can_write, l.created_by, l.expires FROM library_links l JOIN drives d ON d.id = l.drive_id WHERE l.token = ?1",
                [token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        Ok(row.filter(|r| !r.4.is_some_and(|e| e <= now())).map(|(d, n, w, by, _)| (d, n, w, by)))
    }

    /// The library a link belongs to, regardless of expiry.
    pub fn library_link_drive(&self, token: &str) -> Result<Option<i64>> {
        self.db().query_row("SELECT drive_id FROM library_links WHERE token = ?1", [token], |r| r.get(0)).optional()
    }

    pub fn library_links(&self, drive_id: i64) -> Result<Vec<ItemLink>> {
        let db = self.db();
        let st = db.prepare("SELECT token, can_write, created, expires FROM library_links WHERE drive_id = ?1 ORDER BY created")?;
        st.query_map([drive_id], |r| Ok(ItemLink { token: r.get(0)?, can_write: r.get::<_, i64>(1)? != 0, created: r.get(2)?, expires: r.get(3)? }))
    }

    pub fn set_library_link(&self, token: &str, can_write: bool, expires: Option<i64>) -> Result<()> {
        self.db().execute("UPDATE library_links SET can_write = ?1, expires = ?2 WHERE token = ?3", params![can_write as i64, expires, token])?;
        Ok(())
    }

    pub fn revoke_library_link(&self, token: &str) -> Result<()> {
        self.db().execute("DELETE FROM library_links WHERE token = ?1", [token])?;
        Ok(())
    }
}

/// `YYYY-MM-DD` with a plausible month and day.
fn valid_date(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    if p.len() != 3 || p[0].len() != 4 || p[1].len() != 2 || p[2].len() != 2 {
        return false;
    }
    let (Ok(_), Ok(m), Ok(d)) = (p[0].parse::<u32>(), p[1].parse::<u32>(), p[2].parse::<u32>()) else { return false };
    (1..=12).contains(&m) && (1..=31).contains(&d)
}

#[cfg(test)]
#[path = "library_tests.rs"]
mod tests;
