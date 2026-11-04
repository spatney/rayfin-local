//! SQLite storage for rayfin-local: one database file per project (`.rayfin-local/db.sqlite`).
//!
//! [`Db`] is the shared handle. Components keep their own tables (named `_rl_<component>_*`) and
//! evolve them with [`Db::migrate_component`]; the entity tables are derived from the app model:
//! [`codec`] (value storage and conversions), [`schema`] (DDL) and [`migrate`] (migrations).

pub mod codec;
mod error;
pub mod migrate;
pub mod schema;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub use error::{DataLoss, LossReason, SchemaError};
pub use migrate::{MigrationPlan, MigrationReport, MigrationStep, StepClass, apply, plan};
pub use schema::{StoredSchema, create_table_sql, schema_sql};

/// A database failure.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("could not create the database folder {path}: {source}")]
    Folder {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "the database was written by a newer rayfin-local ({component} schema version {found}, this \
         version knows {known}); upgrade rayfin-local or run `rayfin-local reset`"
    )]
    NewerSchema {
        component: String,
        found: i64,
        known: usize,
    },
    #[error("the database task failed: {0}")]
    Task(String),
}

/// A handle to the project database; cheap to clone.
///
/// Work runs on a blocking thread with exclusive use of the one connection, so a closure can run
/// several statements in a single transaction.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

struct Inner {
    conn: Mutex<Connection>,
    path: Option<PathBuf>,
}

impl Db {
    /// Opens (creating if needed) the database file at `path`, in WAL mode with foreign keys on.
    pub fn open(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|source| DbError::Folder {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0))?;
        configure(&conn)?;
        Ok(Self::from_connection(conn, Some(path.to_path_buf())))
    }

    /// A private in-memory database, for tests.
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        configure(&conn)?;
        Ok(Self::from_connection(conn, None))
    }

    fn from_connection(conn: Connection, path: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(Inner {
                conn: Mutex::new(conn),
                path,
            }),
        }
    }

    /// The database file, `None` for in-memory databases.
    pub fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    /// Runs `f` with the connection on a blocking thread.
    pub async fn call<R, F>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&mut Connection) -> R + Send + 'static,
        R: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut conn = inner.conn.lock();
            f(&mut conn)
        })
        .await
        .map_err(|e| DbError::Task(e.to_string()))
    }

    /// Runs `f` with the connection on the current thread (startup, tests, non-async callers).
    pub fn call_blocking<R>(&self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let mut conn = self.inner.conn.lock();
        f(&mut conn)
    }

    /// Brings a component's private tables up to date: runs the SQL batches of `steps` that have
    /// not run yet, in order, each in its own transaction (once, even when several processes
    /// migrate the database at the same time). Steps are append-only: never edit or reorder a
    /// released step, add a new one.
    pub async fn migrate_component(
        &self,
        component: &'static str,
        steps: &'static [&'static str],
    ) -> Result<(), DbError> {
        self.call(move |conn| migrate(conn, component, steps))
            .await?
    }
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(())
}

/// How long [`backup_to`] waits for a busy or locked source.
const BACKUP_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The pause between [`backup_to`]'s attempts.
const BACKUP_RETRY: Duration = Duration::from_millis(50);

/// Copies the database file `src` into `dest`, a new self-contained file (no `-wal` or `-shm`),
/// with SQLite's online backup. Safe while other connections, in this process or another, write
/// `src`: the copy is one consistent snapshot that includes the transactions committed to the
/// write-ahead log but not checkpointed yet. `src` is never created; `dest` must not exist.
pub fn backup_to(src: &Path, dest: &Path) -> Result<(), DbError> {
    use rusqlite::OpenFlags;

    let source = Connection::open_with_flags(
        src,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    source.busy_timeout(BACKUP_BUSY_TIMEOUT)?;
    let mut copy = Connection::open_with_flags(
        dest,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // All pages in one step (-1): the read transaction holds one snapshot throughout, and a WAL
    // writer is never blocked by it. Busy or locked only while a checkpoint or another writer
    // holds the file; retried for as long as the busy timeout.
    {
        use rusqlite::backup::{Backup, StepResult};

        let backup = Backup::new(&source, &mut copy)?;
        let mut waited = Duration::ZERO;
        loop {
            match backup.step(-1)? {
                StepResult::Done => break,
                StepResult::More | StepResult::Busy | StepResult::Locked
                    if waited < BACKUP_BUSY_TIMEOUT =>
                {
                    std::thread::sleep(BACKUP_RETRY);
                    waited += BACKUP_RETRY;
                }
                _ => {
                    return Err(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                        Some("the database stayed busy for the whole backup".into()),
                    )
                    .into());
                }
            }
        }
    }
    // The copied header says WAL; a rollback journal keeps the file self-contained.
    copy.pragma_update_and_check(None, "journal_mode", "DELETE", |row| {
        row.get::<_, String>(0)
    })?;
    copy.close().map_err(|(_, err)| err)?;
    Ok(())
}

fn migrate(conn: &mut Connection, component: &str, steps: &[&str]) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _rl_migrations (
            component TEXT PRIMARY KEY NOT NULL,
            version INTEGER NOT NULL
        )",
    )?;
    if applied_steps(conn, component, steps)? == steps.len() {
        return Ok(());
    }
    loop {
        // IMMEDIATE takes the write lock before the version is read again, so two processes
        // migrating one database (`users add` while `serve` starts) never both run a step.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied = applied_steps(&tx, component, steps)?;
        let Some(step) = steps.get(applied) else {
            return Ok(());
        };
        tx.execute_batch(step)?;
        tx.execute(
            "INSERT INTO _rl_migrations (component, version) VALUES (?1, ?2)
             ON CONFLICT (component) DO UPDATE SET version = excluded.version",
            params![component, applied as i64 + 1],
        )?;
        tx.commit()?;
    }
}

