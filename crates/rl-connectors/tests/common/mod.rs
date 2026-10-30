//! Test doubles: a mock Power BI on 127.0.0.1 with canned `executeDaxQueries` answers, a fake
//! caller resolver and a fake token source.
#![allow(dead_code)]

pub mod sql;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt as _;
use rl_connectors::{ConnectorDef, ConnectorsConfig};
use rl_model::{Caller, CallerError, CallerResolver, Session};
use rl_tokens::{AccessToken, BoxFuture, TokenError, TokenSource};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tower::ServiceExt as _;

/// The reference app's (explorer's) semantic model.
pub const WORKSPACE: &str = "67e66ea4-5ea2-4f76-98da-6144ba7d52fa";
pub const ITEM: &str = "7ee53e06-5065-4c1b-96bb-b8ef12abaad6";

pub const ARROW: &str = "application/vnd.apache.arrow.stream";
pub const SESSION: &str = "valid-session";
pub const DEV_TOKEN: &str = "dev-token";

pub const ROW_QUERY: &str = "EVALUATE ROW(\"x\", 1)";
/// Arrow IPC streams shaped like Power BI's (`tests/sdk/make-fixtures.mjs`).
pub const ROW: &[u8] = include_bytes!("../fixtures/row.arrows");
pub const TYPED: &[u8] = include_bytes!("../fixtures/typed.arrows");
pub const INT64_OVERFLOW: &[u8] = include_bytes!("../fixtures/int64-overflow.arrows");
pub const DAX_ERROR: &[u8] = include_bytes!("../fixtures/dax-error.arrows");

/// Power BI's error bodies, as it answers them.
pub const BAD_REQUEST_BODY: &str = r#"{"error":{"code":"BadRequest","message":"Bad Request","details":[{"message":"Invalid value","target":"executeQueryRequest.queryTimeout"}]}}"#;
pub const UNAUTHORIZED_BODY: &str = r#"{"error":{"code":"Unauthorized","pbi.error":{"code":"Unauthorized","parameters":{},"details":[],"exceptionCulprit":1}}}"#;
pub const NOT_FOUND_BODY: &str = r#"{"error":{"code":"PowerBIEntityNotFound","pbi.error":{"code":"PowerBIEntityNotFound","parameters":{},"details":[{"code":"PowerBIEntityNotFound","detail":{"type":1,"value":"Dataset not found or the user does not have sufficient permissions."}}],"exceptionCulprit":1}}}"#;
pub const THROTTLED_BODY: &str = "Rate limit exceeded. Retry in 30 seconds.";
pub const COMPILE_ERROR_BODY: &str = r#"{"error":{"code":"DatasetExecuteQueriesError","pbi.error":{"code":"DatasetExecuteQueriesError","parameters":{},"details":[{"code":"DetailsMessage","detail":{"type":1,"value":"Query (1, 10) Failed to resolve name 'Nope'. It is not a valid table, variable, or function name."}},{"code":"AnalysisServicesErrorCode","detail":{"type":1,"value":"3241803821"}}]}}}"#;

/// A token shaped like the Azure CLI's: a JWT whose `scp` is only `user_impersonation`.
/// The mock Power BI refuses it `executeDaxQueries` and answers it `executeQueries`.
pub fn az_token() -> String {
    use base64::Engine as _;
    let claims =
        json!({ "aud": "https://analysis.windows.net/powerbi/api", "scp": "user_impersonation" });
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
    format!("eyJ0eXAiOiJKV1QifQ.{payload}.signature")
}

/// The mock's answer to a DAX query: the queries named here fail the way Power BI fails, every
/// other query gets the `EVALUATE ROW("x", 1)` stream.
pub fn canned_answer(query: &str) -> Response {
    match query {
        "EVALUATE Typed" => arrow(TYPED, "pbi-typed"),
        "EVALUATE Big" => arrow(INT64_OVERFLOW, "pbi-big"),
        "EVALUATE Nope" => arrow(DAX_ERROR, "pbi-nope"),
        "EVALUATE BadRequest" => json_error(StatusCode::BAD_REQUEST, BAD_REQUEST_BODY, "pbi-400"),
        "EVALUATE Unauthorized" => {
            json_error(StatusCode::UNAUTHORIZED, UNAUTHORIZED_BODY, "pbi-401")
        }
        "EVALUATE Forbidden" => (
            StatusCode::FORBIDDEN,
            [
                ("requestid", "pbi-403"),
                ("x-powerbi-error-info", "InvalidToken"),
            ],
        )
            .into_response(),
        "EVALUATE Missing" => json_error(StatusCode::NOT_FOUND, NOT_FOUND_BODY, "pbi-404"),
        // No `requestid`: the answer's id is the one the request sent.
        "EVALUATE Throttled" => (
            StatusCode::TOO_MANY_REQUESTS,
            [("content-type", "text/plain"), ("retry-after", "30")],
            THROTTLED_BODY,
        )
            .into_response(),
        "EVALUATE Html" => (
            [("content-type", "text/html"), ("requestid", "pbi-html")],
            "<html>Down for maintenance</html>",
        )
            .into_response(),
        _ => arrow(ROW, "pbi-row"),
    }
}

