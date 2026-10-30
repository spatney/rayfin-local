//! The `_rl_storage_*` tables: object metadata and upload/delete operations.

use std::{collections::HashSet, path::Path};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::BorrowedFormatItem, macros::format_description};

pub(crate) const COMPONENT: &str = "storage";

/// Append-only (`Db::migrate_component`).
pub(crate) const MIGRATIONS: &[&str] = &["
CREATE TABLE _rl_storage_objects (
    id TEXT PRIMARY KEY NOT NULL,
    folder TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    blob TEXT NOT NULL UNIQUE,
    size INTEGER NOT NULL,
    content_type TEXT NOT NULL,
    content_disposition TEXT,
    cache_control TEXT,
    etag TEXT NOT NULL,
    owner_id TEXT,
    user_metadata TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (folder, path)
);
CREATE TABLE _rl_storage_operations (
    id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    caller TEXT NOT NULL,
    status TEXT NOT NULL,
    folder TEXT NOT NULL,
    path TEXT NOT NULL,
    spec TEXT NOT NULL DEFAULT '{}',
    staged_blob TEXT,
    staged_size INTEGER,
    staged_etag TEXT,
    error_code TEXT,
    error_message TEXT,
    expires_at INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE INDEX _rl_storage_operations_expiry ON _rl_storage_operations (expires_at);
"];

/// Operation statuses; the client knows only the terminal `Succeeded` and `Failed`.
pub(crate) const AWAITING_UPLOAD: &str = "AwaitingUpload";
pub(crate) const AWAITING_COMMIT: &str = "AwaitingCommit";
pub(crate) const SUCCEEDED: &str = "Succeeded";
pub(crate) const FAILED: &str = "Failed";

pub(crate) const UPLOAD: &str = "upload";
pub(crate) const DELETE: &str = "delete";

const ISO_MS: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

pub(crate) fn now_unix() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

/// An ISO-8601 UTC timestamp with milliseconds, like JavaScript's `Date.toISOString()`.
pub(crate) fn iso(at: OffsetDateTime) -> String {
    at.format(ISO_MS).unwrap_or_default()
}

pub(crate) fn iso_unix(seconds: i64) -> String {
    OffsetDateTime::from_unix_timestamp(seconds)
        .map(iso)
        .unwrap_or_default()
}

/// A stored object (one version: `blob` names its file).
#[derive(Clone, Debug)]
pub(crate) struct Object {
    pub id: String,
    pub folder: String,
    pub path: String,
    pub name: String,
    pub blob: String,
    pub size: u64,
    pub content_type: String,
    pub content_disposition: Option<String>,
    pub cache_control: Option<String>,
    pub etag: String,
    pub owner_id: Option<String>,
    pub fields: Map<String, Value>,
    pub created_at: String,
    pub updated_at: String,
}

const OBJECT_COLUMNS: &str = "id, folder, path, name, blob, size, content_type, \
     content_disposition, cache_control, etag, owner_id, user_metadata, created_at, updated_at";

impl Object {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let metadata: String = row.get(11)?;
        Ok(Self {
            id: row.get(0)?,
            folder: row.get(1)?,
            path: row.get(2)?,
            name: row.get(3)?,
            blob: row.get(4)?,
            size: u64::try_from(row.get::<_, i64>(5)?).unwrap_or_default(),
            content_type: row.get(6)?,
            content_disposition: row.get(7)?,
            cache_control: row.get(8)?,
            etag: row.get(9)?,
            owner_id: row.get(10)?,
            fields: serde_json::from_str(&metadata).unwrap_or_default(),
            created_at: row.get(12)?,
            updated_at: row.get(13)?,
        })
    }

    /// What rule checks see as `item`: the app fields and the intrinsic fields (which win).
    pub(crate) fn item(&self) -> Map<String, Value> {
        let mut item = self.fields.clone();
        let intrinsic = json!({
            "id": self.id,
            "owner_id": self.owner_id,
            "folder": self.folder,
            "path": self.path,
            "name": self.name,
            "size": self.size,
            "content_type": self.content_type,
            "content_disposition": self.content_disposition,
            "cache_control": self.cache_control,
            "etag": self.etag,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "last_accessed_at": null,
        });
        if let Value::Object(intrinsic) = intrinsic {
            item.extend(intrinsic);
        }
        item
    }

    /// A list item: the client's `StorageObjectRef` keys plus `fields`.
    pub(crate) fn to_wire(&self) -> Value {
        let mut wire = json!({
            "id": self.id,
            "folder": self.folder,
            "path": self.path,
            "name": self.name,
            "size": self.size,
            "contentType": self.content_type,
            "etag": self.etag,
            "createdAt": self.created_at,
            "updatedAt": self.updated_at,
            "lastAccessedAt": null,
            "fields": self.fields,
        });
        if let Some(value) = &self.content_disposition {
            wire["contentDisposition"] = value.as_str().into();
        }
        if let Some(value) = &self.cache_control {
            wire["cacheControl"] = value.as_str().into();
        }
        wire
    }
}

pub(crate) fn object_at(
    conn: &Connection,
    folder: &str,
    path: &str,
) -> rusqlite::Result<Option<Object>> {
    conn.query_row(
        &format!(
            "SELECT {OBJECT_COLUMNS} FROM _rl_storage_objects WHERE folder = ?1 AND path = ?2"
        ),
        params![folder, path],
        Object::from_row,
    )
    .optional()
}

pub(crate) fn object_by_blob(conn: &Connection, blob: &str) -> rusqlite::Result<Option<Object>> {
    conn.query_row(
        &format!("SELECT {OBJECT_COLUMNS} FROM _rl_storage_objects WHERE blob = ?1"),
        [blob],
        Object::from_row,
    )
    .optional()
}

/// Inserts `object`, or replaces the row with its id.
pub(crate) fn save_object(conn: &Connection, object: &Object) -> rusqlite::Result<()> {
    conn.execute(
        &format!(
            "INSERT OR REPLACE INTO _rl_storage_objects ({OBJECT_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
        ),
        params![
            object.id,
            object.folder,
            object.path,
            object.name,
            object.blob,
            i64::try_from(object.size).unwrap_or(i64::MAX),
            object.content_type,
            object.content_disposition,
            object.cache_control,
            object.etag,
            object.owner_id,
            Value::Object(object.fields.clone()).to_string(),
            object.created_at,
            object.updated_at,
        ],
    )?;
    Ok(())
}

/// Deletes the object `id`; the blob of the version it deleted (`None` when it was gone).
pub(crate) fn delete_object(conn: &Connection, id: &str) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "DELETE FROM _rl_storage_objects WHERE id = ?1 RETURNING blob",
        [id],
        |row| row.get(0),
    )
    .optional()
}

