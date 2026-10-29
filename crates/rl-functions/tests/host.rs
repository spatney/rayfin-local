//! The function host on real Node.js: the fixture package `tests/fixtures/functions/basic`
//! (installed and built on first use) and, when it is on this machine, the explorer app's
//! functions (`RAYFIN_LOCAL_EXPLORER_FUNCTIONS` overrides where).

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use bytes::Bytes;
use rl_functions::host::{
    FunctionHost, HostConfig, HostError, HostInvocation, HostManager, HostResponse, HostStatus,
    ManagerOptions, local_settings_values, rayfin_info_header, secrets_header,
};
use rl_node::{LineSink, NodeRuntime, Stream};
use rl_tokens::Audience;
use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(30);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

/// `RAYFIN_LOCAL_EXPLORER_FUNCTIONS`, else explorer's functions in `fabric-apps` beside this
/// repository.
fn explorer_functions() -> PathBuf {
    std::env::var_os("RAYFIN_LOCAL_EXPLORER_FUNCTIONS").map_or_else(
        || repo_root().join("../fabric-apps/apps/explorer/rayfin/functions"),
        PathBuf::from,
    )
}

/// The fixture package, `npm ci`-installed and built once per run.
fn fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let dir = repo_root().join("tests/fixtures/functions/basic");
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

/// A scratch directory under the target dir.
fn scratch() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn cache_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("function-host-cache")
}

async fn node() -> NodeRuntime {
    NodeRuntime::discover()
        .await
        .expect("Node.js 20+ is needed for these tests")
}

/// Records the host's output.
#[derive(Clone, Default)]
struct Output(Arc<Mutex<Vec<(String, Stream, String)>>>);

impl Output {
    fn sink(&self) -> LineSink {
        let lines = self.0.clone();
        Arc::new(move |label: &str, stream: Stream, line: &str| {
            lines
                .lock()
                .unwrap()
                .push((label.to_owned(), stream, line.to_owned()));
        })
    }

    fn contains(&self, stream: Stream, text: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .iter()
            .any(|(label, s, line)| label == "fn" && *s == stream && line.contains(text))
    }

    /// Output is streamed: waits up to 5 s for a line.
    async fn wait_for(&self, stream: Stream, text: &str) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.contains(stream, text) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        false
    }

    fn text(&self) -> String {
        let lines = self.0.lock().unwrap();
        lines
            .iter()
            .map(|(_, _, line)| line.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The fixture's `local.settings.json` `Values`, pointed at a local API, without `IsLocal` (so
/// statuses are the deployed ones) unless `is_local`.
fn fixture_env(is_local: bool) -> Vec<(String, String)> {
    let mut env = local_settings_values(fixture()).unwrap();
    env.retain(|(key, _)| is_local || key != "IsLocal");
    env.push(("RAYFIN_API_URL".into(), "http://127.0.0.1:9/graphql".into()));
    env.push(("RAYFIN_PUBLISHABLE_KEY".into(), "pk_local".into()));
    env
}

#[test]
fn local_settings_values_are_the_func_start_environment() {
    let dir = scratch();
    assert!(local_settings_values(dir.path()).unwrap().is_empty());

    let settings = "\u{feff}{\"IsEncrypted\": false, \"Values\": {\"A\": \"x\", \"N\": 3, \"B\": true, \"Z\": null}}";
    std::fs::write(dir.path().join("local.settings.json"), settings).unwrap();
    assert_eq!(
        local_settings_values(dir.path()).unwrap(),
        [
            ("A".into(), "x".into()),
            ("N".into(), "3".into()),
            ("B".into(), "true".into())
        ]
    );

    std::fs::write(
        dir.path().join("local.settings.json"),
        r#"{"IsEncrypted": true, "Values": {}}"#,
    )
    .unwrap();
    let error = local_settings_values(dir.path()).unwrap_err();
    assert!(
        error.to_string().contains("func settings decrypt"),
        "{error}"
    );
}

async fn fixture_config(output: &Output) -> HostConfig {
    let mut config = HostConfig::new(
        node().await,
        fixture(),
        "dist/src/function_app.js",
        cache_dir(),
        output.sink(),
    );
    config.env = fixture_env(false);
    config
}

/// A package whose entry point is `source`.
fn package_with(source: &str) -> tempfile::TempDir {
    let dir = scratch();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"type": "module", "main": "index.js"}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("index.js"), source).unwrap();
    dir
}

