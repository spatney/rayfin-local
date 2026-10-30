//! The entity tables as a host's control API reads and clears them, whatever the permissions:
//! row counts, pages of rows with each value as the data API answers it (the same
//! `rl_db::codec::json_expr` the queries use), and clearing rows.

use rl_db::codec;
use rl_model::{AppModel, Column, Entity};
use serde::Serialize;
use serde_json::{Map, Value as Json};

use crate::DataApi;
use crate::sql::quote_ident;

/// The most rows [`DataApi::rows`] reads at once.
pub const MAX_ROWS: u32 = 500;

/// Which rows of an entity to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowsQuery {
    pub offset: u64,
    /// At most [`MAX_ROWS`].
    pub limit: u32,
    /// A field (its GraphQL name) to order by; the primary key when `None`, and after the field.
    pub order_by: Option<String>,
    pub desc: bool,
}

/// A page of an entity's rows.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Rows {
    pub columns: Vec<RowColumn>,
    /// One object per row, keyed by field name, in the order of `columns`.
    pub rows: Vec<Json>,
    /// How many rows the entity holds.
    pub total: u64,
}

/// A field of the rows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RowColumn {
    /// The field's GraphQL name.
    pub name: String,
    /// Its GraphQL type: `UUID`, `String`, `Int`, `Decimal`, `Boolean` or `DateTime`.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Whether it is the primary key.
    pub key: bool,
}

/// Why rows could not be read or cleared.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    #[error("the app has no entity {0}")]
    UnknownEntity(String),
    #[error("{entity} has no field {field}; its fields are {fields}")]
    UnknownField {
        entity: String,
        field: String,
        fields: String,
    },
    /// Rows of other entities refer to the rows to clear: `by` names them, with how many rows
    /// of each (empty when the database refused without saying which).
    #[error("{entity}'s rows can't be cleared while {} refer to them", referrers(.by))]
    Referenced {
        entity: String,
        by: Vec<(String, u64)>,
    },
    #[error("the database failed: {0}")]
    Database(String),
}

