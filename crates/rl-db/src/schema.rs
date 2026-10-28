//! The entity schema: [`StoredSchema`], the normalized description of the app's tables that the
//! app model implies (what [`crate::migrate`] stores and diffs), and its SQLite DDL.
//!
//! Each entity is one table named after the entity's table (`P`), with the columns of the DAB
//! `x-schema` in order, stored as [`crate::codec`] describes, and the constraints Fabric's
//! database has: `NOT NULL`, the decorator defaults (the `id` key gets a random uuid, as
//! `NEWID()` gives it), the primary key, unique indexes, non-cascading foreign keys (`ON DELETE
//! NO ACTION ON UPDATE NO ACTION`; none for the system `User`, which has no table here) with an
//! index per foreign key (as EF Core adds), the `@set`, minimum-length, email and (under
//! `cli-minor-fixes`) range CHECKs of the IR, translated from their typed form, plus local
//! CHECKs that stand in for the SQL Server types (lengths, 32-bit ints, decimal precision, the
//! uuid, date and boolean formats).

use rl_model::{AppModel, CheckConstraint, Column, Entity, FieldKind};
use rusqlite::types::Value as SqlValue;
use serde::{Deserialize, Serialize};

use crate::codec::{self, Storage};
use crate::error::SchemaError;

/// The version of the [`StoredSchema`] format.
pub const FORMAT: u32 = 1;

/// The tables of an app, as stored in `_rl_schema`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredSchema {
    pub format: u32,
    pub tables: Vec<TableSchema>,
}

/// One entity table.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TableSchema {
    pub name: String,
    /// The entity, for messages.
    pub entity: String,
    pub columns: Vec<ColumnSchema>,
    pub primary_key: KeySchema,
    pub foreign_keys: Vec<ForeignKeySchema>,
    pub checks: Vec<CheckSchema>,
    /// Unique indexes (the unique constraints), then the foreign key indexes.
    pub indexes: Vec<IndexSchema>,
}

/// A column.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColumnSchema {
    pub name: String,
    pub kind: StoredKind,
    /// `NOCASE` for text compared case-insensitively.
    pub collation: Option<String>,
    pub nullable: bool,
    /// The `DEFAULT` expression.
    pub default: Option<String>,
}

/// What a column stores (the parts of a [`FieldKind`] that shape the data).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum StoredKind {
    Uuid,
    /// `@text`, `@email` and `@set`.
    Text,
    Int,
    Decimal {
        precision: u32,
        scale: u32,
    },
    Boolean,
    Date,
}

impl StoredKind {
    /// The storage of `kind`.
    pub fn of(kind: &FieldKind) -> Self {
        match kind {
            FieldKind::Uuid => Self::Uuid,
            FieldKind::Text { .. } | FieldKind::Email { .. } | FieldKind::Set { .. } => Self::Text,
            FieldKind::Int { .. } => Self::Int,
            FieldKind::Decimal {
                precision, scale, ..
            } => Self::Decimal {
                precision: *precision,
                scale: *scale,
            },
            FieldKind::Boolean => Self::Boolean,
            FieldKind::Date => Self::Date,
        }
    }

    /// A field kind stored this way (for the codecs).
    pub fn field_kind(self) -> FieldKind {
        match self {
            Self::Uuid => FieldKind::Uuid,
            Self::Text => FieldKind::Text {
                min_length: None,
                max_length: None,
                pattern: None,
            },
            Self::Int => FieldKind::Int {
                min: None,
                max: None,
            },
            Self::Decimal { precision, scale } => FieldKind::Decimal {
                precision,
                scale,
                min: None,
                max: None,
            },
            Self::Boolean => FieldKind::Boolean,
            Self::Date => FieldKind::Date,
        }
    }

    pub fn storage(self) -> Storage {
        codec::storage(&self.field_kind())
    }

    /// The name used in messages.
    pub fn describe(self) -> String {
        match self {
            Self::Uuid => "uuid".into(),
            Self::Text => "text".into(),
            Self::Int => "int".into(),
            Self::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
            Self::Boolean => "boolean".into(),
            Self::Date => "date".into(),
        }
    }
}

/// A named key over columns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeySchema {
    pub name: String,
    pub columns: Vec<String>,
}

