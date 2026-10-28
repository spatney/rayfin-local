//! The routes the 1.36.2 `StorageFolderClient` calls, and the signed-URL routes it calls without
//! credentials.

use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, delete, get, post, put},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_util::StreamExt;
use ring::digest;
use rl_model::{
    Action, AppModel, Caller, CallerResolver, OnConflict, StorageFieldType, StorageFolder,
    bearer_token,
};
use rusqlite::Connection;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

use crate::{
    StorageConfig,
    error::{ApiError, ErrorBody},
    names, rules,
    signing::{Purpose, Signer, TokenError},
    store::{self, Object, Operation, UploadSpec},
};

/// Storage requests carry small JSON bodies; bytes go to the signed URLs.
const MAX_JSON_BYTES: usize = 32 * 1024;
/// How long a staged upload waits for its commit after the PUT.
const COMMIT_GRACE_SECS: i64 = 300;
/// How long a finished operation can still be polled.
const FINISHED_RETENTION_SECS: i64 = 600;
/// `expiresInSeconds` bounds (`InvalidSasExpiry` in `storageErrorCodes.js`: 60-3600 s).
const MIN_EXPIRY_SECS: f64 = 60.0;
const MAX_EXPIRY_SECS: f64 = 3600.0;
const DEFAULT_LIST_LIMIT: usize = 100;
const MAX_LIST_LIMIT: usize = 1000;
const CONTINUATION_TTL_SECS: i64 = 24 * 3600;
const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";
/// The poll `errorCode` of an aborted upload (`Unexpected` 400 in the client).
const ABORTED_CODE: &str = "ORCHESTRATION_USER_ERROR";

/// Signed upload (`PUT`) and download (`GET`) URLs.
pub(crate) const UPLOAD_URL_PATH: &str = "/.rayfin-local/storage/u";
pub(crate) const OBJECT_URL_PATH: &str = "/.rayfin-local/storage/o";

pub(crate) struct AppState {
    pub config: StorageConfig,
    pub signer: Signer,
    pub db: rl_db::Db,
    pub model: Arc<AppModel>,
    pub callers: Arc<dyn CallerResolver>,
    pub objects: PathBuf,
    pub staging: PathBuf,
}

type Shared = State<Arc<AppState>>;

pub(crate) fn routes(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/api/storage/upload", post(start_upload))
        .route("/api/storage/operations/{id}/status", get(operation_status))
        .route("/api/storage/uploads/{id}/commit", post(commit_upload))
        .route("/api/storage/uploads/{id}", delete(abort_upload))
        .route("/api/storage/download", post(start_download))
        .route("/api/storage/list", get(list))
        .route("/api/storage/delete", delete(delete_object))
        .route("/api/storage", any(no_route))
        .route("/api/storage/{*rest}", any(no_route))
        .layer(middleware::from_fn(correlate));
    let signed = Router::new()
        .route(&format!("{UPLOAD_URL_PATH}/{{token}}"), put(signed_put))
        .route(&format!("{OBJECT_URL_PATH}/{{token}}"), get(signed_get));
    api.merge(signed).with_state(state)
}

