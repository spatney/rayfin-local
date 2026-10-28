//! `fabric-semanticmodel`: DAX through Power BI's `executeDaxQueries`, as the developer.
//!
//! This is the SDK's direct path (`directExecute.js` in
//! `@microsoft/rayfin-connector-fabric-semanticmodel` 1.36.2) moved behind the route: the same
//! request to Power BI, and its failures reported in the same `FabricSemanticModelTabularResponse`
//! envelope, so `toQueryResult` turns them into the same `QueryError`s. Tokens `executeDaxQueries`
//! refuses for want of a dataset scope (the Azure CLI's) are served by [`json_fallback`].

mod json_fallback;

use std::{
    error::Error as _,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use axum::{
    Json,
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::Engine as _;
use futures_util::TryStreamExt as _;
use reqwest::Url;
use rl_tokens::TokenSource;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::ConnectorDef;

const ARROW_STREAM: &str = "application/vnd.apache.arrow.stream";
const ACCEPT_ARROW_OR_JSON: &str = "application/vnd.apache.arrow.stream, application/json";
/// Power BI's correlation id: sent with every request, and echoed (server-assigned) on answers.
const REQUEST_ID: &str = "requestid";
const ACTIVITY_ID: &str = "activityid";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Power BI error bodies are small; this only bounds what a misbehaving server can make us hold.
const MAX_ERROR_BODY: usize = 1 << 20;

/// `directExecute.js` `parseErrorBody`'s hint for a 401 or 403 with a body.
const ACCESS_HINT: &str = "Sign in again, and confirm you have at least Viewer access to the \
     workspace/semantic model (Builder access if you also need to query it).";
/// `directExecute.js` `parseErrorBody`'s hint for a 401 or 403 with an empty body.
const EMPTY_ACCESS_HINT: &str = "The request was rejected before Power BI evaluated model access, \
     so check the token first: confirm the caller sends one and that it carries the \
     https://analysis.windows.net/powerbi/api audience. Only then check workspace and semantic \
     model permissions.";
/// The delegated scopes `executeDaxQueries` requires.
const DATASET_SCOPES: [&str; 2] = ["Dataset.Read.All", "Dataset.ReadWrite.All"];

/// Power BI's REST API, called with the developer's token.
pub(crate) struct PowerBi {
    client: reqwest::Client,
    base: Url,
    scope: String,
    /// The SDK's `sessionId`: one activity id for every query of this process.
    activity_id: String,
    /// Set once `executeDaxQueries` refused a token without a dataset scope; later such tokens go
    /// straight to the JSON fallback.
    arrow_needs_dataset_scope: AtomicBool,
}

impl PowerBi {
    pub(crate) fn new(base_url: &str, scope: String) -> Result<Self, String> {
        let base = Url::parse(base_url)
            .ok()
            .filter(|url| !url.cannot_be_a_base() && matches!(url.scheme(), "http" | "https"))
            .ok_or_else(|| {
                format!(
                    "The Power BI base URL '{base_url}' isn't an http(s) URL; semantic-model \
                     connectors can't run. Fix it in rayfin-local's configuration."
                )
            })?;
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("rayfin-local/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("Couldn't create the HTTP client for Power BI: {e}."))?;
        Ok(Self {
            client,
            base,
            scope,
            activity_id: Uuid::new_v4().to_string(),
            arrow_needs_dataset_scope: AtomicBool::new(false),
        })
    }

    /// `/v1.0/myorg/groups/{ws}/datasets/{item}/{operation}`, without `/groups/{ws}` for My
    /// Workspace (`me`).
    fn dataset_url(&self, target: &Target<'_>, operation: &str) -> Url {
        let mut url = self.base.clone();
        // `new` only accepts URLs that can be a base.
        if let Ok(mut segments) = url.path_segments_mut() {
            segments.pop_if_empty().extend(["v1.0", "myorg"]);
            if target.workspace_id != "me" {
                segments.extend(["groups", target.workspace_id]);
            }
            segments.extend(["datasets", target.item_id, operation]);
        }
        url
    }

    /// POSTs `body` with the developer's token and the SDK's correlation headers.
    async fn post(
        &self,
        url: &Url,
        token: &str,
        accept: &str,
        request_id: &str,
        timeout: Duration,
        body: &Value,
    ) -> Result<reqwest::Response, QueryFailure> {
        self.client
            .post(url.clone())
            .bearer_auth(token)
            .header(header::ACCEPT, accept)
            .header(ACTIVITY_ID, &self.activity_id)
            .header(REQUEST_ID, request_id)
            .timeout(timeout)
            .json(body)
            .send()
            .await
            .map_err(|error| network_failure(url, &error, timeout))
    }
}

/// The semantic model a connector queries: `config.workspaceId` and `config.itemId`.
pub(crate) struct Target<'a> {
    workspace_id: &'a str,
    item_id: &'a str,
}

impl<'a> Target<'a> {
    pub(crate) fn of(connector: &'a ConnectorDef) -> Result<Self, String> {
        let get = |key: &str| {
            connector
                .config
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        match (get("workspaceId"), get("itemId")) {
            (Some(workspace_id), Some(item_id)) => Ok(Self {
                workspace_id,
                item_id,
            }),
            _ => Err(format!(
                "Connector '{}' needs `config.workspaceId` and `config.itemId` in \
                 rayfin/rayfin.yml; add them, or re-run `npx rayfin connector add` to fill them in.",
                connector.name
            )),
        }
    }
}

/// A failed query, as `directExecute.js` `toErrorResponse` reports it in
/// `output.responseError`.
struct QueryFailure {
    category: &'static str,
    message: String,
    code: Option<Value>,
    details: Option<String>,
    recovery_hint: Option<String>,
}

impl QueryFailure {
    fn new(category: &'static str, message: String) -> Self {
        Self {
            category,
            message,
            code: None,
            details: None,
            recovery_hint: None,
        }
    }

    /// Power BI answered a non-2xx status (`executeDaxDirect`'s `onError`).
    fn api(body: String, status: StatusCode) -> Self {
        let (message, code, recovery_hint) = parse_error_body(&body, status);
        Self {
            category: "api",
            message,
            code,
            details: Some(body),
            recovery_hint,
        }
    }
}

/// Runs `query` and answers with Power BI's Arrow body, the JSON fallback's envelope, or a failed
/// envelope.
pub(crate) async fn execute(
    power_bi: &PowerBi,
    tokens: &dyn TokenSource,
    connector: &str,
    target: &Target<'_>,
    query: &str,
    input: &Map<String, Value>,
) -> Response {
    // Sent to Power BI, and the answer's id whenever Power BI never assigns one.
    let client_request_id = Uuid::new_v4().to_string();

    let token = match tokens.token(&power_bi.scope).await {
        Ok(token) => token.token,
        Err(error) => {
            tracing::warn!(connector, "no Power BI token for the developer: {error}");
            let mut failure = QueryFailure::new(
                "api",
                format!(
                    "rayfin-local couldn't get a Power BI token for the developer: {}",
                    error.message
                ),
            );
            failure.recovery_hint = error.hint;
            return failed(&client_request_id, failure);
        }
    };
    let without_dataset_scope = lacks_dataset_scope(&token);
    if without_dataset_scope && power_bi.arrow_needs_dataset_scope.load(Ordering::Relaxed) {
        return json_fallback::execute(power_bi, connector, target, query, input, &token).await;
    }

    let url = power_bi.dataset_url(target, "executeDaxQueries");
    let timeout = total_timeout(input);
    let body = dax_request_body(query, input);
    let started = Instant::now();
    let sent = power_bi
        .post(
            &url,
            &token,
            ACCEPT_ARROW_OR_JSON,
            &client_request_id,
            timeout,
            &body,
        )
        .await;
    let response = match sent {
        Ok(response) => response,
        Err(failure) => {
            tracing::warn!(connector, request_id = %client_request_id, "{}", failure.message);
            return failed(&client_request_id, failure);
        }
    };
    let status = response.status();
    let request_id = answer_request_id(&response, client_request_id);
    tracing::info!(
        connector,
        status = status.as_u16(),
        latency_ms = started.elapsed().as_millis() as u64,
        request_id = %request_id,
        "semantic-model query"
    );

    if status.is_success() {
        return stream_arrow(response, status, connector, &request_id);
    }
    if status == StatusCode::UNAUTHORIZED && without_dataset_scope {
        if !power_bi
            .arrow_needs_dataset_scope
            .swap(true, Ordering::Relaxed)
        {
            tracing::info!(
                "Power BI refuses executeDaxQueries (Arrow) to developer tokens without the \
                 Dataset.Read.All scope, such as the Azure CLI's; semantic-model queries use \
                 executeQueries (JSON) instead"
            );
        }
        return json_fallback::execute(power_bi, connector, target, query, input, &token).await;
    }
    let body = read_error_body(response).await;
    tracing::warn!(
        connector,
        status = status.as_u16(),
        request_id = %request_id,
        "Power BI refused the query"
    );
    failed(&request_id, QueryFailure::api(body, status))
}

/// Power BI's Arrow body, streamed back unchanged with its content type and request id.
fn stream_arrow(
    response: reqwest::Response,
    status: StatusCode,
    connector: &str,
    request_id: &str,
) -> Response {
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    if !is_binary(content_type.as_ref()) {
        let shown = content_type
            .as_ref()
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).into_owned())
            .unwrap_or_default();
        let message = format!(
            "Power BI answered with Content-Type '{shown}' instead of an Apache Arrow stream."
        );
        return failed(request_id, QueryFailure::new("unknown", message));
    }
    let connector = connector.to_owned();
    let body = response.bytes_stream().inspect_err(move |error| {
        tracing::warn!(connector = %connector, "the Power BI answer broke off: {error}");
    });
    let mut streamed = Response::new(Body::from_stream(body));
    *streamed.status_mut() = status;
    let headers = streamed.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        content_type.unwrap_or(HeaderValue::from_static(ARROW_STREAM)),
    );
    if let Ok(id) = HeaderValue::from_str(request_id) {
        headers.insert(REQUEST_ID, id);
    }
    streamed
}

