//! Cloud storage: personal drives and shared libraries (an Office-365 /
//! SharePoint-style layer) that hold folders, uploaded files and collaborative
//! documents. File contents live on disk under the configured directory; this
//! module owns the metadata in the database. The HTTP surface is in
//! `webmail::drive_api`, and the whole feature is gated behind `[cloud] enabled`.

use anyhow::{bail, Result};
use serde::Serialize;

use crate::db::{params, OptionalExtension};
use crate::store::{now, Store};

/// A member's role in a drive, from least to most privileged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Viewer,
    Editor,
    Owner,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Owner => "owner",
        }
    }
    pub fn parse(s: &str) -> Role {
        match s {
            "owner" => Role::Owner,
            "viewer" => Role::Viewer,
            _ => Role::Editor,
        }
    }
    /// Whether this role may change contents (create, upload, edit, delete).
    pub fn can_write(self) -> bool {
        matches!(self, Role::Editor | Role::Owner)
    }
    /// Whether this role may manage the drive itself (members, rename, remove).
    pub fn can_admin(self) -> bool {
        self == Role::Owner
    }
}

/// A drive the current user can see: their personal drive or a shared library.
#[derive(Debug, Clone, Serialize)]
pub struct Drive {
    pub id: i64,
    /// `personal` or `library`.
    pub kind: String,
    pub name: String,
    pub owner_id: i64,
    /// The current user's role in this drive.
    pub role: String,
}

/// One entry inside a drive: a folder, an uploaded file, or a collab document.
#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub id: i64,
    pub drive_id: i64,
    pub parent_id: Option<i64>,
    /// `folder`, `file` or `doc`.
    pub kind: String,
    pub name: String,
    pub size: i64,
    pub content_type: String,
    pub doc_id: Option<i64>,
    pub updated: i64,
}

/// A recently updated item together with the drive it lives in.
#[derive(Debug, Clone, Serialize)]
pub struct RecentItem {
    #[serde(flatten)]
    pub item: Item,
    pub drive_id: i64,
    pub drive_name: String,
    pub drive_kind: String,
}

/// A share link on a file or folder.
#[derive(Debug, Clone, Serialize)]
pub struct ItemLink {
    pub token: String,
    pub can_write: bool,
    pub created: i64,
    /// Unix time the link stops working, or `None` for no expiry.
    pub expires: Option<i64>,
}

/// A stored previous version of a file's contents.
#[derive(Debug, Clone, Serialize)]
pub struct FileVersion {
    pub id: i64,
    pub item_id: i64,
    pub size: i64,
    pub content_type: String,
    pub created: i64,
    pub created_by: i64,
}

/// A member of a shared library.
#[derive(Debug, Clone, Serialize)]
pub struct Member {
    pub user_id: i64,
    pub email: String,
    pub name: String,
    pub role: String,
}

fn item_from_row(r: &crate::db::Row) -> Result<Item> {
    Ok(Item {
        id: r.get(0)?,
        drive_id: r.get(1)?,
        parent_id: r.get(2)?,
        kind: r.get(3)?,
        name: r.get(4)?,
        size: r.get(5)?,
        content_type: r.get(6)?,
        doc_id: r.get(7)?,
        updated: r.get(8)?,
    })
}

const ITEM_COLUMNS: &str = "id, drive_id, parent_id, kind, name, size, content_type, doc_id, updated";

impl Store {
    // -------------------------------------------------------------- drives

    /// Return the id of the user's personal drive, creating it on first use.
    pub fn ensure_personal_drive(&self, user_id: i64) -> Result<i64> {
        let db = self.db();
        if let Some(id) = db
            .query_row("SELECT id FROM drives WHERE kind = 'personal' AND owner_id = ?1", [user_id], |r| r.get::<_, i64>(0))
            .optional()?
        {
            return Ok(id);
        }
        let ts = now();
        let id: i64 = db.query_row(
            "INSERT INTO drives (kind, name, owner_id, created, updated) VALUES ('personal', 'My Files', ?1, ?2, ?2) RETURNING id",
            params![user_id, ts],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Every drive the user can see: their personal drive first, then the
    /// shared libraries they are a member of, by name.
    pub fn list_drives(&self, user_id: i64) -> Result<Vec<Drive>> {
        let personal = self.ensure_personal_drive(user_id)?;
        let db = self.db();
        let st = db.prepare(
            "SELECT d.id, d.kind, d.name, d.owner_id, \
                    CASE WHEN d.kind = 'personal' THEN 'owner' ELSE COALESCE(m.role, '') END AS role \
             FROM drives d \
             LEFT JOIN drive_members m ON m.drive_id = d.id AND m.user_id = ?1 \
             WHERE (d.kind = 'personal' AND d.owner_id = ?1) OR m.user_id = ?1 \
             ORDER BY CASE WHEN d.kind = 'personal' THEN 0 ELSE 1 END, LOWER(d.name), d.id",
        )?;
        let mut drives: Vec<Drive> = st.query_map([user_id], |r| {
            Ok(Drive { id: r.get(0)?, kind: r.get(1)?, name: r.get(2)?, owner_id: r.get(3)?, role: r.get(4)? })
        })?;
        // Guard against the rare case the personal row sorted oddly.
        drives.sort_by_key(|d| (if d.id == personal { 0 } else { 1 }, d.name.to_lowercase()));
        Ok(drives)
    }

    /// Create a shared library owned by `user_id`; returns its id.
    pub fn create_library(&self, user_id: i64, name: &str) -> Result<i64> {
        let name = name.trim();
        let name = if name.is_empty() { "New library" } else { name };
        let ts = now();
        let db = self.db();
        let id: i64 = db.query_row(
            "INSERT INTO drives (kind, name, owner_id, created, updated) VALUES ('library', ?1, ?2, ?3, ?3) RETURNING id",
            params![name, user_id, ts],
            |r| r.get(0),
        )?;
        db.execute("INSERT INTO drive_members (drive_id, user_id, role) VALUES (?1, ?2, 'owner')", params![id, user_id])?;
        Ok(id)
    }

    /// Rename a drive (library). Personal drives keep their name.
    pub fn rename_drive(&self, drive_id: i64, name: &str) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            bail!("name is empty");
        }
        self.db().execute("UPDATE drives SET name = ?1, updated = ?2 WHERE id = ?3 AND kind = 'library'", params![name, now(), drive_id])?;
        Ok(())
    }

