//! A manual check against a real Fabric SQL database, as the developer signed in to the Azure
//! CLI: the location from the Fabric REST API, an Entra login over TDS, and a read-only GraphQL
//! query of `INFORMATION_SCHEMA.TABLES` (every database has it), so nothing is written. Not run
//! by default:
//!
//! ```text
//! RAYFIN_LOCAL_REAL_SQL="<workspace id>;<item id>[;fabric-sqldatabase]" \
//!   cargo test -p rl-connectors --test real_fabric_sql -- --ignored --nocapture
//! ```
//!
//! Without `RAYFIN_LOCAL_REAL_SQL` it uses the `healthcare` SQL database of the maintainers'
//! `local-todo` app.

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::StatusCode;
use common::{AzCli, FakeCallers, SESSION, json_body, post};
use rl_connectors::{ConnectorDef, ConnectorsConfig, SqlConnectorsConfig};
use rl_model::AppModel;
use serde_json::json;

const HEALTHCARE: &str =
    "c53ffacb-2330-4c61-a0cb-4f84e9b55c83;c7f51886-3a5a-4e46-b5f3-a67886b2ad43";

/// An entity over `INFORMATION_SCHEMA.TABLES`, as the connector analysis would describe it.
fn tables_model() -> AppModel {
    let field = |name: &str, column: &str| {
        json!({
            "name": name, "columnName": column, "primaryKey": false, "isRelationship": false,
            "originalFieldMetadata": { "format": "text", "max": 128, "columnName": column }
        })
    };
    let analysis = json!({
        "entities": [{
            "name": "CatalogTable",
            "tableName": "CatalogTables",
            "fields": [field("tableSchema", "TABLE_SCHEMA"), field("tableName", "TABLE_NAME")]
        }],
        "config": { "entities": { "CatalogTable": {
            "source": "INFORMATION_SCHEMA.TABLES",
            "permissions": [{ "role": "authenticated", "actions": ["read"] }],
            "x-schema": {
                "fields": {
                    "TABLE_SCHEMA": { "dbType": "NVARCHAR(128)", "nullable": false },
                    "TABLE_NAME": { "dbType": "NVARCHAR(128)", "nullable": false }
                },
                "constraints": {}
            },
            "mappings": { "TABLE_SCHEMA": "tableSchema", "TABLE_NAME": "tableName" }
        } } },
        "policies": []
    });
    AppModel::from_connector_analysis(&analysis).unwrap()
}

#[tokio::test]
#[ignore = "reads a real Fabric SQL database as the developer (az login)"]
async fn reads_a_fabric_sql_database_as_the_developer() {
    let target = std::env::var("RAYFIN_LOCAL_REAL_SQL").unwrap_or_else(|_| HEALTHCARE.to_owned());
    let mut parts = target.split(';');
    let (workspace, item) = (parts.next().unwrap(), parts.next().unwrap());
    let kind = parts.next().unwrap_or("fabric-sqldatabase");
    let def = ConnectorDef {
        name: "real".into(),
        kind: kind.into(),
        config: json!({ "workspaceId": workspace, "itemId": item })
            .as_object()
            .unwrap()
            .clone(),
        operations: vec!["read".into()],
        ..ConnectorDef::default()
    };
    let config = ConnectorsConfig {
        connectors: vec![def],
        sql: SqlConnectorsConfig {
            models: HashMap::from([("real".to_owned(), Ok(Arc::new(tables_model())))]),
            ..SqlConnectorsConfig::default()
        },
        ..ConnectorsConfig::default()
    };
    let app = rl_connectors::router(config, Arc::new(FakeCallers), Arc::new(AzCli));
    let query = "{ catalogTables(first: 5, orderBy: {tableSchema: ASC, tableName: ASC}) { items \
                 { tableSchema tableName } hasNextPage endCursor } }";
    let (status, _, body) = post(
        &app,
        "/connectors/real/graphql",
        Some(SESSION),
        json!({ "query": query }),
    )
    .await;
    let body = json_body(&body);
    println!(
        "HTTP {status}\n{}",
        serde_json::to_string_pretty(&body).unwrap()
    );
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("errors").is_none(), "{body}");
    assert!(body["data"]["catalogTables"]["items"].is_array());
}