/// Power BI's `requestid` when it sent one, else the one the request carried.
fn answer_request_id(response: &reqwest::Response, sent: String) -> String {
    response
        .headers()
        .get(REQUEST_ID)
        .map(|id| String::from_utf8_lossy(id.as_bytes()).into_owned())
        .unwrap_or(sent)
}

/// The `executeDaxQueries` body: the query and the options the input defines, in
/// `executeDaxDirect`'s order. A row limit counts only when it is a positive integer, as the SDK
/// runtime's `withValidatedRowLimit` decides.
fn dax_request_body(query: &str, input: &Map<String, Value>) -> Value {
    let mut body = Map::new();
    body.insert("query".into(), query.into());
    for option in ["culture", "schemaOnly", "queryTimeout"] {
        if let Some(value) = input.get(option).filter(|v| !v.is_null()) {
            body.insert(option.into(), value.clone());
        }
    }
    if let Some(limit) = row_limit(input) {
        body.insert("resultSetRowCountLimit".into(), limit.into());
    }
    Value::Object(body)
}

fn row_limit(input: &Map<String, Value>) -> Option<u64> {
    let value = input.get("resultSetRowCountLimit")?;
    if let Some(limit) = value.as_u64() {
        return (limit > 0).then_some(limit);
    }
    let limit = value.as_f64()?;
    (limit >= 1.0 && limit.fract() == 0.0 && limit < u64::MAX as f64).then_some(limit as u64)
}