    /// Delete a shared library and everything in it. Returns the ids of the
    /// file items whose on-disk contents the caller must remove.
    pub fn delete_library(&self, drive_id: i64) -> Result<Vec<i64>> {
        let files: Vec<i64> = {
            let db = self.db();
            let st = db.prepare("SELECT id FROM drive_items WHERE drive_id = ?1 AND kind = 'file'")?;
            st.query_map([drive_id], |r| r.get::<_, i64>(0))?
        };
        // collab_docs referenced by doc items, then the drive (items cascade).
        let db = self.db();
        db.execute(
            "DELETE FROM collab_docs WHERE id IN (SELECT doc_id FROM drive_items WHERE drive_id = ?1 AND doc_id IS NOT NULL)",
            [drive_id],
        )?;
        db.execute("DELETE FROM drives WHERE id = ?1 AND kind = 'library'", [drive_id])?;
        Ok(files)
    }

    /// The user's role in a drive, or `None` if they have no access.
    pub fn role_in_drive(&self, user_id: i64, drive_id: i64) -> Result<Option<Role>> {
        let db = self.db();
        let row: Option<(String, i64)> = db
            .query_row("SELECT kind, owner_id FROM drives WHERE id = ?1", [drive_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .optional()?;
        let Some((kind, owner_id)) = row else { return Ok(None) };
        if kind == "personal" {
            return Ok(if owner_id == user_id { Some(Role::Owner) } else { None });
        }
        let role: Option<String> = db
            .query_row("SELECT role FROM drive_members WHERE drive_id = ?1 AND user_id = ?2", params![drive_id, user_id], |r| r.get(0))
            .optional()?;
        Ok(role.map(|r| Role::parse(&r)))
    }

    // ------------------------------------------------------------- members

    pub fn drive_members(&self, drive_id: i64) -> Result<Vec<Member>> {
        let db = self.db();
        let st = db.prepare(
            "SELECT u.id, u.email, u.name, m.role FROM drive_members m \
             JOIN users u ON u.id = m.user_id WHERE m.drive_id = ?1 \
             ORDER BY CASE m.role WHEN 'owner' THEN 0 WHEN 'editor' THEN 1 ELSE 2 END, LOWER(u.email)",
        )?;
        st.query_map([drive_id], |r| Ok(Member { user_id: r.get(0)?, email: r.get(1)?, name: r.get(2)?, role: r.get(3)? }))
    }

    /// Add or update a member of a shared library.
    pub fn set_drive_member(&self, drive_id: i64, user_id: i64, role: Role) -> Result<()> {
        self.db().execute(
            "INSERT INTO drive_members (drive_id, user_id, role) VALUES (?1, ?2, ?3) \
             ON CONFLICT (drive_id, user_id) DO UPDATE SET role = excluded.role",
            params![drive_id, user_id, role.as_str()],
        )?;
        Ok(())
    }

    pub fn remove_drive_member(&self, drive_id: i64, user_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM drive_members WHERE drive_id = ?1 AND user_id = ?2 AND role <> 'owner'", params![drive_id, user_id])?;
        Ok(())
    }

    // --------------------------------------------------------------- items

    /// List the direct children of a folder (or the drive root when `parent`
    /// is `None`), folders first and then by name.
    pub fn drive_items(&self, drive_id: i64, parent: Option<i64>) -> Result<Vec<Item>> {
        let db = self.db();
        let order = "ORDER BY CASE kind WHEN 'folder' THEN 0 ELSE 1 END, LOWER(name), id";
        match parent {
            None => {
                let st = db.prepare(&format!("SELECT {ITEM_COLUMNS} FROM drive_items WHERE drive_id = ?1 AND parent_id IS NULL AND trashed = 0 {order}"))?;
                st.query_map([drive_id], item_from_row)
            }
            Some(p) => {
                let st = db.prepare(&format!("SELECT {ITEM_COLUMNS} FROM drive_items WHERE drive_id = ?1 AND parent_id = ?2 AND trashed = 0 {order}"))?;
                st.query_map(params![drive_id, p], item_from_row)
            }
        }
    }

    pub fn drive_item(&self, id: i64) -> Result<Option<Item>> {
        let db = self.db();
        db.query_row(&format!("SELECT {ITEM_COLUMNS} FROM drive_items WHERE id = ?1"), [id], item_from_row).optional()
    }

    /// The drive_item that wraps a given collab document, if any.
    pub fn drive_item_for_doc(&self, doc_id: i64) -> Result<Option<Item>> {
        let db = self.db();
        db.query_row(&format!("SELECT {ITEM_COLUMNS} FROM drive_items WHERE doc_id = ?1"), [doc_id], item_from_row).optional()
    }

    /// Total bytes stored in a drive (sum of file sizes), for quota reporting.
    pub fn drive_total_size(&self, drive_id: i64) -> Result<i64> {
        self.db().query_row(
            "SELECT CAST(COALESCE(SUM(size), 0) AS BIGINT) FROM drive_items WHERE drive_id = ?1 AND kind = 'file'",
            [drive_id],
            |r| r.get(0),
        )
    }

    /// The storage quota that applies to a drive, when it is metered: a personal
    /// drive's owner quota (`0` = unlimited). Libraries are not metered, so this
    /// returns `None` for them.
    pub fn drive_quota(&self, drive_id: i64) -> Result<Option<i64>> {
        self.db()
            .query_row(
                "SELECT u.quota FROM drives d JOIN users u ON u.id = d.owner_id WHERE d.id = ?1 AND d.kind = 'personal'",
                [drive_id],
                |r| r.get(0),
            )
            .optional()
    }

    /// Find a direct child of a folder by exact name (case-sensitive), for
    /// path-based access such as WebDAV.
    pub fn child_by_name(&self, drive_id: i64, parent: Option<i64>, name: &str) -> Result<Option<Item>> {
        let db = self.db();
        match parent {
            None => db
                .query_row(&format!("SELECT {ITEM_COLUMNS} FROM drive_items WHERE drive_id = ?1 AND parent_id IS NULL AND trashed = 0 AND name = ?2"), params![drive_id, name], item_from_row)
                .optional(),
            Some(p) => db
                .query_row(&format!("SELECT {ITEM_COLUMNS} FROM drive_items WHERE drive_id = ?1 AND parent_id = ?3 AND trashed = 0 AND name = ?2"), params![drive_id, name, p], item_from_row)
                .optional(),
        }
    }

    /// Resolve a `/`-separated path to an item in a drive. An empty path means
    /// the drive root and returns `None`.
    pub fn resolve_path(&self, drive_id: i64, path: &str) -> Result<Option<Item>> {
        let mut parent = None;
        let mut last = None;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            match self.child_by_name(drive_id, parent, seg)? {
                Some(it) => {
                    parent = Some(it.id);
                    last = Some(it);
                }
                None => return Ok(None),
            }
        }
        Ok(last)
    }

    /// Ensure every folder along a `/`-separated path exists, creating missing
    /// ones; returns the id of the deepest folder (`None` for the root).
    pub fn ensure_folders(&self, drive_id: i64, path: &str, by: i64) -> Result<Option<i64>> {
        let mut parent = None;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            match self.child_by_name(drive_id, parent, seg)? {
                Some(it) if it.kind == "folder" => parent = Some(it.id),
                Some(_) => bail!("path component is not a folder"),
                None => parent = Some(self.create_drive_folder(drive_id, parent, seg, by)?),
            }
        }
        Ok(parent)
    }