/// Echoes the request's `X-Correlation-ID` (or a new one) on every `/api/storage` answer and in
/// error bodies, where the client reads it.
async fn correlate(request: Request, next: Next) -> Response {
    let id = request
        .headers()
        .get("x-correlation-id")
        .filter(|value| !value.is_empty() && value.len() <= 256 && value.to_str().is_ok())
        .cloned()
        .unwrap_or_else(|| {
            HeaderValue::from_str(&Uuid::new_v4().to_string()).expect("a UUID is a header value")
        });
    let mut response = next.run(request).await;
    if let Some(error) = response.extensions_mut().remove::<ErrorBody>() {
        let envelope = error.envelope(id.to_str().ok());
        response = (response.status(), Json(envelope)).into_response();
    }
    let headers = response.headers_mut();
    headers.insert("x-correlation-id", id);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn no_route(method: Method, uri: Uri) -> ApiError {
    ApiError::NotFound(format!(
        "rayfin-local storage has no route for {method} {}.",
        uri.path()
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadRequest {
    folder: Option<String>,
    file_name: Option<String>,
    prefix: Option<String>,
    content_type: Option<String>,
    content_disposition: Option<String>,
    cache_control: Option<String>,
    expires_in_seconds: Option<Value>,
    max_bytes: Option<Value>,
    fields: Option<Value>,
}

/// The body of `/download` and `/delete`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObjectRequest {
    folder: Option<String>,
    file_name: Option<String>,
    prefix: Option<String>,
    expires_in_seconds: Option<Value>,
}

/// `POST /api/storage/upload` → 202 `{operationId}`; the first poll has the signed URL.
async fn start_upload(
    State(state): Shared,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let caller = resolve_caller(&state, &headers)?;
    let request: UploadRequest = json_body(body).await?;
    let folder = find_folder(&state.model, request.folder.as_deref())?.clone();
    let path = names::logical_path(request.file_name.as_deref(), request.prefix.as_deref())?;
    let ttl = expiry_secs(request.expires_in_seconds.as_ref(), state.config.url_ttl)?;
    let max_bytes = match request.max_bytes {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            ApiError::BadRequest(format!(
                "`maxBytes` must be a whole number of bytes; got {value}."
            ))
        })?),
    };
    let fields = check_fields(&folder, request.fields)?;
    let content_type = request
        .content_type
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_CONTENT_TYPE.into());
    for (name, value) in [
        ("contentType", Some(&content_type)),
        ("contentDisposition", request.content_disposition.as_ref()),
        ("cacheControl", request.cache_control.as_ref()),
    ] {
        if value.is_some_and(|value| HeaderValue::from_str(value).is_err()) {
            return Err(ApiError::BadRequest(format!(
                "`{name}` is not a valid HTTP header value."
            )));
        }
    }
    if !rules::may(&folder, &caller, Action::Create)
        && !rules::may(&folder, &caller, Action::Update)
    {
        return Err(denied(&folder, &caller, Action::Create));
    }

    let now = store::now_unix();
    let operation = Operation {
        id: Uuid::new_v4().simple().to_string(),
        kind: store::UPLOAD,
        caller: caller_key(&caller),
        status: store::AWAITING_UPLOAD.into(),
        folder: folder.name.clone(),
        path,
        spec: UploadSpec {
            content_type,
            content_disposition: request.content_disposition,
            cache_control: request.cache_control,
            max_bytes,
            fields,
            url_expires: now + ttl,
        },
        staged_blob: None,
        staged_size: None,
        staged_etag: None,
        error_code: None,
        error_message: None,
        expires_at: now + ttl,
    };
    let limits = Limits::new(&state.config, &folder);
    let (staging, objects) = (state.staging.clone(), state.objects.clone());
    let id = state
        .db
        .call(move |conn| -> Result<String, ApiError> {
            store::sweep(conn, &staging, &objects, now, false)?;
            let existing = store::object_at(conn, &operation.folder, &operation.path)?;
            let size = operation.spec.max_bytes.unwrap_or_default();
            let new = new_version(&operation, existing.as_ref(), &caller, size, now);
            authorize_write(&folder, &caller, &new, existing.as_ref())?;
            limits.check(
                &folder,
                &operation,
                existing.as_ref(),
                store::used_bytes(conn)?,
            )?;
            store::insert_operation(conn, &operation)?;
            Ok(operation.id)
        })
        .await??;
    Ok(accepted(&id))
}

/// `GET /api/storage/operations/{id}/status` → `{operationId, status, signedUrl?, expiresAt?,
/// success?, errorCode?, errorMessage?}`.
async fn operation_status(
    State(state): Shared,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let caller = resolve_caller(&state, &headers)?;
    let now = store::now_unix();
    let operation = state
        .db
        .call(move |conn| store::operation(conn, &id, now))
        .await??;
    let operation = own_operation(operation, &caller)?;
    let mut body = json!({ "operationId": operation.id, "status": operation.status });
    match operation.status.as_str() {
        store::AWAITING_UPLOAD => {
            let token =
                state
                    .signer
                    .sign(Purpose::Upload, &operation.id, operation.spec.url_expires);
            body["signedUrl"] = format!("{}{UPLOAD_URL_PATH}/{token}", origin(&state)).into();
            body["expiresAt"] = store::iso_unix(operation.spec.url_expires).into();
        }
        store::SUCCEEDED => body["success"] = true.into(),
        store::FAILED => {
            body["success"] = false.into();
            body["errorCode"] = operation.error_code.into();
            body["errorMessage"] = operation.error_message.into();
        }
        _ => {}
    }
    Ok(Json(body).into_response())
}

