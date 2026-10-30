//! Category A connectors (`fabric-sqldatabase`, `fabric-warehouse`, `fabric-sqlanalytics`): what
//! the source database says about the tables their entities map to.
//!
//! `rayfin connector add` writes the source's catalog to `rayfin/connectors/<name>/metadata.json`
//! (`SchemaMetadata`: `schemas[].tables[].columns[]` with `dataType`, `isNullable` and, on SQL
//! databases, `identity`, `default`, `computed` and `serverManaged`). The entity classes carry
//! only their decorators; the database's own types and generated columns come from there.

use serde::Deserialize;
use serde_json::Value;

use crate::model::{AppModel, Diagnostic, ModelError, SourceColumn};

/// The result of `js/analyze.mjs` in connectors mode, one model (or why there is none) per
/// connector.
#[derive(Debug)]
pub struct ConnectorsAnalysis {
    pub connectors: Vec<ConnectorAnalysis>,
    /// Every project file the TypeScript program read and the modules imported (to tell when the
    /// analysis is out of date).
    pub inputs: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
}

/// One connector of a [`ConnectorsAnalysis`].
#[derive(Debug)]
pub struct ConnectorAnalysis {
    pub name: String,
    /// The model, or why the connector's entities can't be served (the analyzer's or the
    /// model's message).
    pub model: Result<AppModel, String>,
}

#[derive(Deserialize)]
struct ResultJson {
    #[serde(default)]
    connectors: Vec<Value>,
    #[serde(default)]
    sources: SourcesJson,
    #[serde(default)]
    diagnostics: Vec<Diagnostic>,
}

#[derive(Default, Deserialize)]
struct SourcesJson {
    #[serde(default)]
    inputs: Vec<String>,
}

impl ConnectorsAnalysis {
    /// Reads the helper's `result`.
    pub fn from_result(result: &Value) -> Result<Self, ModelError> {
        let parsed =
            ResultJson::deserialize(result).map_err(|e| ModelError::Malformed(e.to_string()))?;
        let connectors = parsed
            .connectors
            .iter()
            .map(|connector| {
                let name = connector
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let error = connector
                    .get("error")
                    .and_then(|error| error.get("message"))
                    .and_then(Value::as_str);
                let model = match error {
                    Some(message) => Err(message.to_owned()),
                    None => AppModel::from_connector_analysis(connector).map_err(|e| e.to_string()),
                };
                ConnectorAnalysis { name, model }
            })
            .collect();
        Ok(Self {
            connectors,
            inputs: parsed.sources.inputs,
            diagnostics: parsed.diagnostics,
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Metadata {
    #[serde(default)]
    schemas: Vec<SchemaJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SchemaJson {
    schema_name: String,
    #[serde(default)]
    tables: Vec<TableJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TableJson {
    table_name: String,
    #[serde(default)]
    columns: Vec<ColumnJson>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ColumnJson {
    column_name: String,
    data_type: String,
    identity: Option<Value>,
    default: Option<Value>,
    computed: Option<Value>,
    server_managed: Option<Value>,
}

/// The schema and object of a DAB source such as `dbo.orders`; the default schema `dbo` when
/// the source names none.
pub fn split_source(source: &str) -> (&str, &str) {
    source.split_once('.').unwrap_or(("dbo", source))
}

impl AppModel {
    /// Annotates the columns of the entities with what `metadata` (a connector's
    /// `metadata.json`) says about them ([`crate::Column::source`]). Returns the entities whose
    /// source table it doesn't list, which keep their decorators' view.
    pub fn apply_source_metadata(&mut self, metadata: &Value) -> Vec<String> {
        let Ok(metadata) = Metadata::deserialize(metadata) else {
            return self.entities.iter().map(|e| e.name.clone()).collect();
        };
        let mut unknown = Vec::new();
        for entity in &mut self.entities {
            let (schema, table) = split_source(&entity.table);
            let found = metadata
                .schemas
                .iter()
                .filter(|s| s.schema_name.eq_ignore_ascii_case(schema))
                .flat_map(|s| &s.tables)
                .find(|t| t.table_name.eq_ignore_ascii_case(table));
            let Some(found) = found else {
                unknown.push(entity.name.clone());
                continue;
            };
            for column in &mut entity.columns {
                let Some(source) = found
                    .columns
                    .iter()
                    .find(|c| c.column_name == column.name)
                    .or_else(|| {
                        found
                            .columns
                            .iter()
                            .find(|c| c.column_name.eq_ignore_ascii_case(&column.name))
                    })
                else {
                    continue;
                };
                let present = |value: &Option<Value>| {
                    value
                        .as_ref()
                        .is_some_and(|v| !v.is_null() && *v != Value::Bool(false))
                };
                column.source = Some(SourceColumn {
                    data_type: source.data_type.trim().to_ascii_lowercase(),
                    generated: present(&source.identity)
                        || present(&source.computed)
                        || present(&source.server_managed),
                    has_default: present(&source.default),
                });
            }
        }
        unknown
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn sources_split_into_schema_and_object() {
        assert_eq!(split_source("dbo.orders"), ("dbo", "orders"));
        assert_eq!(split_source("sales.order.items"), ("sales", "order.items"));
        assert_eq!(split_source("orders"), ("dbo", "orders"));
    }

    #[test]
    fn metadata_marks_types_and_generated_columns() {
        let column = |name: &str, field: &str, kind: Value, key: bool| {
            json!({
                "name": name, "field": field, "kind": kind, "db_type": "X", "nullable": !key,
                "default": null, "primary_key": key, "unique": false,
                "origin": {"type": "declared"}
            })
        };
        let entity = |name: &str, table: &str, columns: Value| {
            json!({
                "name": name, "table": table,
                "graphql": {"type_name": name, "plural": name, "list_query": name,
                    "by_pk_query": name, "create_mutation": name, "update_mutation": name,
                    "delete_mutation": name},
                "columns": columns, "primary_key": {"name": "", "columns": []},
                "foreign_keys": [], "unique_constraints": [], "check_constraints": [],
                "relationships": [], "permissions": []
            })
        };
        let int = json!({"type": "int", "min": null, "max": null});
        let mut model: AppModel = serde_json::from_value(json!({
            "core_version": null, "typescript_version": null, "dialect": "mssql",
            "feature_flags": null, "storage": null,
            "sources": {"data": [], "storage": [], "inputs": []}, "diagnostics": [],
            "entities": [
                entity("Invoice", "dbo.invoice", json!([
                    column("Id", "id", int.clone(), true),
                    column("Total", "total", int.clone(), false),
                    column("Note", "note", int, false),
                ])),
                entity("Elsewhere", "other.thing", json!([])),
            ]
        }))
        .unwrap();
        let metadata = json!({"schemas": [{"schemaName": "dbo", "tables": [{
            "tableName": "Invoice",
            "columns": [
                {"columnName": "Id", "dataType": "int", "isNullable": false,
                 "identity": {"seed": 1, "increment": 1}},
                {"columnName": "total", "dataType": "Money", "isNullable": false,
                 "default": "((0))"}
            ]
        }]}]});
        let unknown = model.apply_source_metadata(&metadata);
        assert_eq!(unknown, ["Elsewhere"]);
        let columns = &model.entities[0].columns;
        let id = columns[0].source.as_ref().unwrap();
        assert!(id.generated && !id.has_default);
        assert_eq!(id.data_type, "int");
        let total = columns[1].source.as_ref().unwrap();
        assert!(!total.generated && total.has_default);
        assert_eq!(total.data_type, "money");
        assert_eq!(columns[2].source, None);
    }
}