/// Deletes every object and operation; returns how many objects there were (none before the
/// tables exist).
pub(crate) fn clear(conn: &mut Connection) -> rusqlite::Result<u64> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = \
         '_rl_storage_objects')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let deleted = tx.execute("DELETE FROM _rl_storage_objects", [])?;
    tx.execute("DELETE FROM _rl_storage_operations", [])?;
    tx.commit()?;
    Ok(u64::try_from(deleted).unwrap_or_default())
}

/// The bytes of every stored object.
pub(crate) fn used_bytes(conn: &Connection) -> rusqlite::Result<u64> {
    let used: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM _rl_storage_objects",
        [],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(used).unwrap_or_default())
}

/// One page of `folder` in path (byte) order: the objects after `after`, under `prefix/` when
/// given, that pass `keep`; and whether more such objects follow.
pub(crate) fn list_page(
    conn: &Connection,
    folder: &str,
    prefix: Option<&str>,
    after: Option<&str>,
    limit: usize,
    mut keep: impl FnMut(&Object) -> bool,
) -> rusqlite::Result<(Vec<Object>, bool)> {
    // Paths under `p/` sort from `p/` (inclusive) to `p0` (exclusive): `0` follows `/`.
    let range = prefix.map(|prefix| (format!("{prefix}/"), format!("{prefix}0")));
    let (low, high) = range.unzip();
    let mut statement = conn.prepare(&format!(
        "SELECT {OBJECT_COLUMNS} FROM _rl_storage_objects
         WHERE folder = ?1 AND path > ?2 AND (?3 IS NULL OR (path >= ?3 AND path < ?4))
         ORDER BY path"
    ))?;
    let mut rows = statement.query(params![folder, after.unwrap_or_default(), low, high])?;
    let mut page = Vec::new();
    while let Some(row) = rows.next()? {
        let object = Object::from_row(row)?;
        if keep(&object) {
            if page.len() == limit {
                return Ok((page, true));
            }
            page.push(object);
        }
    }
    Ok((page, false))
}

/// What an upload operation was asked for at `/upload`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct UploadSpec {
    pub content_type: String,
    pub content_disposition: Option<String>,
    pub cache_control: Option<String>,
    pub max_bytes: Option<u64>,
    pub fields: Map<String, Value>,
    /// When the signed upload URL expires (Unix seconds).
    pub url_expires: i64,
}

/// An upload or delete operation, polled by the client.
#[derive(Clone, Debug)]
pub(crate) struct Operation {
    pub id: String,
    pub kind: &'static str,
    /// The `sub` of the caller that started it; empty for anonymous callers.
    pub caller: String,
    pub status: String,
    pub folder: String,
    pub path: String,
    pub spec: UploadSpec,
    pub staged_blob: Option<String>,
    pub staged_size: Option<u64>,
    pub staged_etag: Option<String>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub expires_at: i64,
}

const OPERATION_COLUMNS: &str = "id, kind, caller, status, folder, path, spec, staged_blob, \
     staged_size, staged_etag, error_code, error_message, expires_at";

