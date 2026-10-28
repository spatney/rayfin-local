//! [`SchemaError`]: why the entity schema could not be generated or migrated.

use std::fmt;

use crate::DbError;

/// Why the entity schema could not be generated or applied.
#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("the entity schema migration failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "entity `{entity}` uses the table name `{table}`, but names starting with `_rl_` or \
         `sqlite_` are reserved by rayfin-local; rename the entity or set another name with \
         @entity('...')"
    )]
    ReservedName { entity: String, table: String },
    #[error(
        "`{first}` and `{second}` need database objects named `{name}` (SQLite names are \
         case-insensitive); rename one of the entities"
    )]
    NameClash {
        name: String,
        first: String,
        second: String,
    },
    #[error(
        "the default of `{entity}.{field}` ({value}) is not a valid {kind}: {reason}; fix the \
         decorator's `default`"
    )]
    InvalidDefault {
        entity: String,
        field: String,
        value: String,
        kind: &'static str,
        reason: String,
    },
    #[error(
        "the stored entity schema cannot be read ({reason}); upgrade rayfin-local or run \
         `rayfin-local reset` to start over with an empty database"
    )]
    StoredSchema { reason: String },
    #[error(
        "the database already has a table or index named `{name}` that rayfin-local did not \
         create; run `rayfin-local reset` to start over with an empty database"
    )]
    Untracked { name: String },
    #[error(
        "the schema change would lose data:\n{}\nrun with --force-schema to apply it anyway, or \
         `rayfin-local reset` to start over with an empty database",
        list(.losses)
    )]
    DataLoss { losses: Vec<DataLoss> },
    #[error(
        "table `{table}` cannot be migrated: its rows do not satisfy the new schema ({reason}); \
         change or delete those rows, or run `rayfin-local reset` to start over with an empty \
         database"
    )]
    Rebuild { table: String, reason: String },
    #[error(
        "after the migration, {rows} row(s) of `{table}` reference missing rows of `{parent}`; \
         change or delete those rows, or run `rayfin-local reset` to start over with an empty \
         database"
    )]
    ForeignKeyViolation {
        table: String,
        parent: String,
        rows: u64,
    },
}

fn list(losses: &[DataLoss]) -> String {
    losses
        .iter()
        .map(|loss| format!("  - {loss}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Data a migration step would drop or replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataLoss {
    pub table: String,
    /// `None` when the whole table goes.
    pub column: Option<String>,
    /// The rows affected.
    pub rows: u64,
    pub reason: LossReason,
}

/// What happens to the affected values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LossReason {
    /// The entity is gone: its table is dropped.
    DropTable,
    /// The field is gone: its column is dropped (rows counts the values that are not null).
    DropColumn,
    /// The field changes type and these values cannot be converted.
    Unconvertible { from: String, to: String },
    /// The decimal scale shrinks and these values are rounded.
    Rounded { scale: u32 },
    /// The field is required without a default and these rows have no value: they get the
    /// placeholder (what Fabric's EF Core migrations fill in).
    Placeholder { value: String },
}

impl fmt::Display for DataLoss {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let target = match &self.column {
            Some(column) => format!("{}.{column}", self.table),
            None => self.table.clone(),
        };
        let rows = self.rows;
        match &self.reason {
            LossReason::DropTable => {
                write!(f, "{target}: the table is dropped with its {rows} row(s)")
            }
            LossReason::DropColumn => {
                write!(
                    f,
                    "{target}: the column is dropped with its {rows} value(s)"
                )
            }
            LossReason::Unconvertible { from, to } => write!(
                f,
                "{target}: changes from {from} to {to}; {rows} value(s) cannot be converted and are lost"
            ),
            LossReason::Rounded { scale } => write!(
                f,
                "{target}: {rows} value(s) are rounded to {scale} decimal place(s)"
            ),
            LossReason::Placeholder { value } => write!(
                f,
                "{target}: is required and has no default; {rows} row(s) have no value and would get {value}"
            ),
        }
    }
}
