//! Storage usage of an account: how much its mail folders and its personal
//! drive take up, for the overview in Settings and for the admin center.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use crate::db::params;
use crate::store::Store;

/// One mail folder's share of the mailbox.
#[derive(Debug, Clone, Serialize)]
pub struct FolderUsage {
    pub id: i64,
    pub name: String,
    pub role: Option<String>,
    /// `mail`, `calendar`, `contacts`, `tasks` or `notes`.
    pub class: String,
    pub items: i64,
    pub bytes: i64,
}

/// A large file in the personal drive.
#[derive(Debug, Clone, Serialize)]
pub struct LargeFile {
    pub id: i64,
    pub name: String,
    pub size: i64,
    pub trashed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageUsage {
    /// Bytes of all mail, calendar, contact, task and note items.
    pub mail: i64,
    pub folders: Vec<FolderUsage>,
    /// Bytes of the files in the personal drive, the recycle bin included.
    pub files: i64,
    /// The part of `files` that sits in the recycle bin.
    pub files_trash: i64,
    /// Bytes kept as older versions of files (not counted against the quota).
    pub versions: i64,
    pub largest: Vec<LargeFile>,
    /// The account's quota in bytes; `0` means unlimited. It applies to mail
    /// and to the personal drive separately.
    pub quota: i64,
}

impl Store {
    /// Drive usage of every account's personal drive, keyed by account id.
    pub fn drive_usage_by_owner(&self) -> Result<HashMap<i64, i64>> {
        let db = self.db();
        let st = db.prepare(
            "SELECT d.owner_id, CAST(COALESCE(SUM(i.size), 0) AS BIGINT) FROM drives d JOIN drive_items i ON i.drive_id = d.id \
             WHERE d.kind = 'personal' AND i.kind = 'file' GROUP BY d.owner_id",
        )?;
        let rows: Vec<(i64, i64)> = st.query_map(params![], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.into_iter().collect())
    }

    /// The breakdown shown in Settings → Storage.
    pub fn storage_usage(&self, user_id: i64, quota: i64) -> Result<StorageUsage> {
        let drive = self.personal_drive_id(user_id)?;
        let db = self.db();
        let st = db.prepare(
            "SELECT f.id, f.name, f.role, f.class, COUNT(i.id), CAST(COALESCE(SUM(i.size), 0) AS BIGINT) FROM folders f \
             LEFT JOIN items i ON i.folder_id = f.id WHERE f.user_id = ?1 AND f.deleted = 0 \
             GROUP BY f.id, f.name, f.role, f.class ORDER BY 6 DESC, f.name",
        )?;
        let folders: Vec<FolderUsage> = st.query_map([user_id], |r| {
            Ok(FolderUsage { id: r.get(0)?, name: r.get(1)?, role: r.get(2)?, class: r.get(3)?, items: r.get(4)?, bytes: r.get(5)? })
        })?;
        let mail = folders.iter().map(|f| f.bytes).sum();
        let (mut files, mut files_trash, mut versions, mut largest) = (0, 0, 0, Vec::new());
        if let Some(drive) = drive {
            (files, files_trash) = db.query_row(
                "SELECT CAST(COALESCE(SUM(size), 0) AS BIGINT), CAST(COALESCE(SUM(CASE WHEN trashed <> 0 THEN size ELSE 0 END), 0) AS BIGINT) \
                 FROM drive_items WHERE drive_id = ?1 AND kind = 'file'",
                [drive],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            versions = db.query_row(
                "SELECT CAST(COALESCE(SUM(v.size), 0) AS BIGINT) FROM drive_file_versions v JOIN drive_items i ON i.id = v.item_id WHERE i.drive_id = ?1",
                [drive],
                |r| r.get(0),
            )?;
            let st = db.prepare("SELECT id, name, size, trashed FROM drive_items WHERE drive_id = ?1 AND kind = 'file' ORDER BY size DESC, id LIMIT 10")?;
            largest = st.query_map([drive], |r| Ok(LargeFile { id: r.get(0)?, name: r.get(1)?, size: r.get(2)?, trashed: r.get::<_, i64>(3)? != 0 }))?;
        }
        Ok(StorageUsage { mail, folders, files, files_trash, versions, largest, quota })
    }

    /// The id of an account's personal drive, if it has one yet.
    fn personal_drive_id(&self, user_id: i64) -> Result<Option<i64>> {
        use crate::db::OptionalExtension;
        self.db()
            .query_row("SELECT id FROM drives WHERE owner_id = ?1 AND kind = 'personal'", [user_id], |r| r.get(0))
            .optional()
    }
}

#[cfg(test)]
mod tests {
    use crate::app::App;

    #[test]
    fn usage_counts_mail_files_trash_and_versions() {
        let (app, user) = App::test();
        let s = &app.store;
        let usage = s.storage_usage(user.id, 0).unwrap();
        assert_eq!(usage.files, 0);
        assert!(usage.folders.iter().any(|f| f.role.as_deref() == Some("inbox")));

        let drive = s.ensure_personal_drive(user.id).unwrap();
        let a = s.create_drive_file(drive, None, "a.bin", 300, "application/octet-stream", user.id).unwrap();
        let b = s.create_drive_file(drive, None, "b.bin", 100, "application/octet-stream", user.id).unwrap();
        s.trash_item(b, user.id).unwrap();
        s.db()
            .execute("INSERT INTO drive_file_versions (item_id, size, content_type, created, created_by) VALUES (?1, 50, '', 0, ?2)", crate::db::params![a, user.id])
            .unwrap();
        let usage = s.storage_usage(user.id, 1000).unwrap();
        assert_eq!((usage.files, usage.files_trash, usage.versions, usage.quota), (400, 100, 50, 1000));
        assert_eq!(usage.largest[0].name, "a.bin");
        assert!(usage.largest[1].trashed);
        assert_eq!(s.drive_usage_by_owner().unwrap().get(&user.id), Some(&400));
    }
}
