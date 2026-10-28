//! Entity schema migrations: diff the stored [`StoredSchema`] against the one the app model
//! implies, and bring the database to it in one transaction.
//!
//! Additive steps (new tables, new nullable columns or columns with a default, new indexes) run
//! with `CREATE`/`ALTER TABLE ADD COLUMN`. Anything SQLite cannot alter in place (types,
//! nullability, defaults, keys, CHECKs, dropped columns) rebuilds the table with SQLite's
//! documented procedure (<https://sqlite.org/lang_altertable.html#otheralter>): with foreign keys
//! off, create the new table, copy the rows, drop the old table, rename, re-create the indexes,
//! then `PRAGMA foreign_key_check`.
//!
//! A step that would lose data (a dropped table or column holding values, values that cannot be
//! converted to a new type or are rounded to a smaller scale, required columns without a default
//! that rows have no value for) is refused, listing exactly what would be lost, unless forced.
//! Losses are counted on the actual rows, inside the migration's transaction, which is rolled
//! back when they are refused. A rebuild that loses nothing applies on its own, like the
//! `ALTER COLUMN`s Fabric applies without force; rows that break the new constraints fail it.

use std::fmt;

use rl_model::AppModel;
use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params_from_iter};
use serde_json::Value as Json;

use crate::Db;
use crate::codec::{self, UUID_V4_SQL};
use crate::error::{DataLoss, LossReason, SchemaError};
use crate::schema::{
    self, ColumnSchema, FORMAT, IndexSchema, StoredKind, StoredSchema, TableSchema, literal_sql,
    quote_ident,
};

/// The `schema` component's tables (see [`Db::migrate_component`]).
pub const SCHEMA_STEPS: &[&str] = &["CREATE TABLE _rl_schema (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    schema TEXT NOT NULL,
    applied_at TEXT NOT NULL
)"];

/// The ordered steps from one schema to another.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MigrationPlan {
    pub steps: Vec<MigrationStep>,
}

impl MigrationPlan {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Whether some step may lose data (decided on the rows when applied).
    pub fn is_destructive(&self) -> bool {
        self.steps
            .iter()
            .any(|step| step.class() == StepClass::Destructive)
    }
}

/// One migration step.
#[derive(Clone, Debug, PartialEq)]
pub enum MigrationStep {
    CreateTable(TableSchema),
    /// Adds `column` of `table` (the new table) with `ALTER TABLE ADD COLUMN`.
    AddColumn {
        table: TableSchema,
        column: String,
    },
    CreateIndex {
        table: String,
        index: IndexSchema,
    },
    DropIndex {
        table: String,
        index: String,
    },
    /// Re-creates the table as `to`, copying the rows of `from`.
    RebuildTable {
        from: TableSchema,
        to: TableSchema,
    },
    DropTable(TableSchema),
}

/// How a step treats existing data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepClass {
    /// Keeps every row and value as it is.
    Additive,
    /// Rebuilds a table, keeping every value (rows that break new constraints fail it).
    Rebuild,
    /// May drop or replace values, depending on the rows: forced only.
    Destructive,
}

impl MigrationStep {
    pub fn class(&self) -> StepClass {
        match self {
            Self::CreateTable(_)
            | Self::AddColumn { .. }
            | Self::CreateIndex { .. }
            | Self::DropIndex { .. } => StepClass::Additive,
            Self::DropTable(_) => StepClass::Destructive,
            Self::RebuildTable { from, to } => {
                if rebuild_may_lose_data(from, to) {
                    StepClass::Destructive
                } else {
                    StepClass::Rebuild
                }
            }
        }
    }
}