/// `PUT <signed upload URL>` with the raw bytes → 201 with the `ETag`.
async fn signed_put(
    State(state): Shared,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let now = store::now_unix();
    let id = state
        .signer
        .verify(Purpose::Upload, &token, now)
        .map_err(|error| {
            ApiError::UploadSessionExpired(match error {
                TokenError::Expired => "The signed upload URL has expired; upload again.".into(),
                TokenError::Invalid => "The signed upload URL is not valid.".into(),
            })
        })?;
    let (operation, used) = state
        .db
        .call(move |conn| -> rusqlite::Result<_> {
            let Some(operation) = store::operation(conn, &id, now)? else {
                return Ok((None, 0));
            };
            // An overwrite frees the bytes of the version it replaces.
            let replaced = store::object_at(conn, &operation.folder, &operation.path)?
                .map_or(0, |object| object.size);
            let used = store::used_bytes(conn)?.saturating_sub(replaced);
            Ok((Some(operation), used))
        })
        .await??;
    let operation = operation
        .filter(|operation| {
            operation.kind == store::UPLOAD && operation.status == store::AWAITING_UPLOAD
        })
        .ok_or_else(|| {
            ApiError::UploadSessionExpired(
                "The signed upload URL was already used or its upload has ended; upload again."
                    .into(),
            )
        })?;
    if let Some(sent) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        && names::media_type(sent) != names::media_type(&operation.spec.content_type)
    {
        return Err(ApiError::UnsupportedContentType(format!(
            "The upload declared the content type '{}' but sent '{sent}'.",
            operation.spec.content_type
        )));
    }
    let folder = state
        .model
        .storage_folder(&operation.folder)
        .ok_or_else(|| folder_gone(&operation.folder))?;
    let limits = Limits::new(&state.config, folder);
    let mut limit = limits.per_object.min(limits.total.saturating_sub(used));
    if let Some(declared) = operation.spec.max_bytes {
        limit = limit.min(declared);
    }
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if length.is_some_and(|length| length > limit) {
        return Err(body_too_large(limit));
    }

    let blob = Uuid::new_v4().simple().to_string();
    let part = state.staging.join(format!("{blob}.part"));
    let staged = state.staging.join(&blob);
    let received = match receive(body, &part, limit).await {
        Ok(received) => tokio::fs::rename(&part, &staged)
            .await
            .map(|()| received)
            .map_err(ApiError::from),
        Err(error) => Err(error),
    };
    let (size, etag) = match received {
        Ok(received) => received,
        Err(error) => {
            store::remove_file(&part);
            return Err(error);
        }
    };
    let (id, staged_blob, staged_etag) = (operation.id, blob, etag.clone());
    let commit_by = store::now_unix() + COMMIT_GRACE_SECS;
    let marked = state
        .db
        .call(move |conn| {
            store::mark_uploaded(conn, &id, &staged_blob, size, &staged_etag, commit_by)
        })
        .await??;
    if !marked {
        store::remove_file(&staged);
        return Err(ApiError::UploadSessionExpired(
            "The signed upload URL was already used or its upload has ended; upload again.".into(),
        ));
    }
    Ok((StatusCode::CREATED, [(header::ETAG, etag)]).into_response())
}

/// Streams `body` to `path`, at most `limit` bytes; the size and the quoted SHA-256 ETag.
async fn receive(body: Body, path: &FsPath, limit: u64) -> Result<(u64, String), ApiError> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut hash = digest::Context::new(&digest::SHA256);
    let mut size: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            ApiError::BadRequest("The upload body was interrupted; upload again.".into())
        })?;
        size += chunk.len() as u64;
        if size > limit {
            return Err(body_too_large(limit));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok((size, format!("\"{}\"", hex::encode(hash.finish()))))
}

/// `POST /api/storage/uploads/{id}/commit` (no body) → 202 `{operationId}`; the operation has
/// `Succeeded` by the next poll. A failure here also ends the upload: the client sends no abort
/// after a failed commit.
async fn commit_upload(
    State(state): Shared,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let caller = resolve_caller(&state, &headers)?;
    let shared = Arc::clone(&state);
    let id = state
        .db
        .call(move |conn| commit(conn, &shared, &id, &caller))
        .await??;
    Ok(accepted(&id))
}

