//! Backend-agnostic database layer.
//!
//! Ondura stores everything through [`Store`](crate::store::Store), which keeps
//! a single connection behind a `Mutex`. That connection is a [`Db`], which is
//! either an embedded SQLite database (the default, zero configuration) or a
//! PostgreSQL server.
//!
//! The two backends are hidden behind one small synchronous API that mirrors
//! the shape of `rusqlite` (`execute`, `query_one`, `query_opt`, `query_all`,
//! `transaction`), so the storage code reads the same regardless of backend.
//!
//! ## Why PostgreSQL runs on its own thread
//!
//! The synchronous `postgres` client drives an internal Tokio runtime and calls
//! `block_on`. Ondura's HTTP front ends already run inside a Tokio runtime, and
//! calling `block_on` from within a runtime panics. Because every `Store`
//! method takes the connection `Mutex` first, database access is already fully
//! serialised, so the PostgreSQL connection lives on a dedicated worker thread
//! and is reached over a channel. Statements block the caller just like the
//! SQLite path does, without ever nesting runtimes.

use std::marker::PhantomData;
use std::path::Path;
use std::sync::mpsc::Sender;

use anyhow::{Result, anyhow, bail};
use rusqlite::params_from_iter;

/// Error returned by [`Db::query_row`]/[`Tx::query_row`] when the query matched
/// no rows. [`OptionalExtension::optional`] turns it into `Ok(None)`.
#[derive(Debug)]
pub struct NoRows;
impl std::fmt::Display for NoRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "query returned no rows")
    }
}
impl std::error::Error for NoRows {}

/// Mirrors `rusqlite::OptionalExtension`: turn a "no rows" error into `None`.
pub trait OptionalExtension<T> {
    fn optional(self) -> Result<Option<T>>;
}
impl<T> OptionalExtension<T> for Result<T> {
    fn optional(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(e) if e.downcast_ref::<NoRows>().is_some() => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Which backend a [`Db`] talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Sqlite,
    Postgres,
}

// ------------------------------------------------------------------ values

/// A single value carried into or out of the database, independent of backend.
#[derive(Clone, Debug)]
pub enum Val {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

macro_rules! val_from_int {
    ($($t:ty),*) => {$(
        impl From<$t> for Val { fn from(v: $t) -> Self { Val::Int(v as i64) } }
    )*};
}
val_from_int!(i64, i32, i16, i8, u32, u16, u8, usize);

impl From<bool> for Val {
    fn from(v: bool) -> Self {
        Val::Int(v as i64)
    }
}
impl From<f64> for Val {
    fn from(v: f64) -> Self {
        Val::Real(v)
    }
}
impl From<&str> for Val {
    fn from(v: &str) -> Self {
        Val::Text(v.to_string())
    }
}
impl From<String> for Val {
    fn from(v: String) -> Self {
        Val::Text(v)
    }
}
impl From<&String> for Val {
    fn from(v: &String) -> Self {
        Val::Text(v.clone())
    }
}
impl From<Vec<u8>> for Val {
    fn from(v: Vec<u8>) -> Self {
        Val::Blob(v)
    }
}
impl From<&[u8]> for Val {
    fn from(v: &[u8]) -> Self {
        Val::Blob(v.to_vec())
    }
}
impl From<&Vec<u8>> for Val {
    fn from(v: &Vec<u8>) -> Self {
        Val::Blob(v.clone())
    }
}
impl<T: Into<Val>> From<Option<T>> for Val {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(x) => x.into(),
            None => Val::Null,
        }
    }
}
/// Borrowed `Copy` scalars, e.g. `&i64` from iterating a slice of ids, or
/// `&&str`. (A blanket `impl<T: Copy> From<&T>` would collide with the
/// `&String`/`&[u8]` impls under coherence, so these are spelled out.)
macro_rules! val_from_ref {
    ($($t:ty),*) => {$(
        impl From<&$t> for Val { fn from(v: &$t) -> Self { (*v).into() } }
    )*};
}
val_from_ref!(i64, i32, i16, i8, u32, u16, u8, usize, bool, f64);
impl From<&&str> for Val {
    fn from(v: &&str) -> Self {
        Val::Text((*v).to_string())
    }
}