impl fmt::Display for MigrationStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CreateTable(table) => write!(f, "create table `{}`", table.name),
            Self::AddColumn { table, column } => {
                write!(f, "add column `{}.{column}`", table.name)
            }
            Self::CreateIndex { table, index } => {
                write!(f, "create index `{}` on `{table}`", index.name)
            }
            Self::DropIndex { table, index } => write!(f, "drop index `{index}` on `{table}`"),
            Self::RebuildTable { from, to } => {
                let mut changes = Vec::new();
                for column in &to.columns {
                    match from.column(&column.name) {
                        None => changes.push(format!("add {}", column.name)),
                        Some(old) if old != column => {
                            changes.push(format!("change {}", column.name))
                        }
                        Some(_) => {}
                    }
                }
                for column in &from.columns {
                    if to.column(&column.name).is_none() {
                        changes.push(format!("drop {}", column.name));
                    }
                }
                if changes.is_empty() {
                    changes.push("constraints".into());
                }
                write!(f, "rebuild table `{}` ({})", to.name, changes.join(", "))
            }
            Self::DropTable(table) => write!(f, "drop table `{}`", table.name),
        }
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MigrationReport {
    /// The steps applied, in order; empty when the database was up to date.
    pub steps: Vec<MigrationStep>,
    /// The data dropped or replaced because the migration was forced.
    pub forced: Vec<DataLoss>,
    /// Findings about the model worth logging ([`schema::model_warnings`]).
    pub warnings: Vec<String>,
}

/// The steps from `current` (`None`: an empty database) to the schema of `model`.
pub fn plan(
    current: Option<&StoredSchema>,
    model: &AppModel,
) -> Result<MigrationPlan, SchemaError> {
    let target = StoredSchema::from_model(model)?;
    Ok(plan_between(current, &target))
}

/// Brings the database's entity tables to `model`'s schema, in one transaction. With `force`,
/// steps that lose data run too ([`MigrationReport::forced`] lists what was lost); without it,
/// they fail with [`SchemaError::DataLoss`] and nothing changes. Applying the same model again
/// does nothing.
pub async fn apply(db: &Db, model: &AppModel, force: bool) -> Result<MigrationReport, SchemaError> {
    let target = StoredSchema::from_model(model)?;
    let warnings = schema::model_warnings(model);
    db.migrate_component("schema", SCHEMA_STEPS).await?;
    let mut report = db
        .call(move |conn| apply_to(conn, &target, force))
        .await??;
    report.warnings = warnings;
    Ok(report)
}

/// The schema last applied, `None` before the first migration.
pub async fn stored_schema(db: &Db) -> Result<Option<StoredSchema>, SchemaError> {
    db.migrate_component("schema", SCHEMA_STEPS).await?;
    db.call(|conn| read_stored(conn)).await?
}

fn apply_to(
    conn: &mut Connection,
    target: &StoredSchema,
    force: bool,
) -> Result<MigrationReport, SchemaError> {
    let current = read_stored(conn)?;
    let plan = plan_between(current.as_ref(), target);
    if plan.is_empty() {
        if current.as_ref() != Some(target) {
            save(conn, target)?;
        }
        return Ok(MigrationReport::default());
    }
    // The pragma is a no-op inside a transaction, so it goes off before the transaction starts
    // and back on after it ends, whatever happened.
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = run(conn, &plan, target, force);
    let restored = conn.pragma_update(None, "foreign_keys", true);
    let report = result?;
    restored?;
    Ok(report)
}

fn run(
    conn: &mut Connection,
    plan: &MigrationPlan,
    target: &StoredSchema,
    force: bool,
) -> Result<MigrationReport, SchemaError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut losses = Vec::new();
    for step in &plan.steps {
        execute(&tx, step, &mut losses)?;
    }
    if !losses.is_empty() && !force {
        return Err(SchemaError::DataLoss { losses });
    }
    check_foreign_keys(&tx, target)?;
    save(&tx, target)?;
    tx.commit()?;
    Ok(MigrationReport {
        steps: plan.steps.clone(),
        forced: losses,
        warnings: Vec::new(),
    })
}

fn read_stored(conn: &Connection) -> Result<Option<StoredSchema>, SchemaError> {
    let Some(text) = conn
        .query_row("SELECT schema FROM _rl_schema WHERE id = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
    else {
        return Ok(None);
    };
    let format = serde_json::from_str::<Json>(&text)
        .ok()
        .and_then(|json| json.get("format")?.as_u64());
    if format.is_some_and(|format| format > u64::from(FORMAT)) {
        return Err(SchemaError::StoredSchema {
            reason: "it was written by a newer rayfin-local".into(),
        });
    }
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| SchemaError::StoredSchema {
            reason: error.to_string(),
        })
}

