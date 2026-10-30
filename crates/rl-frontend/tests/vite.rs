//! The dev server and the static build, run on copies of the reference app (explorer) and on small
//! Vite apps, with explorer's installed packages.
//!
//! Set `RAYFIN_LOCAL_EXPLORER` to explorer's folder; the default is `fabric-apps/apps/explorer`
//! next to this repository. Tests that need it or Node.js print a note and pass without them.
//!
//! The automatic sign-in test runs on the Vite and `@microsoft/rayfin-local-dev` installed in the
//! basic fixture (`npm ci` in `tests/fixtures/apps/basic`), and in the apps
//! `RAYFIN_LOCAL_TEST_VITE_APPS` lists (folders separated as in `PATH`), such as an app from a
//! Rayfin template with another major version of Vite.

use std::ffi::OsString;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Path as UrlPath;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use rl_frontend::{
    DevServer, DevServerConfig, FrontendError, RayfinEnv, StaticBuildConfig, build_static,
    frontend_env,
};
use rl_node::{LineSink, NodeRuntime, ProcessSpec, run_to_completion};
use rl_server::ServerConfig;
use serde_json::{Value, json};

const KEY: &str = "pk-local-test";
const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const ITEM: &str = "22222222-2222-4222-8222-222222222222";
const SIGN_IN_SECRET: &str = "the-run's-sign-in-secret";

/// A token response, as the backend's sign-in routes answer; `access_token` says which.
fn session(access_token: &str) -> Value {
    json!({
        "accessToken": access_token,
        "tokenType": "Bearer",
        "expiresIn": 3600,
        "refreshToken": null,
        "scope": null,
    })
}

/// Links every package folder of `argv[1]` into `argv[2]`, except Vite's caches.
const LINK_PACKAGES: &str = r"
const fs = require('fs'), path = require('path');
const [from, to] = process.argv.slice(1);
fs.mkdirSync(to, { recursive: true });
for (const name of fs.readdirSync(from)) {
  if (name === '.vite' || name === '.vite-temp') continue;
  const target = path.join(from, name);
  try { if (!fs.statSync(target).isDirectory()) continue; } catch { continue; }
  fs.symlinkSync(target, path.join(to, name), 'junction');
}";

type Lines = Arc<Mutex<Vec<String>>>;

fn collector() -> (LineSink, Lines) {
    let lines: Lines = Arc::default();
    let sink_lines = Arc::clone(&lines);
    let sink: LineSink = Arc::new(move |label, _, line| {
        sink_lines.lock().unwrap().push(format!("[{label}] {line}"));
    });
    (sink, lines)
}

fn dump(lines: &Lines) -> String {
    lines.lock().unwrap().join("\n")
}

fn explorer() -> Option<PathBuf> {
    let dir = std::env::var_os("RAYFIN_LOCAL_EXPLORER")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fabric-apps/apps/explorer")
        });
    if dir.join("node_modules/vite/package.json").is_file() {
        dunce::canonicalize(dir).ok()
    } else {
        eprintln!(
            "skipped: no installed reference app at {} (set RAYFIN_LOCAL_EXPLORER)",
            dir.display()
        );
        None
    }
}

async fn prerequisites() -> Option<(NodeRuntime, PathBuf)> {
    let explorer = explorer()?;
    match NodeRuntime::discover().await {
        Ok(node) => Some((node, explorer)),
        Err(error) => {
            eprintln!("skipped: {error}");
            None
        }
    }
}