/// Parameters passed to a statement. Implemented for `Vec<Val>` (what the
/// `params!` macro builds) and for small homogeneous arrays like `[id]`.
pub trait IntoParams {
    fn into_params(self) -> Vec<Val>;
}
impl IntoParams for Vec<Val> {
    fn into_params(self) -> Vec<Val> {
        self
    }
}
impl<T: Into<Val>, const N: usize> IntoParams for [T; N] {
    fn into_params(self) -> Vec<Val> {
        self.into_iter().map(Into::into).collect()
    }
}

/// Build a `Vec<Val>` from heterogeneous arguments, e.g. `params![id, name]`.
macro_rules! params {
    () => { ::std::vec::Vec::<$crate::db::Val>::new() };
    ($($v:expr),+ $(,)?) => { ::std::vec![$($crate::db::Val::from($v)),+] };
}
pub(crate) use params;

// -------------------------------------------------------------------- rows

/// One row read back from the database, as a list of [`Val`]s.
pub struct Row {
    cols: Vec<Val>,
}

/// Column index. Mirrors `rusqlite` so `r.get::<_, i64>(3)` keeps working.
pub trait RowIdx {
    fn idx(self) -> usize;
}
impl RowIdx for usize {
    fn idx(self) -> usize {
        self
    }
}

impl Row {
    pub fn get<I: RowIdx, T: FromVal>(&self, i: I) -> Result<T> {
        let i = i.idx();
        let v = self.cols.get(i).ok_or_else(|| anyhow!("column {i} out of range"))?;
        T::from_val(v)
    }
}

/// Convert a stored [`Val`] into a concrete Rust type.
pub trait FromVal: Sized {
    fn from_val(v: &Val) -> Result<Self>;
}
impl FromVal for i64 {
    fn from_val(v: &Val) -> Result<Self> {
        match v {
            Val::Int(x) => Ok(*x),
            Val::Real(x) => Ok(*x as i64),
            _ => bail!("expected integer, found {v:?}"),
        }
    }
}
impl FromVal for i32 {
    fn from_val(v: &Val) -> Result<Self> {
        Ok(i64::from_val(v)? as i32)
    }
}
impl FromVal for u32 {
    fn from_val(v: &Val) -> Result<Self> {
        Ok(i64::from_val(v)? as u32)
    }
}
impl FromVal for bool {
    fn from_val(v: &Val) -> Result<Self> {
        Ok(i64::from_val(v)? != 0)
    }
}
impl FromVal for f64 {
    fn from_val(v: &Val) -> Result<Self> {
        match v {
            Val::Real(x) => Ok(*x),
            Val::Int(x) => Ok(*x as f64),
            _ => bail!("expected real, found {v:?}"),
        }
    }
}
impl FromVal for String {
    fn from_val(v: &Val) -> Result<Self> {
        match v {
            Val::Text(s) => Ok(s.clone()),
            _ => bail!("expected text, found {v:?}"),
        }
    }
}
impl FromVal for Vec<u8> {
    fn from_val(v: &Val) -> Result<Self> {
        match v {
            Val::Blob(b) => Ok(b.clone()),
            Val::Text(s) => Ok(s.clone().into_bytes()),
            _ => bail!("expected blob, found {v:?}"),
        }
    }
}
impl<T: FromVal> FromVal for Option<T> {
    fn from_val(v: &Val) -> Result<Self> {
        match v {
            Val::Null => Ok(None),
            _ => Ok(Some(T::from_val(v)?)),
        }
    }
}

// ------------------------------------------------------- the connection

/// A database connection: either embedded SQLite or a PostgreSQL worker.
pub enum Db {
    Sqlite(rusqlite::Connection),
    Postgres(PgHandle),
}

impl Db {
    /// Open the embedded SQLite database at `path`, creating it if needed.
    pub fn open_sqlite(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = rusqlite::Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")?;
        Ok(Db::Sqlite(conn))
    }

