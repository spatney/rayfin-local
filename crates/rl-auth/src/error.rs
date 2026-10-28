//! The errors of the auth service's Rust API.

use std::path::PathBuf;

use rl_db::DbError;

/// Why an auth operation failed. Messages never contain tokens, passwords or email addresses.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(
        "the signing key {path} is unreadable or corrupt ({reason}); delete it and restart to \
         create a new one (signed-in users will have to sign in again)"
    )]
    Key { path: PathBuf, reason: String },
    #[error("could not read or write the signing key {path}: {source}")]
    KeyIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid auth settings: {0}")]
    Settings(String),
    #[error("a user with this username already exists")]
    UsernameTaken,
    #[error("no such user")]
    UserNotFound,
    #[error("the username or password is wrong")]
    InvalidCredentials,
    #[error("{0}")]
    InvalidInput(&'static str),
    /// A refresh token the service does not accept; the message is the one the real service sends.
    #[error("{0}")]
    InvalidGrant(&'static str),
    #[error("password hashing failed: {0}")]
    PasswordHash(String),
    #[error("{0}")]
    Crypto(&'static str),
    #[error("an auth background task failed: {0}")]
    Task(String),
}

impl From<rusqlite::Error> for AuthError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Db(DbError::Sqlite(error))
    }
}

impl From<tokio::task::JoinError> for AuthError {
    fn from(error: tokio::task::JoinError) -> Self {
        Self::Task(error.to_string())
    }
}