    /// Replace a file item's content metadata after its bytes were rewritten.
    pub fn set_file_meta(&self, id: i64, size: i64, content_type: &str, by: i64) -> Result<()> {
        self.db().execute(
            "UPDATE drive_items SET size = ?1, content_type = ?2, updated = ?3, updated_by = ?4 WHERE id = ?5",
            params![size, content_type, now(), by, id],
        )?;
        Ok(())
    }

    /// Set an item's modification time (WebDAV `X-OC-Mtime`), to preserve the
    /// original timestamp of an uploaded photo.
    pub fn set_item_mtime(&self, id: i64, mtime: i64) -> Result<()> {
        self.db().execute("UPDATE drive_items SET updated = ?1 WHERE id = ?2", params![mtime, id])?;
        Ok(())
    }

    /// Whether `node` is `ancestor` or sits somewhere beneath it, so a move into
    /// it would create a cycle.
    pub fn is_within(&self, ancestor: i64, node: Option<i64>) -> Result<bool> {
        let mut cur = node;
        let db = self.db();
        let mut hops = 0;
        while let Some(c) = cur {
            if c == ancestor {
                return Ok(true);
            }
            cur = db.query_row("SELECT parent_id FROM drive_items WHERE id = ?1", [c], |r| r.get(0)).optional()?.flatten();
            hops += 1;
            if hops > 1024 {
                return Ok(true); // treat a pre-existing cycle as "within"
            }
        }
        Ok(false)
    }