impl Operation {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let kind: String = row.get(1)?;
        let spec: String = row.get(6)?;
        Ok(Self {
            id: row.get(0)?,
            kind: if kind == DELETE { DELETE } else { UPLOAD },
            caller: row.get(2)?,
            status: row.get(3)?,
            folder: row.get(4)?,
            path: row.get(5)?,
            spec: serde_json::from_str(&spec).unwrap_or_default(),
            staged_blob: row.get(7)?,
            staged_size: row
                .get::<_, Option<i64>>(8)?
                .map(|size| u64::try_from(size).unwrap_or_default()),
            staged_etag: row.get(9)?,
            error_code: row.get(10)?,
            error_message: row.get(11)?,
            expires_at: row.get(12)?,
        })
    }
}

pub(crate) fn insert_operation(conn: &Connection, operation: &Operation) -> rusqlite::Result<()> {
    let now = iso(OffsetDateTime::now_utc());
    conn.execute(
        &format!(
            "INSERT INTO _rl_storage_operations ({OPERATION_COLUMNS}, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)"
        ),
        params![
            operation.id,
            operation.kind,
            operation.caller,
            operation.status,
            operation.folder,
            operation.path,
            serde_json::to_string(&operation.spec).unwrap_or_default(),
            operation.staged_blob,
            operation
                .staged_size
                .map(|size| i64::try_from(size).unwrap_or(i64::MAX)),
            operation.staged_etag,
            operation.error_code,
            operation.error_message,
            operation.expires_at,
            now,
        ],
    )?;
    Ok(())
}

/// The operation `id`, whatever its caller, unless it has expired.
pub(crate) fn operation(
    conn: &Connection,
    id: &str,
    now: i64,
) -> rusqlite::Result<Option<Operation>> {
    conn.query_row(
        &format!(
            "SELECT {OPERATION_COLUMNS} FROM _rl_storage_operations
             WHERE id = ?1 AND expires_at >= ?2"
        ),
        params![id, now],
        Operation::from_row,
    )
    .optional()
}

/// Records the staged bytes of an upload still awaiting them; `false` when it no longer is.
pub(crate) fn mark_uploaded(
    conn: &Connection,
    id: &str,
    blob: &str,
    size: u64,
    etag: &str,
    expires_at: i64,
) -> rusqlite::Result<bool> {
    let changed = conn.execute(
        "UPDATE _rl_storage_operations
         SET status = ?2, staged_blob = ?3, staged_size = ?4, staged_etag = ?5,
             expires_at = MAX(expires_at, ?6), updated_at = ?7
         WHERE id = ?1 AND status = ?8",
        params![
            id,
            AWAITING_COMMIT,
            blob,
            i64::try_from(size).unwrap_or(i64::MAX),
            etag,
            expires_at,
            iso(OffsetDateTime::now_utc()),
            AWAITING_UPLOAD,
        ],
    )?;
    Ok(changed == 1)
}

/// Ends an operation (`Succeeded`, or `Failed` with an error), kept for polling until
/// `expires_at`.
pub(crate) fn finish_operation(
    conn: &Connection,
    id: &str,
    error: Option<(&str, &str)>,
    expires_at: i64,
) -> rusqlite::Result<()> {
    let (status, code, message) = match error {
        None => (SUCCEEDED, None, None),
        Some((code, message)) => (FAILED, Some(code), Some(message)),
    };
    conn.execute(
        "UPDATE _rl_storage_operations
         SET status = ?2, error_code = ?3, error_message = ?4, staged_blob = NULL,
             expires_at = ?5, updated_at = ?6
         WHERE id = ?1",
        params![
            id,
            status,
            code,
            message,
            expires_at,
            iso(OffsetDateTime::now_utc())
        ],
    )?;
    Ok(())
}

/// Forgets expired operations and deletes their staged bytes. At startup it also deletes files
/// nothing refers to (partial uploads, or versions orphaned by a crash).
pub(crate) fn sweep(
    conn: &Connection,
    staging: &Path,
    objects: &Path,
    now: i64,
    startup: bool,
) -> rusqlite::Result<()> {
    let expired: Vec<String> = conn
        .prepare(
            "SELECT staged_blob FROM _rl_storage_operations
             WHERE expires_at < ?1 AND staged_blob IS NOT NULL",
        )?
        .query_map([now], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    conn.execute(
        "DELETE FROM _rl_storage_operations WHERE expires_at < ?1",
        [now],
    )?;
    for blob in expired {
        remove_file(&staging.join(blob));
    }
    if startup {
        let staged: HashSet<String> = conn
            .prepare(
                "SELECT staged_blob FROM _rl_storage_operations WHERE staged_blob IS NOT NULL",
            )?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        remove_unreferenced(staging, &staged);
        let stored: HashSet<String> = conn
            .prepare("SELECT blob FROM _rl_storage_objects")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        remove_unreferenced(objects, &stored);
    }
    Ok(())
}

fn remove_unreferenced(dir: &Path, keep: &HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !keep.contains(name.to_string_lossy().as_ref()) {
            remove_file(&entry.path());
        }
    }
}

/// Best effort: a file that cannot be removed now is removed by the next startup sweep.
pub(crate) fn remove_file(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!("could not remove {}: {error}", path.display());
    }
}
