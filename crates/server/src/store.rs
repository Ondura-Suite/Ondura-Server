//! Persistent storage (SQLite): domains, mailboxes, folders, items, sync
//! state of the protocol front ends and the outbound queue.
//!
//! Every change to a folder or item gets a new *modseq* from a per-user
//! counter. Deleted items leave a tombstone with the modseq of the deletion.
//! ActiveSync and EWS sync states are just modseq values, so any number of
//! clients can sync incrementally with the same data.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result, anyhow, bail};
use crate::db::{Db, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::mime;
use crate::model::{self, DEFAULT_FOLDERS, FolderClass, ItemKind, Person};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS domains (
    name TEXT PRIMARY KEY,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    email TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL DEFAULT '',
    password TEXT NOT NULL,
    is_admin INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1,
    quota INTEGER NOT NULL DEFAULT 0,
    kind TEXT NOT NULL DEFAULT 'user',
    modseq INTEGER NOT NULL DEFAULT 0,
    settings TEXT NOT NULL DEFAULT '{}',
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS aliases (
    address TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS avatars (
    user_id INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    content_type TEXT NOT NULL,
    data BLOB NOT NULL,
    updated INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS permissions (
    owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    grantee_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    full_access INTEGER NOT NULL DEFAULT 1,
    send_as INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (owner_id, grantee_id)
);
CREATE TABLE IF NOT EXISTS folders (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    parent_id INTEGER,
    name TEXT NOT NULL,
    class TEXT NOT NULL,
    role TEXT,
    modseq INTEGER NOT NULL,
    created_modseq INTEGER NOT NULL DEFAULT 0,
    deleted INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS folders_user ON folders(user_id, modseq);
CREATE TABLE IF NOT EXISTS items (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    folder_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    uid TEXT,
    modseq INTEGER NOT NULL,
    created_modseq INTEGER NOT NULL,
    writer TEXT NOT NULL DEFAULT '',
    received INTEGER NOT NULL,
    size INTEGER NOT NULL DEFAULT 0,
    flags INTEGER NOT NULL DEFAULT 0,
    categories TEXT NOT NULL DEFAULT '[]',
    subject TEXT NOT NULL DEFAULT '',
    sender TEXT,
    recipients TEXT NOT NULL DEFAULT '{}',
    preview TEXT NOT NULL DEFAULT '',
    has_attachments INTEGER NOT NULL DEFAULT 0,
    importance TEXT NOT NULL DEFAULT 'normal',
    message_id TEXT,
    message_class TEXT NOT NULL DEFAULT '',
    data TEXT,
    mime BLOB,
    range_start INTEGER,
    range_end INTEGER
);
CREATE INDEX IF NOT EXISTS items_folder ON items(folder_id, modseq);
CREATE INDEX IF NOT EXISTS items_folder_received ON items(folder_id, received);
CREATE INDEX IF NOT EXISTS items_uid ON items(user_id, uid);
CREATE TABLE IF NOT EXISTS tombstones (
    user_id INTEGER NOT NULL,
    folder_id INTEGER NOT NULL,
    item_id INTEGER NOT NULL,
    modseq INTEGER NOT NULL,
    writer TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS tombstones_folder ON tombstones(folder_id, modseq);
CREATE TABLE IF NOT EXISTS devices (
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    device_id TEXT NOT NULL,
    device_type TEXT NOT NULL DEFAULT '',
    user_agent TEXT NOT NULL DEFAULT '',
    info TEXT NOT NULL DEFAULT '{}',
    policy_key INTEGER NOT NULL DEFAULT 0,
    wipe INTEGER NOT NULL DEFAULT 0,
    blocked INTEGER NOT NULL DEFAULT 0,
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL,
    PRIMARY KEY (user_id, device_id)
);
CREATE TABLE IF NOT EXISTS sync_keys (
    user_id INTEGER NOT NULL,
    device_id TEXT NOT NULL,
    collection TEXT NOT NULL,
    sync_key INTEGER NOT NULL,
    modseq INTEGER NOT NULL,
    created INTEGER NOT NULL,
    PRIMARY KEY (user_id, device_id, collection, sync_key)
);
CREATE TABLE IF NOT EXISTS device_state (
    user_id INTEGER NOT NULL,
    device_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (user_id, device_id, key)
);
CREATE TABLE IF NOT EXISTS queue (
    id INTEGER PRIMARY KEY,
    sender TEXT NOT NULL,
    recipients TEXT NOT NULL,
    mime BLOB NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt INTEGER NOT NULL,
    created INTEGER NOT NULL,
    last_error TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS dkim (
    domain TEXT PRIMARY KEY,
    selector TEXT NOT NULL,
    private_key TEXT NOT NULL,
    public_key TEXT NOT NULL,
    created INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
    token TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created INTEGER NOT NULL,
    expires INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS audit (
    id INTEGER PRIMARY KEY,
    time INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS collab_docs (
    id INTEGER PRIMARY KEY,
    owner_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    content TEXT NOT NULL DEFAULT '',
    revision INTEGER NOT NULL DEFAULT 0,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS collab_docs_owner ON collab_docs(owner_id);
CREATE TABLE IF NOT EXISTS collab_ops (
    doc_id INTEGER NOT NULL REFERENCES collab_docs(id) ON DELETE CASCADE,
    revision INTEGER NOT NULL,
    user_id INTEGER NOT NULL,
    op TEXT NOT NULL,
    base_len INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    PRIMARY KEY (doc_id, revision)
);
CREATE TABLE IF NOT EXISTS collab_shares (
    doc_id INTEGER NOT NULL REFERENCES collab_docs(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    can_write INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (doc_id, user_id)
);
CREATE INDEX IF NOT EXISTS collab_shares_user ON collab_shares(user_id);
"#;

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Maximum size of an avatar image.
pub const MAX_AVATAR_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------- records

#[derive(Debug, Clone, Serialize)]
pub struct User {
    pub id: i64,
    pub email: String,
    pub name: String,
    #[serde(skip)]
    pub password: String,
    pub is_admin: bool,
    pub enabled: bool,
    /// Bytes, 0 = unlimited.
    pub quota: i64,
    /// `user`, `shared` or `room`.
    pub kind: String,
    pub created: i64,
}

impl User {
    pub fn person(&self) -> Person {
        Person::new(self.name.clone(), self.email.clone())
    }

    pub fn domain(&self) -> &str {
        self.email.rsplit_once('@').map(|(_, d)| d).unwrap_or_default()
    }
}

/// Per-mailbox settings stored as JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct UserSettings {
    pub oof: OofSettings,
    /// IANA zone used for all-day events and when a client sends none.
    pub time_zone: Option<String>,
    /// Signature appended to new messages composed in webmail.
    pub signature: String,
    pub forward_to: Option<String>,
    pub keep_forwarded_copy: bool,
    /// Senders that recently got an automatic reply (address → time).
    pub oof_sent: HashMap<String, i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OofSettings {
    /// `Disabled`, `Enabled` or `Scheduled`.
    pub state: String,
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub internal_reply: String,
    pub external_reply: String,
    /// `None`, `Known` or `All`.
    pub external_audience: String,
}

impl OofSettings {
    pub fn active(&self, at: i64) -> bool {
        match self.state.as_str() {
            "Enabled" => true,
            "Scheduled" => self.start.is_none_or(|s| at >= s) && self.end.is_none_or(|e| at < e),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Folder {
    pub id: i64,
    pub user_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub class: FolderClass,
    pub role: Option<String>,
    pub modseq: i64,
    pub deleted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Recipients {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub to: Vec<Person>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cc: Vec<Person>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bcc: Vec<Person>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reply_to: Vec<Person>,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub id: i64,
    pub user_id: i64,
    pub folder_id: i64,
    pub kind: ItemKind,
    pub uid: Option<String>,
    pub modseq: i64,
    pub created_modseq: i64,
    pub received: i64,
    pub size: i64,
    pub flags: i64,
    pub categories: Vec<String>,
    pub subject: String,
    pub sender: Option<Person>,
    pub recipients: Recipients,
    pub preview: String,
    pub has_attachments: bool,
    pub importance: model::Importance,
    pub message_id: Option<String>,
    pub message_class: String,
    /// JSON of the calendar/contact/task/note item.
    pub data: Option<String>,
}

impl Item {
    #[cfg(test)]
    pub fn is_read(&self) -> bool {
        self.flags & model::flags::READ != 0
    }

    pub fn event(&self) -> Option<model::Event> {
        self.data.as_deref().and_then(|d| serde_json::from_str(d).ok())
    }

    pub fn contact(&self) -> Option<model::Contact> {
        self.data.as_deref().and_then(|d| serde_json::from_str(d).ok())
    }

    pub fn task(&self) -> Option<model::Task> {
        self.data.as_deref().and_then(|d| serde_json::from_str(d).ok())
    }

    pub fn note(&self) -> Option<model::Note> {
        self.data.as_deref().and_then(|d| serde_json::from_str(d).ok())
    }
}

const ITEM_COLUMNS: &str = "id, user_id, folder_id, kind, uid, modseq, created_modseq, received, size, flags, categories, subject, sender, recipients, preview, has_attachments, importance, message_id, message_class, data";

fn item_from_row(r: &Row) -> Result<Item> {
    let kind: String = r.get(3)?;
    let categories: String = r.get(10)?;
    let sender: Option<String> = r.get(12)?;
    let recipients: String = r.get(13)?;
    let importance: String = r.get(16)?;
    Ok(Item {
        id: r.get(0)?,
        user_id: r.get(1)?,
        folder_id: r.get(2)?,
        kind: ItemKind::parse(&kind).unwrap_or(ItemKind::Mail),
        uid: r.get(4)?,
        modseq: r.get(5)?,
        created_modseq: r.get(6)?,
        received: r.get(7)?,
        size: r.get(8)?,
        flags: r.get(9)?,
        categories: serde_json::from_str(&categories).unwrap_or_default(),
        subject: r.get(11)?,
        sender: sender.and_then(|s| serde_json::from_str(&s).ok()),
        recipients: serde_json::from_str(&recipients).unwrap_or_default(),
        preview: r.get(14)?,
        has_attachments: r.get::<_, i64>(15)? != 0,
        importance: match importance.as_str() {
            "high" => model::Importance::High,
            "low" => model::Importance::Low,
            _ => model::Importance::Normal,
        },
        message_id: r.get(17)?,
        message_class: r.get(18)?,
        data: r.get(19)?,
    })
}

fn folder_from_row(r: &Row) -> Result<Folder> {
    let class: String = r.get(4)?;
    Ok(Folder {
        id: r.get(0)?,
        user_id: r.get(1)?,
        parent_id: r.get(2)?,
        name: r.get(3)?,
        class: FolderClass::parse(&class).unwrap_or(FolderClass::Mail),
        role: r.get(5)?,
        modseq: r.get(6)?,
        deleted: r.get::<_, i64>(7)? != 0,
    })
}

const FOLDER_COLUMNS: &str = "id, user_id, parent_id, name, class, role, modseq, deleted";

fn user_from_row(r: &Row) -> Result<User> {
    Ok(User {
        id: r.get(0)?,
        email: r.get(1)?,
        name: r.get(2)?,
        password: r.get(3)?,
        is_admin: r.get::<_, i64>(4)? != 0,
        enabled: r.get::<_, i64>(5)? != 0,
        quota: r.get(6)?,
        kind: r.get(7)?,
        created: r.get(8)?,
    })
}

const USER_COLUMNS: &str = "id, email, name, password, is_admin, enabled, quota, kind, created";

/// A change in a folder since some modseq.
#[derive(Debug, Clone)]
pub enum Change {
    /// Item created after the sync point (new to the client).
    Add(Item),
    /// Item that existed at the sync point and changed since.
    Update(Item),
    Delete { item_id: i64, modseq: i64 },
}

impl Change {
    pub fn modseq(&self) -> i64 {
        match self {
            Change::Add(i) | Change::Update(i) => i.modseq,
            Change::Delete { modseq, .. } => *modseq,
        }
    }
}

/// Changes of the folder hierarchy since some modseq.
#[derive(Debug, Clone, Default)]
pub struct HierarchyChanges {
    pub added: Vec<Folder>,
    pub updated: Vec<Folder>,
    pub deleted: Vec<Folder>,
    pub modseq: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notice {
    pub user_id: i64,
    pub folder_id: i64,
}

#[derive(Debug, Clone)]
pub struct QueuedMessage {
    pub id: i64,
    pub sender: String,
    pub recipients: Vec<String>,
    pub mime: Vec<u8>,
    pub attempts: i64,
    pub next_attempt: i64,
    pub created: i64,
    pub last_error: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub user_id: i64,
    pub device_id: String,
    pub device_type: String,
    pub user_agent: String,
    pub info: serde_json::Value,
    pub policy_key: i64,
    pub wipe: i64,
    pub blocked: bool,
    pub first_seen: i64,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DkimKey {
    pub domain: String,
    pub selector: String,
    #[serde(skip)]
    pub private_key: String,
    pub public_key: String,
}

/// Fields of a new mail item.
pub struct NewMail<'a> {
    pub user_id: i64,
    pub folder_id: i64,
    pub mime: &'a [u8],
    pub flags: i64,
    pub received: i64,
    pub writer: &'a str,
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Db>>,
    notify: broadcast::Sender<Notice>,
}

impl Store {
    /// Open the embedded SQLite database at `path` (the default backend).
    pub fn open(path: &Path) -> Result<Store> {
        let db = Db::open_sqlite(path).with_context(|| format!("open database {}", path.display()))?;
        Self::init(db)
    }

    /// Connect to a PostgreSQL server with the given libpq connection string.
    pub fn connect_postgres(url: &str) -> Result<Store> {
        let db = Db::connect_postgres(url).context("connect PostgreSQL")?;
        Self::init(db)
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Store> {
        Self::init(Db::open_sqlite_memory()?)
    }

    fn init(mut db: Db) -> Result<Store> {
        db.execute_batch(SCHEMA)?;
        let (notify, _) = broadcast::channel(1024);
        Ok(Store { conn: Arc::new(Mutex::new(db)), notify })
    }

    pub(crate) fn db(&self) -> MutexGuard<'_, Db> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.notify.subscribe()
    }

    fn notify(&self, user_id: i64, folder_id: i64) {
        let _ = self.notify.send(Notice { user_id, folder_id });
    }

    fn next_modseq(tx: &crate::db::Tx<'_>, user_id: i64) -> Result<i64> {
        tx.query_row("UPDATE users SET modseq = modseq + 1 WHERE id = ?1 RETURNING modseq", [user_id], |r| r.get(0))
    }

    pub fn current_modseq(&self, user_id: i64) -> Result<i64> {
        self.db().query_row("SELECT modseq FROM users WHERE id = ?1", [user_id], |r| r.get(0))
    }



    pub fn audit(&self, actor: &str, action: &str, detail: &str) {
        let _ = self.db().execute("INSERT INTO audit (time, actor, action, detail) VALUES (?1, ?2, ?3, ?4)", params![now(), actor, action, detail]);
    }

    pub fn audit_log(&self, limit: i64) -> Result<Vec<(i64, String, String, String)>> {
        let db = self.db();
        let st = db.prepare("SELECT time, actor, action, detail FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map([limit], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows)
    }

    // ------------------------------------------------------------ domains

    pub fn add_domain(&self, name: &str) -> Result<()> {
        let name = normalize_domain(name)?;
        self.db().execute("INSERT INTO domains (name, created) VALUES (?1, ?2) ON CONFLICT (name) DO NOTHING", params![name, now()])?;
        Ok(())
    }

    pub fn remove_domain(&self, name: &str) -> Result<()> {
        let name = name.trim().to_ascii_lowercase();
        let users: i64 = self.db().query_row("SELECT COUNT(*) FROM users WHERE email LIKE ?1", [format!("%@{name}")], |r| r.get(0))?;
        if users > 0 {
            bail!("domain {name} still has {users} mailbox(es)");
        }
        let db = self.db();
        db.execute("DELETE FROM aliases WHERE address LIKE ?1", [format!("%@{name}")])?;
        db.execute("DELETE FROM domains WHERE name = ?1", [name])?;
        Ok(())
    }

    pub fn domains(&self) -> Result<Vec<String>> {
        let db = self.db();
        let st = db.prepare("SELECT name FROM domains ORDER BY name")?;
        let rows = st.query_map(params![], |r| r.get(0))?;
        Ok(rows)
    }

    pub fn is_local_domain(&self, domain: &str) -> bool {
        self.db()
            .query_row("SELECT 1 FROM domains WHERE name = ?1", [domain.trim().trim_end_matches('.').to_ascii_lowercase()], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    // -------------------------------------------------------------- users

    /// Create a mailbox with its default folders.
    pub fn create_user(&self, email: &str, name: &str, password: &str, kind: &str) -> Result<User> {
        let email = normalize_address(email)?;
        let domain = email.rsplit_once('@').map(|(_, d)| d.to_string()).unwrap_or_default();
        if !self.is_local_domain(&domain) {
            bail!("domain {domain} is not managed by this server (add it first)");
        }
        if self.resolve_address(&email)?.is_some_and(|(_, exact)| exact) {
            bail!("{email} is already in use");
        }
        let hash = crate::auth::hash_password(password)?;
        let mut db = self.db();
        let tx = db.transaction()?;
        let id: i64 = tx.query_row(
            "INSERT INTO users (email, name, password, kind, created) VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            params![email, name.trim(), hash, kind, now()],
            |r| r.get(0),
        )?;
        let modseq = Self::next_modseq(&tx, id)?;
        let root: i64 = tx.query_row(
            "INSERT INTO folders (user_id, parent_id, name, class, role, modseq, created_modseq) VALUES (?1, NULL, 'Root', 'root', 'msgfolderroot', ?2, ?2) RETURNING id",
            params![id, modseq],
            |r| r.get(0),
        )?;
        for (role, class, name) in DEFAULT_FOLDERS {
            tx.execute(
                "INSERT INTO folders (user_id, parent_id, name, class, role, modseq, created_modseq) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
                params![id, root, name, class.as_str(), role, modseq],
            )?;
        }
        tx.commit()?;
        drop(db);
        self.user(id)?.ok_or_else(|| anyhow!("user vanished"))
    }

    pub fn user(&self, id: i64) -> Result<Option<User>> {
        self.db().query_row(&format!("SELECT {USER_COLUMNS} FROM users WHERE id = ?1"), [id], user_from_row).optional()
    }

    pub fn user_by_email(&self, email: &str) -> Result<Option<User>> {
        let email = email.trim().to_ascii_lowercase();
        self.db().query_row(&format!("SELECT {USER_COLUMNS} FROM users WHERE email = ?1"), [email], user_from_row).optional()
    }

    /// Find the mailbox for a login name: the address, an alias, or a bare
    /// local part when it is unambiguous.
    pub fn user_for_login(&self, login: &str) -> Result<Option<User>> {
        let login = login.trim().to_ascii_lowercase();
        // DOMAIN\user style logins.
        let login = login.rsplit('\\').next().unwrap_or(&login).to_string();
        if login.contains('@') {
            return match self.resolve_address(&login)? {
                Some((id, _)) => self.user(id),
                None => Ok(None),
            };
        }
        let db = self.db();
        let st = db.prepare(&format!("SELECT {USER_COLUMNS} FROM users WHERE email LIKE ?1"))?;
        let found: Vec<User> = st.query_map([format!("{login}@%")], user_from_row)?;
        Ok(if found.len() == 1 { found.into_iter().next() } else { None })
    }

    pub fn users(&self) -> Result<Vec<User>> {
        let db = self.db();
        let st = db.prepare(&format!("SELECT {USER_COLUMNS} FROM users ORDER BY email"))?;
        let rows = st.query_map(params![], user_from_row)?;
        Ok(rows)
    }

    pub fn set_password(&self, id: i64, password: &str) -> Result<()> {
        let hash = crate::auth::hash_password(password)?;
        self.db().execute("UPDATE users SET password = ?1 WHERE id = ?2", params![hash, id])?;
        crate::auth::forget(id);
        Ok(())
    }

    pub fn update_user(&self, id: i64, name: Option<&str>, quota: Option<i64>, is_admin: Option<bool>, enabled: Option<bool>) -> Result<()> {
        let db = self.db();
        if let Some(n) = name {
            db.execute("UPDATE users SET name = ?1 WHERE id = ?2", params![n.trim(), id])?;
        }
        if let Some(q) = quota {
            db.execute("UPDATE users SET quota = ?1 WHERE id = ?2", params![q.max(0), id])?;
        }
        if let Some(a) = is_admin {
            db.execute("UPDATE users SET is_admin = ?1 WHERE id = ?2", params![a as i64, id])?;
        }
        if let Some(e) = enabled {
            db.execute("UPDATE users SET enabled = ?1 WHERE id = ?2", params![e as i64, id])?;
            crate::auth::forget(id);
        }
        Ok(())
    }

    pub fn delete_user(&self, id: i64) -> Result<()> {
        let mut db = self.db();
        let tx = db.transaction()?;
        for table in ["items", "folders", "tombstones", "devices", "sync_keys", "device_state", "aliases", "sessions"] {
            tx.execute(&format!("DELETE FROM {table} WHERE user_id = ?1"), [id])?;
        }
        tx.execute("DELETE FROM permissions WHERE owner_id = ?1 OR grantee_id = ?1", [id])?;
        tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
        tx.commit()?;
        crate::auth::forget(id);
        Ok(())
    }

    pub fn settings(&self, id: i64) -> Result<UserSettings> {
        let s: String = self.db().query_row("SELECT settings FROM users WHERE id = ?1", [id], |r| r.get(0))?;
        Ok(serde_json::from_str(&s).unwrap_or_default())
    }

    // ------------------------------------------------------- server settings

    /// Read a value from the key/value `meta` table.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.db().query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    /// Store a value in the `meta` table.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ------------------------------------------------------------ avatars

    /// Set (or replace) a user's avatar image.
    pub fn set_avatar(&self, user_id: i64, content_type: &str, data: &[u8]) -> Result<()> {
        self.db().execute(
            "INSERT INTO avatars (user_id, content_type, data, updated) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (user_id) DO UPDATE SET content_type = excluded.content_type, data = excluded.data, updated = excluded.updated",
            params![user_id, content_type, data, now()],
        )?;
        Ok(())
    }

    /// A user's avatar: content type, image bytes and the time it was set.
    pub fn avatar(&self, user_id: i64) -> Result<Option<(String, Vec<u8>, i64)>> {
        self
            .db()
            .query_row("SELECT content_type, data, updated FROM avatars WHERE user_id = ?1", [user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()
    }

    /// When a user's avatar was last set (for cache-busting), without loading
    /// the image. `None` means no avatar.
    pub fn avatar_updated(&self, user_id: i64) -> Result<Option<i64>> {
        self.db().query_row("SELECT updated FROM avatars WHERE user_id = ?1", [user_id], |r| r.get(0)).optional()
    }

    pub fn delete_avatar(&self, user_id: i64) -> Result<()> {
        self.db().execute("DELETE FROM avatars WHERE user_id = ?1", [user_id])?;
        Ok(())
    }

    pub fn set_settings(&self, id: i64, settings: &UserSettings) -> Result<()> {
        self.db().execute("UPDATE users SET settings = ?1 WHERE id = ?2", params![serde_json::to_string(settings)?, id])?;
        Ok(())
    }

    pub fn used_bytes(&self, user_id: i64) -> Result<i64> {
        self.db().query_row("SELECT CAST(COALESCE(SUM(size), 0) AS BIGINT) FROM items WHERE user_id = ?1", [user_id], |r| r.get(0))
    }

    pub fn user_stats(&self) -> Result<HashMap<i64, (i64, i64)>> {
        let db = self.db();
        let st = db.prepare("SELECT user_id, COUNT(*), CAST(COALESCE(SUM(size), 0) AS BIGINT) FROM items GROUP BY user_id")?;
        let rows = st.query_map(params![], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?;
        Ok(rows.into_iter().collect())
    }

    // ------------------------------------------------------------ aliases

    pub fn add_alias(&self, address: &str, user_id: i64) -> Result<()> {
        let address = address.trim().to_ascii_lowercase();
        let domain = address.rsplit_once('@').map(|(_, d)| d).ok_or_else(|| anyhow!("invalid address {address}"))?;
        if !self.is_local_domain(domain) {
            bail!("domain {domain} is not managed by this server");
        }
        if !address.starts_with('@') {
            normalize_address(&address)?;
        }
        if self.user_by_email(&address)?.is_some() {
            bail!("{address} is a mailbox");
        }
        self.db().execute("INSERT INTO aliases (address, user_id) VALUES (?1, ?2) ON CONFLICT(address) DO UPDATE SET user_id = excluded.user_id", params![address, user_id])?;
        Ok(())
    }

    pub fn remove_alias(&self, address: &str) -> Result<()> {
        self.db().execute("DELETE FROM aliases WHERE address = ?1", [address.trim().to_ascii_lowercase()])?;
        Ok(())
    }

    /// Make `address` the mailbox's primary address. It must currently be one of
    /// the mailbox's aliases; the old primary address becomes an alias, so mail
    /// to it still arrives and the user can still sign in with it.
    pub fn set_primary_address(&self, user_id: i64, address: &str) -> Result<()> {
        let address = address.trim().to_ascii_lowercase();
        if address.starts_with('@') || !address.contains('@') {
            bail!("a catch-all cannot be the primary address");
        }
        let current: String = self.db().query_row("SELECT email FROM users WHERE id = ?1", [user_id], |r| r.get(0))?;
        if current.eq_ignore_ascii_case(&address) {
            return Ok(());
        }
        let owns_alias = self
            .db()
            .query_row("SELECT 1 FROM aliases WHERE address = ?1 AND user_id = ?2", params![&address, user_id], |_| Ok(()))
            .optional()?
            .is_some();
        if !owns_alias {
            bail!("{address} is not an alias of this mailbox");
        }
        let mut db = self.db();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM aliases WHERE address = ?1", [&address])?;
        tx.execute("UPDATE users SET email = ?1 WHERE id = ?2", params![&address, user_id])?;
        tx.execute("INSERT INTO aliases (address, user_id) VALUES (?1, ?2) ON CONFLICT(address) DO UPDATE SET user_id = excluded.user_id", params![current, user_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn aliases(&self) -> Result<Vec<(String, i64)>> {
        let db = self.db();
        let st = db.prepare("SELECT address, user_id FROM aliases ORDER BY address")?;
        let rows = st.query_map(params![], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows)
    }

    pub fn aliases_of(&self, user_id: i64) -> Result<Vec<String>> {
        let db = self.db();
        let st = db.prepare("SELECT address FROM aliases WHERE user_id = ?1 ORDER BY address")?;
        let rows = st.query_map([user_id], |r| r.get(0))?;
        Ok(rows)
    }

    /// Mailbox receiving mail for `address`: (user id, exact match).
    /// Plus-addressing (`name+tag@`) and catch-all aliases (`@domain`) are
    /// resolved; `exact` is false for those.
    pub fn resolve_address(&self, address: &str) -> Result<Option<(i64, bool)>> {
        let address = address.trim().trim_matches(['<', '>']).to_ascii_lowercase();
        let Some((local, domain)) = address.rsplit_once('@') else { return Ok(None) };
        let db = self.db();
        let lookup = |addr: &str| -> Result<Option<i64>> {
            if let Some(id) = db.query_row("SELECT id FROM users WHERE email = ?1", [addr], |r| r.get(0)).optional()? {
                return Ok(Some(id));
            }
            db.query_row("SELECT user_id FROM aliases WHERE address = ?1", [addr], |r| r.get(0)).optional()
        };
        if let Some(id) = lookup(&address)? {
            return Ok(Some((id, true)));
        }
        if let Some((base, _)) = local.split_once('+')
            && let Some(id) = lookup(&format!("{base}@{domain}"))?
        {
            return Ok(Some((id, false)));
        }
        Ok(lookup(&format!("@{domain}"))?.map(|id| (id, false)))
    }

    /// All addresses the user may send from (own, aliases, send-as grants).
    pub fn send_as_addresses(&self, user_id: i64) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if let Some(u) = self.user(user_id)? {
            out.push(u.email);
        }
        out.extend(self.aliases_of(user_id)?.into_iter().filter(|a| !a.starts_with('@')));
        let db = self.db();
        let st = db.prepare("SELECT u.email FROM permissions p JOIN users u ON u.id = p.owner_id WHERE p.grantee_id = ?1 AND p.send_as = 1")?;
        let rows = st.query_map([user_id], |r| r.get::<_, String>(0))?;
        for r in rows {
            out.push(r);
        }
        Ok(out)
    }

    // -------------------------------------------------------- permissions

    pub fn grant(&self, owner: i64, grantee: i64, full_access: bool, send_as: bool) -> Result<()> {
        if !full_access && !send_as {
            self.db().execute("DELETE FROM permissions WHERE owner_id = ?1 AND grantee_id = ?2", params![owner, grantee])?;
        } else {
            self.db().execute(
                "INSERT INTO permissions (owner_id, grantee_id, full_access, send_as) VALUES (?1, ?2, ?3, ?4) ON CONFLICT(owner_id, grantee_id) DO UPDATE SET full_access = excluded.full_access, send_as = excluded.send_as",
                params![owner, grantee, full_access as i64, send_as as i64],
            )?;
        }
        Ok(())
    }

    /// (owner id, grantee id, full access, send as)
    pub fn permissions(&self) -> Result<Vec<(i64, i64, bool, bool)>> {
        let db = self.db();
        let st = db.prepare("SELECT owner_id, grantee_id, full_access, send_as FROM permissions")?;
        let rows = st.query_map(params![], |r| Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? != 0, r.get::<_, i64>(3)? != 0)))?;
        Ok(rows)
    }

    /// Whether `user` may open the mailbox of `owner`.
    pub fn can_access(&self, user: i64, owner: i64) -> bool {
        user == owner
            || self
                .db()
                .query_row("SELECT 1 FROM permissions WHERE owner_id = ?1 AND grantee_id = ?2 AND full_access = 1", params![owner, user], |_| Ok(()))
                .optional()
                .ok()
                .flatten()
                .is_some()
    }

    // ------------------------------------------------------------ folders

    pub fn folders(&self, user_id: i64) -> Result<Vec<Folder>> {
        let db = self.db();
        let st = db.prepare(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE user_id = ?1 AND deleted = 0 ORDER BY id"))?;
        let rows = st.query_map([user_id], folder_from_row)?;
        Ok(rows)
    }

    pub fn folder(&self, id: i64) -> Result<Option<Folder>> {
        self
            .db()
            .query_row(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE id = ?1 AND deleted = 0"), [id], folder_from_row)
            .optional()
    }

    pub fn folder_by_role(&self, user_id: i64, role: &str) -> Result<Option<Folder>> {
        let role = if role == "root" { "msgfolderroot" } else { role };
        self
            .db()
            .query_row(&format!("SELECT {FOLDER_COLUMNS} FROM folders WHERE user_id = ?1 AND role = ?2 AND deleted = 0"), params![user_id, role], folder_from_row)
            .optional()
    }

    /// Folder by role, created when missing (e.g. after an upgrade).
    pub fn ensure_folder(&self, user_id: i64, role: &str) -> Result<Folder> {
        if let Some(f) = self.folder_by_role(user_id, role)? {
            return Ok(f);
        }
        let (_, class, name) = DEFAULT_FOLDERS.iter().find(|(r, _, _)| *r == role).ok_or_else(|| anyhow!("unknown folder {role}"))?;
        let root = self.folder_by_role(user_id, "msgfolderroot")?.ok_or_else(|| anyhow!("mailbox without root folder"))?;
        let f = self.create_folder(user_id, Some(root.id), name, *class, "")?;
        self.db().execute("UPDATE folders SET role = ?1 WHERE id = ?2", params![role, f.id])?;
        self.folder(f.id)?.ok_or_else(|| anyhow!("folder vanished"))
    }

    pub fn create_folder(&self, user_id: i64, parent_id: Option<i64>, name: &str, class: FolderClass, _writer: &str) -> Result<Folder> {
        let name = name.trim();
        if name.is_empty() {
            bail!("folder name is empty");
        }
        let parent = match parent_id {
            Some(p) => p,
            None => self.folder_by_role(user_id, "msgfolderroot")?.ok_or_else(|| anyhow!("mailbox without root folder"))?.id,
        };
        let id = {
            let mut db = self.db();
            let tx = db.transaction()?;
            let exists: Option<i64> = tx
                .query_row("SELECT id FROM folders WHERE user_id = ?1 AND parent_id = ?2 AND LOWER(name) = LOWER(?3) AND deleted = 0", params![user_id, parent, name], |r| r.get(0))
                .optional()?;
            if exists.is_some() {
                bail!("a folder named {name} already exists");
            }
            let owner: Option<i64> = tx.query_row("SELECT user_id FROM folders WHERE id = ?1 AND deleted = 0", [parent], |r| r.get(0)).optional()?;
            if owner != Some(user_id) {
                bail!("parent folder not found");
            }
            let modseq = Self::next_modseq(&tx, user_id)?;
            let id: i64 = tx.query_row(
                "INSERT INTO folders (user_id, parent_id, name, class, modseq, created_modseq) VALUES (?1, ?2, ?3, ?4, ?5, ?5) RETURNING id",
                params![user_id, parent, name, class.as_str(), modseq],
                |r| r.get(0),
            )?;
            tx.commit()?;
            id
        };
        self.notify(user_id, 0);
        self.folder(id)?.ok_or_else(|| anyhow!("folder vanished"))
    }

    pub fn update_folder(&self, id: i64, name: Option<&str>, parent_id: Option<i64>) -> Result<Folder> {
        let f = self.folder(id)?.ok_or_else(|| anyhow!("folder not found"))?;
        if f.role.is_some() && (name.is_some_and(|n| n != f.name) || parent_id.is_some_and(|p| Some(p) != f.parent_id)) {
            bail!("default folders cannot be renamed or moved");
        }
        if let Some(p) = parent_id {
            // No cycles: the new parent must not be inside the folder.
            let mut cur = Some(p);
            while let Some(c) = cur {
                if c == id {
                    bail!("a folder cannot be moved into itself");
                }
                cur = self.folder(c)?.and_then(|x| x.parent_id);
            }
        }
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, f.user_id)?;
            tx.execute(
                "UPDATE folders SET name = COALESCE(?1, name), parent_id = COALESCE(?2, parent_id), modseq = ?3 WHERE id = ?4",
                params![name.map(str::trim), parent_id, modseq, id],
            )?;
            tx.commit()?;
        }
        self.notify(f.user_id, 0);
        self.folder(id)?.ok_or_else(|| anyhow!("folder vanished"))
    }

    /// Delete a folder with its subfolders and items.
    pub fn delete_folder(&self, id: i64, writer: &str) -> Result<()> {
        let f = self.folder(id)?.ok_or_else(|| anyhow!("folder not found"))?;
        if f.role.is_some() {
            bail!("default folders cannot be deleted");
        }
        let all = self.folders(f.user_id)?;
        let mut ids = vec![id];
        let mut i = 0;
        while i < ids.len() {
            let cur = ids[i];
            ids.extend(all.iter().filter(|x| x.parent_id == Some(cur)).map(|x| x.id));
            i += 1;
        }
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, f.user_id)?;
            for fid in &ids {
                tx.execute("UPDATE folders SET deleted = 1, modseq = ?1 WHERE id = ?2", params![modseq, fid])?;
                tx.execute(
                    "INSERT INTO tombstones (user_id, folder_id, item_id, modseq, writer) SELECT user_id, folder_id, id, ?1, ?2 FROM items WHERE folder_id = ?3",
                    params![modseq, writer, fid],
                )?;
                tx.execute("DELETE FROM items WHERE folder_id = ?1", [fid])?;
            }
            tx.commit()?;
        }
        self.notify(f.user_id, 0);
        Ok(())
    }

    /// Delete all items of a folder (and optionally its subfolders).
    pub fn empty_folder(&self, id: i64, subfolders: bool, writer: &str) -> Result<()> {
        let f = self.folder(id)?.ok_or_else(|| anyhow!("folder not found"))?;
        if subfolders {
            for child in self.folders(f.user_id)?.into_iter().filter(|c| c.parent_id == Some(id)) {
                if child.role.is_none() {
                    self.delete_folder(child.id, writer)?;
                }
            }
        }
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, f.user_id)?;
            tx.execute(
                "INSERT INTO tombstones (user_id, folder_id, item_id, modseq, writer) SELECT user_id, folder_id, id, ?1, ?2 FROM items WHERE folder_id = ?3",
                params![modseq, writer, id],
            )?;
            tx.execute("DELETE FROM items WHERE folder_id = ?1", [id])?;
            tx.commit()?;
        }
        self.notify(f.user_id, id);
        Ok(())
    }

    pub fn hierarchy_changes(&self, user_id: i64, since: i64) -> Result<HierarchyChanges> {
        let db = self.db();
        let current: i64 = db.query_row("SELECT modseq FROM users WHERE id = ?1", [user_id], |r| r.get(0))?;
        let st = db.prepare(&format!("SELECT {FOLDER_COLUMNS}, created_modseq FROM folders WHERE user_id = ?1 AND modseq > ?2 ORDER BY id"))?;
        let rows: Vec<(Folder, i64)> = st.query_map(params![user_id, since], |r| Ok((folder_from_row(r)?, r.get(8)?)))?;
        let mut out = HierarchyChanges { modseq: current, ..Default::default() };
        for (f, created) in rows {
            if f.deleted {
                // Folders created and deleted after the sync point never reached the client.
                if since > 0 && created <= since {
                    out.deleted.push(f);
                }
            } else if created > since {
                out.added.push(f);
            } else {
                out.updated.push(f);
            }
        }
        Ok(out)
    }

    pub fn unread_count(&self, folder_id: i64) -> Result<(i64, i64)> {
        self.db().query_row(
            "SELECT COUNT(*), COALESCE(SUM(CASE WHEN flags & 1 = 0 THEN 1 ELSE 0 END), 0) FROM items WHERE folder_id = ?1",
            [folder_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
    }

    pub fn folder_size(&self, folder_id: i64) -> Result<i64> {
        self.db().query_row("SELECT CAST(COALESCE(SUM(size), 0) AS BIGINT) FROM items WHERE folder_id = ?1", [folder_id], |r| r.get(0))
    }

    // -------------------------------------------------------------- items

    pub fn insert_mail(&self, m: NewMail<'_>) -> Result<i64> {
        let meta = mime::parse_meta(m.mime);
        let recipients = Recipients { to: meta.to.clone(), cc: meta.cc.clone(), bcc: meta.bcc.clone(), reply_to: meta.reply_to.clone() };
        let id = {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, m.user_id)?;
            let id: i64 = tx.query_row(
                "INSERT INTO items (user_id, folder_id, kind, modseq, created_modseq, writer, received, size, flags, subject, sender, recipients, preview, has_attachments, importance, message_id, message_class, mime, data)
                 VALUES (?1, ?2, 'mail', ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17) RETURNING id",
                params![
                    m.user_id,
                    m.folder_id,
                    modseq,
                    m.writer,
                    m.received,
                    m.mime.len() as i64,
                    m.flags,
                    meta.subject,
                    meta.from.as_ref().map(|p| serde_json::to_string(p).unwrap_or_default()),
                    serde_json::to_string(&recipients)?,
                    meta.preview,
                    meta.has_attachments as i64,
                    format!("{:?}", meta.importance).to_ascii_lowercase(),
                    meta.message_id,
                    meta.message_class,
                    m.mime,
                    meta.calendar,
                ],
                |r| r.get(0),
            )?;
            tx.commit()?;
            id
        };
        self.notify(m.user_id, m.folder_id);
        Ok(id)
    }

    /// Insert a calendar/contact/task/note item.
    pub fn insert_pim(&self, user_id: i64, folder_id: i64, kind: ItemKind, data: &str, uid: Option<&str>, writer: &str) -> Result<i64> {
        let (subject, range) = pim_index(kind, data);
        let id = {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, user_id)?;
            let id: i64 = tx.query_row(
                "INSERT INTO items (user_id, folder_id, kind, uid, modseq, created_modseq, writer, received, size, subject, data, range_start, range_end, flags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1) RETURNING id",
                params![user_id, folder_id, kind.as_str(), uid, modseq, writer, now(), data.len() as i64, subject, data, range.0, range.1],
                |r| r.get(0),
            )?;
            tx.commit()?;
            id
        };
        self.notify(user_id, folder_id);
        Ok(id)
    }

    pub fn update_pim(&self, id: i64, data: &str, writer: &str) -> Result<()> {
        let item = self.item(id)?.ok_or_else(|| anyhow!("item not found"))?;
        let (subject, range) = pim_index(item.kind, data);
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, item.user_id)?;
            tx.execute(
                "UPDATE items SET data = ?1, subject = ?2, range_start = ?3, range_end = ?4, modseq = ?5, writer = ?6, size = ?7 WHERE id = ?8",
                params![data, subject, range.0, range.1, modseq, writer, data.len() as i64, id],
            )?;
            tx.commit()?;
        }
        self.notify(item.user_id, item.folder_id);
        Ok(())
    }

    /// Set flags and/or categories of an item.
    pub fn update_flags(&self, id: i64, set: i64, clear: i64, categories: Option<&[String]>, writer: &str) -> Result<()> {
        let item = self.item(id)?.ok_or_else(|| anyhow!("item not found"))?;
        let flags = (item.flags | set) & !clear;
        let cats = categories.map(|c| serde_json::to_string(c).unwrap_or_else(|_| "[]".into()));
        if flags == item.flags && cats.as_ref().is_none_or(|c| serde_json::from_str::<Vec<String>>(c).unwrap_or_default() == item.categories) {
            return Ok(());
        }
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, item.user_id)?;
            tx.execute(
                "UPDATE items SET flags = ?1, categories = COALESCE(?2, categories), modseq = ?3, writer = ?4 WHERE id = ?5",
                params![flags, cats, modseq, writer, id],
            )?;
            tx.commit()?;
        }
        self.notify(item.user_id, item.folder_id);
        Ok(())
    }

    /// Replace the MIME content of a mail item (drafts).
    pub fn replace_mime(&self, id: i64, raw: &[u8], writer: &str) -> Result<()> {
        let item = self.item(id)?.ok_or_else(|| anyhow!("item not found"))?;
        let meta = mime::parse_meta(raw);
        let recipients = Recipients { to: meta.to.clone(), cc: meta.cc.clone(), bcc: meta.bcc.clone(), reply_to: meta.reply_to.clone() };
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, item.user_id)?;
            tx.execute(
                "UPDATE items SET mime = ?1, size = ?2, subject = ?3, sender = ?4, recipients = ?5, preview = ?6, has_attachments = ?7, message_id = ?8, modseq = ?9, writer = ?10 WHERE id = ?11",
                params![
                    raw,
                    raw.len() as i64,
                    meta.subject,
                    meta.from.as_ref().map(|p| serde_json::to_string(p).unwrap_or_default()),
                    serde_json::to_string(&recipients)?,
                    meta.preview,
                    meta.has_attachments as i64,
                    meta.message_id,
                    modseq,
                    writer,
                    id
                ],
            )?;
            tx.commit()?;
        }
        self.notify(item.user_id, item.folder_id);
        Ok(())
    }

    /// Move an item to another folder of the same mailbox. The item keeps
    /// its id; the old folder gets a tombstone, the new one sees an add.
    pub fn move_item(&self, id: i64, target_folder: i64, writer: &str) -> Result<()> {
        let item = self.item(id)?.ok_or_else(|| anyhow!("item not found"))?;
        if item.folder_id == target_folder {
            return Ok(());
        }
        let target = self.folder(target_folder)?.ok_or_else(|| anyhow!("target folder not found"))?;
        if target.user_id != item.user_id {
            bail!("cannot move items between mailboxes");
        }
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, item.user_id)?;
            tx.execute(
                "INSERT INTO tombstones (user_id, folder_id, item_id, modseq, writer) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![item.user_id, item.folder_id, id, modseq, writer],
            )?;
            tx.execute(
                "UPDATE items SET folder_id = ?1, modseq = ?2, created_modseq = ?2, writer = ?3 WHERE id = ?4",
                params![target_folder, modseq, writer, id],
            )?;
            tx.commit()?;
        }
        self.notify(item.user_id, item.folder_id);
        self.notify(item.user_id, target_folder);
        Ok(())
    }

    /// Copy an item into a folder (possibly of another mailbox). Returns the new id.
    pub fn copy_item(&self, id: i64, target_folder: i64, writer: &str) -> Result<i64> {
        let item = self.item(id)?.ok_or_else(|| anyhow!("item not found"))?;
        let target = self.folder(target_folder)?.ok_or_else(|| anyhow!("target folder not found"))?;
        if item.kind == ItemKind::Mail {
            let raw = self.mime(id)?.unwrap_or_default();
            self.insert_mail(NewMail { user_id: target.user_id, folder_id: target.id, mime: &raw, flags: item.flags, received: item.received, writer })
        } else {
            let data = item.data.clone().unwrap_or_default();
            self.insert_pim(target.user_id, target.id, item.kind, &data, item.uid.as_deref(), writer)
        }
    }

    pub fn delete_item(&self, id: i64, writer: &str) -> Result<()> {
        let Some(item) = self.item(id)? else { return Ok(()) };
        {
            let mut db = self.db();
            let tx = db.transaction()?;
            let modseq = Self::next_modseq(&tx, item.user_id)?;
            tx.execute(
                "INSERT INTO tombstones (user_id, folder_id, item_id, modseq, writer) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![item.user_id, item.folder_id, id, modseq, writer],
            )?;
            tx.execute("DELETE FROM items WHERE id = ?1", [id])?;
            tx.commit()?;
        }
        self.notify(item.user_id, item.folder_id);
        Ok(())
    }

    pub fn item(&self, id: i64) -> Result<Option<Item>> {
        self.db().query_row(&format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?1"), [id], item_from_row).optional()
    }

    pub fn mime(&self, id: i64) -> Result<Option<Vec<u8>>> {
        Ok(self.db().query_row("SELECT mime FROM items WHERE id = ?1", [id], |r| r.get::<_, Option<Vec<u8>>>(0)).optional()?.flatten())
    }

    /// Items of a folder, newest first.
    pub fn items(&self, folder_id: i64, offset: i64, limit: i64) -> Result<Vec<Item>> {
        let db = self.db();
        let st = db.prepare(&format!("SELECT {ITEM_COLUMNS} FROM items WHERE folder_id = ?1 ORDER BY received DESC, id DESC LIMIT ?2 OFFSET ?3"))?;
        let rows = st.query_map(params![folder_id, limit, offset], item_from_row)?;
        Ok(rows)
    }



    /// Calendar items that may have occurrences in `[from, to)`.
    pub fn events_in_range(&self, folder_id: i64, from: i64, to: i64) -> Result<Vec<Item>> {
        let db = self.db();
        let st = db.prepare(&format!(
            "SELECT {ITEM_COLUMNS} FROM items WHERE folder_id = ?1 AND kind = 'event' AND range_start < ?3 AND (range_end IS NULL OR range_end > ?2)"
        ))?;
        let rows = st.query_map(params![folder_id, from, to], item_from_row)?;
        Ok(rows)
    }

    pub fn items_by_uid(&self, user_id: i64, uid: &str) -> Result<Vec<Item>> {
        let db = self.db();
        let st = db.prepare(&format!("SELECT {ITEM_COLUMNS} FROM items WHERE user_id = ?1 AND uid = ?2"))?;
        let rows = st.query_map(params![user_id, uid], item_from_row)?;
        Ok(rows)
    }


    /// Simple full-text search over subject, sender, recipients, preview and
    /// (for PIM items) the JSON data. All words must match.
    pub fn search(&self, user_id: i64, folder_ids: &[i64], query: &str, limit: i64) -> Result<Vec<Item>> {
        let words: Vec<String> = query.split_whitespace().map(|w| format!("%{}%", w.replace(['%', '_'], ""))).filter(|w| w.len() > 2).collect();
        let db = self.db();
        let mut sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE user_id = ?1");
        if !folder_ids.is_empty() {
            sql.push_str(&format!(" AND folder_id IN ({})", folder_ids.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",")));
        }
        for i in 0..words.len() {
            let p = i + 2;
            sql.push_str(&format!(
                " AND (subject LIKE ?{p} OR COALESCE(sender, '') LIKE ?{p} OR recipients LIKE ?{p} OR preview LIKE ?{p} OR COALESCE(data, '') LIKE ?{p})"
            ));
        }
        sql.push_str(&format!(" ORDER BY received DESC LIMIT {}", limit.max(1)));
        let st = db.prepare(&sql)?;
        let mut values: Vec<crate::db::Val> = vec![user_id.into()];
        values.extend(words.into_iter().map(crate::db::Val::from));
        let rows = st.query_map(values, item_from_row)?;
        Ok(rows)
    }

    /// Changes in a folder after `since`, oldest first. Changes made by
    /// `exclude_writer` (the asking client itself) are left out.
    pub fn changes(&self, folder_id: i64, since: i64, exclude_writer: Option<&str>) -> Result<Vec<Change>> {
        let db = self.db();
        // Omit the writer filter entirely when there is nothing to exclude. (A
        // sentinel value can't be used: PostgreSQL rejects NUL in text and the
        // empty string is a real writer.)
        let filter = if exclude_writer.is_some() { " AND writer != ?3" } else { "" };
        let mut out = Vec::new();
        {
            let sql = format!("SELECT {ITEM_COLUMNS} FROM items WHERE folder_id = ?1 AND modseq > ?2{filter} ORDER BY modseq");
            let mut p: Vec<crate::db::Val> = vec![folder_id.into(), since.into()];
            if let Some(w) = exclude_writer {
                p.push(w.into());
            }
            for item in db.query_all(&sql, p, item_from_row)? {
                out.push(if item.created_modseq > since { Change::Add(item) } else { Change::Update(item) });
            }
        }
        {
            let sql = format!("SELECT item_id, modseq FROM tombstones WHERE folder_id = ?1 AND modseq > ?2{filter} ORDER BY modseq");
            let mut p: Vec<crate::db::Val> = vec![folder_id.into(), since.into()];
            if let Some(w) = exclude_writer {
                p.push(w.into());
            }
            for (item_id, modseq) in db.query_all(&sql, p, |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))? {
                // An item created and deleted after the sync point never reached the client.
                out.push(Change::Delete { item_id, modseq });
            }
        }
        // Drop deletes of items that are (again) present in the folder or were moved back.
        let present: std::collections::HashSet<i64> = out
            .iter()
            .filter_map(|c| match c {
                Change::Add(i) | Change::Update(i) => Some(i.id),
                _ => None,
            })
            .collect();
        out.retain(|c| !matches!(c, Change::Delete { item_id, .. } if present.contains(item_id)));
        out.sort_by_key(|c| c.modseq());
        Ok(out)
    }

    /// Whether anything changed in a folder after `since` (for Ping/streaming).
    /// Aggregate changes across all of a user's folders since `since`, together
    /// with the current modseq to use as the next sync point. Powers the native
    /// client's incremental sync.
    pub fn account_changes(&self, user_id: i64, since: i64) -> Result<(Vec<Change>, i64)> {
        let now = self.current_modseq(user_id)?;
        let mut all = Vec::new();
        for f in self.folders(user_id)? {
            all.extend(self.changes(f.id, since, None)?);
        }
        all.sort_by_key(|c| c.modseq());
        Ok((all, now))
    }

    pub fn has_changes(&self, folder_id: i64, since: i64, exclude_writer: Option<&str>) -> Result<bool> {
        let db = self.db();
        let filter = if exclude_writer.is_some() { " AND writer != ?3" } else { "" };
        let mut p: Vec<crate::db::Val> = vec![folder_id.into(), since.into()];
        if let Some(w) = exclude_writer {
            p.push(w.into());
        }
        let items: i64 = db.query_row(&format!("SELECT COUNT(*) FROM items WHERE folder_id = ?1 AND modseq > ?2{filter}"), p.clone(), |r| r.get(0))?;
        if items > 0 {
            return Ok(true);
        }
        let deleted: i64 = db.query_row(&format!("SELECT COUNT(*) FROM tombstones WHERE folder_id = ?1 AND modseq > ?2{filter}"), p, |r| r.get(0))?;
        Ok(deleted > 0)
    }

    /// Remove tombstones older than `days` (clients that have not synced since then start over).
    pub fn prune(&self, days: i64) -> Result<()> {
        let db = self.db();
        // Tombstones have no timestamp; keep the newest 10 000 per user and drop older sync keys.
        db.execute(
            "DELETE FROM tombstones WHERE rowid IN (SELECT rowid FROM (SELECT rowid, ROW_NUMBER() OVER (PARTITION BY user_id ORDER BY modseq DESC) AS n FROM tombstones) AS t WHERE t.n > 10000)",
            params![],
        )?;
        db.execute("DELETE FROM sync_keys WHERE created < ?1", [now() - days * 86_400])?;
        db.execute("DELETE FROM sessions WHERE expires < ?1", [now()])?;
        Ok(())
    }

    // ----------------------------------------------------- sync bookkeeping

    /// Store a sync key of a device and collection (ActiveSync) with the
    /// modseq it stands for. Older keys than the previous one are dropped.
    pub fn save_sync_key(&self, user_id: i64, device: &str, collection: &str, key: i64, modseq: i64) -> Result<()> {
        let db = self.db();
        db.execute(
            "INSERT INTO sync_keys (user_id, device_id, collection, sync_key, modseq, created) VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (user_id, device_id, collection, sync_key) DO UPDATE SET modseq = excluded.modseq, created = excluded.created",
            params![user_id, device, collection, key, modseq, now()],
        )?;
        db.execute(
            "DELETE FROM sync_keys WHERE user_id = ?1 AND device_id = ?2 AND collection = ?3 AND sync_key < CAST(?4 AS BIGINT) - 1",
            params![user_id, device, collection, key],
        )?;
        Ok(())
    }

    pub fn sync_key(&self, user_id: i64, device: &str, collection: &str, key: i64) -> Result<Option<i64>> {
        self
            .db()
            .query_row(
                "SELECT modseq FROM sync_keys WHERE user_id = ?1 AND device_id = ?2 AND collection = ?3 AND sync_key = ?4",
                params![user_id, device, collection, key],
                |r| r.get(0),
            )
            .optional()
    }

    pub fn latest_sync_key(&self, user_id: i64, device: &str, collection: &str) -> Result<Option<(i64, i64)>> {
        self
            .db()
            .query_row(
                "SELECT sync_key, modseq FROM sync_keys WHERE user_id = ?1 AND device_id = ?2 AND collection = ?3 ORDER BY sync_key DESC LIMIT 1",
                params![user_id, device, collection],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
    }

    pub fn reset_sync(&self, user_id: i64, device: &str, collection: Option<&str>) -> Result<()> {
        let db = self.db();
        match collection {
            Some(c) => db.execute("DELETE FROM sync_keys WHERE user_id = ?1 AND device_id = ?2 AND collection = ?3", params![user_id, device, c])?,
            None => db.execute("DELETE FROM sync_keys WHERE user_id = ?1 AND device_id = ?2", params![user_id, device])?,
        };
        Ok(())
    }

    pub fn device_state(&self, user_id: i64, device: &str, key: &str) -> Result<Option<String>> {
        self
            .db()
            .query_row("SELECT value FROM device_state WHERE user_id = ?1 AND device_id = ?2 AND key = ?3", params![user_id, device, key], |r| r.get(0))
            .optional()
    }

    pub fn set_device_state(&self, user_id: i64, device: &str, key: &str, value: &str) -> Result<()> {
        self.db().execute("INSERT INTO device_state (user_id, device_id, key, value) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (user_id, device_id, key) DO UPDATE SET value = excluded.value", params![user_id, device, key, value])?;
        Ok(())
    }

    // ------------------------------------------------------------ devices

    pub fn touch_device(&self, user_id: i64, device: &str, device_type: &str, user_agent: &str) -> Result<Device> {
        let t = now();
        {
            let db = self.db();
            db.execute(
                "INSERT INTO devices (user_id, device_id, device_type, user_agent, first_seen, last_seen) VALUES (?1, ?2, ?3, ?4, ?5, ?5)
                 ON CONFLICT(user_id, device_id) DO UPDATE SET last_seen = excluded.last_seen, device_type = excluded.device_type, user_agent = CASE WHEN excluded.user_agent = '' THEN user_agent ELSE excluded.user_agent END",
                params![user_id, device, device_type, user_agent, t],
            )?;
        }
        self.device(user_id, device)?.ok_or_else(|| anyhow!("device vanished"))
    }

    pub fn device(&self, user_id: i64, device: &str) -> Result<Option<Device>> {
        self
            .db()
            .query_row(
                "SELECT user_id, device_id, device_type, user_agent, info, policy_key, wipe, blocked, first_seen, last_seen FROM devices WHERE user_id = ?1 AND device_id = ?2",
                params![user_id, device],
                device_from_row,
            )
            .optional()
    }

    pub fn devices(&self, user_id: Option<i64>) -> Result<Vec<Device>> {
        let db = self.db();
        let st = db.prepare(
            // CAST so PostgreSQL can infer the parameter type (a bare `?1 IS
            // NULL` leaves it undetermined); harmless under SQLite.
            "SELECT user_id, device_id, device_type, user_agent, info, policy_key, wipe, blocked, first_seen, last_seen FROM devices WHERE CAST(?1 AS BIGINT) IS NULL OR user_id = CAST(?1 AS BIGINT) ORDER BY last_seen DESC",
        )?;
        let rows = st.query_map([user_id], device_from_row)?;
        Ok(rows)
    }

    pub fn set_device_info(&self, user_id: i64, device: &str, info: &serde_json::Value) -> Result<()> {
        self.db().execute("UPDATE devices SET info = ?1 WHERE user_id = ?2 AND device_id = ?3", params![info.to_string(), user_id, device])?;
        Ok(())
    }

    pub fn set_policy_key(&self, user_id: i64, device: &str, key: i64) -> Result<()> {
        self.db().execute("UPDATE devices SET policy_key = ?1 WHERE user_id = ?2 AND device_id = ?3", params![key, user_id, device])?;
        Ok(())
    }

    /// 0 = none, 1 = wipe requested, 2 = wipe acknowledged.
    pub fn set_wipe(&self, user_id: i64, device: &str, wipe: i64) -> Result<()> {
        self.db().execute("UPDATE devices SET wipe = ?1 WHERE user_id = ?2 AND device_id = ?3", params![wipe, user_id, device])?;
        Ok(())
    }

    pub fn set_blocked(&self, user_id: i64, device: &str, blocked: bool) -> Result<()> {
        self.db().execute("UPDATE devices SET blocked = ?1 WHERE user_id = ?2 AND device_id = ?3", params![blocked as i64, user_id, device])?;
        Ok(())
    }

    pub fn remove_device(&self, user_id: i64, device: &str) -> Result<()> {
        let db = self.db();
        db.execute("DELETE FROM devices WHERE user_id = ?1 AND device_id = ?2", params![user_id, device])?;
        db.execute("DELETE FROM sync_keys WHERE user_id = ?1 AND device_id = ?2", params![user_id, device])?;
        db.execute("DELETE FROM device_state WHERE user_id = ?1 AND device_id = ?2", params![user_id, device])?;
        Ok(())
    }

    // -------------------------------------------------------------- queue

    pub fn enqueue(&self, sender: &str, recipients: &[String], raw: &[u8]) -> Result<i64> {
        let db = self.db();
        let id: i64 = db.query_row(
            "INSERT INTO queue (sender, recipients, mime, next_attempt, created) VALUES (?1, ?2, ?3, ?4, ?4) RETURNING id",
            params![sender, serde_json::to_string(recipients)?, raw, now()],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn due_messages(&self, limit: i64) -> Result<Vec<QueuedMessage>> {
        let db = self.db();
        let st = db.prepare("SELECT id, sender, recipients, mime, attempts, next_attempt, created, last_error FROM queue WHERE next_attempt <= ?1 ORDER BY next_attempt LIMIT ?2")?;
        let rows = st.query_map(params![now(), limit], queue_from_row)?;
        Ok(rows)
    }

    pub fn queue(&self) -> Result<Vec<QueuedMessage>> {
        let db = self.db();
        let st = db.prepare("SELECT id, sender, recipients, mime, attempts, next_attempt, created, last_error FROM queue ORDER BY created")?;
        let rows = st.query_map(params![], queue_from_row)?;
        Ok(rows)
    }

    /// Keep only the recipients still to be tried and schedule the next attempt.
    pub fn defer(&self, id: i64, recipients: &[String], next_attempt: i64, error: &str) -> Result<()> {
        self.db().execute(
            "UPDATE queue SET recipients = ?1, attempts = attempts + 1, next_attempt = ?2, last_error = ?3 WHERE id = ?4",
            params![serde_json::to_string(recipients)?, next_attempt, error, id],
        )?;
        Ok(())
    }

    pub fn retry_now(&self, id: i64) -> Result<()> {
        self.db().execute("UPDATE queue SET next_attempt = ?1 WHERE id = ?2", params![now(), id])?;
        Ok(())
    }

    pub fn dequeue(&self, id: i64) -> Result<()> {
        self.db().execute("DELETE FROM queue WHERE id = ?1", [id])?;
        Ok(())
    }

    // --------------------------------------------------------------- dkim

    pub fn dkim(&self, domain: &str) -> Result<Option<DkimKey>> {
        self
            .db()
            .query_row("SELECT domain, selector, private_key, public_key FROM dkim WHERE domain = ?1", [domain.to_ascii_lowercase()], |r| {
                Ok(DkimKey { domain: r.get(0)?, selector: r.get(1)?, private_key: r.get(2)?, public_key: r.get(3)? })
            })
            .optional()
    }

    pub fn set_dkim(&self, key: &DkimKey) -> Result<()> {
        self.db().execute(
            "INSERT INTO dkim (domain, selector, private_key, public_key, created) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT (domain) DO UPDATE SET selector = excluded.selector, private_key = excluded.private_key, public_key = excluded.public_key, created = excluded.created",
            params![&key.domain, &key.selector, &key.private_key, &key.public_key, now()],
        )?;
        Ok(())
    }

    // ----------------------------------------------------------- sessions

    pub fn create_session(&self, user_id: i64, lifetime: i64) -> Result<String> {
        let mut bytes = [0u8; 32];
        rand::fill(&mut bytes);
        let token = hex::encode(bytes);
        self.db().execute("INSERT INTO sessions (token, user_id, created, expires) VALUES (?1, ?2, ?3, ?4)", params![&token, user_id, now(), now() + lifetime])?;
        Ok(token)
    }

    pub fn session_user(&self, token: &str) -> Result<Option<User>> {
        let id: Option<i64> = self
            .db()
            .query_row("SELECT user_id FROM sessions WHERE token = ?1 AND expires > ?2", params![token, now()], |r| r.get(0))
            .optional()?;
        match id {
            Some(id) => self.user(id),
            None => Ok(None),
        }
    }

    pub fn delete_session(&self, token: &str) -> Result<()> {
        self.db().execute("DELETE FROM sessions WHERE token = ?1", [token])?;
        Ok(())
    }
}

fn device_from_row(r: &Row) -> Result<Device> {
    let info: String = r.get(4)?;
    Ok(Device {
        user_id: r.get(0)?,
        device_id: r.get(1)?,
        device_type: r.get(2)?,
        user_agent: r.get(3)?,
        info: serde_json::from_str(&info).unwrap_or(serde_json::Value::Null),
        policy_key: r.get(5)?,
        wipe: r.get(6)?,
        blocked: r.get::<_, i64>(7)? != 0,
        first_seen: r.get(8)?,
        last_seen: r.get(9)?,
    })
}

fn queue_from_row(r: &Row) -> Result<QueuedMessage> {
    let recipients: String = r.get(2)?;
    Ok(QueuedMessage {
        id: r.get(0)?,
        sender: r.get(1)?,
        recipients: serde_json::from_str(&recipients).unwrap_or_default(),
        mime: r.get(3)?,
        attempts: r.get(4)?,
        next_attempt: r.get(5)?,
        created: r.get(6)?,
        last_error: r.get(7)?,
    })
}

/// Subject and time range of a PIM item for the index columns.
fn pim_index(kind: ItemKind, data: &str) -> (String, (Option<i64>, Option<i64>)) {
    match kind {
        ItemKind::Event => match serde_json::from_str::<model::Event>(data) {
            Ok(e) => (e.subject.clone(), (Some(e.start), e.last_end())),
            Err(_) => (String::new(), (None, None)),
        },
        ItemKind::Contact => (serde_json::from_str::<model::Contact>(data).map(|c| c.effective_name()).unwrap_or_default(), (None, None)),
        ItemKind::Task => (serde_json::from_str::<model::Task>(data).map(|t| t.subject).unwrap_or_default(), (None, None)),
        ItemKind::Note => (serde_json::from_str::<model::Note>(data).map(|t| t.subject).unwrap_or_default(), (None, None)),
        ItemKind::Mail => (String::new(), (None, None)),
    }
}

pub fn normalize_domain(name: &str) -> Result<String> {
    let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
    if name.is_empty() || !name.contains('.') || name.chars().any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '.')) {
        bail!("invalid domain name {name:?}");
    }
    Ok(name)
}

pub fn normalize_address(address: &str) -> Result<String> {
    let a = address.trim().to_ascii_lowercase();
    let Some((local, domain)) = a.rsplit_once('@') else { bail!("invalid address {address:?}") };
    if local.is_empty() || local.contains([' ', '<', '>', '"', ',']) {
        bail!("invalid address {address:?}");
    }
    normalize_domain(domain)?;
    Ok(a)
}

#[cfg(test)]
pub(crate) fn test_store() -> (Store, User) {
    let s = Store::open_memory().unwrap();
    s.add_domain("example.com").unwrap();
    let u = s.create_user("niklas@example.com", "Niklas Linz", "secret", "user").unwrap();
    (s, u)
}

/// Run the full store smoke test against a real PostgreSQL server when the
/// `ONDURA_TEST_PG` environment variable holds a libpq connection string, e.g.
/// `host=/var/run/postgresql user=ondura dbname=ondura`. The test resets the
/// `public` schema first, so point it only at a throwaway database.
#[cfg(test)]
#[test]
fn postgres_backend_smoke() {
    let Ok(url) = std::env::var("ONDURA_TEST_PG") else {
        eprintln!("skipping: set ONDURA_TEST_PG to a libpq connection string to run");
        return;
    };
    // Clean slate: drop and recreate the public schema.
    {
        let mut admin = postgres::Client::connect(&url, postgres::NoTls).expect("connect to reset schema");
        admin.batch_execute("DROP SCHEMA public CASCADE; CREATE SCHEMA public;").expect("reset schema");
    }

    let s = Store::connect_postgres(&url).expect("connect_postgres");
    s.add_domain("example.com").unwrap();
    // ON CONFLICT DO NOTHING: adding the same domain twice must not error.
    s.add_domain("example.com").unwrap();
    let u = s.create_user("niklas@example.com", "Niklas Linz", "secret", "user").unwrap();
    assert!(u.id > 0, "RETURNING id yielded a real id");

    // find_user exercises LIKE -> ILIKE (case-insensitive local part).
    let found = s.user_for_login("NIKLAS").unwrap().expect("user found case-insensitively");
    assert_eq!(found.id, u.id);

    // Mail insert (RETURNING + transaction + next_modseq) and change feed.
    let inbox = s.folder_by_role(u.id, "inbox").unwrap().unwrap();
    let start = s.current_modseq(u.id).unwrap();
    let raw = b"From: a@b.de\r\nSubject: Projektplan\r\n\r\nHallo Welt";
    let id = s.insert_mail(NewMail { user_id: u.id, folder_id: inbox.id, mime: raw, flags: 0, received: now(), writer: "smtp" }).unwrap();
    assert!(matches!(&s.changes(inbox.id, start, None).unwrap()[..], [Change::Add(i)] if i.subject == "Projektplan"));

    // Full-text search (dynamic params + ILIKE).
    let hits = s.search(u.id, &[inbox.id], "projektplan", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, id);

    // Flags, move, tombstones.
    let mid = s.current_modseq(u.id).unwrap();
    s.update_flags(id, model::flags::READ, 0, None, "eas:dev1").unwrap();
    let trash = s.folder_by_role(u.id, "deleteditems").unwrap().unwrap();
    s.move_item(id, trash.id, "ews").unwrap();
    assert!(matches!(&s.changes(inbox.id, mid, None).unwrap()[..], [Change::Delete { .. }]));

    // Mime round-trip through BYTEA.
    assert_eq!(s.mime(id).unwrap().as_deref(), Some(&raw[..]));

    // Collaborative document: create + server-side OT apply (transaction,
    // RETURNING, op log).
    let doc = s.create_doc(u.id, "Notes").unwrap();
    let op = serde_json::json!(["Hallo"]);
    let (rev, _) = s.apply_op(doc, u.id, 0, &op).unwrap();
    assert_eq!(rev, 1);
    assert_eq!(s.doc(doc).unwrap().unwrap().content, "Hallo");

    // Sessions and DKIM upsert.
    let token = s.create_session(u.id, 3600).unwrap();
    assert_eq!(s.session_user(&token).unwrap().unwrap().id, u.id);
    let key = DkimKey { domain: "example.com".into(), selector: "s1".into(), private_key: "priv".into(), public_key: "pub".into() };
    s.set_dkim(&key).unwrap();
    s.set_dkim(&DkimKey { selector: "s2".into(), ..key.clone() }).unwrap(); // ON CONFLICT DO UPDATE
    assert_eq!(s.dkim("example.com").unwrap().unwrap().selector, "s2");

    // Device listing: the optional-filter query (CAST so PostgreSQL can infer
    // the parameter type) must work both ways.
    assert!(s.devices(None).unwrap().is_empty());
    assert!(s.devices(Some(u.id)).unwrap().is_empty());

    // Admin Center queries, including aggregates — SUM over a BIGINT column
    // returns `numeric` in PostgreSQL, so these must be cast to BIGINT.
    s.used_bytes(u.id).unwrap();
    s.user_stats().unwrap();
    s.unread_count(trash.id).unwrap();
    s.audit_log(10).unwrap();
    s.domains().unwrap();
    s.users().unwrap();
    s.permissions().unwrap();
    s.aliases().unwrap();

    // Avatars round-trip through a BYTEA column.
    assert!(s.avatar_updated(u.id).unwrap().is_none());
    s.set_avatar(u.id, "image/png", b"\x89PNG fake").unwrap();
    assert!(s.avatar_updated(u.id).unwrap().is_some());
    let (ct, data, _) = s.avatar(u.id).unwrap().unwrap();
    assert_eq!(ct, "image/png");
    assert_eq!(data, b"\x89PNG fake");
    s.delete_avatar(u.id).unwrap();
    assert!(s.avatar(u.id).unwrap().is_none());

    // Queue enqueue/dequeue (RETURNING id).
    let qid = s.enqueue("a@example.com", &["b@x.de".into()], b"raw").unwrap();
    assert_eq!(s.queue().unwrap().len(), 1);
    s.dequeue(qid).unwrap();
    assert!(s.queue().unwrap().is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mailbox_and_changes() {
        let (s, u) = test_store();
        let inbox = s.folder_by_role(u.id, "inbox").unwrap().unwrap();
        let start = s.current_modseq(u.id).unwrap();
        let raw = b"From: a@b.de\r\nSubject: Hi\r\n\r\nHallo";
        let id = s.insert_mail(NewMail { user_id: u.id, folder_id: inbox.id, mime: raw, flags: 0, received: now(), writer: "smtp" }).unwrap();
        let c = s.changes(inbox.id, start, None).unwrap();
        assert!(matches!(&c[..], [Change::Add(i)] if i.subject == "Hi"));
        let mid = s.current_modseq(u.id).unwrap();
        s.update_flags(id, model::flags::READ, 0, None, "eas:dev1").unwrap();
        assert!(s.changes(inbox.id, mid, Some("eas:dev1")).unwrap().is_empty());
        assert!(matches!(&s.changes(inbox.id, mid, Some("ews")).unwrap()[..], [Change::Update(_)]));
        let trash = s.folder_by_role(u.id, "deleteditems").unwrap().unwrap();
        s.move_item(id, trash.id, "ews").unwrap();
        assert!(matches!(&s.changes(inbox.id, mid, None).unwrap()[..], [Change::Delete { .. }]));
        assert!(matches!(&s.changes(trash.id, mid, None).unwrap()[..], [Change::Add(_)]));
        assert_eq!(s.unread_count(trash.id).unwrap(), (1, 0));
    }

    #[test]
    fn addresses_and_aliases() {
        let (s, u) = test_store();
        s.add_alias("info@example.com", u.id).unwrap();
        assert_eq!(s.resolve_address("Info@Example.com").unwrap(), Some((u.id, true)));
        assert_eq!(s.resolve_address("niklas+news@example.com").unwrap(), Some((u.id, false)));
        assert_eq!(s.resolve_address("nobody@example.com").unwrap(), None);
        s.add_alias("@example.com", u.id).unwrap();
        assert_eq!(s.resolve_address("nobody@example.com").unwrap(), Some((u.id, false)));
        assert!(s.create_user("x@other.org", "", "pw", "user").is_err());
        assert_eq!(s.user_for_login("niklas").unwrap().unwrap().id, u.id);
        assert_eq!(s.user_for_login("CORP\\niklas@example.com").unwrap().unwrap().id, u.id);
    }

    #[test]
    fn switch_primary_address() {
        let (s, u) = test_store();
        s.add_alias("sales@example.com", u.id).unwrap();
        s.set_primary_address(u.id, "sales@example.com").unwrap();
        // The new address is primary, the old one is now an alias.
        assert_eq!(s.user(u.id).unwrap().unwrap().email, "sales@example.com");
        assert!(s.aliases_of(u.id).unwrap().iter().any(|a| a == "niklas@example.com"));
        // Both addresses still resolve to the same mailbox, so neither sign-in breaks.
        assert_eq!(s.resolve_address("niklas@example.com").unwrap(), Some((u.id, true)));
        assert_eq!(s.resolve_address("sales@example.com").unwrap(), Some((u.id, true)));
        // A catch-all or a non-alias cannot become primary.
        s.add_alias("@example.com", u.id).unwrap();
        assert!(s.set_primary_address(u.id, "@example.com").is_err());
        assert!(s.set_primary_address(u.id, "stranger@example.com").is_err());
    }

    #[test]
    fn folders_hierarchy() {
        let (s, u) = test_store();
        let base = s.current_modseq(u.id).unwrap();
        let all = s.hierarchy_changes(u.id, 0).unwrap();
        assert_eq!(all.added.len(), DEFAULT_FOLDERS.len() + 1);
        let f = s.create_folder(u.id, None, "Projekte", FolderClass::Mail, "ews").unwrap();
        let ch = s.hierarchy_changes(u.id, base).unwrap();
        assert_eq!(ch.added.len(), 1);
        s.delete_folder(f.id, "ews").unwrap();
        let ch = s.hierarchy_changes(u.id, ch.modseq).unwrap();
        assert_eq!(ch.deleted.len(), 1);
    }
}
