//! A manual check against the real Power BI: queries the reference app's semantic model as the
//! developer signed in to the Azure CLI. Not run by default:
//!
//! ```text
//! cargo test -p rl-connectors --test real_power_bi -- --ignored --nocapture
//! ```

mod common;

use std::sync::Arc;

use axum::http::header;
use common::*;
use rl_connectors::{ConnectorsConfig, router};
use serde_json::{Value, json};

#[tokio::test]
#[ignore = "calls the real Power BI as the developer (az login)"]
async fn queries_the_reference_model_as_the_developer() {
    let config = ConnectorsConfig {
        connectors: vec![semantic_model(
            "missioncontroltelemetry",
            WORKSPACE,
            &["executeQuery"],
        )],
        ..ConnectorsConfig::default()
    };
    let app = router(config, Arc::new(FakeCallers), Arc::new(AzCli));

    let input = json!({ "query": ROW_QUERY });
    let (status, headers, body) = post(
        &app,
        "/connector-invoke/missioncontroltelemetry",
        Some(SESSION),
        execute_query(input),
    )
    .await;

    let content_type = headers[header::CONTENT_TYPE].to_str().unwrap();
    println!(
        "HTTP {status}, {content_type}, requestid {:?}, {} bytes",
        headers.get("requestid"),
        body.len()
    );
    if content_type.starts_with("application/json") {
        // The failed-query envelope: Power BI's answer, never the token.
        let envelope: Value = serde_json::from_slice(&body).unwrap();
        println!("{}", serde_json::to_string_pretty(&envelope).unwrap());
    }
    assert_eq!(status, 200);
}