fn referrers(by: &[(String, u64)]) -> String {
    if by.is_empty() {
        return "other rows".to_owned();
    }
    by.iter()
        .map(|(entity, rows)| {
            let noun = if *rows == 1 { "row" } else { "rows" };
            format!("{rows} {entity} {noun}")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl From<rusqlite::Error> for AdminError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

impl From<rl_db::DbError> for AdminError {
    fn from(error: rl_db::DbError) -> Self {
        Self::Database(error.to_string())
    }
}

impl DataApi {
    /// How many rows each entity of the served model holds, in the model's order.
    pub async fn row_counts(&self) -> Result<Vec<(String, u64)>, AdminError> {
        let model = self.model();
        let entities: Vec<(String, String)> = model
            .entities
            .iter()
            .map(|entity| (entity.name.clone(), entity.table.clone()))
            .collect();
        let counts = self
            .db
            .call(move |conn| {
                entities
                    .into_iter()
                    .map(|(name, table)| Ok((name, count_rows(conn, &table, None)?)))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .await??;
        Ok(counts)
    }

    /// A page of `entity`'s rows (by name, as in the model), whatever the permissions.
    pub async fn rows(&self, entity: &str, query: &RowsQuery) -> Result<Rows, AdminError> {
        let model = self.model();
        let entity = find_entity(&model, entity)?;
        let mut order = Vec::new();
        if let Some(field) = &query.order_by {
            let column = find_field(entity, field)?;
            order.push(format!(
                "{} {}",
                quote_ident(&column.name),
                if query.desc { "DESC" } else { "ASC" }
            ));
        }
        for key in &entity.primary_key.columns {
            order.push(format!("{} ASC", quote_ident(key)));
        }
        let values = entity
            .columns
            .iter()
            .map(|column| {
                // json_quote keeps the JSON a boolean's expression makes, and quotes the rest.
                format!(
                    "json_quote({})",
                    codec::json_expr(&column.kind, &quote_ident(&column.name))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT {values} FROM {} ORDER BY {} LIMIT ?1 OFFSET ?2",
            quote_ident(&entity.table),
            order.join(", ")
        );
        let table = entity.table.clone();
        let limit = i64::from(query.limit.clamp(1, MAX_ROWS));
        let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
        let fields: Vec<String> = entity.columns.iter().map(|c| c.field.clone()).collect();
        let (texts, total) = self
            .db
            .call(move |conn| -> rusqlite::Result<_> {
                let mut statement = conn.prepare(&sql)?;
                let width = statement.column_count();
                let texts = statement
                    .query_map([limit, offset], |row| {
                        (0..width)
                            .map(|index| row.get::<_, String>(index))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok((texts, count_rows(conn, &table, None)?))
            })
            .await??;
        let rows = texts
            .into_iter()
            .map(|values| {
                let row: Map<String, Json> = fields
                    .iter()
                    .zip(values)
                    .map(|(field, text)| {
                        (
                            field.clone(),
                            serde_json::from_str(&text).unwrap_or(Json::Null),
                        )
                    })
                    .collect();
                Json::Object(row)
            })
            .collect();
        Ok(Rows {
            columns: entity.columns.iter().map(row_column).collect(),
            rows,
            total,
        })
    }

    /// Deletes every row of `entity`; returns how many. Refused while rows of other entities
    /// refer to them (none cascade).
    pub async fn clear_rows(&self, entity: &str) -> Result<u64, AdminError> {
        let model = self.model();
        let entity = find_entity(&model, entity)?;
        let name = entity.name.clone();
        let table = entity.table.clone();
        let referring = referring(&model, entity);
        let outcome = self
            .db
            .call(
                move |conn| -> rusqlite::Result<Result<u64, Vec<(String, u64)>>> {
                    let tx = conn.transaction()?;
                    let mut by = Vec::new();
                    for (entity, table, columns) in &referring {
                        let rows = count_rows(&tx, table, Some(columns))?;
                        if rows > 0 {
                            by.push((entity.clone(), rows));
                        }
                    }
                    if !by.is_empty() {
                        return Ok(Err(by));
                    }
                    let deleted =
                        tx.execute(&format!("DELETE FROM {}", quote_ident(&table)), [])?;
                    tx.commit()?;
                    Ok(Ok(u64::try_from(deleted).unwrap_or_default()))
                },
            )
            .await?;
        match outcome {
            Ok(Ok(deleted)) => Ok(deleted),
            Ok(Err(by)) => Err(AdminError::Referenced { entity: name, by }),
            Err(error) if is_foreign_key_violation(&error) => Err(AdminError::Referenced {
                entity: name,
                by: Vec::new(),
            }),
            Err(error) => Err(error.into()),
        }
    }

    /// Deletes every row of every entity of the served model; returns how many.
    pub async fn clear_all_rows(&self) -> Result<u64, AdminError> {
        let model = self.model();
        let tables: Vec<String> = model
            .entities
            .iter()
            .map(|entity| entity.table.clone())
            .collect();
        let deleted = self
            .db
            .call(move |conn| -> rusqlite::Result<u64> {
                let tx = conn.transaction()?;
                // Rows refer to each other across tables: check the references once all are gone.
                tx.pragma_update(None, "defer_foreign_keys", true)?;
                let mut deleted = 0;
                for table in &tables {
                    let rows = tx.execute(&format!("DELETE FROM {}", quote_ident(table)), [])?;
                    deleted += u64::try_from(rows).unwrap_or_default();
                }
                tx.commit()?;
                Ok(deleted)
            })
            .await??;
        Ok(deleted)
    }
}

fn find_entity<'a>(model: &'a AppModel, name: &str) -> Result<&'a Entity, AdminError> {
    let name = name.trim();
    model
        .entities
        .iter()
        .find(|entity| entity.name == name)
        .or_else(|| {
            model
                .entities
                .iter()
                .find(|entity| entity.name.eq_ignore_ascii_case(name))
        })
        .ok_or_else(|| AdminError::UnknownEntity(name.to_owned()))
}

fn find_field<'a>(entity: &'a Entity, field: &str) -> Result<&'a Column, AdminError> {
    entity
        .columns
        .iter()
        .find(|column| column.field == field)
        .or_else(|| {
            entity
                .columns
                .iter()
                .find(|column| column.field.eq_ignore_ascii_case(field))
        })
        .ok_or_else(|| AdminError::UnknownField {
            entity: entity.name.clone(),
            field: field.to_owned(),
            fields: entity
                .columns
                .iter()
                .map(|column| column.field.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        })
}

fn row_column(column: &Column) -> RowColumn {
    RowColumn {
        name: column.field.clone(),
        kind: column.kind.graphql_scalar(),
        key: column.primary_key,
    }
}

/// The other entities whose foreign keys point at `entity`'s table: name, table and key columns.
fn referring(model: &AppModel, entity: &Entity) -> Vec<(String, String, Vec<String>)> {
    model
        .entities
        .iter()
        .filter(|other| other.name != entity.name)
        .flat_map(|other| {
            other
                .foreign_keys
                .iter()
                .filter(|key| key.referenced_table.eq_ignore_ascii_case(&entity.table))
                .map(|key| (other.name.clone(), other.table.clone(), key.columns.clone()))
        })
        .collect()
}

/// The rows of `table`; with `set`, only those where each of these columns has a value.
fn count_rows(
    conn: &rusqlite::Connection,
    table: &str,
    set: Option<&[String]>,
) -> rusqlite::Result<u64> {
    let mut sql = format!("SELECT count(*) FROM {}", quote_ident(table));
    if let Some(columns) = set.filter(|columns| !columns.is_empty()) {
        let conditions: Vec<String> = columns
            .iter()
            .map(|column| format!("{} IS NOT NULL", quote_ident(column)))
            .collect();
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(u64::try_from(count).unwrap_or_default())
}

fn is_foreign_key_violation(error: &rusqlite::Error) -> bool {
    codec::constraint_violation(error)
        .is_some_and(|violation| violation.kind == codec::ConstraintKind::ForeignKey)
}
