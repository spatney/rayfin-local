//! The invoke routes end to end: `FunctionsService` on real Node.js with the fixture package
//! `tests/fixtures/functions/basic` (installed and built on first use), fake callers and tokens,
//! requests through `router()`. The manual smoke test runs explorer's functions with `az` tokens.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant, SystemTime},
};

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Method, Request, StatusCode, header},
};
use http_body_util::BodyExt;
use rl_functions::{FunctionsConfig, FunctionsService};
use rl_model::{Caller, CallerError, CallerResolver, Session};
use rl_node::{LineSink, NodeRuntime};
use rl_tokens::{AccessToken, Audience, BoxFuture, TokenError, TokenSource};
use serde_json::{Map, Value, json};
use tower::ServiceExt;

const API_ORIGIN: &str = "http://127.0.0.1:9";
/// The fixture is built once by [`fixture`]; services on it skip the build.
const NO_BUILD: &str = "exit 0";
const SECRET: &str = "s3cr€t-from-config";
const DEFAULT_EXPLORER: &str = r"C:\Users\sapatney\code\fabric-apps\apps\explorer\rayfin\functions";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// The fixture package, `npm ci`-installed and built once per run.
fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let dir = std::path::absolute(repo_root().join("tests/fixtures/functions/basic")).unwrap();
        if !dir
            .join("node_modules/@microsoft/fabric-user-data-functions")
            .is_dir()
        {
            npm(&dir, "ci --no-audit --no-fund");
        }
        npm(&dir, "run build");
        dir
    })
}

fn npm(dir: &Path, args: &str) {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/d", "/s", "/c", &format!("npm {args}")]);
        command
    } else {
        let mut command = Command::new("sh");
        command.args(["-c", &format!("npm {args}")]);
        command
    };
    let status = command.current_dir(dir).status().expect("npm runs");
    assert!(status.success(), "`npm {args}` failed in {}", dir.display());
}

fn scratch() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn cache_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("functions-invoke-cache")
}

async fn node() -> NodeRuntime {
    NodeRuntime::discover()
        .await
        .expect("Node.js 20+ is needed for these tests")
}

fn sink() -> LineSink {
    Arc::new(|label, _stream, line| eprintln!("[{label}] {line}"))
}

/// Accepts the bearer token `good`, rejects every other one.
struct Callers;

impl CallerResolver for Callers {
    fn resolve(&self, bearer: Option<&str>) -> Result<Caller, CallerError> {
        match bearer {
            None => Ok(Caller::anonymous()),
            Some("good") => Ok(Caller::authenticated(Session {
                user_id: "user-1".into(),
                email: None,
                claims: Map::new(),
                access_token: "good".into(),
            })),
            Some(_) => Err(CallerError::InvalidToken),
        }
    }
}

/// JWT-like tokens whose length depends on the scope; fails for `failing`.
#[derive(Default)]
struct Tokens {
    failing: Option<&'static str>,
    requested: Mutex<Vec<String>>,
}

impl Tokens {
    fn token_for(scope: &str) -> String {
        let length = if scope == Audience::Ado.default_scope() {
            11
        } else {
            23
        };
        format!("eyJ{}", "x".repeat(length - 3))
    }
}

impl TokenSource for Tokens {
    fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
        Box::pin(async move {
            self.requested.lock().unwrap().push(scope.to_owned());
            if self.failing == Some(scope) {
                return Err(TokenError {
                    scope: scope.to_owned(),
                    message: "no consent for this resource".into(),
                    hint: Some("Run `az login`.".into()),
                });
            }
            Ok(AccessToken {
                token: Self::token_for(scope),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            })
        })
    }
}

async fn fixture_config() -> FunctionsConfig {
    let mut config =
        FunctionsConfig::new(node().await, fixture(), cache_dir(), API_ORIGIN, "pk_local");
    config.build_command = Some(NO_BUILD.into());
    config.secrets = vec![("API_KEY".into(), SECRET.into())];
    config.sink = sink();
    config
}