fn arrow(bytes: &'static [u8], request_id: &'static str) -> Response {
    ([("content-type", ARROW), ("requestid", request_id)], bytes).into_response()
}

/// The mock's `executeQueries` (JSON) answer, Power BI's shapes for the same queries.
pub fn canned_json_answer(query: &str) -> Response {
    let answer = |result: Value, request_id: &'static str| {
        let body = json!({
            "informationProtectionLabel": { "id": "9fbde396", "name": "Internal Only" },
            "results": [result],
        });
        let headers = [
            ("content-type", "application/json; charset=utf-8"),
            ("requestid", request_id),
        ];
        (headers, body.to_string()).into_response()
    };
    match query {
        "EVALUATE Typed" => answer(
            json!({ "tables": [{ "rows": [
                {
                    "Sales[Region]": "West", "Sales[OrderDate]": "2024-08-02T00:00:00",
                    "[Total]": 1713.98, "[Orders]": 12, "[HasProduct]": true,
                },
                {
                    "Sales[Region]": "East", "Sales[OrderDate]": "2023-09-27T01:12:12",
                    "[Total]": 0.5, "[Orders]": 3, "[HasProduct]": false,
                },
                {
                    "Sales[Region]": null, "Sales[OrderDate]": null, "[Total]": null,
                    "[Orders]": null, "[HasProduct]": null,
                },
            ] }] }),
            "pbi-json-typed",
        ),
        "EVALUATE Nope" => json_error(StatusCode::BAD_REQUEST, COMPILE_ERROR_BODY, "pbi-json-nope"),
        "EVALUATE EvalError" => answer(
            json!({
                "tables": [{ "rows": [] }],
                "error": {
                    "code": "QueryUserError",
                    "message": "Query (1, 19) Cannot convert value 'a' of type Text to type Number.",
                    "details": [{
                        "code": "AnalysisServicesErrorCode",
                        "detail": { "type": "1", "value": 3241541768u64 },
                    }],
                },
            }),
            "pbi-json-eval",
        ),
        "EVALUATE Many" => answer(
            json!({
                "tables": [{ "rows": [{ "[Value]": 1 }, { "[Value]": 2 }] }],
                "error": {
                    "code": "DaxRowCountNotSupported",
                    "message": "More than 100000 rows have been encountered. Only 100000 rows are \
                                allowed given the number of columns returned which is 1.",
                },
            }),
            "pbi-json-many",
        ),
        "EVALUATE Two" => answer(
            json!({
                "tables": [{ "rows": [{ "[a]": 1 }] }],
                "error": {
                    "code": "DaxResultsetCountNotSupported",
                    "message": "More than 1 result set has been encountered. Only 1 result set \
                                is supported.",
                },
            }),
            "pbi-json-two",
        ),
        "EVALUATE Missing" => json_error(StatusCode::NOT_FOUND, NOT_FOUND_BODY, "pbi-404"),
        _ => answer(
            json!({ "tables": [{ "rows": [{ "[x]": 1 }] }] }),
            "pbi-json-row",
        ),
    }
}

fn json_error(status: StatusCode, body: &'static str, request_id: &'static str) -> Response {
    let headers = [
        ("content-type", "application/json; charset=utf-8"),
        ("requestid", request_id),
    ];
    (status, headers, body).into_response()
}

/// A request the mock Power BI received.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Recorded {
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .map(|v| v.to_str().unwrap())
            .unwrap_or_default()
    }
}