/// A scratch app folder under the target dir.
struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn empty() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("rl-frontend-")
            .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
            .unwrap();
        Self { dir }
    }

    /// An app whose `node_modules` links to each of explorer's packages, so Vite's caches
    /// (`node_modules/.vite*`) are written here, never in explorer.
    async fn linked(node: &NodeRuntime, explorer: &Path) -> Self {
        let project = Self::empty();
        let args: [OsString; 4] = [
            "-e".into(),
            LINK_PACKAGES.into(),
            explorer.join("node_modules").into(),
            project.path().join("node_modules").into(),
        ];
        let (sink, lines) = collector();
        let spec = ProcessSpec::new("link", &node.path, project.path())
            .args(args)
            .sink(sink);
        run_to_completion(spec)
            .await
            .unwrap_or_else(|e| panic!("{e}\n{}", dump(&lines)));
        project
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// Copies what explorer's Vite dev server and build read (its `.env.local` included).
    fn copy_app(&self, explorer: &Path) {
        for item in [
            "package.json",
            "vite.config.ts",
            "index.html",
            "tsconfig.json",
            ".env.local",
            "src",
            "public",
            "rayfin/rayfin.yml",
            "rayfin/tsconfig.json",
            "rayfin/.env",
            "rayfin/connectors",
            "rayfin/data",
        ] {
            let source = explorer.join(item);
            if source.exists() {
                copy_tree(&source, &self.path().join(item));
            }
        }
    }

    fn dev_config(
        &self,
        node: &NodeRuntime,
        port: u16,
        backend: &str,
        sink: LineSink,
    ) -> DevServerConfig {
        DevServerConfig {
            node: node.clone(),
            frontend_dir: self.path().to_owned(),
            cache_dir: self.path().join(".rayfin-local").join("cache"),
            host: "127.0.0.1".to_owned(),
            port,
            backend_origin: backend.to_owned(),
            env: frontend_env(&local_env(backend)),
            sign_in_secret: None,
            sink,
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Unlink the package links before the folder is removed, so removal can't reach explorer.
        let Ok(entries) = std::fs::read_dir(self.path().join("node_modules")) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                let _ = std::fs::remove_dir(&path).or_else(|_| std::fs::remove_file(&path));
            }
        }
    }
}

fn copy_tree(source: &Path, target: &Path) {
    if source.is_dir() {
        std::fs::create_dir_all(target).unwrap();
        for entry in std::fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            copy_tree(&entry.path(), &target.join(entry.file_name()));
        }
    } else {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source, target).unwrap();
    }
}

fn local_env(origin: &str) -> RayfinEnv {
    RayfinEnv {
        api_origin: origin.to_owned(),
        functions_url: format!("{origin}/.rayfin-local/functions"),
        publishable_key: KEY.to_owned(),
        workspace_id: WORKSPACE.to_owned(),
        item_id: ITEM.to_owned(),
        tenant_id: None,
        portal_url: format!("{origin}/.rayfin-local/portal"),
        extra: vec![],
    }
}

/// The deployed values `rayfin env` wrote to an `.env.local` (none when there is no file).
fn deployed_values(env_file: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(env_file).unwrap_or_default();
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim(), value.trim().trim_matches('"')))
        .filter(|(name, value)| {
            (name.starts_with("VITE_RAYFIN_") || name.starts_with("VITE_FABRIC_"))
                && value.len() >= 8
        })
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// A stand-in backend on the real router (publishable-key and Host checks included).
struct Backend {
    origin: String,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Backend {
    async fn start() -> Self {
        let listeners = rl_server::bind("127.0.0.1", 0).await.unwrap();
        let origin = format!(
            "http://127.0.0.1:{}",
            listeners[0].local_addr().unwrap().port()
        );
        let config = ServerConfig {
            api_origin: origin.clone(),
            publishable_key: KEY.to_owned(),
            workspace_id: WORKSPACE.to_owned(),
            item_id: ITEM.to_owned(),
            tenant_id: None,
            portal_url: format!("{origin}/.rayfin-local/portal"),
            allowed_origins: vec![],
            allowed_hosts: Some(vec![]),
            static_site: None,
            publishable_key_exempt: vec![],
        };
        let routes = Router::new()
            .route(
                "/.well-known/jwks.json",
                get(|| async { Json(json!({"keys": [], "from": "backend"})) }),
            )
            .route("/api/ping", get(|| async { Json(json!({"pong": true})) }))
            .route(
                "/functions/{name}/invoke",
                any(|UrlPath(name): UrlPath<String>| async move {
                    Json(json!({"route": "invoke", "function": name}))
                }),
            )
            .route(
                "/.rayfin-local/functions/api/{name}",
                any(|UrlPath(name): UrlPath<String>| async move {
                    Json(json!({"route": "host", "function": name}))
                }),
            )
            .route(
                "/.rayfin-local/dev/session-token",
                get(|headers: HeaderMap| async move {
                    if headers
                        .get("x-rayfin-local-sign-in")
                        .map(|value| value.as_bytes())
                        != Some(SIGN_IN_SECRET.as_bytes())
                    {
                        return (StatusCode::FORBIDDEN, Json(json!({"error": "no secret"})));
                    }
                    (StatusCode::OK, Json(session("from-rayfin-local")))
                }),
            )
            // Where rayfinLocalDev()'s own automatic sign-in exchanges an Entra token.
            .route(
                "/api/auth/v1/brokered/token",
                post(|| async { Json(session("from-the-entra-exchange")) }),
            );
        let router = rl_server::build_router(config, vec![routes]);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(rl_server::serve(router, listeners, async {
            let _ = stopped.await;
        }));
        Self { origin, stop, task }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
    }
}

fn client() -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
}

