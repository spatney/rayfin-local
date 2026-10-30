//! The real Rayfin SDK 1.36.2 Category A client against rayfin-local's
//! `/connectors/<name>/graphql` over a real SQL Server (`tests/sdk/category-a.mjs`), with the
//! `connectors` fixture app's entity classes compiled by `js/analyze.mjs`.
//!
//! Needs `RAYFIN_LOCAL_TEST_MSSQL` (see `sql_server.rs`), Node.js, and the fixture's packages
//! (`npm install` in `tests/fixtures/apps/connectors`); skipped without them.

mod common;

use std::path::{Path, PathBuf};

use common::serve;
use common::sql::{Database, HAMMER, RAKE, router, server};
use rl_model::{ConnectorSource, ConnectorsAnalyzeInput};
use serde_json::{Value, json};
use tokio::process::Command;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/apps/connectors")
        .canonicalize()
        .unwrap()
}

/// Compiles the fixture's connector entities with the analysis helper, into the project's
/// `.rayfin-local/` so they import its packages; returns their folder.
async fn compile_entities(project: &Path) -> PathBuf {
    let scratch = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rl-connectors-sql-sdk");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    let mut input = ConnectorsAnalyzeInput::for_project(
        project,
        vec![ConnectorSource {
            name: "inventory".into(),
            dir: project.join("rayfin/connectors/inventory"),
            dialect: "mssql".into(),
        }],
    );
    input.out_dir = project.join(".rayfin-local/cache/sdk-test-connectors");
    let script = scratch.join("analyze.mjs");
    std::fs::write(&script, rl_model::ANALYZE_JS).unwrap();
    std::fs::write(scratch.join("input.json"), input.to_json().to_string()).unwrap();
    let status = Command::new("node")
        .arg(&script)
        .arg("--input")
        .arg(scratch.join("input.json"))
        .arg("--output")
        .arg(scratch.join("output.json"))
        .current_dir(project)
        .status()
        .await
        .unwrap();
    assert!(status.success());
    let output: Value =
        serde_json::from_slice(&std::fs::read(scratch.join("output.json")).unwrap()).unwrap();
    assert_eq!(output["ok"], true, "{output}");
    input.out_dir.join("connectors").join("inventory")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sdk_reads_and_writes_through_rayfin_local() {
    let Some(server) = server() else {
        return;
    };
    let project = fixture();
    let node_modules = project.join("node_modules");
    if !node_modules.join("@microsoft/rayfin-connectors").is_dir() {
        eprintln!("skipped: run `npm install` in {}", project.display());
        return;
    }
    if Command::new("node")
        .arg("--version")
        .output()
        .await
        .is_err()
    {
        eprintln!("skipped: node isn't on PATH");
        return;
    }
    let compiled = compile_entities(&project).await;
    let database = Database::create(&server, "sdk").await;
    let api = serve(router(
        &server,
        &database.name,
        &[
            ("inventory", "fabric-sqldatabase"),
            ("stock", "fabric-warehouse"),
            ("lake", "fabric-sqlanalytics"),
        ],
        None,
    ))
    .await;

    let settings = json!({
        "nodeModules": node_modules,
        "compiled": compiled,
        "api": api,
        "hammer": HAMMER,
        "rake": RAKE,
    });
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sdk/category-a.mjs");
    let output = Command::new("node")
        .arg(script)
        .env("RL_SDK_TEST", settings.to_string())
        .output()
        .await
        .unwrap();
    database.drop().await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    assert!(output.status.success(), "{stdout}\n{stderr}");
}