fn save(conn: &Connection, schema: &StoredSchema) -> Result<(), SchemaError> {
    let json = serde_json::to_string(schema).expect("the schema serializes to JSON");
    conn.execute(
        "INSERT INTO _rl_schema (id, schema, applied_at)
         VALUES (1, ?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT (id) DO UPDATE SET schema = excluded.schema, applied_at = excluded.applied_at",
        [json],
    )?;
    Ok(())
}

fn plan_between(current: Option<&StoredSchema>, target: &StoredSchema) -> MigrationPlan {
    let mut steps = Vec::new();
    for table in &target.tables {
        match current.and_then(|current| current.table(&table.name)) {
            None => steps.push(MigrationStep::CreateTable(table.clone())),
            Some(old) if same_structure(old, table) => {}
            Some(old) => match additive_steps(old, table) {
                Some(additive) => steps.extend(additive),
                None => steps.push(MigrationStep::RebuildTable {
                    from: old.clone(),
                    to: table.clone(),
                }),
            },
        }
    }
    for old in current
        .map(|current| current.tables.as_slice())
        .unwrap_or_default()
    {
        if target.table(&old.name).is_none() {
            steps.push(MigrationStep::DropTable(old.clone()));
        }
    }
    MigrationPlan { steps }
}

fn same_set<T: PartialEq>(a: &[T], b: &[T]) -> bool {
    a.len() == b.len() && a.iter().all(|item| b.contains(item))
}

/// Equal tables up to column order (and the entity name).
fn same_structure(a: &TableSchema, b: &TableSchema) -> bool {
    a.name == b.name
        && same_set(&a.columns, &b.columns)
        && a.primary_key == b.primary_key
        && same_set(&a.foreign_keys, &b.foreign_keys)
        && same_set(&a.checks, &b.checks)
        && same_set(&a.indexes, &b.indexes)
}

/// The in-place steps from `old` to `new`, `None` when the table needs a rebuild.
fn additive_steps(old: &TableSchema, new: &TableSchema) -> Option<Vec<MigrationStep>> {
    if old.name != new.name || old.primary_key != new.primary_key {
        return None;
    }
    if !old
        .columns
        .iter()
        .all(|column| new.columns.contains(column))
    {
        return None;
    }
    let added: Vec<&ColumnSchema> = new
        .columns
        .iter()
        .filter(|column| old.column(&column.name).is_none())
        .collect();
    let is_added = |name: &str| added.iter().any(|column| column.name == name);
    for column in &added {
        let has_fk = new.single_column_fk(&column.name).is_some();
        let expression_default = column
            .default
            .as_deref()
            .is_some_and(|d| d.starts_with('('));
        if (!column.nullable && column.default.is_none())
            || expression_default
            || (has_fk && column.default.is_some())
        {
            return None;
        }
    }
    if !old.checks.iter().all(|check| new.checks.contains(check))
        || !new
            .checks
            .iter()
            .all(|check| old.checks.contains(check) || is_added(&check.column))
        || !old
            .foreign_keys
            .iter()
            .all(|fk| new.foreign_keys.contains(fk))
        || !new.foreign_keys.iter().all(|fk| {
            old.foreign_keys.contains(fk) || (fk.columns.len() == 1 && is_added(&fk.columns[0]))
        })
    {
        return None;
    }
    let mut steps = Vec::new();
    for index in old.indexes.iter().filter(|i| !new.indexes.contains(i)) {
        steps.push(MigrationStep::DropIndex {
            table: old.name.clone(),
            index: index.name.clone(),
        });
    }
    for column in added {
        steps.push(MigrationStep::AddColumn {
            table: new.clone(),
            column: column.name.clone(),
        });
    }
    for index in new.indexes.iter().filter(|i| !old.indexes.contains(i)) {
        steps.push(MigrationStep::CreateIndex {
            table: new.name.clone(),
            index: index.clone(),
        });
    }
    Some(steps)
}

fn rebuild_may_lose_data(from: &TableSchema, to: &TableSchema) -> bool {
    let dropped = from.columns.iter().any(|c| to.column(&c.name).is_none());
    dropped
        || to
            .columns
            .iter()
            .any(|column| match from.column(&column.name) {
                None => !column.nullable && column.default.is_none(),
                Some(old) => {
                    !lossless(old.kind, column.kind)
                        || (old.nullable && !column.nullable && column.default.is_none())
                }
            })
}

/// Whether every value of `from` converts to `to` unchanged.
fn lossless(from: StoredKind, to: StoredKind) -> bool {
    match (from, to) {
        _ if from == to => true,
        (_, StoredKind::Text) => true,
        (StoredKind::Int | StoredKind::Boolean, StoredKind::Decimal { .. }) => true,
        (StoredKind::Boolean, StoredKind::Int) => true,
        (StoredKind::Decimal { scale: a, .. }, StoredKind::Decimal { scale: b, .. }) => b >= a,
        _ => false,
    }
}

fn execute(
    conn: &Connection,
    step: &MigrationStep,
    losses: &mut Vec<DataLoss>,
) -> Result<(), SchemaError> {
    match step {
        MigrationStep::CreateTable(table) => {
            ensure_untracked_absent(conn, &table.name)?;
            conn.execute_batch(&table.create_sql())?;
            for index in table.index_sql() {
                conn.execute_batch(&index)?;
            }
        }
        MigrationStep::AddColumn { table, column } => {
            let column = table.column(column).expect("added columns are columns");
            conn.execute_batch(&table.add_column_sql(column))
                .map_err(|error| rebuild_error(&table.name, error))?;
        }
        MigrationStep::CreateIndex { table, index } => {
            conn.execute_batch(&schema::index_sql(table, index))
                .map_err(|error| rebuild_error(table, error))?;
        }
        MigrationStep::DropIndex { index, .. } => {
            conn.execute_batch(&format!("DROP INDEX IF EXISTS {}", quote_ident(index)))?;
        }
        MigrationStep::RebuildTable { from, to } => rebuild(conn, from, to, losses)?,
        MigrationStep::DropTable(table) => {
            let rows = conn
                .query_row(
                    &format!("SELECT count(*) FROM {}", quote_ident(&table.name)),
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .ok()
                .flatten()
                .unwrap_or(0) as u64;
            if rows > 0 {
                losses.push(DataLoss {
                    table: table.name.clone(),
                    column: None,
                    rows,
                    reason: LossReason::DropTable,
                });
            }
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS {}",
                quote_ident(&table.name)
            ))?;
        }
    }
    Ok(())
}

