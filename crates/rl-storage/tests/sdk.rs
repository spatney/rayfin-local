//! Drives the real `@microsoft/rayfin-storage` 1.36.2 client (`tests/sdk/storage.mjs`) against the
//! storage routes served by `rl-server`. Skipped (with a note) without Node.js or without
//! `tests/sdk/node_modules` (`npm install` in crates/rl-storage/tests/sdk).

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

use rl_db::Db;
use rl_model::{AppModel, Caller, CallerError, CallerResolver, Session};
use rl_server::ServerConfig;
use rl_storage::StorageConfig;
use serde_json::{Map, Value};

const TOKEN: &str = "sdk-test-token";
const PUBLISHABLE_KEY: &str = "pk-sdk-test";

/// Accepts one bearer token, as user `sdk-user`.
struct OneToken;

impl CallerResolver for OneToken {
    fn resolve(&self, bearer: Option<&str>) -> Result<Caller, CallerError> {
        match bearer {
            None => Ok(Caller::anonymous()),
            Some(TOKEN) => Ok(Caller::authenticated(Session {
                user_id: "sdk-user".into(),
                email: None,
                claims: Map::new(),
                access_token: TOKEN.into(),
            })),
            Some(_) => Err(CallerError::InvalidToken),
        }
    }
}

fn node() -> Option<OsString> {
    let node = std::env::var_os("RAYFIN_LOCAL_NODE").unwrap_or_else(|| "node".into());
    let found = Command::new(&node)
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    found.then_some(node)
}

fn sdk_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("sdk")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_rayfin_storage_client_uploads_lists_downloads_and_deletes() {
    let Some(node) = node() else {
        eprintln!("skipped: Node.js was not found (set RAYFIN_LOCAL_NODE)");
        return;
    };
    let sdk = sdk_dir();
    if !sdk.join("node_modules/@microsoft/rayfin-storage").is_dir() {
        eprintln!("skipped: run `npm install` in {}", sdk.display());
        return;
    }

    let analysis: Value = serde_json::from_str(include_str!(
        "../../rl-model/tests/fixtures/basic.analysis.json"
    ))
    .unwrap();
    let model = AppModel::from_analysis(&analysis).unwrap();
    let listeners = rl_server::bind("localhost", 0).await.unwrap();
    let origin = format!(
        "http://localhost:{}",
        listeners[0].local_addr().unwrap().port()
    );
    let dir = tempfile::tempdir().unwrap();
    let config = StorageConfig::new(dir.path().join("storage"), &origin);
    let db = Db::open_in_memory().unwrap();
    let storage = rl_storage::router(config, db, Arc::new(model), Arc::new(OneToken))
        .await
        .unwrap();
    let server = ServerConfig {
        api_origin: origin.clone(),
        publishable_key: PUBLISHABLE_KEY.into(),
        workspace_id: "workspace".into(),
        item_id: "item".into(),
        tenant_id: None,
        portal_url: format!("{origin}/.rayfin-local/portal"),
        allowed_origins: vec![],
        allowed_hosts: None,
        static_site: None,
        publishable_key_exempt: vec![],
    };
    let app = rl_server::build_router(server, vec![storage]);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(rl_server::serve(app, listeners, async {
        let _ = stopped.await;
    }));

    let output = tokio::process::Command::new(node)
        .arg("storage.mjs")
        .current_dir(&sdk)
        .env("RL_BASE_URL", &origin)
        .env("RL_PUBLISHABLE_KEY", PUBLISHABLE_KEY)
        .env("RL_TOKEN", TOKEN)
        .output()
        .await
        .unwrap();
    let _ = stop.send(());
    serving.await.unwrap().unwrap();
    assert!(
        output.status.success(),
        "storage.mjs failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