/// An enforced foreign key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForeignKeySchema {
    pub name: String,
    pub columns: Vec<String>,
    pub referenced_table: String,
    pub referenced_columns: Vec<String>,
}

/// A CHECK constraint on one column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckSchema {
    pub name: String,
    pub column: String,
    /// SQLite SQL.
    pub expression: String,
}

/// An index.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSchema {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

impl StoredSchema {
    /// The tables `model` needs. Fails on reserved or clashing names and invalid defaults.
    pub fn from_model(model: &AppModel) -> Result<Self, SchemaError> {
        let tables = model
            .entities
            .iter()
            .map(table_schema)
            .collect::<Result<Vec<_>, _>>()?;
        check_names(&tables)?;
        Ok(Self {
            format: FORMAT,
            tables,
        })
    }

    /// The table named `name` (case-insensitive, as SQLite names are).
    pub fn table(&self, name: &str) -> Option<&TableSchema> {
        self.tables
            .iter()
            .find(|table| table.name.eq_ignore_ascii_case(name))
    }
}

impl TableSchema {
    /// The column named `name` (case-insensitive).
    pub fn column(&self, name: &str) -> Option<&ColumnSchema> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
    }

    /// `CREATE TABLE` for this table.
    pub fn create_sql(&self) -> String {
        self.create_sql_named(&self.name)
    }

    /// `CREATE TABLE` under another name (a table rebuild's new copy); foreign keys still
    /// reference the final names.
    pub(crate) fn create_sql_named(&self, name: &str) -> String {
        let mut lines: Vec<String> = self
            .columns
            .iter()
            .map(|column| format!("    {}", column_definition(column)))
            .collect();
        lines.push(format!(
            "    CONSTRAINT {} PRIMARY KEY ({})",
            quote_ident(&self.primary_key.name),
            ident_list(&self.primary_key.columns)
        ));
        for fk in &self.foreign_keys {
            lines.push(format!(
                "    CONSTRAINT {} FOREIGN KEY ({}) {}",
                quote_ident(&fk.name),
                ident_list(&fk.columns),
                references(fk)
            ));
        }
        for check in &self.checks {
            lines.push(format!(
                "    CONSTRAINT {} CHECK ({})",
                quote_ident(&check.name),
                check.expression
            ));
        }
        format!(
            "CREATE TABLE {} (\n{}\n)",
            quote_ident(name),
            lines.join(",\n")
        )
    }

    /// `CREATE INDEX` statements for this table's indexes.
    pub fn index_sql(&self) -> Vec<String> {
        self.indexes
            .iter()
            .map(|index| index_sql(&self.name, index))
            .collect()
    }

    /// The column definition for `ALTER TABLE ADD COLUMN`, with its checks and foreign key.
    pub(crate) fn add_column_sql(&self, column: &ColumnSchema) -> String {
        let mut sql = column_definition(column);
        for check in self.checks.iter().filter(|c| c.column == column.name) {
            sql.push_str(&format!(
                " CONSTRAINT {} CHECK ({})",
                quote_ident(&check.name),
                check.expression
            ));
        }
        if let Some(fk) = self.single_column_fk(&column.name) {
            sql.push_str(&format!(
                " CONSTRAINT {} {}",
                quote_ident(&fk.name),
                references(fk)
            ));
        }
        format!("ALTER TABLE {} ADD COLUMN {sql}", quote_ident(&self.name))
    }

    pub(crate) fn single_column_fk(&self, column: &str) -> Option<&ForeignKeySchema> {
        self.foreign_keys
            .iter()
            .find(|fk| fk.columns.len() == 1 && fk.columns[0] == column)
    }
}

pub(crate) fn index_sql(table: &str, index: &IndexSchema) -> String {
    format!(
        "CREATE {}INDEX {} ON {} ({})",
        if index.unique { "UNIQUE " } else { "" },
        quote_ident(&index.name),
        quote_ident(table),
        ident_list(&index.columns)
    )
}