/// How many of `steps` the database has run for `component`; refuses a newer database.
fn applied_steps(conn: &Connection, component: &str, steps: &[&str]) -> Result<usize, DbError> {
    let version: i64 = conn
        .query_row(
            "SELECT version FROM _rl_migrations WHERE component = ?1",
            [component],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if version > steps.len() as i64 {
        return Err(DbError::NewerSchema {
            component: component.to_owned(),
            found: version,
            known: steps.len(),
        });
    }
    Ok(version as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEPS: &[&str] = &[
        "CREATE TABLE _rl_test_a (id INTEGER PRIMARY KEY)",
        "ALTER TABLE _rl_test_a ADD COLUMN name TEXT",
    ];

    #[tokio::test]
    async fn migrations_run_once_and_in_order() {
        let db = Db::open_in_memory().unwrap();
        db.migrate_component("test", &STEPS[..1]).await.unwrap();
        db.migrate_component("test", STEPS).await.unwrap();
        db.migrate_component("test", STEPS).await.unwrap();
        let version: i64 = db.call_blocking(|c| {
            c.query_row(
                "SELECT version FROM _rl_migrations WHERE component = 'test'",
                [],
                |r| r.get(0),
            )
            .unwrap()
        });
        assert_eq!(version, 2);
        db.call(|c| c.execute("INSERT INTO _rl_test_a (id, name) VALUES (1, 'x')", []))
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_newer_database_is_refused() {
        let db = Db::open_in_memory().unwrap();
        db.migrate_component("test", STEPS).await.unwrap();
        let err = db.migrate_component("test", &STEPS[..1]).await.unwrap_err();
        assert!(matches!(err, DbError::NewerSchema { found: 2, .. }));
    }

    #[test]
    fn a_backup_includes_the_writes_still_in_the_write_ahead_log() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("db.sqlite");
        let db = Db::open(&src).unwrap();
        db.call_blocking(|c| {
            c.execute_batch(
                "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);
                 INSERT INTO t (name) VALUES ('a'), ('b'), ('c');",
            )
        })
        .unwrap();
        // The writer stays open, so nothing has been checkpointed into the main file.
        assert!(
            std::fs::metadata(dir.path().join("db.sqlite-wal"))
                .unwrap()
                .len()
                > 0
        );

        let dest = dir.path().join("copy.sqlite");
        backup_to(&src, &dest).unwrap();
        db.call_blocking(|c| c.execute("INSERT INTO t (name) VALUES ('later')", []))
            .unwrap();

        assert!(!dir.path().join("copy.sqlite-wal").exists());
        assert!(!dir.path().join("copy.sqlite-shm").exists());
        let copy = Connection::open(&dest).unwrap();
        let mode: String = copy
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "delete");
        let names: Vec<String> = copy
            .prepare("SELECT name FROM t ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(names, ["a", "b", "c"]);
    }

    #[test]
    fn a_backup_never_creates_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("missing.sqlite");
        assert!(backup_to(&src, &dir.path().join("copy.sqlite")).is_err());
        assert!(!src.exists());
    }

    #[test]
    fn file_databases_use_wal_and_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("nested").join("db.sqlite")).unwrap();
        let (mode, fk): (String, i64) = db.call_blocking(|c| {
            let mode = c
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .unwrap();
            let fk = c
                .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
                .unwrap();
            (mode, fk)
        });
        assert_eq!(mode.to_lowercase(), "wal");
        assert_eq!(fk, 1);
    }

    #[test]
    fn concurrent_migrations_run_each_step_once() {
        // Neither step can run twice: the table exists, or the count is off.
        const STEPS: &[&str] = &[
            "CREATE TABLE _rl_test_b (id INTEGER PRIMARY KEY)",
            "INSERT INTO _rl_test_b DEFAULT VALUES",
        ];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let dbs: Vec<Db> = (0..4).map(|_| Db::open(&path).unwrap()).collect();
        // With the bookkeeping table in place, all of them read the version at once.
        dbs[0].call_blocking(|c| migrate(c, "race", &[])).unwrap();
        let start = std::sync::Barrier::new(dbs.len());
        std::thread::scope(|scope| {
            for db in &dbs {
                let start = &start;
                scope.spawn(move || {
                    start.wait();
                    db.call_blocking(|c| migrate(c, "race", STEPS)).unwrap();
                });
            }
        });
        let rows: i64 = dbs[0].call_blocking(|c| {
            c.query_row("SELECT count(*) FROM _rl_test_b", [], |r| r.get(0))
                .unwrap()
        });
        assert_eq!(rows, 1);
    }
}