    /// Open an in-memory SQLite database (tests).
    pub fn open_sqlite_memory() -> Result<Db> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        Ok(Db::Sqlite(conn))
    }

    /// Connect to a PostgreSQL server. The connection runs on a dedicated
    /// worker thread; `url` is a libpq-style connection string, e.g.
    /// `host=/var/run/postgresql user=ondura dbname=ondura`.
    pub fn connect_postgres(url: &str) -> Result<Db> {
        Ok(Db::Postgres(pg::spawn(url)?))
    }

    pub fn backend(&self) -> Backend {
        match self {
            Db::Sqlite(_) => Backend::Sqlite,
            Db::Postgres(_) => Backend::Postgres,
        }
    }

    /// Run several statements separated by `;` (schema setup). The SQL is in
    /// SQLite dialect and is translated for PostgreSQL.
    pub fn execute_batch(&mut self, sql: &str) -> Result<()> {
        match self {
            Db::Sqlite(c) => Ok(c.execute_batch(sql)?),
            Db::Postgres(h) => h.batch(&pg::schema_to_pg(sql)),
        }
    }

    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<u64> {
        let p = params.into_params();
        match self {
            Db::Sqlite(c) => Ok(c.execute(sql, params_from_iter(p.iter().map(sqlite_value)))? as u64),
            Db::Postgres(h) => h.execute(&pg::sql_to_pg(sql), p),
        }
    }

    pub fn query_all<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<Vec<T>> {
        let p = params.into_params();
        match self {
            Db::Sqlite(c) => sqlite_query(c, sql, p, f),
            Db::Postgres(h) => h.query(&pg::sql_to_pg(sql), p, f),
        }
    }

    pub fn query_opt<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<Option<T>> {
        Ok(self.query_all(sql, params, f)?.into_iter().next())
    }

    pub fn query_one<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<T> {
        self.query_opt(sql, params, f)?.ok_or_else(|| anyhow!("query returned no rows"))
    }

    /// Fetch exactly one row, like `rusqlite`'s `query_row`. Returns the
    /// [`NoRows`] error when the query matched nothing, so `.optional()` can
    /// turn that into `Ok(None)`.
    pub fn query_row<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<T> {
        self.query_opt(sql, params, f)?.ok_or_else(|| NoRows.into())
    }

    /// Compatibility shim for the `let st = db.prepare(sql)?; st.query_map(..)`
    /// idiom: the returned [`Stmt`] runs the query eagerly into a `Vec`.
    pub fn prepare(&self, sql: &str) -> Result<Stmt<'_>> {
        Ok(Stmt { db: self, sql: sql.to_string() })
    }

    pub fn transaction(&mut self) -> Result<Tx<'_>> {
        match self {
            Db::Sqlite(c) => Ok(Tx::Sqlite(Some(c.transaction()?))),
            Db::Postgres(h) => {
                h.batch("BEGIN")?;
                Ok(Tx::Postgres { sender: h.sender.clone(), done: false, _marker: PhantomData })
            }
        }
    }
}

/// A "prepared" statement. For the abstraction this just remembers the SQL;
/// [`Stmt::query_map`] runs it and collects every row into a `Vec`, matching
/// how the storage code used `rusqlite`'s `query_map(...).collect()`.
pub struct Stmt<'a> {
    db: &'a Db,
    sql: String,
}
impl Stmt<'_> {
    pub fn query_map<T>(&self, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<Vec<T>> {
        self.db.query_all(&self.sql, params, f)
    }
}