async fn package_config(dir: &Path, output: &Output) -> HostConfig {
    HostConfig::new(node().await, dir, "index.js", cache_dir(), output.sink())
}

fn call(function: &str, body: Value) -> HostInvocation {
    HostInvocation::new(function, body.to_string()).header("content-type", "application/json")
}

fn envelope(response: &HostResponse) -> Value {
    serde_json::from_slice(&response.body).unwrap()
}

async fn output_of(host: &FunctionHost, invocation: HostInvocation) -> Value {
    let response = host.invoke(invocation, TIMEOUT).await.unwrap();
    let envelope = envelope(&response);
    assert_eq!(envelope["status"], "Succeeded", "{envelope}");
    envelope["output"].clone()
}

#[tokio::test]
async fn reports_every_registered_function() {
    let output = Output::default();
    let host = FunctionHost::start(fixture_config(&output).await)
        .await
        .unwrap();
    let names: Vec<&str> = host.functions().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "hello", "echo", "tokens", "whoami", "secret", "fail", "reject", "logs", "counter",
            "slow", "crash", "bigint", "raw", "nightly"
        ]
    );
    let hello = host.function("hello").unwrap();
    assert!(hello.accepts("POST") && !hello.accepts("GET"));
    assert_eq!(hello.auth_level.as_deref(), Some("anonymous"));
    assert_eq!(
        host.function("tokens").unwrap().audiences(),
        [Audience::Ado, Audience::Fabric]
    );
    assert!(host.function("whoami").unwrap().audiences().is_empty());
    assert_eq!(
        host.function("raw").unwrap().route.as_deref(),
        Some("raw/{id?}")
    );
    assert!(!host.function("nightly").unwrap().is_http());
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn returns_the_udf_envelope_with_bound_parameters() {
    let output = Output::default();
    let host = FunctionHost::start(fixture_config(&output).await)
        .await
        .unwrap();

    let mut hello = HostInvocation::new("hello", Bytes::new());
    hello.invocation_id = "11111111-2222-3333-4444-555555555555".into();
    let response = host.invoke(hello, TIMEOUT).await.unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(response.udf_status(), Some("Succeeded"));
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(
        envelope(&response),
        json!({
            "functionName": "hello",
            "invocationId": "11111111-2222-3333-4444-555555555555",
            "status": "Succeeded",
            "output": "Hello from rayfin-local!",
            "errors": []
        })
    );

    let args = json!({
        "text": 42, "count": "7", "flag": false, "options": {"nested": [1, {"a": null}]},
        "tags": ["x", "y"], "scores": [], "note": null
    });
    assert_eq!(
        output_of(&host, call("echo", args)).await,
        json!({
            "text": "42", "count": 7, "flag": false, "options": {"nested": [1, {"a": null}]},
            "tags": ["x", "y"], "scores": [], "note": null, "greeting": "hi"
        })
    );

    let missing = host.invoke(call("echo", json!({})), TIMEOUT).await.unwrap();
    assert_eq!(
        (missing.status, missing.udf_status()),
        (200, Some("BadRequest"))
    );
    assert_eq!(envelope(&missing)["errors"][0]["errorCode"], "MissingInput");
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn token_bindings_reach_ctx_tokens() {
    let output = Output::default();
    let host = FunctionHost::start(fixture_config(&output).await)
        .await
        .unwrap();
    let tokens = host.function("tokens").unwrap();
    let (ado, fabric) = ("a".repeat(11), "f".repeat(23));

    let mut invocation = call("tokens", json!({}));
    invocation.bindings = tokens.token_bindings([
        (Audience::Ado, ado.as_str()),
        (Audience::Fabric, fabric.as_str()),
    ]);
    assert_eq!(
        output_of(&host, invocation).await,
        json!({"audiences": ["ADO", "Fabric"], "adoLength": 11, "fabricLength": 23})
    );

    // An audience without a token throws when the function reads it.
    let mut invocation = call("tokens", json!({}));
    invocation.bindings = tokens.token_bindings([(Audience::Fabric, fabric.as_str())]);
    let response = host.invoke(invocation, TIMEOUT).await.unwrap();
    assert_eq!(response.udf_status(), Some("Failed"));
    let message = envelope(&response)["errors"][0]["message"].clone();
    assert!(
        message
            .as_str()
            .unwrap()
            .contains("No token available for audience 'ADO'")
    );
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn context_and_secrets_come_from_env_and_headers() {
    let output = Output::default();
    let host = FunctionHost::start(fixture_config(&output).await)
        .await
        .unwrap();

    // Development mode (from local.settings.json): env endpoint and key, the caller's bearer.
    let whoami = call("whoami", json!({}))
        .header("authorization", "Bearer caller-jwt")
        .header(
            "x-ms-rayfin-info",
            rayfin_info_header("http://ignored.invalid", "pk_ignored", Some("caller-jwt")),
        );
    assert_eq!(
        output_of(&host, whoami).await,
        json!({"baseUrl": "http://127.0.0.1:9", "hasPublishableKey": true, "hasAccessToken": true})
    );
    let anonymous = call("whoami", json!({}));
    assert_eq!(output_of(&host, anonymous).await["hasAccessToken"], false);

    let secret = call("secret", json!({})).header(
        "x-ms-rayfin-secrets",
        secrets_header([("API_KEY", "s3cr€t")]),
    );
    assert_eq!(output_of(&host, secret).await, json!({"length": 6}));
    // Without the header, the value comes from the environment (local.settings.json).
    let from_env = output_of(&host, call("secret", json!({}))).await;
    assert_eq!(from_env, json!({"length": "fixture-api-key".len()}));
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn deployed_mode_reads_x_ms_rayfin_info() {
    let output = Output::default();
    let mut config = fixture_config(&output).await;
    config
        .env
        .retain(|(key, _)| key != "AZURE_FUNCTIONS_ENVIRONMENT");
    let host = FunctionHost::start(config).await.unwrap();

    let whoami = call("whoami", json!({})).header(
        "x-ms-rayfin-info",
        rayfin_info_header("http://127.0.0.1:7/graphql", "pk_header", None),
    );
    assert_eq!(
        output_of(&host, whoami).await,
        json!({"baseUrl": "http://127.0.0.1:7", "hasPublishableKey": true, "hasAccessToken": false})
    );
    let missing = host
        .invoke(call("whoami", json!({})), TIMEOUT)
        .await
        .unwrap();
    assert_eq!(missing.udf_status(), Some("BadRequest"));
    assert_eq!(
        envelope(&missing)["errors"][0]["message"],
        "Missing required header 'x-ms-rayfin-info'."
    );
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn failures_keep_the_deployed_or_local_status_mapping() {
    let output = Output::default();
    let host = FunctionHost::start(fixture_config(&output).await)
        .await
        .unwrap();

    let failed = host.invoke(call("fail", json!({})), TIMEOUT).await.unwrap();
    assert_eq!((failed.status, failed.udf_status()), (200, Some("Failed")));
    let error = &envelope(&failed)["errors"][0];
    assert_eq!(error["errorCode"], "InternalError");
    assert_eq!(
        error["properties"],
        json!({"error_type": "Error", "error_message": "boom"})
    );

    let rejected = host
        .invoke(call("reject", json!({"reason": "r"})), TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        (rejected.status, rejected.udf_status()),
        (200, Some("BadRequest"))
    );
    assert_eq!(envelope(&rejected)["errors"][0]["errorCode"], "UserThrown");
    assert!(
        output
            .wait_for(Stream::Stderr, "fail: Error during function invoke")
            .await
    );

    match host.invoke(call("bigint", json!({})), TIMEOUT).await {
        Err(HostError::Failed {
            function,
            error_type,
            message,
            ..
        }) => {
            assert_eq!(
                (function.as_str(), error_type.as_str()),
                ("bigint", "TypeError")
            );
            assert!(message.contains("BigInt"), "{message}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    assert!(matches!(
        host.invoke(call("missing", json!({})), TIMEOUT).await,
        Err(HostError::NotFound(name)) if name == "missing"
    ));
    assert!(matches!(
        host.invoke(call("nightly", json!({})), TIMEOUT).await,
        Err(HostError::NotInvocable { .. })
    ));
    host.stop(Duration::from_secs(5)).await;

    // With `IsLocal=true`, as `rayfin dev` runs them, the statuses follow the envelope.
    let mut config = fixture_config(&output).await;
    config.env = fixture_env(true);
    let host = FunctionHost::start(config).await.unwrap();
    let statuses = [
        ("fail", json!({}), 500),
        ("reject", json!({"reason": "r"}), 422),
        ("echo", json!({}), 400),
        ("hello", json!({}), 200),
    ];
    for (function, body, status) in statuses {
        let response = host.invoke(call(function, body), TIMEOUT).await.unwrap();
        assert_eq!(response.status, status, "{function}");
    }
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn raw_http_functions_keep_their_own_responses() {
    let output = Output::default();
    let host = FunctionHost::start(fixture_config(&output).await)
        .await
        .unwrap();
    let mut get = HostInvocation::new("raw", Bytes::new());
    get.method = "GET".into();
    get.url = Some("http://localhost/api/raw/7?q=find".into());
    let response = host.invoke(get, TIMEOUT).await.unwrap();
    assert_eq!(
        (response.status, response.header("x-raw")),
        (201, Some("yes"))
    );
    assert_eq!(response.udf_status(), None);
    assert_eq!(
        serde_json::from_slice::<Value>(&response.body).unwrap(),
        json!({"method": "GET", "id": null, "q": "find", "body": null})
    );

    let mut post = HostInvocation::new("raw", Bytes::from_static(b"plain text"));
    post.url = Some("http://localhost/api/raw".into());
    let response = host.invoke(post, TIMEOUT).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&response.body).unwrap()["body"],
        "plain text"
    );
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn invocations_run_concurrently_and_log_to_the_sink() {
    let output = Output::default();
    let host = Arc::new(
        FunctionHost::start(fixture_config(&output).await)
            .await
            .unwrap(),
    );

    let started = Instant::now();
    let calls = (0..8).map(|_| {
        let host = host.clone();
        tokio::spawn(async move { output_of(&host, call("slow", json!({"ms": 400}))).await })
    });
    for result in futures_join(calls).await {
        assert_eq!(result, json!(400));
    }
    assert!(
        started.elapsed() < Duration::from_millis(400 * 4),
        "{:?}",
        started.elapsed()
    );

    let marker = uuid::Uuid::new_v4().to_string();
    output_of(&host, call("logs", json!({"message": marker}))).await;
    assert!(
        output
            .wait_for(
                Stream::Stdout,
                &format!("logs: [UserDataFunctions] log: {marker}")
            )
            .await,
        "{}",
        output.text()
    );
    assert!(
        output
            .wait_for(
                Stream::Stderr,
                &format!("logs: [UserDataFunctions] error: {marker}")
            )
            .await
    );
    host.stop(Duration::from_secs(5)).await;
}

async fn futures_join(handles: impl Iterator<Item = tokio::task::JoinHandle<Value>>) -> Vec<Value> {
    let mut results = Vec::new();
    for handle in handles.collect::<Vec<_>>() {
        results.push(handle.await.unwrap());
    }
    results
}

#[tokio::test]
async fn load_failures_carry_the_javascript_error() {
    let output = Output::default();
    let broken = package_with("export const x = 1;\nthrow new TypeError('broken at load');\n");
    match FunctionHost::start(package_config(broken.path(), &output).await).await {
        Err(HostError::Load {
            entry,
            message,
            stack,
        }) => {
            assert_eq!(entry, "index.js");
            assert_eq!(message, "TypeError: broken at load");
            assert!(stack.unwrap().contains("index.js"));
        }
        other => panic!("expected a load error, got {other:?}"),
    }

    let exits = package_with("process.exit(3);\n");
    match FunctionHost::start(package_config(exits.path(), &output).await).await {
        Err(HostError::ExitedBeforeReady { exit }) => assert!(exit.contains('3'), "{exit}"),
        other => panic!("expected an early exit, got {other:?}"),
    }

    let mut missing = package_config(exits.path(), &output).await;
    missing.entry = "dist/nope.js".into();
    assert!(matches!(
        FunctionHost::start(missing).await,
        Err(HostError::MissingEntry { .. })
    ));

    // No `@azure/functions` at all: loads, registers nothing.
    let empty = package_with("export {};\n");
    let host = FunctionHost::start(package_config(empty.path(), &output).await)
        .await
        .unwrap();
    assert!(host.functions().is_empty());
    host.stop(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn stop_ends_the_process_after_in_flight_invocations() {
    let output = Output::default();
    let host = Arc::new(
        FunctionHost::start(fixture_config(&output).await)
            .await
            .unwrap(),
    );
    assert!(host.is_alive() && host.pid().is_some());

    let slow = {
        let host = host.clone();
        tokio::spawn(async move { output_of(&host, call("slow", json!({"ms": 500}))).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    host.stop(Duration::from_secs(5)).await;
    assert_eq!(slow.await.unwrap(), json!(500));
    assert!(!host.is_alive());
    assert_eq!(host.exit_status().unwrap().code, Some(0));
    assert!(matches!(
        host.invoke(call("hello", json!({})), TIMEOUT).await,
        Err(HostError::Exited { .. } | HostError::Io(_))
    ));
}

#[tokio::test]
async fn a_crash_fails_in_flight_calls_and_ends_the_host() {
    let output = Output::default();
    let host = Arc::new(
        FunctionHost::start(fixture_config(&output).await)
            .await
            .unwrap(),
    );
    let slow = {
        let host = host.clone();
        tokio::spawn(async move {
            host.invoke(call("slow", json!({"ms": 5000})), TIMEOUT)
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(output_of(&host, call("crash", json!({}))).await, "crashing");
    let exit = host.exited().await;
    assert_eq!(exit.code, Some(1));
    assert!(
        matches!(slow.await.unwrap(), Err(HostError::Exited { function, .. }) if function == "slow")
    );
    assert!(output.wait_for(Stream::Stderr, "deliberate crash").await);
}

fn manager_options() -> ManagerOptions {
    ManagerOptions {
        start_wait: Duration::from_secs(60),
        stop_grace: Duration::from_secs(2),
        restart_on_timeout: true,
    }
}

async fn counter(manager: &HostManager) -> Value {
    let response = manager
        .invoke(call("counter", json!({})), TIMEOUT)
        .await
        .unwrap();
    envelope(&response)["output"].clone()
}

#[tokio::test]
async fn manager_starts_on_demand_and_restarts_after_crashes() {
    let output = Output::default();
    let manager = HostManager::with_options(fixture_config(&output).await, manager_options());
    assert!(matches!(manager.status(), HostStatus::Idle));

    // The first invocation starts the host.
    assert_eq!(counter(&manager).await, json!(1));
    assert_eq!(counter(&manager).await, json!(2));
    let first = manager.host().await.unwrap();

    let crash = manager
        .invoke(call("crash", json!({})), TIMEOUT)
        .await
        .unwrap();
    assert_eq!(envelope(&crash)["output"], "crashing");
    first.exited().await;
    // A new host, with fresh module state.
    assert_eq!(counter(&manager).await, json!(1));
    let second = manager.host().await.unwrap();
    assert_ne!(first.id(), second.id());

    manager.stop().await;
    assert!(!second.is_alive());
    assert!(matches!(manager.status(), HostStatus::Stopped));
    assert!(matches!(
        manager.invoke(call("hello", json!({})), TIMEOUT).await,
        Err(HostError::Stopped)
    ));
}

#[tokio::test]
async fn manager_reload_and_timeouts_replace_the_host() {
    let output = Output::default();
    let manager = HostManager::with_options(fixture_config(&output).await, manager_options());
    let functions = manager.start().await.unwrap();
    assert_eq!(functions.len(), 14);
    assert_eq!(counter(&manager).await, json!(1));
    assert_eq!(counter(&manager).await, json!(2));

    let before = manager.host().await.unwrap();
    manager.reload().await.unwrap();
    assert_eq!(counter(&manager).await, json!(1));
    assert!(!before.is_alive());

    let timed_out = manager
        .invoke(
            call("slow", json!({"ms": 10_000})),
            Duration::from_millis(300),
        )
        .await;
    assert!(
        matches!(timed_out, Err(HostError::Timeout { .. })),
        "{timed_out:?}"
    );
    assert_eq!(counter(&manager).await, json!(1));
    manager.stop().await;
}

#[tokio::test]
async fn manager_keeps_a_load_failure_until_reloaded() {
    let output = Output::default();
    let package = package_with("throw new Error('not yet');\n");
    let manager = HostManager::with_options(
        package_config(package.path(), &output).await,
        manager_options(),
    );
    assert!(matches!(manager.start().await, Err(HostError::Load { .. })));
    assert!(matches!(
        manager.invoke(call("hello", json!({})), TIMEOUT).await,
        Err(HostError::Load { message, .. }) if message.contains("not yet")
    ));

    // "Rebuilt": the package now loads.
    std::fs::write(package.path().join("index.js"), "export {};\n").unwrap();
    assert!(manager.reload().await.unwrap().is_empty());
    assert!(matches!(manager.status(), HostStatus::Ready { .. }));
    manager.stop().await;
}

#[tokio::test]
async fn loads_the_explorer_functions() {
    let dir = explorer_functions();
    if !dir.join("dist/src/function_app.js").is_file() {
        eprintln!("skipped: no built explorer functions at {}", dir.display());
        return;
    }
    let output = Output::default();
    let config = HostConfig::new(
        node().await,
        &dir,
        "dist/src/function_app.js",
        cache_dir(),
        output.sink(),
    );
    let host = FunctionHost::start(config).await.unwrap();
    let mut names: Vec<&str> = host.functions().iter().map(|f| f.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "askAnalyst",
            "createWorkItem",
            "draftWorkItem",
            "getBacklog",
            "getFeatureDetail",
            "getFilingOptions",
            "getModelRefreshStatus",
            "getRecentActivity",
            "startModelRefresh"
        ]
    );
    assert!(host.functions().iter().all(|f| f.accepts("POST")));
    assert_eq!(
        host.function("getBacklog").unwrap().audiences(),
        [Audience::Ado]
    );
    assert_eq!(
        host.function("askAnalyst").unwrap().audiences(),
        [Audience::Ado, Audience::AzureAi]
    );
    assert_eq!(
        host.function("startModelRefresh").unwrap().audiences(),
        [Audience::Fabric]
    );
    host.stop(Duration::from_secs(5)).await;
}
