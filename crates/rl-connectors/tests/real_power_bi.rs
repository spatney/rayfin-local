//! A manual check against the real Power BI: queries the reference app's semantic model as the
//! developer signed in to the Azure CLI. Not run by default:
//!
//! ```text
//! cargo test -p rl-connectors --test real_power_bi -- --ignored --nocapture
//! ```

mod common;

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::http::header;
use common::*;
use rl_connectors::{ConnectorsConfig, router};
use rl_tokens::{AccessToken, BoxFuture, TokenError, TokenSource};
use serde_json::{Value, json};
use tokio::process::Command;

/// `az account get-access-token --scope <scope>`.
struct AzCli;

impl TokenSource for AzCli {
    fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
        Box::pin(async move {
            let args = [
                "account",
                "get-access-token",
                "--scope",
                scope,
                "-o",
                "json",
            ];
            let output = if cfg!(windows) {
                Command::new("cmd")
                    .arg("/C")
                    .arg("az")
                    .args(args)
                    .output()
                    .await
            } else {
                Command::new("az").args(args).output().await
            };
            let failed = |message: String| TokenError {
                scope: scope.to_owned(),
                message,
                hint: Some("Run `az login`, then retry.".to_owned()),
            };
            let output = output.map_err(|e| failed(format!("couldn't run az: {e}")))?;
            if !output.status.success() {
                return Err(failed(
                    String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                ));
            }
            let answer: Value = serde_json::from_slice(&output.stdout)
                .map_err(|e| failed(format!("az printed something unexpected: {e}")))?;
            let token = answer["accessToken"]
                .as_str()
                .ok_or_else(|| failed("az printed no accessToken".to_owned()))?;
            let expires_at = answer["expires_on"]
                .as_u64()
                .map(|seconds| UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(300));
            Ok(AccessToken {
                token: token.to_owned(),
                expires_at,
            })
        })
    }
}

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