/// An open transaction. Mirrors the [`Db`] query API; call [`Tx::commit`] to
/// keep the changes (dropping without commit rolls back).
pub enum Tx<'a> {
    Sqlite(Option<rusqlite::Transaction<'a>>),
    Postgres { sender: Sender<pg::Cmd>, done: bool, _marker: PhantomData<&'a mut ()> },
}

impl Tx<'_> {
    pub fn execute(&self, sql: &str, params: impl IntoParams) -> Result<u64> {
        let p = params.into_params();
        match self {
            Tx::Sqlite(t) => {
                let t = t.as_ref().ok_or_else(|| anyhow!("transaction already finished"))?;
                Ok(t.execute(sql, params_from_iter(p.iter().map(sqlite_value)))? as u64)
            }
            Tx::Postgres { sender, .. } => pg::exec(sender, &pg::sql_to_pg(sql), p),
        }
    }

    pub fn query_all<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<Vec<T>> {
        let p = params.into_params();
        match self {
            Tx::Sqlite(t) => {
                let t = t.as_ref().ok_or_else(|| anyhow!("transaction already finished"))?;
                sqlite_query(t, sql, p, f)
            }
            Tx::Postgres { sender, .. } => pg::query(sender, &pg::sql_to_pg(sql), p, f),
        }
    }

    pub fn query_opt<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<Option<T>> {
        Ok(self.query_all(sql, params, f)?.into_iter().next())
    }

    pub fn query_one<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<T> {
        self.query_opt(sql, params, f)?.ok_or_else(|| anyhow!("query returned no rows"))
    }

    pub fn query_row<T>(&self, sql: &str, params: impl IntoParams, f: impl FnMut(&Row) -> Result<T>) -> Result<T> {
        self.query_opt(sql, params, f)?.ok_or_else(|| NoRows.into())
    }

    pub fn commit(mut self) -> Result<()> {
        match &mut self {
            Tx::Sqlite(t) => {
                if let Some(t) = t.take() {
                    t.commit()?;
                }
                Ok(())
            }
            Tx::Postgres { sender, done, .. } => {
                pg::batch(sender, "COMMIT")?;
                *done = true;
                Ok(())
            }
        }
    }
}

impl Drop for Tx<'_> {
    fn drop(&mut self) {
        // SQLite rolls back automatically when the Transaction is dropped.
        // PostgreSQL needs an explicit ROLLBACK if the caller never committed.
        if let Tx::Postgres { sender, done, .. } = self
            && !*done {
                let _ = pg::batch(sender, "ROLLBACK");
            }
    }
}

// ------------------------------------------------------------ sqlite glue

fn sqlite_value(v: &Val) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        Val::Null => Value::Null,
        Val::Int(i) => Value::Integer(*i),
        Val::Real(r) => Value::Real(*r),
        Val::Text(s) => Value::Text(s.clone()),
        Val::Blob(b) => Value::Blob(b.clone()),
    }
}

fn sqlite_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    use rusqlite::types::ValueRef;
    let n = r.as_ref().column_count();
    let mut cols = Vec::with_capacity(n);
    for i in 0..n {
        let v = match r.get_ref(i)? {
            ValueRef::Null => Val::Null,
            ValueRef::Integer(x) => Val::Int(x),
            ValueRef::Real(x) => Val::Real(x),
            ValueRef::Text(t) => Val::Text(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => Val::Blob(b.to_vec()),
        };
        cols.push(v);
    }
    Ok(Row { cols })
}

fn sqlite_query<T>(conn: &rusqlite::Connection, sql: &str, params: Vec<Val>, mut f: impl FnMut(&Row) -> Result<T>) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params_from_iter(params.iter().map(sqlite_value)))?;
    let mut out = Vec::new();
    while let Some(r) = rows.next()? {
        let row = sqlite_row(r)?;
        out.push(f(&row)?);
    }
    Ok(out)
}

// --------------------------------------------------------- postgres glue

/// Handle to the PostgreSQL worker thread.
pub struct PgHandle {
    sender: Sender<pg::Cmd>,
}
impl PgHandle {
    fn execute(&self, sql: &str, params: Vec<Val>) -> Result<u64> {
        pg::exec(&self.sender, sql, params)
    }
    fn query<T>(&self, sql: &str, params: Vec<Val>, f: impl FnMut(&Row) -> Result<T>) -> Result<Vec<T>> {
        pg::query(&self.sender, sql, params, f)
    }
    fn batch(&self, sql: &str) -> Result<()> {
        pg::batch(&self.sender, sql)
    }
}

