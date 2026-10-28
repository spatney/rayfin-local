//! The real Rayfin SDK 1.36.2 (`fabricSemanticModel()` on `createConnectorsApi`, as the reference
//! app wires it) against rayfin-local's routes, compared with the SDK's own direct path to the
//! same mock Power BI (`tests/sdk/decode.mjs`).
//!
//! Needs Node and the reference app's node_modules (`RAYFIN_SDK_NODE_MODULES`, or
//! `fabric-apps/apps/explorer` next to this repository); skipped without them.

mod common;

use std::{path::Path, sync::Arc};

use common::*;
use rl_connectors::router;
use serde_json::json;
use tokio::process::Command;

const TOKEN_ERROR: &str = "az isn't signed in";
const TOKEN_HINT: &str = "Run `az login`, then retry.";

#[tokio::test(flavor = "multi_thread")]
async fn the_sdk_normalizes_rayfin_local_answers_as_its_direct_path() {
    let Some(node_modules) = sdk_node_modules() else {
        eprintln!("skipped: the Rayfin SDK 1.36.2 node_modules aren't installed");
        return;
    };
    if Command::new("node")
        .arg("--version")
        .output()
        .await
        .is_err()
    {
        eprintln!("skipped: node isn't on PATH");
        return;
    }

    let power_bi = MockPowerBi::start().await;
    let offline_power_bi = closed_origin().await;
    let api = serve(router(
        config(&power_bi.base_url),
        Arc::new(FakeCallers),
        FakeTokens::with_token(DEV_TOKEN),
    ))
    .await;
    let offline_api = serve(router(
        config(&offline_power_bi),
        Arc::new(FakeCallers),
        FakeTokens::with_token(DEV_TOKEN),
    ))
    .await;
    let tokenless_api = serve(router(
        config(&power_bi.base_url),
        Arc::new(FakeCallers),
        FakeTokens::failing(TOKEN_ERROR, TOKEN_HINT),
    ))
    .await;
    let az_api = serve(router(
        config(&power_bi.base_url),
        Arc::new(FakeCallers),
        FakeTokens::with_token(&az_token()),
    ))
    .await;

    let settings = json!({
        "nodeModules": node_modules,
        "api": api,
        "offlineApi": offline_api,
        "tokenlessApi": tokenless_api,
        "azApi": az_api,
        "powerBi": power_bi.base_url,
        "offlinePowerBi": offline_power_bi,
        "workspaceId": WORKSPACE,
        "itemId": ITEM,
        "session": SESSION,
        "tokenError": TOKEN_ERROR,
        "tokenHint": TOKEN_HINT,
    });
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sdk/decode.mjs");
    let output = Command::new("node")
        .arg(script)
        .env("RL_SDK_TEST", settings.to_string())
        .output()
        .await
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    assert!(output.status.success(), "{stdout}\n{stderr}");
}