    /// Move an item to a new parent folder and name in one step (WebDAV MOVE).
    pub fn relocate_item(&self, id: i64, parent: Option<i64>, name: &str, by: i64) -> Result<()> {
        if self.is_within(id, parent)? {
            bail!("cannot move an item into itself");
        }
        let name = sanitize_name(name)?;
        self.db().execute(
            "UPDATE drive_items SET parent_id = ?1, name = ?2, updated = ?3, updated_by = ?4 WHERE id = ?5",
            params![parent, name.as_str(), now(), by, id],
        )?;
        Ok(())
    }

    fn touch_drive(db: &crate::db::Db, drive_id: i64) {
        let _ = db.execute("UPDATE drives SET updated = ?1 WHERE id = ?2", params![now(), drive_id]);
    }

    pub fn create_drive_folder(&self, drive_id: i64, parent: Option<i64>, name: &str, by: i64) -> Result<i64> {
        let name = sanitize_name(name)?;
        let ts = now();
        let db = self.db();
        let id: i64 = db.query_row(
            "INSERT INTO drive_items (drive_id, parent_id, kind, name, created, created_by, updated, updated_by) \
             VALUES (?1, ?2, 'folder', ?3, ?4, ?5, ?4, ?5) RETURNING id",
            params![drive_id, parent, name, ts, by],
            |r| r.get(0),
        )?;
        Self::touch_drive(&db, drive_id);
        Ok(id)
    }

    /// Create a collaborative document as an item in a drive; returns the item
    /// id and the underlying collab document id.
    pub fn create_drive_doc(&self, drive_id: i64, parent: Option<i64>, name: &str, by: i64) -> Result<(i64, i64)> {
        let name = sanitize_name(name)?;
        let doc_id = self.create_doc(by, &name)?;
        let ts = now();
        let db = self.db();
        let id: i64 = db.query_row(
            "INSERT INTO drive_items (drive_id, parent_id, kind, name, doc_id, created, created_by, updated, updated_by) \
             VALUES (?1, ?2, 'doc', ?3, ?4, ?5, ?6, ?5, ?6) RETURNING id",
            params![drive_id, parent, name, doc_id, ts, by],
            |r| r.get(0),
        )?;
        Self::touch_drive(&db, drive_id);
        Ok((id, doc_id))
    }

    /// Record a newly uploaded file; the caller writes the bytes to disk.
    pub fn create_drive_file(&self, drive_id: i64, parent: Option<i64>, name: &str, size: i64, content_type: &str, by: i64) -> Result<i64> {
        let name = sanitize_name(name)?;
        let ts = now();
        let db = self.db();
        let id: i64 = db.query_row(
            "INSERT INTO drive_items (drive_id, parent_id, kind, name, size, content_type, created, created_by, updated, updated_by) \
             VALUES (?1, ?2, 'file', ?3, ?4, ?5, ?6, ?7, ?6, ?7) RETURNING id",
            params![drive_id, parent, name, size, content_type, ts, by],
            |r| r.get(0),
        )?;
        Self::touch_drive(&db, drive_id);
        Ok(id)
    }

    pub fn rename_drive_item(&self, id: i64, name: &str, by: i64) -> Result<()> {
        let name = sanitize_name(name)?;
        let db = self.db();
        db.execute("UPDATE drive_items SET name = ?1, updated = ?2, updated_by = ?3 WHERE id = ?4", params![name.as_str(), now(), by, id])?;
        // Keep a wrapped collab document's title in step with the item name.
        db.execute(
            "UPDATE collab_docs SET name = ?1 WHERE id = (SELECT doc_id FROM drive_items WHERE id = ?2)",
            params![name.as_str(), id],
        )?;
        Ok(())
    }

    /// Move an item to a new parent folder (or the root when `parent` is
    /// `None`) within the same drive.
    pub fn move_drive_item(&self, id: i64, parent: Option<i64>, by: i64) -> Result<()> {
        if self.is_within(id, parent)? {
            bail!("cannot move a folder into itself");
        }
        self.db().execute("UPDATE drive_items SET parent_id = ?1, updated = ?2, updated_by = ?3 WHERE id = ?4", params![parent, now(), by, id])?;
        Ok(())
    }

    /// Delete an item (recursively for folders). Returns the ids of the file
    /// items whose on-disk contents the caller must remove.
    pub fn delete_drive_item(&self, id: i64) -> Result<Vec<i64>> {
        // Collect the item and all descendants.
        let mut stack = vec![id];
        let mut all = Vec::new();
        while let Some(cur) = stack.pop() {
            all.push(cur);
            let children: Vec<i64> = {
                let db = self.db();
                let st = db.prepare("SELECT id FROM drive_items WHERE parent_id = ?1")?;
                st.query_map([cur], |r| r.get::<_, i64>(0))?
            };
            stack.extend(children);
        }
        let db = self.db();
        let mut files = Vec::new();
        for item in &all {
            let kind: Option<String> = db.query_row("SELECT kind FROM drive_items WHERE id = ?1", [*item], |r| r.get(0)).optional()?;
            match kind.as_deref() {
                Some("file") => files.push(*item),
                Some("doc") => {
                    db.execute("DELETE FROM collab_docs WHERE id = (SELECT doc_id FROM drive_items WHERE id = ?1)", [*item])?;
                }
                _ => {}
            }
        }
        for item in &all {
            db.execute("DELETE FROM drive_items WHERE id = ?1", [*item])?;
        }
        Ok(files)
    }

