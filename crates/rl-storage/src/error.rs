//! Failures, answered in the envelope `StorageFolderClient.handleErrorResponse` reads:
//! `{"error":{"code","message","correlationId"}}` (`code` becomes the `StorageError` code).

use std::path::PathBuf;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;

/// Why the storage router could not start.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error(
        "could not prepare the storage folder {path}: {source}; check that the project folder is \
         writable"
    )]
    Folder {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not set up the storage tables: {0}")]
    Db(#[from] rl_db::DbError),
    #[error("could not clean up the storage tables: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// A request failure. Each variant is one `StorageErrorCode` of the 1.36.2 client
/// (`storageErrorCodes.js`).
#[derive(Debug, thiserror::Error)]
pub(crate) enum ApiError {
    #[error("{0}")]
    Unauthenticated(String),
    #[error("{0}")]
    PermissionDenied(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(
        "The app declares no storage folders; add a @blob class to the app's model to use storage."
    )]
    NoStorage,
    #[error("{0}")]
    InvalidFolder(String),
    #[error("{0}")]
    InvalidName(String),
    #[error("{0}")]
    InvalidPrefix(String),
    #[error("{0}")]
    InvalidSasExpiry(String),
    #[error("{0}")]
    InvalidContinuation(String),
    #[error("{0}")]
    TooLarge(String),
    #[error("{0}")]
    UnsupportedContentType(String),
    /// A malformed request (`Unexpected` 400).
    #[error("{0}")]
    BadRequest(String),
    /// A body over a limit (`Unexpected` 413).
    #[error("{0}")]
    PayloadTooLarge(String),
    /// A signed upload URL that is invalid, expired or used (403, which the client reports as
    /// `UploadSessionExpired`).
    #[error("{0}")]
    UploadSessionExpired(String),
    /// A signed download URL that is invalid or expired.
    #[error("{0}")]
    SignatureRejected(String),
    /// A `Range` that selects none of the object (416 on the signed download URL).
    #[error("{0}")]
    RangeNotSatisfiable(String),
    #[error("rayfin-local storage failed: {0}")]
    Internal(String),
}

impl ApiError {
    pub(crate) fn status(&self) -> StatusCode {
        match self {
            Self::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            Self::PermissionDenied(_)
            | Self::UploadSessionExpired(_)
            | Self::SignatureRejected(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::RangeNotSatisfiable(_) => StatusCode::RANGE_NOT_SATISFIABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::NoStorage
            | Self::InvalidFolder(_)
            | Self::InvalidName(_)
            | Self::InvalidPrefix(_)
            | Self::InvalidSasExpiry(_)
            | Self::InvalidContinuation(_)
            | Self::TooLarge(_)
            | Self::UnsupportedContentType(_)
            | Self::BadRequest(_) => StatusCode::BAD_REQUEST,
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated(_) => "Unauthenticated",
            Self::PermissionDenied(_) => "PermissionDenied",
            Self::NotFound(_) => "NotFound",
            Self::Conflict(_) => "Conflict",
            Self::NoStorage => "NoStorageEntitiesConfigured",
            Self::InvalidFolder(_) => "InvalidFolder",
            Self::InvalidName(_) => "InvalidName",
            Self::InvalidPrefix(_) => "InvalidPrefix",
            Self::InvalidSasExpiry(_) => "InvalidSasExpiry",
            Self::InvalidContinuation(_) => "InvalidContinuation",
            Self::TooLarge(_) => "TooLarge",
            Self::UnsupportedContentType(_) => "UnsupportedContentType",
            Self::UploadSessionExpired(_) => "UploadSessionExpired",
            Self::BadRequest(_)
            | Self::PayloadTooLarge(_)
            | Self::SignatureRejected(_)
            | Self::RangeNotSatisfiable(_)
            | Self::Internal(_) => "Unexpected",
        }
    }

    /// The operation-poll `errorCode` (`OPERATION_ERROR_MAP` of `storageErrorCodes.js`) that the
    /// client maps back to this error's code.
    pub(crate) fn operation_code(&self) -> &'static str {
        match self {
            Self::PermissionDenied(_) | Self::Unauthenticated(_) => "PERMISSION_DENIED",
            Self::NotFound(_) => "STORAGE_OBJECT_NOT_FOUND",
            Self::Conflict(_) => "UPLOAD_TARGET_EXISTS",
            Self::TooLarge(_) => "UPLOAD_SIZE_EXCEEDS_LIMIT",
            Self::UnsupportedContentType(_) => "UNSUPPORTED_CONTENT_TYPE",
            Self::Internal(_) => "ORCHESTRATION_SYSTEM_ERROR",
            _ => "ORCHESTRATION_USER_ERROR",
        }
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Internal(format!("the storage database failed: {error}"))
    }
}

impl From<rl_db::DbError> for ApiError {
    fn from(error: rl_db::DbError) -> Self {
        Self::Internal(format!("the storage database failed: {error}"))
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::Internal(format!("a storage file operation failed: {error}"))
    }
}

/// Marks an error response so the `/api/storage` layer can add the request's correlation id.
#[derive(Clone, Debug)]
pub(crate) struct ErrorBody {
    pub code: &'static str,
    pub message: String,
}

impl ErrorBody {
    pub(crate) fn envelope(&self, correlation_id: Option<&str>) -> serde_json::Value {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(id) = correlation_id {
            error["correlationId"] = id.into();
        }
        json!({ "error": error })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Self::Internal(detail) = &self {
            tracing::error!("{detail}");
        }
        let body = ErrorBody {
            code: self.code(),
            message: self.to_string(),
        };
        let mut response = (self.status(), Json(body.envelope(None))).into_response();
        response.extensions_mut().insert(body);
        response
    }
}
