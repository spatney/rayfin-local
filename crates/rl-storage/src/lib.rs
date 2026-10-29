//! Rayfin storage (`/api/storage/*`) on the local file system (`.rayfin-local/storage`), with the
//! app's `@blob` folder rules.
//!
//! The routes are the ones the 1.36.2 `StorageFolderClient` calls (`@microsoft/rayfin-storage`
//! `dist/StorageFolderClient.js`):
//!
//! | Request | Answer |
//! | --- | --- |
//! | `POST /api/storage/upload` `{folder, fileName, prefix?, contentType, contentDisposition?, cacheControl?, expiresInSeconds?, maxBytes?, fields?}` | 202 `{operationId}` |
//! | `GET /api/storage/operations/{id}/status` | 200 `{operationId, status, signedUrl?, expiresAt?, success?, errorCode?, errorMessage?}` |
//! | `PUT {signedUrl}` (the bytes) | 201, `ETag` |
//! | `POST /api/storage/uploads/{id}/commit` | 202 `{operationId}` |
//! | `DELETE /api/storage/uploads/{id}` | 204 |
//! | `POST /api/storage/download` `{folder, fileName, prefix?, expiresInSeconds?}` | 200 `{signedUrl, expiresAt}` |
//! | `GET {signedUrl}` (`Range: bytes=…` optional) | 200, the bytes; or 206, one byte range |
//! | `GET /api/storage/list?folder&prefix&continuation&limit` | 200 `{items, continuation?}` |
//! | `DELETE /api/storage/delete` `{folder, fileName, prefix?}` | 202 `{operationId}` |
//!
//! Every step finishes synchronously, so the client's first poll already sees the signed URL or
//! `Succeeded`. Signed URLs live under `/.rayfin-local/storage/` and need no credentials (the
//! client sends none): their HMAC token is the capability. Files are named by random ids under
//! `objects/` (uploads are staged under `staging/` and moved in on commit); paths and metadata live
//! in the `_rl_storage_*` tables.

mod error;
mod handlers;
mod names;
mod rules;
mod signing;
mod store;

use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use axum::Router;
use ring::rand::{SecureRandom, SystemRandom};
use rl_db::Db;
use rl_model::{AppModel, CallerResolver};

pub use error::StorageError;

/// Where storage keeps its files and how it signs its URLs.
#[derive(Clone)]
pub struct StorageConfig {
    /// The folder of the stored files, normally `<project>/.rayfin-local/storage`.
    pub root: PathBuf,
    /// The origin signed URLs point at (the `apiUrl`), such as `http://localhost:7171`.
    pub api_origin: String,
    /// The HMAC key of signed URLs and list continuations.
    pub signing_key: [u8; 32],
    /// The largest object; a folder's `maxSize` can only lower it.
    pub max_file_bytes: u64,
    /// The bytes of all objects together.
    pub max_total_bytes: u64,
    /// The lifetime of signed URLs when the request names none (`expiresInSeconds`).
    pub url_ttl: Duration,
}

impl StorageConfig {
    /// 25 MiB.
    pub const DEFAULT_MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
    /// 1 GiB.
    pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 1024 * 1024 * 1024;
    /// 15 minutes.
    pub const DEFAULT_URL_TTL: Duration = Duration::from_secs(900);

    /// The default limits, with a new random signing key.
    pub fn new(root: impl Into<PathBuf>, api_origin: impl Into<String>) -> Self {
        let mut signing_key = [0; 32];
        SystemRandom::new()
            .fill(&mut signing_key)
            .expect("the system random number generator works");
        Self {
            root: root.into(),
            api_origin: api_origin.into(),
            signing_key,
            max_file_bytes: Self::DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: Self::DEFAULT_MAX_TOTAL_BYTES,
            url_ttl: Self::DEFAULT_URL_TTL,
        }
    }
}

impl fmt::Debug for StorageConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageConfig")
            .field("root", &self.root)
            .field("api_origin", &self.api_origin)
            .field("max_file_bytes", &self.max_file_bytes)
            .field("max_total_bytes", &self.max_total_bytes)
            .field("url_ttl", &self.url_ttl)
            .finish_non_exhaustive()
    }
}

/// The storage routes (state applied): `/api/storage/*` and the signed URLs under
/// `/.rayfin-local/storage/`. Creates the tables and folders it needs, and deletes files left
/// over by interrupted uploads.
pub async fn router(
    config: StorageConfig,
    db: Db,
    model: Arc<AppModel>,
    callers: Arc<dyn CallerResolver>,
) -> Result<Router, StorageError> {
    db.migrate_component(store::COMPONENT, store::MIGRATIONS)
        .await?;
    let objects = config.root.join("objects");
    let staging = config.root.join("staging");
    for dir in [&objects, &staging] {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| StorageError::Folder {
                path: dir.clone(),
                source,
            })?;
    }
    let (staging_dir, objects_dir) = (staging.clone(), objects.clone());
    db.call(move |conn| store::sweep(conn, &staging_dir, &objects_dir, store::now_unix(), true))
        .await??;
    let state = handlers::AppState {
        signer: signing::Signer::new(&config.signing_key),
        config,
        db,
        model,
        callers,
        objects,
        staging,
    };
    Ok(handlers::routes(Arc::new(state)))
}