    /// Move an item (and, implicitly, its subtree) to the recycle bin.
    pub fn trash_item(&self, id: i64, by: i64) -> Result<()> {
        self.db().execute("UPDATE drive_items SET trashed = 1, trashed_at = ?1, updated_by = ?2 WHERE id = ?3", params![now(), by, id])?;
        Ok(())
    }

    /// Restore a trashed item. If its parent is gone or still trashed, it is
    /// restored to the drive root.
    pub fn restore_item(&self, id: i64) -> Result<()> {
        let db = self.db();
        let parent: Option<i64> = db.query_row("SELECT parent_id FROM drive_items WHERE id = ?1", [id], |r| r.get(0)).optional()?.flatten();
        let parent_ok = match parent {
            None => true,
            Some(p) => db.query_row("SELECT 1 FROM drive_items WHERE id = ?1 AND trashed = 0", [p], |r| r.get::<_, i64>(0)).optional()?.is_some(),
        };
        if parent_ok {
            db.execute("UPDATE drive_items SET trashed = 0, trashed_at = NULL WHERE id = ?1", [id])?;
        } else {
            db.execute("UPDATE drive_items SET trashed = 0, trashed_at = NULL, parent_id = NULL WHERE id = ?1", [id])?;
        }
        Ok(())
    }

    /// The items currently in a drive's recycle bin, most recent first.
    pub fn trashed_items(&self, drive_id: i64) -> Result<Vec<Item>> {
        let db = self.db();
        let st = db.prepare(&format!("SELECT {ITEM_COLUMNS} FROM drive_items WHERE drive_id = ?1 AND trashed = 1 ORDER BY trashed_at DESC, id DESC"))?;
        st.query_map([drive_id], item_from_row)
    }

    /// Permanently delete every trashed item in a drive (and their subtrees).
    /// Returns the file ids whose on-disk contents the caller must remove.
    pub fn empty_trash(&self, drive_id: i64) -> Result<Vec<i64>> {
        let roots: Vec<i64> = {
            let db = self.db();
            let st = db.prepare("SELECT id FROM drive_items WHERE drive_id = ?1 AND trashed = 1")?;
            st.query_map([drive_id], |r| r.get::<_, i64>(0))?
        };
        let mut files = Vec::new();
        for id in roots {
            files.extend(self.delete_drive_item(id)?);
        }
        Ok(files)
    }

    /// Recently updated files and documents across every drive the user can see
    /// (personal drive plus libraries they belong to), newest first. Each entry
    /// carries the drive it lives in so the UI can show its location.
    pub fn recent_items(&self, user_id: i64, limit: i64) -> Result<Vec<RecentItem>> {
        self.ensure_personal_drive(user_id)?;
        let cols: String = ITEM_COLUMNS.split(", ").map(|c| format!("i.{c}")).collect::<Vec<_>>().join(", ");
        let db = self.db();
        let st = db.prepare(&format!(
            "SELECT {cols}, d.id, d.name, d.kind FROM drive_items i \
             JOIN drives d ON d.id = i.drive_id \
             WHERE i.trashed = 0 AND i.kind != 'folder' \
               AND ((d.kind = 'personal' AND d.owner_id = ?1) \
                    OR d.id IN (SELECT drive_id FROM drive_members WHERE user_id = ?1)) \
             ORDER BY i.updated DESC LIMIT ?2"
        ))?;
        st.query_map(params![user_id, limit], |r| {
            Ok(RecentItem {
                item: item_from_row(r)?,
                drive_id: r.get(9)?,
                drive_name: r.get(10)?,
                drive_kind: r.get(11)?,
            })
        })
    }

    // --------------------------------------------------------- file versions

    /// Record the file's current contents as a new version and return its id.
    /// The caller copies the on-disk file to the version's path. Returns `None`
    /// if the item is not a file.
    pub fn snapshot_file_version(&self, item_id: i64, by: i64) -> Result<Option<i64>> {
        let db = self.db();
        let meta: Option<(String, i64, String)> = db
            .query_row("SELECT kind, size, content_type FROM drive_items WHERE id = ?1", [item_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))
            .optional()?;
        let Some((kind, size, ct)) = meta else { return Ok(None) };
        if kind != "file" {
            return Ok(None);
        }
        let id: i64 = db.query_row(
            "INSERT INTO drive_file_versions (item_id, size, content_type, created, created_by) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            params![item_id, size, ct, now(), by],
            |r| r.get(0),
        )?;
        Ok(Some(id))
    }

    /// A file's stored versions, newest first.
    pub fn item_versions(&self, item_id: i64) -> Result<Vec<FileVersion>> {
        let db = self.db();
        let st = db.prepare("SELECT id, item_id, size, content_type, created, created_by FROM drive_file_versions WHERE item_id = ?1 ORDER BY created DESC, id DESC")?;
        st.query_map([item_id], |r| {
            Ok(FileVersion { id: r.get(0)?, item_id: r.get(1)?, size: r.get(2)?, content_type: r.get(3)?, created: r.get(4)?, created_by: r.get(5)? })
        })
    }