mod pg {
    use super::{Row, Val};
    use anyhow::{Result, anyhow, bail};
    use postgres::types::{IsNull, ToSql, Type, to_sql_checked};
    use std::sync::mpsc::{Sender, channel};

    /// A request to the worker thread. Each carries a reply channel.
    pub enum Cmd {
        Exec { sql: String, params: Vec<Val>, resp: Sender<Result<u64>> },
        Query { sql: String, params: Vec<Val>, resp: Sender<Result<Vec<Row>>> },
        Batch { sql: String, resp: Sender<Result<()>> },
    }

    /// Start the worker thread and connect. Returns once the connection is
    /// established (or fails).
    pub fn spawn(url: &str) -> Result<super::PgHandle> {
        let (tx, rx) = channel::<Cmd>();
        let (ready_tx, ready_rx) = channel::<Result<()>>();
        let url = url.to_string();
        std::thread::Builder::new().name("ondura-postgres".into()).spawn(move || {
            let mut client = match postgres::Client::connect(&url, postgres::NoTls) {
                Ok(c) => {
                    let _ = ready_tx.send(Ok(()));
                    c
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(anyhow!("connect PostgreSQL: {e}")));
                    return;
                }
            };
            for cmd in rx {
                match cmd {
                    Cmd::Exec { sql, params, resp } => {
                        let _ = resp.send(run_exec(&mut client, &sql, &params));
                    }
                    Cmd::Query { sql, params, resp } => {
                        let _ = resp.send(run_query(&mut client, &sql, &params));
                    }
                    Cmd::Batch { sql, resp } => {
                        let _ = resp.send(client.batch_execute(&sql).map_err(|e| anyhow!("{e}")));
                    }
                }
            }
        })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(super::PgHandle { sender: tx }),
            Ok(Err(e)) => Err(e),
            Err(_) => bail!("PostgreSQL worker did not start"),
        }
    }

    /// Format a `postgres` error with the server's own message, which lives in
    /// the error's source rather than its terse `Display` ("db error").
    fn pg_err(e: postgres::Error, sql: &str) -> anyhow::Error {
        use std::error::Error;
        let detail = e.source().map(|s| s.to_string()).unwrap_or_else(|| e.to_string());
        anyhow!("{detail} (while running: {sql})")
    }

    fn run_exec(client: &mut postgres::Client, sql: &str, params: &[Val]) -> Result<u64> {
        let refs = pg_refs(params);
        client.execute(sql, &refs).map_err(|e| pg_err(e, sql))
    }

    fn run_query(client: &mut postgres::Client, sql: &str, params: &[Val]) -> Result<Vec<Row>> {
        let refs = pg_refs(params);
        let rows = client.query(sql, &refs).map_err(|e| pg_err(e, sql))?;
        rows.iter().map(pg_row).collect()
    }

    fn pg_refs(params: &[Val]) -> Vec<&(dyn ToSql + Sync)> {
        params.iter().map(|v| v as &(dyn ToSql + Sync)).collect()
    }

    fn pg_row(row: &postgres::Row) -> Result<Row> {
        let mut cols = Vec::with_capacity(row.len());
        for i in 0..row.len() {
            let ty = row.columns()[i].type_().clone();
            let v = if ty == Type::INT8 {
                row.try_get::<_, Option<i64>>(i)?.map(Val::Int).unwrap_or(Val::Null)
            } else if ty == Type::INT4 {
                row.try_get::<_, Option<i32>>(i)?.map(|x| Val::Int(x as i64)).unwrap_or(Val::Null)
            } else if ty == Type::INT2 {
                row.try_get::<_, Option<i16>>(i)?.map(|x| Val::Int(x as i64)).unwrap_or(Val::Null)
            } else if ty == Type::BOOL {
                row.try_get::<_, Option<bool>>(i)?.map(|b| Val::Int(b as i64)).unwrap_or(Val::Null)
            } else if ty == Type::FLOAT8 {
                row.try_get::<_, Option<f64>>(i)?.map(Val::Real).unwrap_or(Val::Null)
            } else if ty == Type::FLOAT4 {
                row.try_get::<_, Option<f32>>(i)?.map(|x| Val::Real(x as f64)).unwrap_or(Val::Null)
            } else if ty == Type::BYTEA {
                row.try_get::<_, Option<Vec<u8>>>(i)?.map(Val::Blob).unwrap_or(Val::Null)
            } else {
                row.try_get::<_, Option<String>>(i)?.map(Val::Text).unwrap_or(Val::Null)
            };
            cols.push(v);
        }
        Ok(Row { cols })
    }

    // --- synchronous request helpers (called while the Store mutex is held) --

    pub fn exec(sender: &Sender<Cmd>, sql: &str, params: Vec<Val>) -> Result<u64> {
        let (resp, rx) = channel();
        sender.send(Cmd::Exec { sql: sql.to_string(), params, resp }).map_err(|_| anyhow!("PostgreSQL worker gone"))?;
        rx.recv().map_err(|_| anyhow!("PostgreSQL worker gone"))?
    }

    pub fn query<T>(sender: &Sender<Cmd>, sql: &str, params: Vec<Val>, f: impl FnMut(&Row) -> Result<T>) -> Result<Vec<T>> {
        let (resp, rx) = channel();
        sender.send(Cmd::Query { sql: sql.to_string(), params, resp }).map_err(|_| anyhow!("PostgreSQL worker gone"))?;
        let rows = rx.recv().map_err(|_| anyhow!("PostgreSQL worker gone"))??;
        rows.iter().map(f).collect()
    }

    pub fn batch(sender: &Sender<Cmd>, sql: &str) -> Result<()> {
        let (resp, rx) = channel();
        sender.send(Cmd::Batch { sql: sql.to_string(), resp }).map_err(|_| anyhow!("PostgreSQL worker gone"))?;
        rx.recv().map_err(|_| anyhow!("PostgreSQL worker gone"))?
    }

    // ------------------------------------------------------- parameter codec

    impl ToSql for Val {
        fn to_sql(&self, ty: &Type, out: &mut postgres::types::private::BytesMut) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
            match self {
                Val::Null => Ok(IsNull::Yes),
                Val::Int(i) => {
                    if *ty == Type::INT4 {
                        (*i as i32).to_sql(ty, out)
                    } else if *ty == Type::INT2 {
                        (*i as i16).to_sql(ty, out)
                    } else if *ty == Type::BOOL {
                        (*i != 0).to_sql(ty, out)
                    } else if *ty == Type::FLOAT8 {
                        (*i as f64).to_sql(ty, out)
                    } else if *ty == Type::FLOAT4 {
                        (*i as f32).to_sql(ty, out)
                    } else {
                        i.to_sql(&Type::INT8, out)
                    }
                }
                Val::Real(f) => {
                    if *ty == Type::FLOAT4 {
                        (*f as f32).to_sql(ty, out)
                    } else {
                        f.to_sql(&Type::FLOAT8, out)
                    }
                }
                Val::Text(s) => s.to_sql(ty, out),
                Val::Blob(b) => b.to_sql(&Type::BYTEA, out),
            }
        }

        fn accepts(_ty: &Type) -> bool {
            true
        }

        to_sql_checked!();
    }

    // ------------------------------------------------------- SQL translation

    /// Translate a statement from SQLite dialect to PostgreSQL: positional
    /// placeholders `?1` become `$1`, and `LIKE` becomes `ILIKE` to keep
    /// SQLite's case-insensitive ASCII matching.
    pub fn sql_to_pg(sql: &str) -> String {
        let placed = convert_placeholders(sql);
        let ilike = like_to_ilike(&placed);
        // SQLite's implicit `rowid` is PostgreSQL's physical-row id `ctid`.
        word_replace(&ilike, "rowid", "ctid")
    }

    /// Replace a whole-word token outside string literals.
    fn word_replace(sql: &str, from: &str, to: &str) -> String {
        let chars: Vec<char> = sql.chars().collect();
        let mut out = String::with_capacity(sql.len());
        let mut in_str = false;
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\'' {
                in_str = !in_str;
                out.push(c);
                i += 1;
                continue;
            }
            if !in_str && matches_keyword(&chars, i, from) && boundary_before(&chars, i) && boundary_after(&chars, i + from.chars().count()) {
                out.push_str(to);
                i += from.chars().count();
                continue;
            }
            out.push(c);
            i += 1;
        }
        out
    }

    fn convert_placeholders(sql: &str) -> String {
        let bytes = sql.as_bytes();
        let mut out = String::with_capacity(sql.len() + 4);
        let mut i = 0;
        let mut in_str = false;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if c == '\'' {
                in_str = !in_str;
                out.push(c);
                i += 1;
            } else if c == '?' && !in_str && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit() {
                out.push('$');
                i += 1;
            } else {
                out.push(c);
                i += 1;
            }
        }
        out
    }

    fn like_to_ilike(sql: &str) -> String {
        // Replace the whole-word LIKE (outside string literals) with ILIKE.
        let mut out = String::with_capacity(sql.len() + 8);
        let mut in_str = false;
        let chars: Vec<char> = sql.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\'' {
                in_str = !in_str;
                out.push(c);
                i += 1;
                continue;
            }
            if !in_str
                && (c == 'L' || c == 'l')
                && matches_keyword(&chars, i, "LIKE")
                && boundary_before(&chars, i)
                && boundary_after(&chars, i + 4)
            {
                out.push_str("ILIKE");
                i += 4;
                continue;
            }
            out.push(c);
            i += 1;
        }
        out
    }

    fn matches_keyword(chars: &[char], at: usize, kw: &str) -> bool {
        let kw: Vec<char> = kw.chars().collect();
        if at + kw.len() > chars.len() {
            return false;
        }
        for (j, k) in kw.iter().enumerate() {
            if !chars[at + j].eq_ignore_ascii_case(k) {
                return false;
            }
        }
        true
    }

    fn boundary_before(chars: &[char], at: usize) -> bool {
        at == 0 || !chars[at - 1].is_alphanumeric() && chars[at - 1] != '_'
    }
    fn boundary_after(chars: &[char], at: usize) -> bool {
        at >= chars.len() || !chars[at].is_alphanumeric() && chars[at] != '_'
    }

    /// Translate the SQLite `CREATE TABLE` schema to PostgreSQL types.
    pub fn schema_to_pg(sql: &str) -> String {
        // Order matters: the identity column first, then remaining INTEGER.
        let s = sql.replace("INTEGER PRIMARY KEY", "BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY");
        let s = s.replace("INTEGER", "BIGINT");
        s.replace("BLOB", "BYTEA")
    }
}

#[cfg(test)]
mod tests {
    use super::pg::{schema_to_pg, sql_to_pg};

    #[test]
    fn placeholders_become_dollar() {
        assert_eq!(sql_to_pg("SELECT * FROM t WHERE a = ?1 AND b = ?2"), "SELECT * FROM t WHERE a = $1 AND b = $2");
    }

    #[test]
    fn like_becomes_ilike_outside_strings() {
        assert_eq!(sql_to_pg("SELECT 1 WHERE email LIKE ?1"), "SELECT 1 WHERE email ILIKE $1");
        // A column called "unlike" must not be touched.
        assert_eq!(sql_to_pg("SELECT unliked FROM t"), "SELECT unliked FROM t");
    }

    #[test]
    fn question_marks_in_strings_are_left_alone() {
        assert_eq!(sql_to_pg("SELECT '?1 literal'"), "SELECT '?1 literal'");
    }

    #[test]
    fn schema_types_translate() {
        let pg = schema_to_pg("id INTEGER PRIMARY KEY, n INTEGER NOT NULL, mime BLOB");
        assert!(pg.contains("BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY"));
        assert!(pg.contains("n BIGINT NOT NULL"));
        assert!(pg.contains("mime BYTEA"));
    }
}