fn ensure_untracked_absent(conn: &Connection, name: &str) -> Result<(), SchemaError> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_schema WHERE name = ?1 COLLATE NOCASE)",
        [name],
        |row| row.get(0),
    )?;
    if exists {
        return Err(SchemaError::Untracked {
            name: name.to_owned(),
        });
    }
    Ok(())
}

fn rebuild_error(table: &str, error: rusqlite::Error) -> SchemaError {
    match codec::constraint_violation(&error) {
        Some(violation) => SchemaError::Rebuild {
            table: table.to_owned(),
            reason: violation.message,
        },
        None => SchemaError::Sqlite(error),
    }
}

/// How a rebuilt column gets its values.
enum Source<'a> {
    /// The old column at this position of the SELECT.
    Old(usize, &'a ColumnSchema),
    New,
}

#[derive(Default)]
struct Counts {
    unconvertible: u64,
    rounded: u64,
    placeholders: u64,
}

/// SQLite's table rebuild, with foreign keys off (the caller's job) and inside the transaction.
fn rebuild(
    conn: &Connection,
    from: &TableSchema,
    to: &TableSchema,
    losses: &mut Vec<DataLoss>,
) -> Result<(), SchemaError> {
    let temp = format!("_rl_new_{}", to.name);
    conn.execute_batch(&to.create_sql_named(&temp))?;

    let sources: Vec<Source<'_>> = to
        .columns
        .iter()
        .map(|column| {
            match from
                .columns
                .iter()
                .position(|old| old.name.eq_ignore_ascii_case(&column.name))
            {
                Some(index) => Source::Old(index, &from.columns[index]),
                None => Source::New,
            }
        })
        .collect();
    let fills: Vec<Fill> = to
        .columns
        .iter()
        .map(|column| Fill::of(conn, column))
        .collect::<Result<_, _>>()?;
    let select = format!(
        "SELECT {} FROM {} ORDER BY rowid",
        from.columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", "),
        quote_ident(&from.name)
    );
    let insert = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote_ident(&temp),
        to.columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Vec<_>>()
            .join(", "),
        vec!["?"; to.columns.len()].join(", ")
    );
    let mut dropped = vec![0u64; from.columns.len()];
    let kept: Vec<bool> = from
        .columns
        .iter()
        .map(|old| to.column(&old.name).is_some())
        .collect();
    let mut counts: Vec<Counts> = to.columns.iter().map(|_| Counts::default()).collect();
    {
        let mut read = conn.prepare(&select)?;
        let mut write = conn.prepare(&insert)?;
        let mut rows = read.query([])?;
        while let Some(row) = rows.next()? {
            for (index, kept) in kept.iter().enumerate() {
                if !kept && !matches!(row.get_ref(index)?, ValueRef::Null) {
                    dropped[index] += 1;
                }
            }
            let mut values = Vec::with_capacity(to.columns.len());
            for (position, column) in to.columns.iter().enumerate() {
                let count = &mut counts[position];
                let value = match &sources[position] {
                    Source::Old(index, old) => {
                        match convert(old.kind, column.kind, row.get_ref(*index)?) {
                            Converted::Value(value) => value,
                            Converted::Rounded(value) => {
                                count.rounded += 1;
                                value
                            }
                            Converted::Lost => {
                                count.unconvertible += 1;
                                SqlValue::Null
                            }
                        }
                    }
                    Source::New => SqlValue::Null,
                };
                let value = match value {
                    SqlValue::Null => fills[position].value(column, count),
                    value => value,
                };
                values.push(value);
            }
            write
                .execute(params_from_iter(values))
                .map_err(|error| rebuild_error(&to.name, error))?;
        }
    }
    for (index, old) in from.columns.iter().enumerate() {
        if dropped[index] > 0 {
            losses.push(DataLoss {
                table: from.name.clone(),
                column: Some(old.name.clone()),
                rows: dropped[index],
                reason: LossReason::DropColumn,
            });
        }
    }
    for (position, column) in to.columns.iter().enumerate() {
        let count = &counts[position];
        let loss = |rows: u64, reason: LossReason| DataLoss {
            table: to.name.clone(),
            column: Some(column.name.clone()),
            rows,
            reason,
        };
        if let Source::Old(_, old) = &sources[position] {
            if count.unconvertible > 0 {
                losses.push(loss(
                    count.unconvertible,
                    LossReason::Unconvertible {
                        from: old.kind.describe(),
                        to: column.kind.describe(),
                    },
                ));
            }
            if let (StoredKind::Decimal { scale, .. }, true) = (column.kind, count.rounded > 0) {
                losses.push(loss(count.rounded, LossReason::Rounded { scale }));
            }
        }
        if count.placeholders > 0 {
            losses.push(loss(
                count.placeholders,
                LossReason::Placeholder {
                    value: literal_sql(&placeholder(column.kind)),
                },
            ));
        }
    }

    conn.execute_batch(&format!("DROP TABLE {}", quote_ident(&from.name)))?;
    conn.execute_batch(&format!(
        "ALTER TABLE {} RENAME TO {}",
        quote_ident(&temp),
        quote_ident(&to.name)
    ))?;
    for index in to.index_sql() {
        conn.execute_batch(&index)
            .map_err(|error| rebuild_error(&to.name, error))?;
    }
    Ok(())
}

