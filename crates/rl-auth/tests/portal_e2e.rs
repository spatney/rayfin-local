//! The sign-in popup end to end: a real browser (Playwright) runs the real
//! `ensureSignedInWithFabric` (SDK 1.36.2) against the auth routes behind `rl-server`, and signs
//! in with a password, by signing up, with "Continue as" (a fake developer token), then cancels
//! and closes the popup. It also runs the real `initEmbeddedAuth` in the local Fabric host's frame:
//! signs in on the host, reloads the page and the app, and switches to a new account. A second
//! test signs in through the popup without asking, as the backend's sign-in identity.
//!
//! Ignored by default. Set up once: `npm install` and `npx playwright install chromium` in
//! `tests/e2e/portal` (without the Chromium download it falls back to the installed Edge). The
//! SDK comes from `RAYFIN_SDK_NODE_MODULES`, by default the explorer app next to this repository
//! (`../fabric-apps/apps/explorer/node_modules`). Run with
//! `cargo test -p rl-auth --test portal_e2e -- --ignored --nocapture`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rl_auth::{AuthService, AuthSettings, PORTAL_PATH, SignInAs, User};
use rl_db::Db;
use rl_tokens::{AccessToken, BoxFuture, TokenError, TokenSource};
use serde_json::{Value, json};

const APP_ID: &str = "6c6f6361-6c00-4000-8000-0000000000e2";
const WORKSPACE_ID: &str = "6c6f6361-6c00-4000-8000-0000000000e3";
const PUBLISHABLE_KEY: &str = "pk-portal-e2e";
const USER: &str = "ada@contoso.test";
const PASSWORD: &str = "correct horse";
const NEW_USER: &str = "grace@contoso.test";
const EMBED_USER: &str = "hopper@contoso.test";
/// The local user the backend signs in as automatically, created without a password.
const PREVIEW_USER: &str = "preview@contoso.test";

/// The developer's identity for "Continue as": an unsigned token with a user's claims.
struct DeveloperTokens;

