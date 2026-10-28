//! The real Rayfin SDK 1.36.2 data client (`client.data.<Entity>`) against rl-graphql behind
//! rl-server, signed in through rl-auth, on the explorer app's model (`tests/sdk/data.mjs`).
//!
//! Needs Node and the SDK's node_modules: `RAYFIN_SDK_NODE_MODULES`, or the explorer app checked
//! out next to this repository (`../fabric-apps/apps/explorer/node_modules`); skipped without them.

mod common;

use std::path::{Path, PathBuf};

use common::{App, explorer_model};
use rl_server::{ServerConfig, build_router};
use serde_json::{Value, json};

const KEY: &str = "pk-local-test";
const PASSWORD: &str = "correct horse battery";

fn sdk_node_modules() -> Option<PathBuf> {
    let path = std::env::var_os("RAYFIN_SDK_NODE_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../fabric-apps/apps/explorer/node_modules")
        });
    let marker = path.join("@microsoft/rayfin-data/package.json");
    marker.is_file().then_some(path)
}

fn id(n: u32) -> String {
    format!("00000000-0000-4000-8000-{n:012}")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sdk_data_client_reads_and_writes_through_rayfin_local() {
    let Some(node_modules) = sdk_node_modules() else {
        eprintln!("skipped: the Rayfin SDK 1.36.2 node_modules aren't installed");
        return;
    };
    let node = tokio::process::Command::new("node")
        .arg("--version")
        .output()
        .await;
    if node.is_err() {
        eprintln!("skipped: node isn't on PATH");
        return;
    }
    let _ = rustls::crypto::ring::default_provider().install_default();

    let app = App::new(explorer_model()).await;
    let alice = app
        .auth
        .create_user("alice@example.test", PASSWORD)
        .await
        .unwrap();
    let bob = app
        .auth
        .create_user("bob@example.test", PASSWORD)
        .await
        .unwrap();
    let (alice_id, bob_id) = (alice.id.clone(), bob.id.clone());
    let thread = |n: u32, user: &str, title: &str, response: &str, day: u32| {
        format!(
            "('{}', '{user}', '{title}', {response}, '2026-01-0{day}T00:00:00.0000000Z', \
             '2026-01-0{day}T00:00:00.0000000Z')",
            id(n)
        )
    };
    let message = |n: u32, role: &str, content: &str, second: u32| {
        format!(
            "('{}', '{alice_id}', '{}', '{role}', '{content}', '2026-01-01T00:00:0{second}.0000000Z')",
            id(n),
            id(101)
        )
    };
    app.seed(&format!(
        "INSERT INTO \"AskThreads\" (id, user_id, title, responseId, createdAt, updatedAt) VALUES
           {}, {}, {}, {};
         INSERT INTO \"AskMessages\" (id, user_id, thread_id, role, content, createdAt) VALUES
           {}, {}, {};",
        thread(101, &alice_id, "Alice one", "NULL", 1),
        thread(102, &alice_id, "Alice two", "'resp_1'", 2),
        thread(103, &alice_id, "alice three", "NULL", 3),
        thread(104, &bob_id, "Bob one", "NULL", 1),
        message(201, "user", "first question", 1),
        message(202, "assistant", "an answer", 2),
        message(203, "user", "second question", 3),
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let config = ServerConfig {
        api_origin: origin.clone(),
        publishable_key: KEY.into(),
        workspace_id: "11111111-1111-4111-8111-111111111111".into(),
        item_id: "22222222-2222-4222-8222-222222222222".into(),
        tenant_id: None,
        portal_url: format!("{origin}/.rayfin-local/portal"),
        allowed_origins: Vec::new(),
        allowed_hosts: None,
        static_site: None,
        publishable_key_exempt: rl_auth::PUBLISHABLE_KEY_EXEMPT_PATHS
            .iter()
            .map(|p| p.to_string())
            .collect(),
    };
    let router = build_router(
        config,
        vec![app.api.clone().router(), app.auth.clone().router()],
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let settings = json!({
        "nodeModules": node_modules.to_string_lossy().replace('\\', "/"),
        "api": origin,
        "publishableKey": KEY,
        "password": PASSWORD,
        "alice": {"email": "alice@example.test", "id": alice_id},
        "bob": {"email": "bob@example.test", "id": bob_id},
        "threads": {"one": id(101), "two": id(102), "bob": id(104)},
    });
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sdk/data.mjs");
    let output = tokio::process::Command::new("node")
        .arg(script)
        .env("RL_SDK_TEST", settings.to_string())
        .output()
        .await
        .unwrap();
    server.abort();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "node failed:\n{stdout}\n{stderr}");
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("RESULT "))
        .unwrap_or_else(|| panic!("no result:\n{stdout}\n{stderr}"));
    let out: Value = serde_json::from_str(line).unwrap();
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    assert_eq!(
        out["pages"],
        json!([["Alice two", "alice three"], ["Alice one"]])
    );
    assert_eq!(out["anonymous"]["status"], 401);
    assert_eq!(out["mutations"]["renamed"], "Renamed");
    assert_eq!(
        out["mutations"]["forged"],
        "GraphQL errors: Could not insert row with given values."
    );
}

/// `groupBy(...).aggregate(...)` on the basic model's todos (`tests/sdk/aggregate.mjs`).
#[tokio::test(flavor = "multi_thread")]
async fn the_sdk_data_client_aggregates_through_rayfin_local() {
    let app = App::new(common::basic_model()).await;
    let alice = app
        .auth
        .create_user("alice@example.test", PASSWORD)
        .await
        .unwrap()
        .id;
    let bob = "bob-user-id";
    let todo = |n: u32, user: &str, status: &str, priority: &str, numbers: &str, public: u8| {
        format!(
            "('{}', '{user}', 'todo {n}', '{status}', {priority}, {numbers}, {public}, \
             '2026-01-0{n}T00:00:00.0000000Z')",
            id(n)
        )
    };
    app.seed(&format!(
        "INSERT INTO \"Todos\" (id, user_id, title, status, priority, estimate, cost, weight,
           isPublic, createdAt) VALUES {}, {}, {}, {}, {}, {};",
        todo(1, &alice, "doing", "'low'", "3, 1.1, 2.25", 1),
        todo(2, &alice, "done", "NULL", "NULL, 10, NULL", 0),
        todo(3, &alice, "doing", "'high'", "4, 2.25, 1", 0),
        todo(4, bob, "doing", "'high'", "5, 99.99, NULL", 1),
        todo(5, bob, "done", "'low'", "7, 3, NULL", 0),
        todo(6, &alice, "done", "'low'", "3, 0.2, 0.1", 0),
    ));
    let settings = json!({"alice": {"email": "alice@example.test"}});
    let Some(out) = run_sdk_script(&app, "tests/sdk/aggregate.mjs", settings).await else {
        return;
    };
    assert_eq!(out["byStatus"][0]["aggregations"]["mean"], 34.446667);
    assert_eq!(
        out["forbidden"],
        "GraphQL errors: Access forbidden to field 'estimate' referenced in the aggregation \
         function 'sum'."
    );
}

/// Serves `app` (rl-graphql and rl-auth behind rl-server) and runs the SDK `script` with
/// `settings` plus the server's; returns its `RESULT`, or `None` (skipped) without Node or the SDK.
async fn run_sdk_script(app: &App, script: &str, mut settings: Value) -> Option<Value> {
    let Some(node_modules) = sdk_node_modules() else {
        eprintln!("skipped: the Rayfin SDK 1.36.2 node_modules aren't installed");
        return None;
    };
    let node = tokio::process::Command::new("node")
        .arg("--version")
        .output()
        .await;
    if node.is_err() {
        eprintln!("skipped: node isn't on PATH");
        return None;
    }
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let config = ServerConfig {
        api_origin: origin.clone(),
        publishable_key: KEY.into(),
        workspace_id: "11111111-1111-4111-8111-111111111111".into(),
        item_id: "22222222-2222-4222-8222-222222222222".into(),
        tenant_id: None,
        portal_url: format!("{origin}/.rayfin-local/portal"),
        allowed_origins: Vec::new(),
        allowed_hosts: None,
        static_site: None,
        publishable_key_exempt: rl_auth::PUBLISHABLE_KEY_EXEMPT_PATHS
            .iter()
            .map(|p| p.to_string())
            .collect(),
    };
    let router = build_router(
        config,
        vec![app.api.clone().router(), app.auth.clone().router()],
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    settings["nodeModules"] = json!(node_modules.to_string_lossy().replace('\\', "/"));
    settings["api"] = json!(origin);
    settings["publishableKey"] = json!(KEY);
    settings["password"] = json!(PASSWORD);
    let output = tokio::process::Command::new("node")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(script))
        .env("RL_SDK_TEST", settings.to_string())
        .output()
        .await
        .unwrap();
    server.abort();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "node failed:\n{stdout}\n{stderr}");
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("RESULT "))
        .unwrap_or_else(|| panic!("no result:\n{stdout}\n{stderr}"));
    let out: Value = serde_json::from_str(line).unwrap();
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    Some(out)
}