fn column_definition(column: &ColumnSchema) -> String {
    let mut sql = format!(
        "{} {}",
        quote_ident(&column.name),
        column.kind.storage().sql_type()
    );
    if let Some(collation) = &column.collation {
        sql.push_str(&format!(" COLLATE {collation}"));
    }
    if !column.nullable {
        sql.push_str(" NOT NULL");
    }
    if let Some(default) = &column.default {
        sql.push_str(&format!(" DEFAULT {default}"));
    }
    sql
}

fn references(fk: &ForeignKeySchema) -> String {
    format!(
        "REFERENCES {} ({}) ON DELETE NO ACTION ON UPDATE NO ACTION",
        quote_ident(&fk.referenced_table),
        ident_list(&fk.referenced_columns)
    )
}

/// The table of `entity`.
pub fn table_schema(entity: &Entity) -> Result<TableSchema, SchemaError> {
    let case_insensitive = !entity
        .primary_key_column()
        .db_type
        .eq_ignore_ascii_case("UUID");
    let mut columns = Vec::with_capacity(entity.columns.len());
    let mut checks = Vec::new();
    for column in &entity.columns {
        columns.push(ColumnSchema {
            name: column.name.clone(),
            kind: StoredKind::of(&column.kind),
            collation: (case_insensitive && codec::is_textual(&column.kind))
                .then(|| "NOCASE".to_owned()),
            nullable: column.nullable && !column.primary_key,
            default: default_sql(entity, column)?,
        });
        checks.extend(column_checks(entity, column, case_insensitive));
    }
    let foreign_keys: Vec<ForeignKeySchema> = entity
        .foreign_keys
        .iter()
        .filter(|fk| fk.referenced_entity.is_some())
        .map(|fk| ForeignKeySchema {
            name: fk.name.clone(),
            columns: fk.columns.clone(),
            referenced_table: fk.referenced_table.clone(),
            referenced_columns: fk.referenced_columns.clone(),
        })
        .collect();
    let mut indexes: Vec<IndexSchema> = entity
        .unique_constraints
        .iter()
        .map(|key| IndexSchema {
            name: key.name.clone(),
            columns: key.columns.clone(),
            unique: true,
        })
        .collect();
    // EF Core indexes every foreign key unless an index already starts with its columns.
    for fk in &entity.foreign_keys {
        let covered = indexes
            .iter()
            .any(|index| index.columns.starts_with(&fk.columns))
            || entity.primary_key.columns.starts_with(&fk.columns);
        if !covered {
            indexes.push(IndexSchema {
                name: format!("IX_{}_{}", entity.table, fk.columns.join("_")),
                columns: fk.columns.clone(),
                unique: false,
            });
        }
    }
    Ok(TableSchema {
        name: entity.table.clone(),
        entity: entity.name.clone(),
        columns,
        primary_key: KeySchema {
            name: entity.primary_key.name.clone(),
            columns: entity.primary_key.columns.clone(),
        },
        foreign_keys,
        checks,
        indexes,
    })
}

/// `CREATE TABLE` and `CREATE INDEX` statements for `entity` (`;`-terminated).
pub fn create_table_sql(entity: &Entity) -> Result<String, SchemaError> {
    let table = table_schema(entity)?;
    let mut sql = format!("{};\n", table.create_sql());
    for index in table.index_sql() {
        sql.push_str(&format!("{index};\n"));
    }
    Ok(sql)
}

/// The whole schema of `model` (`rayfin-local schema`).
pub fn schema_sql(model: &AppModel) -> Result<String, SchemaError> {
    let schema = StoredSchema::from_model(model)?;
    let mut sql = String::from(
        "-- The entity tables rayfin-local keeps in SQLite for this app (generated from \
         rayfin/data).\n",
    );
    for (entity, table) in model.entities.iter().zip(&schema.tables) {
        sql.push_str(&format!(
            "\n-- Entity `{}`\n{};\n",
            entity.name,
            table.create_sql()
        ));
        for index in table.index_sql() {
            sql.push_str(&format!("{index};\n"));
        }
        for check in unenforced_checks(entity) {
            sql.push_str(&format!(
                "-- Not enforced locally: {} CHECK ({})\n",
                check.name, check.expression
            ));
        }
    }
    Ok(sql)
}