/// Power BI stops a query after its `queryTimeout` (seconds) and says so; wait long enough to
/// receive that answer.
fn total_timeout(input: &Map<String, Value>) -> Duration {
    input
        .get("queryTimeout")
        .and_then(Value::as_f64)
        .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
        .map(|seconds| Duration::from_secs_f64(seconds.min(3600.0) + 15.0).max(DEFAULT_TIMEOUT))
        .unwrap_or(DEFAULT_TIMEOUT)
}

/// The types the SDK's `ApiClient` reads as bytes; a missing type is read as bytes too.
fn is_binary(content_type: Option<&HeaderValue>) -> bool {
    let Some(content_type) = content_type else {
        return true;
    };
    let content_type = String::from_utf8_lossy(content_type.as_bytes()).to_ascii_lowercase();
    content_type.contains(ARROW_STREAM) || content_type.contains("application/octet-stream")
}

/// The body as `response.text()` reads it: UTF-8 without a byte order mark, and empty when it
/// can't be read.
async fn read_error_body(mut response: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = MAX_ERROR_BODY - bytes.len();
                bytes.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if bytes.len() == MAX_ERROR_BODY {
                    break;
                }
            }
            Ok(None) => break,
            Err(_) => return String::new(),
        }
    }
    let text = String::from_utf8_lossy(&bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}