async fn start(config: FunctionsConfig, tokens: Arc<Tokens>) -> Arc<FunctionsService> {
    FunctionsService::start(config, Arc::new(Callers), tokens)
        .await
        .unwrap()
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&self.body)))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The envelope's `output`, after checking the call succeeded.
    fn output(&self) -> Value {
        let envelope = self.json();
        assert_eq!(self.status, StatusCode::OK, "{envelope}");
        assert_eq!(envelope["status"], "Succeeded", "{envelope}");
        envelope["output"].clone()
    }

    /// `(error, message)` of an error answer.
    fn error(&self) -> (String, String) {
        let body = self.json();
        let text = |key: &str| body[key].as_str().unwrap_or_default().to_owned();
        (text("error"), text("message"))
    }
}

fn post(path: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
}

async fn send(router: &Router, request: Request<Body>) -> Reply {
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body,
    }
}

async fn invoke(router: &Router, name: &str, body: Value) -> Reply {
    let request = post(&format!("/functions/{name}/invoke"))
        .body(Body::from(body.to_string()))
        .unwrap();
    send(router, request).await
}

/// Invokes `name` until `done` accepts the reply.
async fn eventually(
    router: &Router,
    name: &str,
    body: Value,
    within: Duration,
    done: impl Fn(&Reply) -> bool,
) -> Reply {
    let deadline = Instant::now() + within;
    loop {
        let reply = invoke(router, name, body.clone()).await;
        if done(&reply) || Instant::now() > deadline {
            return reply;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[tokio::test]
async fn functions_answer_with_the_deployed_envelope() {
    let service = start(fixture_config().await, Arc::default()).await;
    let router = service.router();
    assert!(service.functions().contains(&"hello".to_owned()));
    assert!(!service.functions().contains(&"nightly".to_owned()));

    // The SDK's route, with an empty body.
    let hello = send(
        &router,
        post("/functions/hello/invoke").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(hello.status, StatusCode::OK);
    assert_eq!(hello.header("content-type"), Some("application/json"));
    assert_eq!(hello.header("x-fabric-udf-status"), Some("Succeeded"));
    let envelope = hello.json();
    assert_eq!(envelope["functionName"], "hello");
    assert_eq!(envelope["output"], "Hello from rayfin-local!");
    assert_eq!(envelope["errors"], json!([]));
    let id = envelope["invocationId"].as_str().unwrap();
    assert!(id.len() == 36 && id.matches('-').count() == 4, "{id}");

    // The Vite plugin's rewrite target, with objects, arrays and nulls.
    let args = json!({
        "text": "hi", "count": 7, "flag": true, "options": {"nested": [1, {"a": null}]},
        "tags": ["x"], "scores": [], "note": null
    });
    let echo = send(
        &router,
        post("/.rayfin-local/functions/api/echo")
            .body(Body::from(args.to_string()))
            .unwrap(),
    )
    .await;
    let mut expected = args.clone();
    expected["greeting"] = json!("hi");
    assert_eq!(echo.output(), expected);

    // Failed envelopes are still HTTP 200 (IsLocal=true in local.settings.json is overridden).
    let missing = invoke(&router, "echo", json!({})).await;
    assert_eq!(missing.status, StatusCode::OK);
    assert_eq!(missing.header("x-fabric-udf-status"), Some("BadRequest"));
    assert_eq!(missing.json()["errors"][0]["errorCode"], "MissingInput");
    let thrown = invoke(&router, "reject", json!({"reason": "r"})).await;
    assert_eq!(
        (thrown.status, thrown.header("x-fabric-udf-status")),
        (StatusCode::OK, Some("BadRequest"))
    );
    assert_eq!(thrown.json()["errors"][0]["errorCode"], "UserThrown");
    let failed = invoke(&router, "fail", json!({})).await;
    assert_eq!(
        (failed.status, failed.header("x-fabric-udf-status")),
        (StatusCode::OK, Some("Failed"))
    );

    // No envelope at all: the deployed route's 500.
    let bigint = invoke(&router, "bigint", json!({})).await;
    assert_eq!(bigint.status, StatusCode::INTERNAL_SERVER_ERROR);
    let (code, message) = bigint.error();
    assert_eq!(code, "FUNCTION_INVOKE_FAILED");
    assert!(message.contains("BigInt"), "{message}");

    service.stop().await;
    let stopped = invoke(&router, "hello", json!({})).await;
    assert_eq!(stopped.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn requests_are_checked_in_the_deployed_order() {
    let service = start(fixture_config().await, Arc::default()).await;
    let router = service.router();

    let bad_token = send(
        &router,
        post("/functions/hello/invoke")
            .header(header::AUTHORIZATION, "Bearer bad")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(bad_token.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        bad_token.json(),
        json!({"error": "Invalid authentication token"})
    );

    let unknown = invoke(&router, "missing", json!({})).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    let (code, message) = unknown.error();
    assert_eq!(code, "FUNCTION_NOT_FOUND");
    assert!(message.contains("hello"), "lists the functions: {message}");
    let timer = invoke(&router, "nightly", json!({})).await;
    assert_eq!(timer.status, StatusCode::NOT_FOUND);

    let invalid_name = invoke(&router, "get-backlog", json!({})).await;
    assert_eq!(invalid_name.status, StatusCode::BAD_REQUEST);
    assert_eq!(invalid_name.error().0, "FUNCTION_INVALID_NAME");

    let text = send(
        &router,
        Request::post("/functions/hello/invoke")
            .header(header::CONTENT_TYPE, "text/plain")
            .body(Body::from("hello"))
            .unwrap(),
    )
    .await;
    assert_eq!(text.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(
        text.error(),
        (
            "FUNCTION_INVOKE_INVALID_CONTENT_TYPE".into(),
            "Only application/json content type is supported. Received: text/plain".into()
        )
    );
    let not_json = send(
        &router,
        post("/functions/hello/invoke")
            .body(Body::from("{\"name\": "))
            .unwrap(),
    )
    .await;
    assert_eq!(not_json.status, StatusCode::BAD_REQUEST);
    assert_eq!(not_json.error().0, "FUNCTION_INVOKE_INVALID_BODY");

    let huge = send(
        &router,
        post("/functions/hello/invoke")
            .body(Body::from(vec![b' '; 30 * 1024 * 1024 + 1]))
            .unwrap(),
    )
    .await;
    assert_eq!(huge.status, StatusCode::PAYLOAD_TOO_LARGE);

    let get = send(
        &router,
        Request::get("/functions/hello/invoke")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(get.status, StatusCode::METHOD_NOT_ALLOWED);
    service.stop().await;
}

#[tokio::test]
async fn functions_get_the_caller_secrets_and_tokens() {
    let tokens = Arc::new(Tokens::default());
    let service = start(fixture_config().await, tokens.clone()).await;
    let router = service.router();

    let signed_in = send(
        &router,
        post("/functions/whoami/invoke")
            .header(header::AUTHORIZATION, "Bearer good")
            .body(Body::from("{}"))
            .unwrap(),
    )
    .await;
    assert_eq!(
        signed_in.output(),
        json!({"baseUrl": format!("{API_ORIGIN}/"), "hasPublishableKey": true, "hasAccessToken": true})
    );
    let anonymous = invoke(&router, "whoami", json!({})).await;
    assert_eq!(anonymous.output()["hasAccessToken"], false);

    // Non-ASCII survives the header (the fixture's local.settings.json value is shorter).
    let secret = invoke(&router, "secret", json!({})).await;
    assert_eq!(
        secret.output(),
        json!({"length": SECRET.encode_utf16().count()})
    );

    let minted = invoke(&router, "tokens", json!({})).await;
    assert_eq!(
        minted.output(),
        json!({"audiences": ["ADO", "Fabric"], "adoLength": 11, "fabricLength": 23})
    );
    let mut requested = tokens.requested.lock().unwrap().clone();
    requested.sort();
    assert_eq!(
        requested,
        [
            Audience::Ado.default_scope(),
            Audience::Fabric.default_scope()
        ]
    );
    service.stop().await;

    // A token that can't be minted: the function runs, and reading it throws.
    let failing = Arc::new(Tokens {
        failing: Some(Audience::Fabric.default_scope()),
        ..Tokens::default()
    });
    let service = start(fixture_config().await, failing).await;
    let reply = invoke(&service.router(), "tokens", json!({})).await;
    assert_eq!(
        (reply.status, reply.header("x-fabric-udf-status")),
        (StatusCode::OK, Some("Failed"))
    );
    let message = reply.json()["errors"][0]["message"].clone();
    assert!(
        message
            .as_str()
            .unwrap()
            .contains("No token available for audience 'Fabric'"),
        "{message}"
    );
    service.stop().await;
}

#[tokio::test]
async fn a_timeout_answers_408_and_the_host_is_replaced() {
    let mut config = fixture_config().await;
    config.invoke_timeout = Duration::from_millis(1500);
    let service = start(config, Arc::default()).await;
    let router = service.router();

    assert_eq!(invoke(&router, "counter", json!({})).await.output(), 1);
    let slow = invoke(&router, "slow", json!({"ms": 20_000})).await;
    assert_eq!(slow.status, StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        slow.error(),
        (
            "FUNCTION_INVOKE_TIMEOUT".into(),
            "Function 'slow' did not respond within 1.5s.".into()
        )
    );
    // A new host, with fresh module state.
    assert_eq!(invoke(&router, "counter", json!({})).await.output(), 1);
    service.stop().await;
}

/// A copy of a functions package whose `node_modules` links to the original's.
struct PackageCopy {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl PackageCopy {
    fn of(source: &Path) -> Self {
        let dir = scratch();
        let path = dir.path().join("functions");
        copy_tree(source, &path);
        link_dir(&source.join("node_modules"), &path.join("node_modules"));
        Self { _dir: dir, path }
    }

    fn edit(&self, file: &str, change: impl FnOnce(String) -> String) {
        let path = self.path.join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, change(text)).unwrap();
    }

    async fn config(&self) -> FunctionsConfig {
        let mut config = FunctionsConfig::new(
            node().await,
            &self.path,
            self._dir.path().join("cache"),
            API_ORIGIN,
            "pk_local",
        );
        config.sink = sink();
        config
    }
}

impl Drop for PackageCopy {
    fn drop(&mut self) {
        // Remove the link itself first, so that nothing behind it is touched.
        let link = self.path.join("node_modules");
        let _ = std::fs::remove_dir(&link).or_else(|_| std::fs::remove_file(&link));
    }
}

/// Copies a package without `node_modules` and `dist`.
fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "node_modules" || name == "dist" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &to.join(&name));
        } else {
            std::fs::copy(entry.path(), to.join(&name)).unwrap();
        }
    }
}

fn link_dir(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let status = Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J {}", link.display());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

#[tokio::test]
async fn rebuilt_functions_are_reloaded() {
    let package = PackageCopy::of(fixture());
    let mut config = package.config().await;
    let inspector = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    config.inspect_port = Some(inspector);
    let service = start(config, Arc::default()).await;
    let router = service.router();
    assert_eq!(
        invoke(&router, "hello", json!({})).await.output(),
        "Hello from rayfin-local!"
    );

    // A changed function and a new one with a parameter, which binds only with fresh metadata.
    package.edit("src/function_app.ts", |source| {
        source.replace("Hello from rayfin-local!", "Hello after a rebuild!")
            + "\nudf.func('added', async (who: string) => `hi ${who}`);\n"
    });
    let hello = eventually(
        &router,
        "hello",
        json!({}),
        Duration::from_secs(90),
        |reply| {
            reply.status == StatusCode::OK && reply.json()["output"] == "Hello after a rebuild!"
        },
    )
    .await;
    assert_eq!(hello.output(), "Hello after a rebuild!");
    let added = invoke(&router, "added", json!({"who": "Ada"})).await;
    assert_eq!(added.output(), "hi Ada");
    assert!(service.functions().contains(&"added".to_owned()));
    // The old host made way for the new one on the inspector's port.
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", inspector))
            .await
            .is_ok(),
        "the reloaded host has no inspector on port {inspector}"
    );
    service.stop().await;
}

#[tokio::test]
async fn a_broken_build_answers_503_until_it_is_fixed() {
    let package = PackageCopy::of(fixture());
    package.edit("tsconfig.json", |config| {
        config.replace(
            "\"strict\": true,",
            "\"strict\": true, \"noEmitOnError\": true,",
        )
    });
    let original = std::fs::read_to_string(package.path.join("src/function_app.ts")).unwrap();
    package.edit("src/function_app.ts", |source| {
        source + "\nconst broken: number = 'not a number';\n"
    });

    let service = start(package.config().await, Arc::default()).await;
    let router = service.router();
    assert!(service.functions().is_empty());
    let unavailable = invoke(&router, "hello", json!({})).await;
    assert_eq!(unavailable.status, StatusCode::SERVICE_UNAVAILABLE);
    let (code, message) = unavailable.error();
    assert_eq!(code, "FUNCTIONS_UNAVAILABLE");
    assert!(message.contains("functions build failed"), "{message}");

    std::fs::write(package.path.join("src/function_app.ts"), original).unwrap();
    let hello = eventually(
        &router,
        "hello",
        json!({}),
        Duration::from_secs(90),
        |reply| reply.status == StatusCode::OK,
    )
    .await;
    assert_eq!(hello.output(), "Hello from rayfin-local!");
    service.stop().await;
}

/// Explorer's functions, copied (explorer is never written to), with the developer's `az` tokens.
/// Run with `cargo test -p rl-functions --test invoke -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "manual: needs the explorer app, network access and `az login`"]
async fn explorer_functions_reach_ado_with_az_tokens() {
    let source = std::env::var_os("RAYFIN_LOCAL_EXPLORER_FUNCTIONS")
        .map_or_else(|| PathBuf::from(DEFAULT_EXPLORER), PathBuf::from);
    if !source.join("node_modules").is_dir() {
        eprintln!(
            "skipped: no installed explorer functions at {}",
            source.display()
        );
        return;
    }
    let package = PackageCopy::of(&source);
    // The copy builds on its own: drop the project reference to explorer's `rayfin/` folder.
    package.edit("tsconfig.json", |text| {
        let mut config: Value = serde_json::from_str(&text).unwrap();
        config.as_object_mut().unwrap().remove("references");
        serde_json::to_string_pretty(&config).unwrap()
    });
    let broker = rl_tokens::TokenBroker::new(rl_tokens::TokenBrokerConfig {
        mode: rl_tokens::TokenMode::Az,
        ..Default::default()
    });
    let service =
        FunctionsService::start(package.config().await, Arc::new(Callers), Arc::new(broker))
            .await
            .unwrap();
    let router = service.router();
    let mut outcomes = HashMap::new();
    for (function, body) in [
        ("getFilingOptions", json!({})),
        ("getBacklog", json!({"refresh": false})),
    ] {
        let started = Instant::now();
        let reply = invoke(&router, function, body).await;
        let envelope = reply.json();
        let outcome = if envelope["status"] == "Succeeded" {
            format!(
                "ADO answered ({} bytes of output)",
                envelope["output"].to_string().len()
            )
        } else {
            format!(
                "no answer from ADO: {} {}",
                envelope["errors"][0]["errorCode"], envelope["errors"][0]["message"]
            )
        };
        eprintln!(
            "{function}: HTTP {} in {:?}, {outcome}",
            reply.status,
            started.elapsed()
        );
        outcomes.insert(function, reply.status);
    }
    service.stop().await;
    assert!(outcomes.values().all(|status| *status == StatusCode::OK));
}