fn commit(
    conn: &mut Connection,
    state: &AppState,
    id: &str,
    caller: &Caller,
) -> Result<String, ApiError> {
    let now = store::now_unix();
    let operation = own_operation(store::operation(conn, id, now)?, caller)?;
    match operation.status.as_str() {
        store::SUCCEEDED => return Ok(operation.id),
        store::AWAITING_UPLOAD => {
            return Err(ApiError::Conflict(
                "The upload's bytes have not been sent; PUT them to the signed URL before \
                 committing."
                    .into(),
            ));
        }
        store::AWAITING_COMMIT => {}
        _ => {
            return Err(ApiError::Conflict(
                "The upload has already ended (aborted or failed); upload again.".into(),
            ));
        }
    }
    let staged = state
        .staging
        .join(operation.staged_blob.as_deref().unwrap_or_default());
    match store_upload(conn, state, &operation, caller, &staged, now) {
        Ok(replaced) => {
            if let Some(blob) = replaced {
                store::remove_file(&state.objects.join(blob));
            }
            Ok(operation.id)
        }
        Err(error) => {
            store::remove_file(&staged);
            let message = error.to_string();
            let failure = Some((error.operation_code(), message.as_str()));
            store::finish_operation(conn, &operation.id, failure, now + FINISHED_RETENTION_SECS)?;
            Err(error)
        }
    }
}

/// Re-checks the rules against the current object, moves the staged file into place and saves
/// the new version; the blob of the version it replaced.
fn store_upload(
    conn: &mut Connection,
    state: &AppState,
    operation: &Operation,
    caller: &Caller,
    staged: &FsPath,
    now: i64,
) -> Result<Option<String>, ApiError> {
    let folder = state
        .model
        .storage_folder(&operation.folder)
        .ok_or_else(|| folder_gone(&operation.folder))?;
    let tx = conn.transaction()?;
    let existing = store::object_at(&tx, &operation.folder, &operation.path)?;
    let mut object = new_version(
        operation,
        existing.as_ref(),
        caller,
        operation.staged_size.unwrap_or_default(),
        now,
    );
    authorize_write(folder, caller, &object, existing.as_ref())?;
    object.blob = operation.staged_blob.clone().unwrap_or_default();
    object.etag = operation.staged_etag.clone().unwrap_or_default();
    let target = state.objects.join(&object.blob);
    std::fs::rename(staged, &target)?;
    let saved = store::save_object(&tx, &object)
        .and_then(|()| {
            store::finish_operation(&tx, &operation.id, None, now + FINISHED_RETENTION_SECS)
        })
        .and_then(|()| tx.commit());
    if let Err(error) = saved {
        store::remove_file(&target);
        return Err(error.into());
    }
    Ok(existing.map(|existing| existing.blob))
}