/// The value of a rebuilt row's column that has none.
enum Fill {
    Value(SqlValue),
    Uuid,
    Null,
    Placeholder,
}

impl Fill {
    fn of(conn: &Connection, column: &ColumnSchema) -> Result<Self, SchemaError> {
        Ok(match &column.default {
            Some(default) if default == UUID_V4_SQL => Self::Uuid,
            Some(default) => {
                Self::Value(conn.query_row(&format!("SELECT {default}"), [], |row| {
                    row.get::<_, SqlValue>(0)
                })?)
            }
            None if column.nullable => Self::Null,
            None => Self::Placeholder,
        })
    }

    fn value(&self, column: &ColumnSchema, counts: &mut Counts) -> SqlValue {
        match self {
            Self::Value(value) => value.clone(),
            Self::Uuid => SqlValue::Text(codec::new_uuid()),
            Self::Null => SqlValue::Null,
            Self::Placeholder => {
                counts.placeholders += 1;
                placeholder(column.kind)
            }
        }
    }
}

/// What EF Core fills a new required column with on Fabric: the CLR type's default.
fn placeholder(kind: StoredKind) -> SqlValue {
    match kind {
        StoredKind::Uuid => SqlValue::Text("00000000-0000-0000-0000-000000000000".into()),
        StoredKind::Text => SqlValue::Text(String::new()),
        StoredKind::Int | StoredKind::Boolean => SqlValue::Integer(0),
        StoredKind::Decimal { .. } => SqlValue::Real(0.0),
        StoredKind::Date => SqlValue::Text("0001-01-01T00:00:00.0000000Z".into()),
    }
}