/// Things about `model` the developer should know: CHECK constraints rayfin-local cannot
/// translate, and `@set` defaults the database rejects (rayfin-core removes quotes from the
/// values it allows, so `default: "it's new"` fails on insert, on Fabric too).
pub fn model_warnings(model: &AppModel) -> Vec<String> {
    let mut warnings = Vec::new();
    for entity in &model.entities {
        for check in unenforced_checks(entity) {
            warnings.push(format!(
                "entity `{}`: the CHECK constraint {} ({}) is not enforced locally",
                entity.name, check.name, check.expression
            ));
        }
        for column in &entity.columns {
            let (FieldKind::Set { allowed, .. }, Some(serde_json::Value::String(default))) =
                (&column.kind, &column.default)
            else {
                continue;
            };
            if !allowed
                .iter()
                .any(|value| value.eq_ignore_ascii_case(default))
            {
                warnings.push(format!(
                    "entity `{}`: the default of `{}` ({default:?}) is not one of the values the \
                     database accepts ({}), so creating a `{}` without it fails, as on Fabric",
                    entity.name,
                    column.field,
                    allowed.join(", "),
                    entity.name
                ));
            }
        }
    }
    warnings
}

/// The IR's CHECK constraints that [`table_schema`] does not translate.
fn unenforced_checks(entity: &Entity) -> Vec<&CheckConstraint> {
    entity
        .check_constraints
        .iter()
        .filter(|check| {
            !entity.columns.iter().any(|column| {
                ["", "_minlen", "_email", "_range"]
                    .iter()
                    .any(|suffix| check.name == ir_check_name(entity, column, suffix))
            })
        })
        .collect()
}

fn ir_check_name(entity: &Entity, column: &Column, suffix: &str) -> String {
    format!("CK_{}_{}{suffix}", entity.table, column.name)
}

/// The CHECKs of one column: the IR's (by its typed rules) and the local type checks.
fn column_checks(entity: &Entity, column: &Column, case_insensitive: bool) -> Vec<CheckSchema> {
    let q = quote_ident(&column.name);
    let mut checks = Vec::new();
    let mut push = |suffix: &str, expression: String| {
        checks.push(CheckSchema {
            name: ir_check_name(entity, column, suffix),
            column: column.name.clone(),
            expression,
        });
    };
    let ir_check = |suffix: &str| {
        let name = ir_check_name(entity, column, suffix);
        entity
            .check_constraints
            .iter()
            .find(|check| check.name == name)
    };
    // SQL Server's LEN ignores trailing spaces; PostgreSQL's LENGTH does not.
    let min_length = |n: u32| {
        if case_insensitive {
            format!("length(rtrim({q}, ' ')) >= {n}")
        } else {
            format!("length({q}) >= {n}")
        }
    };
    let max_length = |n: u32| format!("length({q}) <= {n}");
    match &column.kind {
        FieldKind::Uuid => push(
            "_type",
            format!("{q} IS NULL OR {q} GLOB '{}'", uuid_glob()),
        ),
        FieldKind::Text {
            min_length: min,
            max_length: max,
            ..
        } => {
            if let Some(max) = max {
                push("_maxlen", max_length(*max));
            }
            if let Some(min) = min {
                push("_minlen", min_length(*min));
            }
        }
        FieldKind::Email {
            min_length: min,
            max_length: max,
        } => {
            if let Some(max) = max {
                push("_maxlen", max_length(*max));
            }
            let mut expression = format!("{q} LIKE '_%@_%._%' AND instr({q}, ' ') = 0");
            if let Some(min) = min {
                expression.push_str(&format!(" AND {}", min_length(*min)));
            }
            push("_email", expression);
        }
        FieldKind::Set { allowed, .. } => {
            // `cli-minor-fixes` makes the check case-sensitive (Latin1_General_100_BIN2).
            let binary = !case_insensitive
                || ir_check("").is_some_and(|check| check.expression.contains("COLLATE"));
            let values: Vec<String> = allowed.iter().map(|v| quote_literal(v)).collect();
            push(
                "",
                format!(
                    "{q}{} IN ({})",
                    if binary { " COLLATE BINARY" } else { "" },
                    values.join(", ")
                ),
            );
        }
        FieldKind::Int { min, max } => {
            push(
                "_type",
                format!(
                    "{q} IS NULL OR (typeof({q}) = 'integer' AND {q} BETWEEN -2147483648 AND 2147483647)"
                ),
            );
            if ir_check("_range").is_some() {
                push("_range", range(&q, *min, *max));
            }
        }
        FieldKind::Decimal {
            precision,
            scale,
            min,
            max,
        } => {
            push(
                "_type",
                format!(
                    "{q} IS NULL OR (typeof({q}) IN ('real', 'integer') AND abs({q}) < 1e{})",
                    precision.saturating_sub(*scale)
                ),
            );
            if ir_check("_range").is_some() {
                push("_range", range(&q, *min, *max));
            }
        }
        FieldKind::Boolean => push("_type", format!("{q} IN (0, 1)")),
        FieldKind::Date => push(
            "_type",
            format!("{q} IS NULL OR {q} GLOB '{}'", date_glob()),
        ),
    }
    checks
}