impl TokenSource for DeveloperTokens {
    fn token<'a>(&'a self, _scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
        let claims = json!({
            "aud": "https://analysis.windows.net/powerbi/api",
            "oid": "7d3c6c58-36b7-4d5f-9d7a-0d5e8c1f2a3b",
            "tid": "72f988bf-86f1-41af-91ab-2d7cd011db47",
            "upn": "dev@contoso.test",
            "name": "Dev Eloper",
            "scp": "user_impersonation",
        });
        let token = format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        Box::pin(async move {
            Ok(AccessToken {
                token,
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            })
        })
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sdk_node_modules() -> PathBuf {
    std::env::var_os("RAYFIN_SDK_NODE_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|| repo_root().join("../fabric-apps/apps/explorer/node_modules"))
}

/// Where `run.mjs` and the SDK are, or `None` (with a SKIPPED line) when one is missing.
fn prerequisites() -> Option<(PathBuf, PathBuf)> {
    let harness = repo_root().join("tests/e2e/portal");
    let sdk = sdk_node_modules();
    for (needed, what) in [
        (
            harness.join("node_modules/playwright"),
            "run `npm install` in tests/e2e/portal",
        ),
        (
            sdk.join("@microsoft/rayfin-auth-provider-fabric"),
            "set RAYFIN_SDK_NODE_MODULES to a node_modules with the Rayfin SDK 1.36.2",
        ),
    ] {
        if !needed.exists() {
            println!("SKIPPED: {} is missing: {what}", needed.display());
            return None;
        }
    }
    Some((harness, sdk))
}

/// The auth routes behind `rl-server` on a free port, with a password user, signing in
/// automatically as `identity`.
struct Backend {
    auth: Arc<AuthService>,
    user: User,
    api: String,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}

async fn start(identity: SignInAs) -> Backend {
    let dir = tempfile::tempdir().unwrap();
    let mut settings = AuthSettings::new(APP_ID);
    settings.fabric_enabled = true;
    settings.app_name = Some("Portal E2E".into());
    settings.entra_signin.enabled = true;
    let auth = AuthService::new(
        Db::open_in_memory().unwrap(),
        dir.path().join("keys"),
        settings,
    )
    .await
    .unwrap()
    .with_entra(Arc::new(DeveloperTokens));
    auth.set_sign_in_as(identity);
    let user = auth.create_user(USER, PASSWORD).await.unwrap();
    let auth = Arc::new(auth);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let router = rl_server::build_router(
        rl_server::ServerConfig {
            api_origin: api.clone(),
            publishable_key: PUBLISHABLE_KEY.into(),
            workspace_id: WORKSPACE_ID.into(),
            item_id: APP_ID.into(),
            tenant_id: None,
            portal_url: format!("{api}{PORTAL_PATH}"),
            allowed_origins: Vec::new(),
            allowed_hosts: Some(Vec::new()),
            static_site: None,
            publishable_key_exempt: rl_auth::PUBLISHABLE_KEY_EXEMPT_PATHS
                .iter()
                .map(|path| (*path).to_owned())
                .collect(),
        },
        vec![Arc::clone(&auth).router()],
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    Backend {
        auth,
        user,
        api,
        server,
        _dir: dir,
    }
}

/// Runs `run.mjs`'s `scenarios` (comma-separated) against `backend`; returns what they observed.
async fn run(backend: &Backend, harness: &Path, sdk: &Path, scenarios: &str) -> Value {
    let output = tokio::process::Command::new("node")
        .arg(harness.join("run.mjs"))
        .current_dir(harness)
        .env("RL_API_URL", &backend.api)
        .env("RL_PUBLISHABLE_KEY", PUBLISHABLE_KEY)
        .env("RL_WORKSPACE_ID", WORKSPACE_ID)
        .env("RL_ITEM_ID", APP_ID)
        .env("RL_SDK_NODE_MODULES", sdk)
        .env("RL_USER", USER)
        .env("RL_PASSWORD", PASSWORD)
        .env("RL_NEW_USER", NEW_USER)
        .env("RL_EMBED_USER", EMBED_USER)
        .env("RL_SCENARIOS", scenarios)
        .output()
        .await
        .expect("node runs (is Node on PATH?)");
    backend.server.abort();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "node failed:\n{stdout}\n{stderr}");
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("RESULT "))
        .unwrap_or_else(|| panic!("no result:\n{stdout}\n{stderr}"));
    let out: Value = serde_json::from_str(line).unwrap();
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    if !stderr.trim().is_empty() {
        println!("node stderr:\n{stderr}");
    }
    out
}

#[tokio::test]
#[ignore = "needs Node, Playwright in tests/e2e/portal and the SDK 1.36.2 (see the module docs)"]
async fn the_sdk_signs_in_through_the_portal_popup() {
    let Some((harness, sdk)) = prerequisites() else {
        return;
    };
    // Nobody to sign in as automatically, so the popup asks.
    let backend = start(SignInAs::None).await;
    let scenarios = "password,signup,entra,cancel,closed,noOpener,embedded";
    let out = run(&backend, &harness, &sdk, scenarios).await;
    let (auth, user) = (&backend.auth, &backend.user);
    let password = &out["password"];
    assert_eq!(password["target"], out["appOrigin"]);
    assert_eq!(
        password["wrongPassword"],
        "The username or password is wrong."
    );
    let signed_in = &password["result"];
    assert_eq!(signed_in["ok"], true, "{password}");
    assert_eq!(signed_in["session"]["isAuthenticated"], true);
    assert_eq!(signed_in["session"]["user"]["id"], user.id.as_str());
    assert_eq!(signed_in["session"]["user"]["email"], USER);
    assert_eq!(signed_in["refreshed"], true);
    assert_eq!(password["popupClosed"], true);

    let signed_up = &out["signup"]["result"];
    assert_eq!(signed_up["ok"], true, "{}", out["signup"]);
    assert_eq!(signed_up["session"]["user"]["email"], NEW_USER);
    let created = auth.find_user(NEW_USER).await.unwrap().unwrap();
    assert_eq!(signed_up["session"]["user"]["id"], created.id.as_str());

    let entra = &out["entra"];
    assert_eq!(entra["button"], "Continue as Dev Eloper (dev@contoso.test)");
    assert_eq!(entra["result"]["ok"], true, "{entra}");
    assert_eq!(
        entra["result"]["session"]["user"]["email"],
        "dev@contoso.test"
    );
    assert_eq!(entra["result"]["refreshed"], true);

    for cancelled in ["cancel", "closed"] {
        let result = &out[cancelled]["result"];
        assert_eq!(result["ok"], false, "{cancelled}: {}", out[cancelled]);
        assert_eq!(result["error"]["code"], "access_denied", "{cancelled}");
        assert_eq!(result["error"]["message"], "Sign-in was cancelled.");
        assert!(result["ms"].as_u64().unwrap() < 30_000, "{cancelled}");
    }
    assert_eq!(out["noOpener"]["title"], "Open this from the app");

    let embedded = &out["embedded"];
    assert_eq!(embedded["framedBeforeSignIn"], 0, "{embedded}");
    assert_eq!(embedded["user"], format!("Signed in as {USER}"));
    assert_eq!(embedded["formAfterReload"], false);
    for (step, email) in [
        ("first", USER),
        ("reloaded", USER),
        ("reloadedApp", USER),
        ("switched", EMBED_USER),
    ] {
        let result = &embedded[step];
        assert_eq!(result["ok"], true, "{step}: {embedded}");
        assert_eq!(result["session"]["user"]["email"], email, "{step}");
        assert_eq!(result["refreshed"], true, "{step}");
        assert_eq!(result["unsupported"]["code"], "NOT_SUPPORTED", "{step}");
        assert!(
            result["unsupported"]["ms"].as_u64().unwrap() < 5_000,
            "{step}"
        );
    }
    assert_eq!(embedded["first"]["session"]["user"]["id"], user.id.as_str());
    let hopper = auth.find_user(EMBED_USER).await.unwrap().unwrap();
    assert_eq!(
        embedded["switched"]["session"]["user"]["id"],
        hopper.id.as_str()
    );
    assert_eq!(
        embedded["switchedUser"],
        format!("Signed in as {EMBED_USER}")
    );
}

#[tokio::test]
#[ignore = "needs Node, Playwright in tests/e2e/portal and the SDK 1.36.2 (see the module docs)"]
async fn the_popup_signs_in_as_the_sign_in_identity_without_asking() {
    let Some((harness, sdk)) = prerequisites() else {
        return;
    };
    let backend = start(SignInAs::User(PREVIEW_USER.into())).await;
    let out = run(&backend, &harness, &sdk, "identity").await;
    let identity = &out["identity"];
    let result = &identity["result"];
    assert_eq!(result["ok"], true, "{identity}");
    assert_eq!(result["session"]["user"]["email"], PREVIEW_USER);
    assert_eq!(result["refreshed"], true);
    let previewed = backend.auth.find_user(PREVIEW_USER).await.unwrap().unwrap();
    assert_eq!(result["session"]["user"]["id"], previewed.id.as_str());
    assert_eq!(identity["popupClosed"], true);
}