enum Converted {
    Value(SqlValue),
    Rounded(SqlValue),
    Lost,
}

fn owned(value: ValueRef<'_>) -> SqlValue {
    match value {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(n) => SqlValue::Integer(n),
        ValueRef::Real(n) => SqlValue::Real(n),
        ValueRef::Text(text) => SqlValue::Text(String::from_utf8_lossy(text).into_owned()),
        ValueRef::Blob(bytes) => SqlValue::Blob(bytes.to_vec()),
    }
}

/// A stored value of `from` as a value of `to`, as SQL Server converts it.
fn convert(from: StoredKind, to: StoredKind, value: ValueRef<'_>) -> Converted {
    if matches!(value, ValueRef::Null) || from == to {
        return Converted::Value(owned(value));
    }
    let Ok(json) = codec::decode(&from.field_kind(), value) else {
        return Converted::Lost;
    };
    let target = to.field_kind();
    let converted = match (&json, to) {
        (Json::String(text), StoredKind::Text) => Ok(SqlValue::Text(text.clone())),
        (Json::Number(number), StoredKind::Text) => Ok(SqlValue::Text(number.to_string())),
        (Json::Bool(flag), StoredKind::Text) => Ok(SqlValue::Text(flag.to_string())),
        (Json::String(text), _) => codec::parse_text(&target, text),
        (Json::Bool(flag), _) => codec::encode(&target, &Json::from(i64::from(*flag))),
        (Json::Number(number), StoredKind::Boolean) => match number.as_f64() {
            Some(n) if n == 0.0 || n == 1.0 => Ok(SqlValue::Integer(n as i64)),
            _ => return Converted::Lost,
        },
        (Json::Number(number), StoredKind::Int) => match number.as_f64() {
            Some(n) if n.fract() == 0.0 && n.abs() <= f64::from(i32::MAX) => {
                Ok(SqlValue::Integer(n as i64))
            }
            _ => return Converted::Lost,
        },
        _ => codec::encode(&target, &json),
    };
    match converted {
        Ok(SqlValue::Real(new)) if json.as_f64().is_some_and(|old| old != new) => {
            Converted::Rounded(SqlValue::Real(new))
        }
        Ok(value) => Converted::Value(value),
        Err(_) => Converted::Lost,
    }
}

fn check_foreign_keys(conn: &Connection, target: &StoredSchema) -> Result<(), SchemaError> {
    for table in target.tables.iter().filter(|t| !t.foreign_keys.is_empty()) {
        let mut statement = conn
            .prepare("SELECT parent, count(*) FROM pragma_foreign_key_check(?1) GROUP BY parent")?;
        let violation = statement
            .query_row([&table.name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })
            .optional()?;
        if let Some((parent, rows)) = violation {
            return Err(SchemaError::ForeignKeyViolation {
                table: table.name.clone(),
                parent,
                rows,
            });
        }
    }
    Ok(())
}