fn range(q: &str, min: Option<f64>, max: Option<f64>) -> String {
    let number = |n: f64| {
        if n.fract() == 0.0 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            format!("{n:?}")
        }
    };
    let mut parts = Vec::new();
    if let Some(min) = min {
        parts.push(format!("{q} >= {}", number(min)));
    }
    if let Some(max) = max {
        parts.push(format!("{q} <= {}", number(max)));
    }
    if parts.is_empty() {
        "1".into()
    } else {
        parts.join(" AND ")
    }
}

fn uuid_glob() -> String {
    [8, 4, 4, 4, 12]
        .iter()
        .map(|n| "[0-9a-f]".repeat(*n))
        .collect::<Vec<_>>()
        .join("-")
}

fn date_glob() -> String {
    let d = |n: usize| "[0-9]".repeat(n);
    format!(
        "{}-{}-{}T{}:{}:{}.{}Z",
        d(4),
        d(2),
        d(2),
        d(2),
        d(2),
        d(2),
        d(7)
    )
}

fn default_sql(entity: &Entity, column: &Column) -> Result<Option<String>, SchemaError> {
    match column.default.as_ref().filter(|value| !value.is_null()) {
        Some(value) => {
            let stored = codec::encode(&column.kind, value).map_err(|error| {
                SchemaError::InvalidDefault {
                    entity: entity.name.clone(),
                    field: column.field.clone(),
                    value: value.to_string(),
                    kind: column.kind.graphql_scalar(),
                    reason: error.to_string(),
                }
            })?;
            Ok(Some(literal_sql(&stored)))
        }
        None if codec::is_generated_key(column) => Ok(Some(codec::UUID_V4_SQL.to_owned())),
        None => Ok(None),
    }
}

/// A SQL literal for a stored value.
pub(crate) fn literal_sql(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "NULL".into(),
        SqlValue::Integer(n) => n.to_string(),
        SqlValue::Real(n) => format!("{n:?}"),
        SqlValue::Text(text) => quote_literal(text),
        SqlValue::Blob(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            format!("X'{hex}'")
        }
    }
}

fn check_names(tables: &[TableSchema]) -> Result<(), SchemaError> {
    let mut seen: Vec<(String, &str)> = Vec::new();
    for table in tables {
        let lower = table.name.to_ascii_lowercase();
        if lower.starts_with("_rl_") || lower.starts_with("sqlite_") {
            return Err(SchemaError::ReservedName {
                entity: table.entity.clone(),
                table: table.name.clone(),
            });
        }
        let names = std::iter::once(&table.name).chain(table.indexes.iter().map(|i| &i.name));
        for name in names {
            let lower = name.to_ascii_lowercase();
            if let Some((_, first)) = seen.iter().find(|(seen, _)| *seen == lower) {
                return Err(SchemaError::NameClash {
                    name: name.clone(),
                    first: (*first).to_owned(),
                    second: table.entity.clone(),
                });
            }
            seen.push((lower, &table.entity));
        }
    }
    Ok(())
}

/// A quoted SQL identifier.
pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A quoted SQL string literal.
pub fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn ident_list(names: &[String]) -> String {
    names
        .iter()
        .map(|name| quote_ident(name))
        .collect::<Vec<_>>()
        .join(", ")
}
