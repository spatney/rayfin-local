//! SQLite storage for rayfin-local: one database file per project (`.rayfin-local/db.sqlite`).
//!
//! [`Db`] is the shared handle. Components keep their own tables (named `_rl_<component>_*`) and
//! evolve them with [`Db::migrate_component`]; the entity tables are derived from the app model.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};

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
    /// not run yet, in order, each in its own transaction. Steps are append-only: never edit or
    /// reorder a released step, add a new one.
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

fn migrate(conn: &mut Connection, component: &str, steps: &[&str]) -> Result<(), DbError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _rl_migrations (
            component TEXT PRIMARY KEY NOT NULL,
            version INTEGER NOT NULL
        )",
    )?;
    let current: i64 = conn
        .query_row(
            "SELECT version FROM _rl_migrations WHERE component = ?1",
            [component],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    if current > steps.len() as i64 {
        return Err(DbError::NewerSchema {
            component: component.to_owned(),
            found: current,
            known: steps.len(),
        });
    }
    for (index, step) in steps.iter().enumerate().skip(current as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(step)?;
        tx.execute(
            "INSERT INTO _rl_migrations (component, version) VALUES (?1, ?2)
             ON CONFLICT (component) DO UPDATE SET version = excluded.version",
            params![component, index as i64 + 1],
        )?;
        tx.commit()?;
    }
    Ok(())
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
}