pub struct MockPowerBi {
    pub base_url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl MockPowerBi {
    pub async fn start() -> Self {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let az_authorization = format!("Bearer {}", az_token());
        let app = Router::new().fallback(move |request: Request| {
            let recorded = recorded.clone();
            let az_authorization = az_authorization.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = body.collect().await.unwrap().to_bytes();
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let query = body["query"]
                    .as_str()
                    .or(body["queries"][0]["query"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                let path = parts.uri.path().to_owned();
                let from_az = parts
                    .headers
                    .get("authorization")
                    .is_some_and(|v| v == &az_authorization);
                recorded.lock().unwrap().push(Recorded {
                    method: parts.method.to_string(),
                    path: path.clone(),
                    headers: parts.headers,
                    body,
                });
                if path.ends_with("/executeQueries") {
                    canned_json_answer(&query)
                } else if from_az {
                    json_error(StatusCode::UNAUTHORIZED, UNAUTHORIZED_BODY, "pbi-401")
                } else {
                    canned_answer(&query)
                }
            }
        });
        Self {
            base_url: serve(app).await,
            requests,
        }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

/// Serves `app` on a free loopback port; returns its origin.
pub async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{address}")
}

/// An origin nothing listens on.
pub async fn closed_origin() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}")
}

/// [`SESSION`] is a signed-in user, no token is anonymous, any other token is invalid.
pub struct FakeCallers;

impl CallerResolver for FakeCallers {
    fn resolve(&self, bearer: Option<&str>) -> Result<Caller, CallerError> {
        match bearer {
            None => Ok(Caller::anonymous()),
            Some(SESSION) => Ok(Caller::authenticated(Session {
                user_id: "user-1".into(),
                email: None,
                claims: Map::new(),
                access_token: SESSION.into(),
            })),
            Some(_) => Err(CallerError::InvalidToken),
        }
    }
}

/// Answers every scope with the same token, or the same error; records the scopes asked for.
pub struct FakeTokens {
    answer: Result<String, TokenError>,
    pub scopes: Mutex<Vec<String>>,
}

impl FakeTokens {
    pub fn with_token(token: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: Ok(token.to_owned()),
            scopes: Mutex::new(Vec::new()),
        })
    }

    pub fn failing(message: &str, hint: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: Err(TokenError {
                scope: String::new(),
                message: message.to_owned(),
                hint: Some(hint.to_owned()),
            }),
            scopes: Mutex::new(Vec::new()),
        })
    }
}

impl TokenSource for FakeTokens {
    fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
        self.scopes.lock().unwrap().push(scope.to_owned());
        let answer = self.answer.clone().map(|token| AccessToken {
            token,
            expires_at: SystemTime::now() + Duration::from_secs(3600),
        });
        Box::pin(async move { answer })
    }
}

pub fn semantic_model(name: &str, workspace_id: &str, operations: &[&str]) -> ConnectorDef {
    let config = json!({ "workspaceId": workspace_id, "itemId": ITEM });
    ConnectorDef {
        name: name.to_owned(),
        kind: "fabric-semanticmodel".to_owned(),
        version: Some("1".to_owned()),
        config: config.as_object().unwrap().clone(),
        auth_type: Some("delegated".to_owned()),
        operations: operations.iter().map(|op| (*op).to_owned()).collect(),
    }
}

pub fn other_connector(name: &str, kind: &str) -> ConnectorDef {
    ConnectorDef {
        name: name.to_owned(),
        kind: kind.to_owned(),
        ..ConnectorDef::default()
    }
}

/// `sales` (a workspace), `mine` (My Workspace, no operations listed), `telemetry` (kusto) and
/// `warehouse` (Category A), with Power BI at `powerbi_base_url`.
pub fn config(powerbi_base_url: &str) -> ConnectorsConfig {
    ConnectorsConfig {
        connectors: vec![
            semantic_model("sales", WORKSPACE, &["executeQuery"]),
            semantic_model("mine", "me", &[]),
            other_connector("telemetry", "kusto"),
            other_connector("warehouse", "fabric-warehouse"),
        ],
        powerbi_base_url: powerbi_base_url.to_owned(),
        ..ConnectorsConfig::default()
    }
}

pub fn execute_query(input: Value) -> Value {
    json!({ "operation": "executeQuery", "input": input })
}

/// POSTs `body` as the SDK does.
pub async fn post(
    app: &Router,
    path: &str,
    bearer: Option<&str>,
    body: Value,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut request = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            "application/vnd.apache.arrow.stream, application/json",
        )
        .header("x-publishable-key", "pk");
    if let Some(bearer) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    }
    let request = request.body(Body::from(body.to_string())).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let (parts, body) = response.into_parts();
    (
        parts.status,
        parts.headers,
        body.collect().await.unwrap().to_bytes(),
    )
}

pub fn json_body(body: &Bytes) -> Value {
    serde_json::from_slice(body).unwrap()
}

/// The reference app's `node_modules` with the Rayfin SDK 1.36.2: `RAYFIN_SDK_NODE_MODULES`, else
/// `fabric-apps/apps/explorer` next to this repository. `None` when it isn't installed.
pub fn sdk_node_modules() -> Option<PathBuf> {
    let path = std::env::var_os("RAYFIN_SDK_NODE_MODULES")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../fabric-apps/apps/explorer/node_modules")
        });
    let marker = path.join("@microsoft/rayfin-connector-fabric-semanticmodel/package.json");
    marker.is_file().then_some(path)
}

/// `az account get-access-token --scope <scope>`.
pub struct AzCli;

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
                tokio::process::Command::new("cmd")
                    .arg("/C")
                    .arg("az")
                    .args(args)
                    .output()
                    .await
            } else {
                tokio::process::Command::new("az").args(args).output().await
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
                .map(|seconds| std::time::UNIX_EPOCH + Duration::from_secs(seconds))
                .unwrap_or_else(|| SystemTime::now() + Duration::from_secs(300));
            Ok(AccessToken {
                token: token.to_owned(),
                expires_at,
            })
        })
    }
}