/// `directExecute.js` `parseErrorBody`: Power BI's `{ error: { code, message } }`, or the raw body
/// with the status as the code.
fn parse_error_body(body: &str, status: StatusCode) -> (String, Option<Value>, Option<String>) {
    let trimmed = body.trim();
    let recovery_hint = matches!(status.as_u16(), 401 | 403).then(|| {
        if trimmed.is_empty() {
            EMPTY_ACCESS_HINT
        } else {
            ACCESS_HINT
        }
        .to_owned()
    });
    if let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(body)
        && let Some(Value::Object(error)) = parsed.get("error")
        && let Some(message) = error
            .get("message")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
    {
        return (
            message.to_owned(),
            error.get("code").cloned(),
            recovery_hint,
        );
    }
    let message = if trimmed.is_empty() {
        format!(
            "Power BI returned HTTP {} with an empty body.",
            status.as_u16()
        )
    } else {
        trimmed.to_owned()
    };
    (
        message,
        Some(Value::String(status.as_u16().to_string())),
        recovery_hint,
    )
}

/// Whether the token's `scp` claim lacks both dataset scopes, as the Azure CLI's
/// (`user_impersonation`) does. Read without verifying the token; `false` when it isn't a JWT
/// with an `scp` claim.
fn lacks_dataset_scope(token: &str) -> bool {
    let scopes = || -> Option<String> {
        let payload = token.split('.').nth(1)?;
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(payload.trim_end_matches('='))
            .ok()?;
        let claims: Value = serde_json::from_slice(&payload).ok()?;
        claims.get("scp")?.as_str().map(str::to_owned)
    };
    scopes().is_some_and(|scopes| {
        !scopes
            .split_whitespace()
            .any(|scope| DATASET_SCOPES.iter().any(|s| s.eq_ignore_ascii_case(scope)))
    })
}

/// `directExecute.js` `toNetworkErrorResponse`: Power BI never answered.
fn network_failure(url: &Url, error: &reqwest::Error, timeout: Duration) -> QueryFailure {
    let origin = url.origin().ascii_serialization();
    let (kind, message) = if error.is_timeout() {
        (
            "timeout",
            format!(
                "Power BI at {origin} didn't answer within {} s.",
                timeout.as_secs()
            ),
        )
    } else {
        let mut causes = Vec::new();
        let mut source = error.source();
        while let Some(cause) = source {
            causes.push(cause.to_string());
            source = cause.source();
        }
        let cause = if causes.is_empty() {
            error.to_string()
        } else {
            causes.join(": ")
        };
        let kind = if error.is_connect() {
            "connect"
        } else {
            "request"
        };
        (
            kind,
            format!("Couldn't reach Power BI at {origin}: {cause}"),
        )
    };
    let mut details = vec![format!("hostname={}", url.host_str().unwrap_or_default())];
    if let Some(port) = url.port_or_known_default() {
        details.push(format!("port={port}"));
    }
    details.push(format!("kind={kind}"));
    let mut failure = QueryFailure::new("network", message);
    failure.details = Some(details.join(", "));
    failure
}

