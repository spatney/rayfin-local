//! What the T-SQL executor needs from a database connection: [`SqlBackend`] runs statements, and
//! [`SqlTransaction`] runs a write and its read-back atomically. `rl-connectors` implements them
//! over TDS; tests use fakes.

use std::future::Future;
use std::pin::Pin;

/// A boxed, sendable future (object-safe async trait methods).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A T-SQL statement: text with `@P1`…`@Pn` placeholders, and their values in that order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Statement {
    pub text: String,
    pub params: Vec<Param>,
}

/// A parameter value.
#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// An exact decimal: `value` × 10^-`scale`.
    Decimal {
        value: i128,
        scale: u8,
    },
    /// Text, bound as `NVARCHAR`. UUIDs are bound as text too (lowercase `D` format), which SQL
    /// Server converts to compare with a `uniqueidentifier` column.
    Text(String),
    /// A UTC date-time `YYYY-MM-DDTHH:MM:SS.fffffff`, bound as text: statements cast it with
    /// `CAST(@Pn AS DATETIME2(7))`.
    DateTime(String),
}

/// A value read from a result column.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    /// A `decimal`/`numeric`/`money` value as decimal text.
    Decimal(String),
    Text(String),
    /// A `uniqueidentifier`, lowercase `D` format.
    Uuid(String),
    /// A date and/or time as ISO 8601 text (`YYYY-MM-DDTHH:MM:SS.fffffff`, with an offset for
    /// `datetimeoffset`).
    DateTime(String),
    /// Anything else (binary data), as text.
    Other(String),
}

/// One result row.
pub type Row = Vec<Value>;

/// Why a statement failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    /// The database ran the statement and refused it (a constraint, a conversion, an unknown
    /// column…): DAB's `DatabaseOperationFailed`, with the server's message as its detail.
    #[error("{message} (error {number})")]
    Database { number: u32, message: String },
    /// The database can't be used: no token, no access, unreachable. `message` says what to do.
    #[error("{0}")]
    Unavailable(String),
}

/// A database the T-SQL executor runs statements on.
pub trait SqlBackend: Send + Sync {
    /// Runs a read and returns its rows.
    fn query<'a>(&'a self, statement: &'a Statement) -> BoxFuture<'a, Result<Vec<Row>, SqlError>>;

    /// Starts a transaction for a write. Dropping it without [`SqlTransaction::commit`] rolls
    /// it back.
    fn begin(&self) -> BoxFuture<'_, Result<Box<dyn SqlTransaction>, SqlError>>;
}

/// A write in progress.
pub trait SqlTransaction: Send {
    /// Runs a statement and returns its rows (an `OUTPUT` clause's, or a read's).
    fn query<'a>(
        &'a mut self,
        statement: &'a Statement,
    ) -> BoxFuture<'a, Result<Vec<Row>, SqlError>>;

    /// Runs a statement and returns how many rows it changed.
    fn execute<'a>(&'a mut self, statement: &'a Statement) -> BoxFuture<'a, Result<u64, SqlError>>;

    fn commit(self: Box<Self>) -> BoxFuture<'static, Result<(), SqlError>>;
}