/// `DELETE /api/storage/uploads/{id}` → 204; drops the staged bytes of an uncommitted upload.
async fn abort_upload(
    State(state): Shared,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let caller = resolve_caller(&state, &headers)?;
    let staging = state.staging.clone();
    state
        .db
        .call(move |conn| -> Result<(), ApiError> {
            let now = store::now_unix();
            let operation = own_operation(store::operation(conn, &id, now)?, &caller)?;
            match operation.status.as_str() {
                store::SUCCEEDED => Err(ApiError::Conflict(
                    "The upload is already committed; delete the object instead.".into(),
                )),
                store::FAILED => Ok(()),
                _ => {
                    let aborted = Some((ABORTED_CODE, "The upload was aborted."));
                    store::finish_operation(conn, &id, aborted, now + FINISHED_RETENTION_SECS)?;
                    if let Some(blob) = operation.staged_blob {
                        store::remove_file(&staging.join(blob));
                    }
                    Ok(())
                }
            }
        })
        .await??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/storage/download` → 200 `{signedUrl, expiresAt}`.
async fn start_download(
    State(state): Shared,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let caller = resolve_caller(&state, &headers)?;
    let request: ObjectRequest = json_body(body).await?;
    let folder = find_folder(&state.model, request.folder.as_deref())?;
    let path = names::logical_path(request.file_name.as_deref(), request.prefix.as_deref())?;
    let ttl = expiry_secs(request.expires_in_seconds.as_ref(), state.config.url_ttl)?;
    let object = authorized_object(&state, folder, &caller, path, Action::Read).await?;
    let expires = store::now_unix() + ttl;
    let token = state.signer.sign(Purpose::Download, &object.blob, expires);
    Ok(Json(json!({
        "signedUrl": format!("{}{OBJECT_URL_PATH}/{token}", origin(&state)),
        "expiresAt": store::iso_unix(expires),
    }))
    .into_response())
}

/// `GET <signed download URL>` → 200 with the bytes and their headers.
async fn signed_get(State(state): Shared, Path(token): Path<String>) -> Result<Response, ApiError> {
    let blob = state
        .signer
        .verify(Purpose::Download, &token, store::now_unix())
        .map_err(|error| {
            ApiError::SignatureRejected(match error {
                TokenError::Expired => {
                    "The signed download URL has expired; download again.".into()
                }
                TokenError::Invalid => "The signed download URL is not valid.".into(),
            })
        })?;
    let gone = || {
        ApiError::NotFound(
            "The object was deleted or replaced since this URL was issued; download it again."
                .into(),
        )
    };
    let lookup = blob.clone();
    let object = state
        .db
        .call(move |conn| store::object_by_blob(conn, &lookup))
        .await??
        .ok_or_else(gone)?;
    let file = match tokio::fs::File::open(state.objects.join(&blob)).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Err(gone()),
        Err(error) => return Err(error.into()),
    };
    let mut response = Response::new(Body::from_stream(ReaderStream::new(file)));
    let headers = response.headers_mut();
    let header_value = |value: &str, fallback: &'static str| {
        HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static(fallback))
    };
    headers.insert(
        header::CONTENT_TYPE,
        header_value(&object.content_type, DEFAULT_CONTENT_TYPE),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(object.size));
    headers.insert(header::ETAG, header_value(&object.etag, "\"\""));
    if let Some(disposition) = object
        .content_disposition
        .as_deref()
        .and_then(|value| HeaderValue::from_str(value).ok())
    {
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    let cache = object
        .cache_control
        .as_deref()
        .unwrap_or("private, no-cache");
    headers.insert(
        header::CACHE_CONTROL,
        header_value(cache, "private, no-cache"),
    );
    // Objects are served from the API's origin: never let one run as a page there.
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("sandbox; default-src 'none'"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

/// `GET /api/storage/list?folder=&prefix=&continuation=&limit=` → 200 `{items, continuation?}`:
/// the readable objects under `prefix/` (any depth) in path order.
async fn list(
    State(state): Shared,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let caller = resolve_caller(&state, &headers)?;
    let folder = find_folder(&state.model, query.get("folder").map(String::as_str))?.clone();
    let prefix = query
        .get("prefix")
        .filter(|prefix| !prefix.is_empty())
        .cloned();
    if let Some(prefix) = &prefix {
        names::check_prefix(prefix)?;
    }
    let after = match query.get("continuation").filter(|token| !token.is_empty()) {
        Some(token) => Some(continuation_path(&state.signer, token)?),
        None => None,
    };
    let limit = query
        .get("limit")
        .and_then(|limit| limit.trim().parse::<usize>().ok())
        .filter(|limit| *limit >= 1)
        .map_or(DEFAULT_LIST_LIMIT, |limit| limit.min(MAX_LIST_LIMIT));
    if !rules::may(&folder, &caller, Action::Read) {
        return Err(denied(&folder, &caller, Action::Read));
    }
    let (page, more) = state
        .db
        .call(move |conn| {
            store::list_page(
                conn,
                &folder.name,
                prefix.as_deref(),
                after.as_deref(),
                limit,
                |object| rules::allows(&folder, &caller, Action::Read, &object.item()),
            )
        })
        .await??;
    let mut body = json!({ "items": page.iter().map(Object::to_wire).collect::<Vec<_>>() });
    if let Some(last) = page.last().filter(|_| more) {
        let id = URL_SAFE_NO_PAD.encode(&last.path);
        let expires = store::now_unix() + CONTINUATION_TTL_SECS;
        body["continuation"] = state
            .signer
            .sign(Purpose::Continuation, &id, expires)
            .into();
    }
    Ok(Json(body).into_response())
}

fn continuation_path(signer: &Signer, token: &str) -> Result<String, ApiError> {
    signer
        .verify(Purpose::Continuation, token, store::now_unix())
        .ok()
        .and_then(|id| URL_SAFE_NO_PAD.decode(id).ok())
        .and_then(|path| String::from_utf8(path).ok())
        .ok_or_else(|| {
            ApiError::InvalidContinuation(
                "The continuation token is not one this server issued, or it has expired; list \
                 from the start."
                    .into(),
            )
        })
}

/// `DELETE /api/storage/delete` with `{folder, fileName, prefix?}` → 202 `{operationId}`; the
/// object is gone (and the operation `Succeeded`) when this answers.
async fn delete_object(
    State(state): Shared,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let caller = resolve_caller(&state, &headers)?;
    let request: ObjectRequest = json_body(body).await?;
    let folder = find_folder(&state.model, request.folder.as_deref())?;
    let path = names::logical_path(request.file_name.as_deref(), request.prefix.as_deref())?;
    let object = authorized_object(&state, folder, &caller, path, Action::Delete).await?;
    let now = store::now_unix();
    let operation = Operation {
        id: Uuid::new_v4().simple().to_string(),
        kind: store::DELETE,
        caller: caller_key(&caller),
        status: store::SUCCEEDED.into(),
        folder: object.folder.clone(),
        path: object.path.clone(),
        spec: UploadSpec::default(),
        staged_blob: None,
        staged_size: None,
        staged_etag: None,
        error_code: None,
        error_message: None,
        expires_at: now + FINISHED_RETENTION_SECS,
    };
    let id = operation.id.clone();
    let object_id = object.id.clone();
    let deleted_blob = state
        .db
        .call(move |conn| -> rusqlite::Result<Option<String>> {
            let tx = conn.transaction()?;
            let blob = store::delete_object(&tx, &object_id)?;
            store::insert_operation(&tx, &operation)?;
            tx.commit()?;
            Ok(blob)
        })
        .await??;
    if let Some(blob) = deleted_blob {
        store::remove_file(&state.objects.join(blob));
    }
    Ok(accepted(&id))
}

/// The object at `path` when a rule grants `action` on it.
async fn authorized_object(
    state: &AppState,
    folder: &StorageFolder,
    caller: &Caller,
    path: String,
    action: Action,
) -> Result<Object, ApiError> {
    if !rules::may(folder, caller, action) {
        return Err(denied(folder, caller, action));
    }
    let (name, lookup) = (folder.name.clone(), path.clone());
    let object = state
        .db
        .call(move |conn| store::object_at(conn, &name, &lookup))
        .await??
        .ok_or_else(|| {
            ApiError::NotFound(format!(
                "'{path}' does not exist in storage folder '{}'.",
                folder.name
            ))
        })?;
    if !rules::allows(folder, caller, action, &object.item()) {
        return Err(denied(folder, caller, action));
    }
    Ok(object)
}

/// The version an upload writes: a new object, or the next version of `existing` (whose id,
/// owner and creation time it keeps).
fn new_version(
    operation: &Operation,
    existing: Option<&Object>,
    caller: &Caller,
    size: u64,
    now: i64,
) -> Object {
    let spec = &operation.spec;
    let now = store::iso_unix(now);
    Object {
        id: existing.map_or_else(|| Uuid::new_v4().to_string(), |e| e.id.clone()),
        folder: operation.folder.clone(),
        path: operation.path.clone(),
        name: operation
            .path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_owned(),
        blob: String::new(),
        size,
        content_type: spec.content_type.clone(),
        content_disposition: spec.content_disposition.clone(),
        cache_control: spec.cache_control.clone(),
        etag: String::new(),
        owner_id: match existing {
            Some(existing) => existing.owner_id.clone(),
            None => caller
                .session
                .as_ref()
                .map(|session| session.user_id.clone()),
        },
        fields: spec.fields.clone(),
        created_at: existing.map_or_else(|| now.clone(), |e| e.created_at.clone()),
        updated_at: now,
    }
}

/// A new object needs `create`; replacing one needs `onConflict: 'overwrite'` and `update` on
/// both the stored and the new version.
fn authorize_write(
    folder: &StorageFolder,
    caller: &Caller,
    new: &Object,
    existing: Option<&Object>,
) -> Result<(), ApiError> {
    let Some(existing) = existing else {
        return if rules::allows(folder, caller, Action::Create, &new.item()) {
            Ok(())
        } else {
            Err(denied(folder, caller, Action::Create))
        };
    };
    if folder.on_conflict == OnConflict::Error {
        if !rules::allows(folder, caller, Action::Create, &new.item()) {
            return Err(denied(folder, caller, Action::Create));
        }
        return Err(ApiError::Conflict(format!(
            "'{}' already exists in storage folder '{}', whose onConflict is 'error'; delete it \
             first or upload under another name.",
            existing.path, folder.name
        )));
    }
    if rules::allows(folder, caller, Action::Update, &existing.item())
        && rules::allows(folder, caller, Action::Update, &new.item())
    {
        Ok(())
    } else {
        Err(denied(folder, caller, Action::Update))
    }
}

/// Object size limits: the folder's `maxSize` lowers the per-object cap.
struct Limits {
    per_object: u64,
    total: u64,
}

impl Limits {
    fn new(config: &StorageConfig, folder: &StorageFolder) -> Self {
        Self {
            per_object: folder
                .max_size
                .map_or(config.max_file_bytes, |max| max.min(config.max_file_bytes)),
            total: config.max_total_bytes,
        }
    }

    fn check(
        &self,
        folder: &StorageFolder,
        operation: &Operation,
        existing: Option<&Object>,
        used: u64,
    ) -> Result<(), ApiError> {
        let content_type = &operation.spec.content_type;
        if !names::content_type_allowed(&folder.allowed_content_types, content_type) {
            return Err(ApiError::UnsupportedContentType(format!(
                "Storage folder '{}' accepts {}, not '{content_type}'.",
                folder.name,
                folder.allowed_content_types.join(", ")
            )));
        }
        let Some(size) = operation.spec.max_bytes else {
            return Ok(());
        };
        if size > self.per_object {
            return Err(ApiError::TooLarge(format!(
                "The upload is {size} bytes; storage folder '{}' accepts at most {} bytes per \
                 object.",
                folder.name, self.per_object
            )));
        }
        let replaced = existing.map_or(0, |existing| existing.size);
        if used.saturating_sub(replaced).saturating_add(size) > self.total {
            return Err(ApiError::TooLarge(format!(
                "The upload would exceed the local storage quota of {} bytes ({used} in use); \
                 delete objects or raise the limit.",
                self.total
            )));
        }
        Ok(())
    }
}

/// Checks `fields` against the folder's declared fields.
fn check_fields(
    folder: &StorageFolder,
    fields: Option<Value>,
) -> Result<Map<String, Value>, ApiError> {
    let fields = match fields {
        None | Some(Value::Null) => return Ok(Map::new()),
        Some(Value::Object(fields)) => fields,
        Some(_) => {
            return Err(ApiError::BadRequest(
                "`fields` must be an object of the folder's declared fields.".into(),
            ));
        }
    };
    for (name, value) in &fields {
        let Some(field) = folder.fields.iter().find(|field| field.name == *name) else {
            let declared: Vec<&str> = folder.fields.iter().map(|f| f.name.as_str()).collect();
            return Err(ApiError::BadRequest(format!(
                "`{name}` is not a field of storage folder '{}' (declared: {}); declare it on the \
                 @blob class.",
                folder.name,
                if declared.is_empty() {
                    "none".into()
                } else {
                    declared.join(", ")
                }
            )));
        };
        let valid = match (field.kind, value) {
            (_, Value::Null) => field.nullable,
            (StorageFieldType::String, Value::String(_))
            | (StorageFieldType::Number, Value::Number(_))
            | (StorageFieldType::Boolean, Value::Bool(_)) => true,
            (StorageFieldType::Date, Value::String(date)) => {
                OffsetDateTime::parse(date, &Rfc3339).is_ok()
            }
            _ => false,
        };
        if !valid {
            return Err(ApiError::BadRequest(format!(
                "Field `{name}` of storage folder '{}' must be a {}{}; got {value}.",
                folder.name,
                format!("{:?}", field.kind).to_lowercase(),
                if field.nullable { " or null" } else { "" }
            )));
        }
    }
    Ok(fields)
}

fn resolve_caller(state: &AppState, headers: &HeaderMap) -> Result<Caller, ApiError> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    state
        .callers
        .resolve(bearer_token(authorization))
        .map_err(|_| {
            ApiError::Unauthenticated(
                "The session is invalid or has expired; sign in again.".into(),
            )
        })
}

/// Operations belong to the caller that started them (by `sub`; anonymous ones to anonymous
/// callers).
fn caller_key(caller: &Caller) -> String {
    caller
        .session
        .as_ref()
        .map(|session| session.user_id.clone())
        .unwrap_or_default()
}

fn own_operation(operation: Option<Operation>, caller: &Caller) -> Result<Operation, ApiError> {
    let key = caller_key(caller);
    operation
        .filter(|operation| operation.caller == key)
        .ok_or_else(|| {
            ApiError::NotFound(
                "The storage operation does not exist, has expired, or was started by another \
                 user."
                    .into(),
            )
        })
}

fn find_folder<'m>(model: &'m AppModel, name: Option<&str>) -> Result<&'m StorageFolder, ApiError> {
    let folders = model
        .storage
        .as_ref()
        .map(|storage| storage.folders.as_slice())
        .unwrap_or_default();
    if folders.is_empty() {
        return Err(ApiError::NoStorage);
    }
    let name = name
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::InvalidFolder("The request names no `folder`.".into()))?;
    folders
        .iter()
        .find(|folder| folder.name == name)
        .ok_or_else(|| {
            let known: Vec<&str> = folders.iter().map(|folder| folder.name.as_str()).collect();
            ApiError::NotFound(format!(
                "The app has no storage folder '{name}' (it has: {}); folders are the app's @blob \
                 classes.",
                known.join(", ")
            ))
        })
}

fn folder_gone(name: &str) -> ApiError {
    ApiError::NotFound(format!(
        "The storage folder '{name}' no longer exists in the app's model; upload again."
    ))
}

/// Anonymous callers are told to sign in; signed-in ones that a rule denies get
/// `PermissionDenied`.
fn denied(folder: &StorageFolder, caller: &Caller, action: Action) -> ApiError {
    let verb = match action {
        Action::Create => "create",
        Action::Read => "read",
        Action::Update => "update",
        Action::Delete => "delete",
        _ => "use",
    };
    if caller.is_authenticated() {
        ApiError::PermissionDenied(format!(
            "No rule of storage folder '{}' allows this user to {verb} this object.",
            folder.name
        ))
    } else {
        ApiError::Unauthenticated(format!(
            "Sign in first: no rule of storage folder '{}' lets anonymous users {verb} objects.",
            folder.name
        ))
    }
}

fn expiry_secs(value: Option<&Value>, default: Duration) -> Result<i64, ApiError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(i64::try_from(default.as_secs()).unwrap_or(i64::MAX / 2));
    };
    value
        .as_f64()
        .filter(|seconds| (MIN_EXPIRY_SECS..=MAX_EXPIRY_SECS).contains(seconds))
        .map(|seconds| seconds as i64)
        .ok_or_else(|| {
            ApiError::InvalidSasExpiry(format!(
                "`expiresInSeconds` must be a number from {MIN_EXPIRY_SECS} to {MAX_EXPIRY_SECS}; \
                 got {value}."
            ))
        })
}

async fn json_body<T: DeserializeOwned>(body: Body) -> Result<T, ApiError> {
    let bytes = axum::body::to_bytes(body, MAX_JSON_BYTES)
        .await
        .map_err(|_| {
            ApiError::PayloadTooLarge(format!(
                "The request body could not be read or is over {} KiB.",
                MAX_JSON_BYTES / 1024
            ))
        })?;
    serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::BadRequest(format!(
            "The request body is not the expected JSON object: {error}."
        ))
    })
}

fn body_too_large(limit: u64) -> ApiError {
    ApiError::PayloadTooLarge(format!(
        "The upload is over its limit of {limit} bytes (its declared size, the folder's maxSize, \
         the per-object cap or the remaining quota)."
    ))
}

fn accepted(operation_id: &str) -> Response {
    (
        StatusCode::ACCEPTED,
        Json(json!({ "operationId": operation_id })),
    )
        .into_response()
}

fn origin(state: &AppState) -> &str {
    state.config.api_origin.trim_end_matches('/')
}