/// 200 with the failed `FabricSemanticModelTabularResponse` (`directExecute.js` `toErrorResponse`).
/// `toQueryResult` reads `output.responseError` as `{ status: 'error', error, requestId }`.
fn failed(request_id: &str, failure: QueryFailure) -> Response {
    let mut error = Map::new();
    error.insert("message".into(), failure.message.into());
    error.insert("category".into(), failure.category.into());
    if let Some(code) = failure.code {
        error.insert("code".into(), code);
    }
    if let Some(details) = failure.details.filter(|d| !d.is_empty()) {
        error.insert("details".into(), details.into());
    }
    if let Some(hint) = failure.recovery_hint.filter(|h| !h.is_empty()) {
        error.insert("recoveryHint".into(), hint.into());
    }
    let output = json!({ "tables": [], "requestId": request_id, "responseError": error });
    envelope(request_id, "Failed", output)
}

/// A `FabricSemanticModelTabularResponse`, with the request id also in the `requestid` header.
fn envelope(request_id: &str, status: &str, output: Value) -> Response {
    let body = json!({ "status": status, "output": output, "errors": [] });
    let mut response = (StatusCode::OK, Json(body)).into_response();
    if let Ok(id) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert(REQUEST_ID, id);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies_parse_as_the_direct_path_parses_them() {
        let (message, code, hint) = parse_error_body(
            r#"{"error":{"code":"BadRequest","message":"Bad Request"}}"#,
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(message, "Bad Request");
        assert_eq!(code, Some(json!("BadRequest")));
        assert_eq!(hint, None);

        // No `error.message`: the raw body is the message and the status the code.
        let body = r#" {"error":{"code":"Unauthorized"}} "#;
        let (message, code, hint) = parse_error_body(body, StatusCode::UNAUTHORIZED);
        assert_eq!(message, body.trim());
        assert_eq!(code, Some(json!("401")));
        assert_eq!(hint.as_deref(), Some(ACCESS_HINT));

        let (message, code, hint) = parse_error_body("  ", StatusCode::FORBIDDEN);
        assert_eq!(message, "Power BI returned HTTP 403 with an empty body.");
        assert_eq!(code, Some(json!("403")));
        assert_eq!(hint.as_deref(), Some(EMPTY_ACCESS_HINT));
    }

    #[test]
    fn row_limits_must_be_positive_integers() {
        let limit = |value: Value| {
            row_limit(
                json!({ "resultSetRowCountLimit": value })
                    .as_object()
                    .unwrap(),
            )
        };
        assert_eq!(limit(json!(10)), Some(10));
        assert_eq!(limit(json!(10.0)), Some(10));
        for unusable in [json!(0), json!(-1), json!(1.5), json!("10"), json!(null)] {
            assert_eq!(limit(unusable.clone()), None, "{unusable}");
        }
    }

    #[test]
    fn dataset_scopes_are_read_from_the_tokens_scp_claim() {
        let jwt = |claims: Value| {
            let payload =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string());
            format!("eyJhbGciOiJub25lIn0.{payload}.sig")
        };
        assert!(lacks_dataset_scope(&jwt(
            json!({ "scp": "user_impersonation" })
        )));
        assert!(!lacks_dataset_scope(&jwt(
            json!({ "scp": "dataset.read.all Workspace.Read.All" })
        )));
        // App-only tokens (`roles`) and opaque tokens aren't second-guessed.
        assert!(!lacks_dataset_scope(&jwt(
            json!({ "roles": ["Tenant.Read.All"] })
        )));
        assert!(!lacks_dataset_scope("opaque"));
    }

    #[test]
    fn query_timeouts_extend_the_wait() {
        let input = |v: Value| v.as_object().cloned().unwrap();
        assert_eq!(total_timeout(&input(json!({}))), DEFAULT_TIMEOUT);
        assert_eq!(
            total_timeout(&input(json!({ "queryTimeout": 30 }))),
            DEFAULT_TIMEOUT
        );
        assert_eq!(
            total_timeout(&input(json!({ "queryTimeout": 600 }))),
            Duration::from_secs(615)
        );
    }
}