async fn fetch(client: &reqwest::Client, method: &str, url: &str, accept: &str) -> (u16, String) {
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let response = client
        .request(method, url)
        .header("accept", accept)
        .header("x-publishable-key", KEY)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{url}: {e}"));
    (response.status().as_u16(), response.text().await.unwrap())
}

async fn fetch_json(client: &reqwest::Client, method: &str, url: &str) -> Value {
    let (status, body) = fetch(client, method, url, "application/json").await;
    assert_eq!(status, 200, "{method} {url}: {body}");
    serde_json::from_str(&body).unwrap_or_else(|e| panic!("{url}: {e}: {body}"))
}

fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

async fn wait_until_port_is_free(port: u16) -> bool {
    for _ in 0..50 {
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// The runtime config `rayfinLocalDev()` (or the shim) serves: this origin and the local values.
fn expected_runtime_config(vite: &str, backend: &str) -> Value {
    json!({
        "apiUrl": vite,
        "publishableKey": KEY,
        "workspaceId": WORKSPACE,
        "itemId": ITEM,
        "portalUrl": format!("{backend}/.rayfin-local/portal"),
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn explorer_dev_server_uses_the_local_backend() {
    let Some((node, explorer)) = prerequisites().await else {
        return;
    };
    let project = Project::linked(&node, &explorer).await;
    project.copy_app(&explorer);
    let backend = Backend::start().await;
    let port = free_port();
    let (sink, lines) = collector();
    let server = DevServer::start(project.dev_config(&node, port, &backend.origin, sink))
        .await
        .unwrap_or_else(|e| panic!("{e}\n{}", dump(&lines)));
    let vite = format!("http://127.0.0.1:{port}");
    assert_eq!(server.url(), format!("{vite}/"));
    assert!(server.uses_rayfin_plugin());
    assert!(
        !dump(&lines).contains("@@rayfin-local:"),
        "{}",
        dump(&lines)
    );

    let client = client();
    // Served by the app's rayfinLocalDev(); the deployed tenant in .env.local is blanked.
    let config = fetch_json(&client, "GET", &format!("{vite}/rayfin.config.json")).await;
    assert_eq!(config, expected_runtime_config(&vite, &backend.origin));

    let (status, index) = fetch(&client, "GET", &format!("{vite}/"), "text/html").await;
    assert_eq!(status, 200, "{index}");
    assert!(index.contains("/src/main.tsx") && index.contains("/@vite/client"));

    let url = format!("{vite}/src/services/bootstrap.ts");
    let (status, module) = fetch(&client, "GET", &url, "*/*").await;
    assert_eq!(status, 200, "{module}");
    for expected in [
        format!(r#""VITE_RAYFIN_API_URL": "{}/""#, backend.origin),
        format!(
            r#""VITE_FABRIC_PORTAL_URL": "{}/.rayfin-local/portal""#,
            backend.origin
        ),
        r#""VITE_FABRIC_TENANT_ID": """#.to_owned(),
        format!(r#""VITE_PORT": "{port}""#),
    ] {
        assert!(
            module.contains(&expected),
            "{expected} is missing:\n{module}"
        );
    }
    for (name, value) in deployed_values(&project.path().join(".env.local")) {
        assert!(
            !module.contains(&value),
            "the deployed {name} reached the app"
        );
    }

    let jwks = fetch_json(&client, "GET", &format!("{vite}/.well-known/jwks.json")).await;
    assert_eq!(jwks["from"], "backend");
    let ping = fetch_json(&client, "GET", &format!("{vite}/api/ping")).await;
    assert_eq!(ping, json!({"pong": true}));
    // rayfinLocalDev() sends function calls to `<functions_url>/api/<name>`.
    let invoked = fetch_json(&client, "POST", &format!("{vite}/functions/hello/invoke")).await;
    assert_eq!(invoked, json!({"route": "host", "function": "hello"}));

    server.stop().await.unwrap();
    assert!(wait_until_port_is_free(port).await, "Vite still listens");
    backend.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn apps_without_rayfin_local_dev_get_the_same_routes() {
    let Some((node, explorer)) = prerequisites().await else {
        return;
    };
    let project = Project::linked(&node, &explorer).await;
    project.write(
        "package.json",
        r#"{"name": "plain-vite-app", "private": true, "type": "module", "devDependencies": {"vite": "*"}}"#,
    );
    project.write(
        "index.html",
        r#"<!doctype html><script type="module" src="/main.js"></script>"#,
    );
    project.write(
        "main.js",
        "export const apiUrl = import.meta.env.VITE_RAYFIN_API_URL;\n",
    );
    // Templates pin the port from VITE_PORT; the port rayfin-local picks wins.
    project.write(
        "vite.config.mjs",
        "export default { server: { port: 1, strictPort: true } };\n",
    );
    project.write(
        ".env.local",
        "VITE_RAYFIN_API_URL=https://deployed.example.invalid/\n",
    );
    let backend = Backend::start().await;
    let port = free_port();
    let (sink, lines) = collector();
    let server = DevServer::start(project.dev_config(&node, port, &backend.origin, sink))
        .await
        .unwrap_or_else(|e| panic!("{e}\n{}", dump(&lines)));
    let vite = format!("http://127.0.0.1:{port}");
    assert_eq!(server.url(), format!("{vite}/"));
    assert!(!server.uses_rayfin_plugin());

    let client = client();
    let config = fetch_json(&client, "GET", &format!("{vite}/rayfin.config.json")).await;
    assert_eq!(config, expected_runtime_config(&vite, &backend.origin));
    let (status, module) = fetch(&client, "GET", &format!("{vite}/main.js"), "*/*").await;
    assert_eq!(status, 200, "{module}");
    assert!(module.contains(&format!("{}/", backend.origin)), "{module}");
    assert!(!module.contains("deployed.example.invalid"), "{module}");

    let jwks = fetch_json(&client, "GET", &format!("{vite}/.well-known/jwks.json")).await;
    assert_eq!(jwks["from"], "backend");
    let ping = fetch_json(&client, "GET", &format!("{vite}/api/ping")).await;
    assert_eq!(ping, json!({"pong": true}));
    let invoked = fetch_json(&client, "POST", &format!("{vite}/functions/hello/invoke")).await;
    assert_eq!(invoked, json!({"route": "invoke", "function": "hello"}));

    server.stop().await.unwrap();
    assert!(wait_until_port_is_free(port).await, "Vite still listens");
    backend.stop().await;
}

/// The apps whose installed Vite and `@microsoft/rayfin-local-dev` the automatic sign-in test
/// runs on: the basic fixture, and those `RAYFIN_LOCAL_TEST_VITE_APPS` lists.
fn vite_apps() -> Vec<PathBuf> {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/apps/basic");
    let listed = std::env::var_os("RAYFIN_LOCAL_TEST_VITE_APPS")
        .map(|apps| std::env::split_paths(&apps).collect::<Vec<_>>())
        .unwrap_or_default();
    std::iter::once(fixture)
        .chain(listed)
        .filter(|app| {
            ["vite", "@microsoft/rayfin-local-dev"]
                .iter()
                .all(|package| {
                    let manifest = app.join("node_modules").join(package).join("package.json");
                    manifest.is_file()
                })
        })
        .filter_map(|app| dunce::canonicalize(app).ok())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dev_server_signs_the_app_in_before_rayfin_local_dev_does() {
    let node = match NodeRuntime::discover().await {
        Ok(node) => node,
        Err(error) => {
            eprintln!("skipped: {error}");
            return;
        }
    };
    let apps = vite_apps();
    if apps.is_empty() {
        eprintln!(
            "skipped: no app has Vite and @microsoft/rayfin-local-dev installed (run `npm ci` in \
             tests/fixtures/apps/basic)"
        );
    }
    for app in apps {
        let project = Project::linked(&node, &app).await;
        project.write(
            "package.json",
            r#"{"name": "sign-in-test", "private": true, "type": "module"}"#,
        );
        project.write("index.html", "<!doctype html><title>sign-in test</title>");
        // The template's plugin with its automatic sign-in; the config notes whether it can see
        // the sign-in secret.
        project.write(
            "vite.config.mjs",
            "import { writeFileSync } from 'node:fs';\n\
             import { rayfinLocalDev } from '@microsoft/rayfin-local-dev/vite';\n\
             writeFileSync('secret-seen.txt', process.env.RAYFIN_LOCAL_SIGN_IN_SECRET ?? '');\n\
             export default { plugins: [rayfinLocalDev({ autoLogin: true })] };\n",
        );
        let backend = Backend::start().await;
        let client = client();
        let mut answered_by = Vec::new();
        let mut versions = Vec::new();
        for secret in [None, Some(SIGN_IN_SECRET)] {
            let port = free_port();
            let (sink, lines) = collector();
            let mut config = project.dev_config(&node, port, &backend.origin, sink);
            config.sign_in_secret = secret.map(str::to_owned);
            // What rayfinLocalDev()'s own sign-in uses instead of an Entra sign-in in a browser.
            config
                .env
                .push(("RAYFIN_TOKEN".to_owned(), "ambient-test-token".to_owned()));
            let server = DevServer::start(config)
                .await
                .unwrap_or_else(|e| panic!("{e}\n{}", dump(&lines)));
            let vite = server.vite_version().to_owned();
            // As the app's page asks for it: a same-origin GET, so without an Origin.
            let url = format!("http://127.0.0.1:{port}/.rayfin/dev/session-token");
            let (status, body) = fetch(&client, "GET", &url, "application/json").await;
            assert_eq!(status, 200, "Vite {vite}: {body}\n{}", dump(&lines));
            let token: Value = serde_json::from_str(&body).unwrap();
            answered_by.push(token["accessToken"].as_str().unwrap_or_default().to_owned());
            if secret.is_some() {
                let other_page = client
                    .get(&url)
                    .header("origin", "http://localhost:3000")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(other_page.status(), 403, "Vite {vite}");
            }
            server.stop().await.unwrap();
            versions.push(vite);
        }
        eprintln!("signed in through Vite {}: {}", versions[0], app.display());
        assert_eq!(
            answered_by,
            ["from-the-entra-exchange", "from-rayfin-local"],
            "{}",
            app.display()
        );
        let seen = std::fs::read_to_string(project.path().join("secret-seen.txt")).unwrap();
        assert_eq!(seen, "", "the app's config saw the sign-in secret");
        backend.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn startup_failures_are_reported() {
    let Some((node, explorer)) = prerequisites().await else {
        return;
    };
    let project = Project::linked(&node, &explorer).await;
    project.write("package.json", r#"{"devDependencies": {"vite": "*"}}"#);
    project.write(
        "vite.config.mjs",
        "throw new Error('rl-frontend test: broken config');\n",
    );
    let unused_backend = "http://127.0.0.1:9";

    let (sink, lines) = collector();
    let config = project.dev_config(&node, free_port(), unused_backend, sink);
    let error = DevServer::start(config).await.unwrap_err();
    assert!(
        matches!(error, FrontendError::ViteFailed { .. })
            && error
                .to_string()
                .contains("rl-frontend test: broken config"),
        "{error}\n{}",
        dump(&lines)
    );

    let taken = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let (sink, _) = collector();
    let config = project.dev_config(&node, port, unused_backend, sink);
    let error = DevServer::start(config).await.unwrap_err();
    assert!(
        matches!(error, FrontendError::PortInUse { port: p } if p == port),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_vite_is_reported_before_starting() {
    let project = Project::empty();
    project.write("package.json", r#"{"devDependencies": {"vite": "^7.0.0"}}"#);
    let node = NodeRuntime {
        path: "node".into(),
        version: "24.0.0".to_owned(),
        major: 24,
    };
    let (sink, _) = collector();
    let config = project.dev_config(&node, 0, "http://127.0.0.1:9", sink);
    let error = DevServer::start(config).await.unwrap_err();
    assert!(
        matches!(error, FrontendError::ViteNotInstalled { .. })
            && error.to_string().contains("npm install"),
        "{error}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn explorer_static_build_uses_the_local_values() {
    let Some((node, explorer)) = prerequisites().await else {
        return;
    };
    let project = Project::linked(&node, &explorer).await;
    project.copy_app(&explorer);
    let env_local = project.path().join(".env.local");
    let _ = std::fs::remove_file(&env_local);
    let origin = "http://localhost:7171";
    let (sink, lines) = collector();
    let dist = build_static(StaticBuildConfig {
        frontend_dir: project.path().to_owned(),
        // The template's `prebuild` (`rayfin env`, which rewrites .env.local with the deployed
        // values), then its Vite build without the type check.
        build_command: Some("npm run prebuild && npx vite build --logLevel warn".to_owned()),
        output_dir: "dist".into(),
        index_document: None,
        env: frontend_env(&local_env(origin)),
        sink,
    })
    .await
    .unwrap_or_else(|e| panic!("{e}\n{}", dump(&lines)));
    assert_eq!(dist, project.path().join("dist"));
    assert!(
        env_local.is_file(),
        "prebuild did not run:\n{}",
        dump(&lines)
    );

    let mut bundle = String::new();
    for entry in std::fs::read_dir(dist.join("assets")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "js") {
            bundle.push_str(&std::fs::read_to_string(path).unwrap());
        }
    }
    for expected in [
        format!("{origin}/"),
        format!("{origin}/.rayfin-local/portal"),
        KEY.to_owned(),
        WORKSPACE.to_owned(),
        ITEM.to_owned(),
    ] {
        assert!(
            bundle.contains(&expected),
            "{expected} is not in the bundle"
        );
    }
    for (name, value) in deployed_values(&env_local) {
        assert!(
            !bundle.contains(&value),
            "the deployed {name} reached the bundle"
        );
    }
}

#[tokio::test]
async fn build_failures_are_actionable() {
    let project = Project::empty();
    let build = |command: &str| {
        let (sink, _) = collector();
        build_static(StaticBuildConfig {
            frontend_dir: project.path().to_owned(),
            build_command: Some(command.to_owned()),
            output_dir: "dist".into(),
            index_document: None,
            env: vec![],
            sink,
        })
    };

    let error = build("exit 3").await.unwrap_err();
    assert!(
        matches!(&error, FrontendError::BuildFailed { status, .. } if status.code() == Some(3)),
        "{error}"
    );
    let error = build("echo built").await.unwrap_err();
    let FrontendError::MissingBuildOutput { index, .. } = &error else {
        panic!("{error}");
    };
    assert!(
        index.ends_with(Path::new("dist").join("index.html")),
        "{error}"
    );
}