    /// Look up one stored version by its id.
    pub fn file_version(&self, version_id: i64) -> Result<Option<FileVersion>> {
        self.db()
            .query_row(
                "SELECT id, item_id, size, content_type, created, created_by FROM drive_file_versions WHERE id = ?1",
                [version_id],
                |r| Ok(FileVersion { id: r.get(0)?, item_id: r.get(1)?, size: r.get(2)?, content_type: r.get(3)?, created: r.get(4)?, created_by: r.get(5)? }),
            )
            .optional()
    }

    /// Drop a stored version's row (the caller removes its file).
    pub fn delete_file_version(&self, version_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM drive_file_versions WHERE id = ?1", [version_id])?;
        Ok(())
    }

    /// Trim a file's version history to the newest `keep`, returning the ids of
    /// the versions dropped so the caller can delete their files.
    pub fn prune_file_versions(&self, item_id: i64, keep: i64) -> Result<Vec<i64>> {
        let db = self.db();
        let st = db.prepare("SELECT id FROM drive_file_versions WHERE item_id = ?1 ORDER BY created DESC, id DESC LIMIT -1 OFFSET ?2")?;
        let gone: Vec<i64> = st.query_map(params![item_id, keep], |r| r.get(0))?;
        for id in &gone {
            db.execute("DELETE FROM drive_file_versions WHERE id = ?1", [*id])?;
        }
        Ok(gone)
    }

    // ------------------------------------------------------------ share links

    /// Create a share link for a file or folder and return its token. The link
    /// grants view or edit access to anyone who holds it, signed in or not,
    /// until `expires` (a Unix time, or `None` for a link that never expires).
    pub fn create_item_link(&self, item_id: i64, can_write: bool, expires: Option<i64>, by: i64) -> Result<String> {
        let mut bytes = [0u8; 24];
        rand::fill(&mut bytes);
        let token = hex::encode(bytes);
        self.db().execute(
            "INSERT INTO drive_links (token, item_id, can_write, created, created_by, expires) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![&token, item_id, can_write as i64, now(), by, expires],
        )?;
        Ok(token)
    }

    /// Resolve a link token to the item it points at, if the link exists, has
    /// not expired, and the item is still live (not trashed).
    pub fn item_by_link(&self, token: &str) -> Result<Option<(Item, bool)>> {
        let row: Option<(i64, bool, Option<i64>)> = self
            .db()
            .query_row("SELECT item_id, can_write, expires FROM drive_links WHERE token = ?1", [token], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? != 0, r.get(2)?)))
            .optional()?;
        let Some((item_id, can_write, expires)) = row else { return Ok(None) };
        if expires.is_some_and(|e| e <= now()) {
            return Ok(None);
        }
        // A trashed item's links stop resolving until it is restored.
        let live: Option<i64> = self.db().query_row("SELECT 1 FROM drive_items WHERE id = ?1 AND trashed = 0", [item_id], |r| r.get(0)).optional()?;
        if live.is_none() {
            return Ok(None);
        }
        match self.drive_item(item_id)? {
            Some(it) => Ok(Some((it, can_write))),
            None => Ok(None),
        }
    }

    /// A share link's item id, regardless of trashed state (for authorizing
    /// changes to the link itself).
    pub fn item_link_item(&self, token: &str) -> Result<Option<i64>> {
        self.db()
            .query_row("SELECT item_id FROM drive_links WHERE token = ?1", [token], |r| r.get(0))
            .optional()
    }

    /// The account that created a share link (a guest's edits are attributed to
    /// the person who shared the link).
    pub fn item_link_creator(&self, token: &str) -> Result<Option<i64>> {
        self.db()
            .query_row("SELECT created_by FROM drive_links WHERE token = ?1", [token], |r| r.get(0))
            .optional()
    }

    /// An item's share links.
    pub fn item_links(&self, item_id: i64) -> Result<Vec<ItemLink>> {
        let db = self.db();
        let stmt = db.prepare("SELECT token, can_write, created, expires FROM drive_links WHERE item_id = ?1 ORDER BY created")?;
        stmt.query_map([item_id], |r| Ok(ItemLink { token: r.get(0)?, can_write: r.get::<_, i64>(1)? != 0, created: r.get(2)?, expires: r.get(3)? }))
    }

    pub fn set_item_link(&self, token: &str, can_write: bool, expires: Option<i64>) -> Result<()> {
        self.db().execute("UPDATE drive_links SET can_write = ?1, expires = ?2 WHERE token = ?3", params![can_write as i64, expires, token])?;
        Ok(())
    }

    pub fn revoke_item_link(&self, token: &str) -> Result<()> {
        self.db().execute("DELETE FROM drive_links WHERE token = ?1", [token])?;
        Ok(())
    }

    /// Breadcrumb trail from the drive root down to `id` (inclusive), each a
    /// `(id, name)` pair.
    pub fn drive_item_path(&self, id: i64) -> Result<Vec<(i64, String)>> {
        let mut trail = Vec::new();
        let mut cur = Some(id);
        let db = self.db();
        while let Some(c) = cur {
            let row: Option<(i64, String, Option<i64>)> = db
                .query_row("SELECT id, name, parent_id FROM drive_items WHERE id = ?1", [c], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .optional()?;
            match row {
                Some((iid, name, parent)) => {
                    trail.push((iid, name));
                    cur = parent;
                }
                None => break,
            }
            if trail.len() > 256 {
                break; // guard against a cycle
            }
        }
        trail.reverse();
        Ok(trail)
    }
}

/// Reject names that would let an item escape its folder or break listings.
fn sanitize_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("name is empty");
    }
    if name.len() > 255 || name.contains('/') || name.contains('\\') || name.contains('\0') || name == "." || name == ".." {
        bail!("invalid name");
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::Role;
    use crate::app::App;

    #[test]
    fn personal_drive_folders_files_and_docs() {
        let (app, user) = App::test();
        let s = &app.store;
        let d = s.ensure_personal_drive(user.id).unwrap();
        // The personal drive shows up for its owner.
        assert!(s.list_drives(user.id).unwrap().iter().any(|x| x.id == d && x.kind == "personal"));

        let folder = s.create_drive_folder(d, None, "Docs", user.id).unwrap();
        let (item, doc) = s.create_drive_doc(d, Some(folder), "Plan", user.id).unwrap();
        let file = s.create_drive_file(d, Some(folder), "a.txt", 3, "text/plain", user.id).unwrap();

        let items = s.drive_items(d, Some(folder)).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(s.drive_item(file).unwrap().unwrap().size, 3);
        // The document is a real collab doc the owner can edit.
        assert!(s.doc_access(user.id, doc).unwrap().unwrap().write);
        // Breadcrumb trail runs root -> folder -> item.
        assert_eq!(s.drive_item_path(item).unwrap(), vec![(folder, "Docs".into()), (item, "Plan".into())]);

        // Deleting the folder removes its children; the file id is returned for
        // on-disk cleanup, and the wrapped collab doc is gone.
        let files = s.delete_drive_item(folder).unwrap();
        assert_eq!(files, vec![file]);
        assert!(s.drive_item(item).unwrap().is_none());
        assert!(s.doc(doc).unwrap().is_none());
    }

    #[test]
    fn library_membership_controls_doc_access() {
        let (app, owner) = App::test();
        let s = &app.store;
        let bob = s.create_user("bob@example.com", "Bob", "pw", "user").unwrap();
        let lib = s.create_library(owner.id, "Team").unwrap();
        assert_eq!(s.role_in_drive(owner.id, lib).unwrap().unwrap(), Role::Owner);
        assert!(s.role_in_drive(bob.id, lib).unwrap().is_none());

        let (_it, doc) = s.create_drive_doc(lib, None, "Shared", owner.id).unwrap();
        // A non-member cannot open a document stored in the library.
        assert!(s.doc_access(bob.id, doc).unwrap().is_none());

        s.set_drive_member(lib, bob.id, Role::Editor).unwrap();
        let acc = s.doc_access(bob.id, doc).unwrap().unwrap();
        assert!(acc.read && acc.write);
        assert!(s.list_drives(bob.id).unwrap().iter().any(|x| x.id == lib));

        // Downgrading to viewer makes the document read-only.
        s.set_drive_member(lib, bob.id, Role::Viewer).unwrap();
        let acc = s.doc_access(bob.id, doc).unwrap().unwrap();
        assert!(acc.read && !acc.write);

        // An owner cannot be removed; a normal member can.
        s.remove_drive_member(lib, bob.id).unwrap();
        assert!(s.role_in_drive(bob.id, lib).unwrap().is_none());
        s.remove_drive_member(lib, owner.id).unwrap();
        assert_eq!(s.role_in_drive(owner.id, lib).unwrap().unwrap(), Role::Owner);
    }

    #[test]
    fn cannot_move_a_folder_into_itself() {
        let (app, user) = App::test();
        let s = &app.store;
        let d = s.ensure_personal_drive(user.id).unwrap();
        let a = s.create_drive_folder(d, None, "A", user.id).unwrap();
        let b = s.create_drive_folder(d, Some(a), "B", user.id).unwrap();
        // Into itself or into a descendant is rejected (would create a cycle).
        assert!(s.move_drive_item(a, Some(a), user.id).is_err());
        assert!(s.move_drive_item(a, Some(b), user.id).is_err());
        assert!(s.relocate_item(a, Some(b), "A", user.id).is_err());
        // Moving a child up to the root is fine.
        assert!(s.move_drive_item(b, None, user.id).is_ok());
    }

    #[test]
    fn recycle_bin_trash_restore_and_empty() {
        let (app, user) = App::test();
        let s = &app.store;
        let d = s.ensure_personal_drive(user.id).unwrap();
        let folder = s.create_drive_folder(d, None, "Docs", user.id).unwrap();
        let file = s.create_drive_file(d, Some(folder), "a.txt", 3, "text/plain", user.id).unwrap();

        // Trashing the folder hides it (and its subtree) from navigation but
        // keeps it findable by id and listed in the recycle bin.
        s.trash_item(folder, user.id).unwrap();
        assert!(s.drive_items(d, None).unwrap().is_empty());
        assert_eq!(s.trashed_items(d).unwrap().len(), 1);
        assert!(s.drive_item(folder).unwrap().is_some());
        // A new folder may reuse the freed name while the old one is trashed.
        let fresh = s.create_drive_folder(d, None, "Docs", user.id).unwrap();
        assert_ne!(fresh, folder);

        // Restoring brings the whole subtree back into view.
        s.restore_item(folder).unwrap();
        assert_eq!(s.drive_items(d, None).unwrap().len(), 2);
        assert_eq!(s.drive_items(d, Some(folder)).unwrap().len(), 1);
        assert!(s.trashed_items(d).unwrap().is_empty());

        // Emptying the bin hard-deletes trashed subtrees and returns file ids.
        s.trash_item(folder, user.id).unwrap();
        let files = s.empty_trash(d).unwrap();
        assert_eq!(files, vec![file]);
        assert!(s.drive_item(folder).unwrap().is_none());
        assert!(s.trashed_items(d).unwrap().is_empty());
    }

    #[test]
    fn file_versions_snapshot_and_prune() {
        let (app, user) = App::test();
        let s = &app.store;
        let d = s.ensure_personal_drive(user.id).unwrap();
        let file = s.create_drive_file(d, None, "a.txt", 3, "text/plain", user.id).unwrap();

        // A folder has no versions to snapshot.
        let folder = s.create_drive_folder(d, None, "F", user.id).unwrap();
        assert!(s.snapshot_file_version(folder, user.id).unwrap().is_none());

        // Snapshot the file a few times; each is recorded newest-first.
        for _ in 0..3 {
            assert!(s.snapshot_file_version(file, user.id).unwrap().is_some());
        }
        assert_eq!(s.item_versions(file).unwrap().len(), 3);

        // Pruning keeps the newest N and reports the rest for disk cleanup.
        let gone = s.prune_file_versions(file, 1).unwrap();
        assert_eq!(gone.len(), 2);
        assert_eq!(s.item_versions(file).unwrap().len(), 1);

        // A version resolves back to its item.
        let v = s.item_versions(file).unwrap()[0].clone();
        assert_eq!(s.file_version(v.id).unwrap().unwrap().item_id, file);
    }

    #[test]
    fn item_share_links_resolve_and_revoke() {
        let (app, user) = App::test();
        let s = &app.store;
        let d = s.ensure_personal_drive(user.id).unwrap();
        let file = s.create_drive_file(d, None, "a.txt", 3, "text/plain", user.id).unwrap();

        let token = s.create_item_link(file, false, None, user.id).unwrap();
        let (it, can_write) = s.item_by_link(&token).unwrap().unwrap();
        assert_eq!(it.id, file);
        assert!(!can_write);
        assert_eq!(s.item_links(file).unwrap().len(), 1);

        // A trashed item's links stop resolving, and resolve again once restored.
        s.trash_item(file, user.id).unwrap();
        assert!(s.item_by_link(&token).unwrap().is_none());
        s.restore_item(file).unwrap();
        assert!(s.item_by_link(&token).unwrap().is_some());

        // Revoking the link makes it a dead token.
        s.revoke_item_link(&token).unwrap();
        assert!(s.item_by_link(&token).unwrap().is_none());
        assert!(s.item_links(file).unwrap().is_empty());
    }

    #[test]
    fn share_links_carry_edit_and_expiry() {
        use crate::store::now;
        let (app, user) = App::test();
        let s = &app.store;
        let d = s.ensure_personal_drive(user.id).unwrap();
        let folder = s.create_drive_folder(d, None, "Shared", user.id).unwrap();

        // An edit link resolves with write access.
        let edit = s.create_item_link(folder, true, None, user.id).unwrap();
        assert!(s.item_by_link(&edit).unwrap().unwrap().1);

        // An already-expired link does not resolve; a future one does.
        let past = s.create_item_link(folder, false, Some(now() - 10), user.id).unwrap();
        assert!(s.item_by_link(&past).unwrap().is_none());
        let future = s.create_item_link(folder, false, Some(now() + 3600), user.id).unwrap();
        assert!(s.item_by_link(&future).unwrap().is_some());

        // Changing a link to view-only with no expiry takes effect.
        s.set_item_link(&edit, false, None).unwrap();
        assert!(!s.item_by_link(&edit).unwrap().unwrap().1);
        assert_eq!(s.item_link_creator(&edit).unwrap(), Some(user.id));
    }

    #[test]
    fn restoring_into_a_trashed_parent_lands_at_the_root() {
        let (app, user) = App::test();
        let s = &app.store;
        let d = s.ensure_personal_drive(user.id).unwrap();
        let folder = s.create_drive_folder(d, None, "Docs", user.id).unwrap();
        let child = s.create_drive_folder(d, Some(folder), "Sub", user.id).unwrap();
        // Trash the child on its own, then trash its parent too.
        s.trash_item(child, user.id).unwrap();
        s.trash_item(folder, user.id).unwrap();
        // Restoring only the child, whose parent is still in the bin, falls back
        // to the drive root rather than staying hidden under a trashed folder.
        s.restore_item(child).unwrap();
        let roots = s.drive_items(d, None).unwrap();
        assert!(roots.iter().any(|x| x.id == child));
        assert!(!roots.iter().any(|x| x.id == folder));
    }
}
