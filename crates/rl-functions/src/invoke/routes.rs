//! The invoke routes: the request checks in the deployed order, the invocation, and the answers
//! (see the [module docs](super)).

use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::{Bytes, BytesMut};
use http_body_util::BodyExt;
use rl_model::bearer_token;
use serde_json::json;
use tokio::time::Instant;

use super::{FunctionsService, MAX_BODY_BYTES, tokens};
use crate::host::{
    FunctionHost, HostError, HostInvocation, HostResponse, RegisteredFunction, rayfin_info_header,
};

/// The longest function name the deployed route accepts.
const MAX_NAME_LENGTH: usize = 128;

/// What a call answered while the host (re)starts should wait before retrying, in seconds.
const RETRY_AFTER_SECONDS: &str = "5";

/// A response, and how the call ended for the log line (the UDF status or an error code).
type Answer = (Response, String);

pub(super) fn router(service: Arc<FunctionsService>) -> Router {
    Router::new()
        .route("/functions/{name}/invoke", post(invoke))
        .route("/.rayfin-local/functions/api/{name}", post(invoke))
        .with_state(service)
}

async fn invoke(
    State(service): State<Arc<FunctionsService>>,
    Path(name): Path<String>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let (response, outcome) = answer(&service, &name, request).await;
    tracing::info!(
        "fn {} → {} in {} ms ({outcome})",
        shown(&name),
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

async fn answer(service: &FunctionsService, name: &str, request: Request) -> Answer {
    let (parts, body) = request.into_parts();
    let authorization = match parts.headers.get(header::AUTHORIZATION) {
        None => None,
        Some(value) => match value.to_str() {
            Ok(value) => Some(value),
            Err(_) => return unauthorized(),
        },
    };
    let Ok(caller) = service.callers.resolve(bearer_token(authorization)) else {
        return unauthorized();
    };
    if let Err(message) = validate_name(name) {
        return refuse(StatusCode::BAD_REQUEST, "FUNCTION_INVALID_NAME", message);
    }
    let content_type = match json_content_type(&parts.headers) {
        Ok(content_type) => content_type,
        Err(received) => {
            return refuse(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "FUNCTION_INVOKE_INVALID_CONTENT_TYPE",
                format!("Only application/json content type is supported. Received: {received}"),
            );
        }
    };
    let body = match read_json_body(&parts.headers, body).await {
        Ok(body) => body,
        Err(refusal) => return refusal,
    };

    if let Some(reason) = service.unavailable() {
        return refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "FUNCTIONS_UNAVAILABLE",
            format!(
                "The functions are not available: {reason} rayfin-local loads them as soon as a \
                 build succeeds."
            ),
        );
    }
    let host = match service.manager.host().await {
        Ok(host) => host,
        Err(error) => return host_error(name, error),
    };
    let function = match host.function(name) {
        Some(function) if function.accepts("POST") => function,
        other => return not_found(service, &host, name, other),
    };

    let tokens = tokens::mint(
        service.tokens.as_ref(),
        &function.audiences(),
        name,
        &service.token_warnings,
    )
    .await;
    let mut invocation = HostInvocation::new(name, body);
    invocation.bindings = function.token_bindings(
        tokens
            .iter()
            .map(|(audience, token)| (*audience, token.as_str())),
    );
    invocation
        .headers
        .push(("content-type".into(), content_type.to_owned()));
    if let Some(authorization) = authorization {
        invocation
            .headers
            .push(("authorization".into(), authorization.to_owned()));
    }
    let caller_token = caller.session.as_ref().map(|s| s.access_token.as_str());
    invocation.headers.push((
        "x-ms-rayfin-info".into(),
        rayfin_info_header(
            &service.api_url,
            &service.config.publishable_key,
            caller_token,
        ),
    ));
    invocation
        .headers
        .push(("x-ms-rayfin-secrets".into(), service.secrets_header.clone()));
    tracing::debug!(
        invocation_id = %invocation.invocation_id,
        tokens = tokens.len(),
        "invoking function '{name}'"
    );

    match service
        .manager
        .invoke_on(&host, invocation, service.config.invoke_timeout)
        .await
    {
        Ok(response) => pass_through(response),
        Err(error) => host_error(name, error),
    }
}

/// The host's response as the deployed route copies it: status, content type (JSON when unset),
/// `x-fabric-udf-status` and body.
fn pass_through(response: HostResponse) -> Answer {
    let udf_status = response.udf_status().map(str::to_owned);
    let content_type = response
        .header("content-type")
        .and_then(|value| HeaderValue::from_str(value).ok())
        .unwrap_or(HeaderValue::from_static("application/json"));
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(response.status).unwrap_or(StatusCode::OK))
        .header(header::CONTENT_TYPE, content_type);
    if let Some(value) = udf_status
        .as_deref()
        .and_then(|value| HeaderValue::from_str(value).ok())
    {
        builder = builder.header("x-fabric-udf-status", value);
    }
    let outcome = udf_status.unwrap_or_else(|| "not a UDF response".into());
    match builder.body(Body::from(response.body)) {
        Ok(response) => (response, outcome),
        Err(error) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "FUNCTION_INVOKE_FAILED",
            format!("The function's response could not be relayed: {error}"),
        ),
    }
}

fn host_error(name: &str, error: HostError) -> Answer {
    match error {
        HostError::Starting => {
            let (mut response, outcome) = refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "FUNCTION_HOST_STARTING",
                "The function host is starting; try again in a moment.",
            );
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_static(RETRY_AFTER_SECONDS),
            );
            (response, outcome)
        }
        HostError::Stopped => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "FUNCTIONS_UNAVAILABLE",
            "The function host is stopped: rayfin-local is shutting down.",
        ),
        HostError::NotFound(_) | HostError::NotInvocable { .. } => refuse(
            StatusCode::NOT_FOUND,
            "FUNCTION_NOT_FOUND",
            format!("Function '{name}' is not registered by the functions package."),
        ),
        HostError::Timeout { timeout, .. } => refuse(
            StatusCode::REQUEST_TIMEOUT,
            "FUNCTION_INVOKE_TIMEOUT",
            format!(
                "Function '{name}' did not respond within {}.",
                describe_timeout(timeout)
            ),
        ),
        HostError::Failed {
            error_type,
            message,
            ..
        } => invoke_failed(name, &format!("{error_type}: {message}")),
        error @ (HostError::Exited { .. } | HostError::Protocol(_) | HostError::Io(_)) => {
            invoke_failed(name, &error.to_string())
        }
        start_failure => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "FUNCTIONS_UNAVAILABLE",
            format!(
                "The functions package can't run: {start_failure}. rayfin-local reloads it when \
                 the build output changes."
            ),
        ),
    }
}

/// The function ran but produced no response.
fn invoke_failed(name: &str, cause: &str) -> Answer {
    tracing::warn!("function '{name}' failed without a response: {cause}");
    refuse(
        StatusCode::INTERNAL_SERVER_ERROR,
        "FUNCTION_INVOKE_FAILED",
        format!("Failed to invoke function '{name}': {cause}"),
    )
}

fn not_found(
    service: &FunctionsService,
    host: &FunctionHost,
    name: &str,
    function: Option<&RegisteredFunction>,
) -> Answer {
    let message = match function {
        Some(function) if !function.is_http() => format!(
            "Function '{name}' has a {} trigger; only HTTP functions can be invoked.",
            function.trigger.as_deref().unwrap_or("missing")
        ),
        Some(_) => format!("Function '{name}' does not accept POST requests."),
        None => {
            let registered: Vec<&str> = host
                .functions()
                .iter()
                .filter(|function| function.accepts("POST"))
                .map(|function| function.name.as_str())
                .collect();
            format!(
                "No function named '{name}' is registered by the functions in {}. Registered \
                 functions: [{}].",
                service.config.functions_dir.display(),
                registered.join(", ")
            )
        }
    };
    refuse(StatusCode::NOT_FOUND, "FUNCTION_NOT_FOUND", message)
}

/// The deployed authentication middleware's answer to a bad bearer token; the SDK refreshes the
/// session and replays the call.
fn unauthorized() -> Answer {
    let body = json!({ "error": "Invalid authentication token" });
    (
        (StatusCode::UNAUTHORIZED, Json(body)).into_response(),
        "invalid bearer token".into(),
    )
}

/// An error in the deployed service's shape, `{"error": <code>, "message"}`.
fn refuse(status: StatusCode, code: &str, message: impl Into<String>) -> Answer {
    let body = json!({ "error": code, "message": message.into() });
    ((status, Json(body)).into_response(), code.to_owned())
}

/// The deployed route's name rule: 1 to 128 ASCII letters, digits and underscores.
fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Function name must not be empty.".into());
    }
    if name.encode_utf16().count() > MAX_NAME_LENGTH {
        return Err(format!(
            "Function name exceeds the maximum length of {MAX_NAME_LENGTH} characters."
        ));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!(
            "Function name '{name}' is invalid. Only alphanumeric characters and underscores \
             are allowed."
        ));
    }
    Ok(())
}

/// The request's content type when its media type is `application/json`, else what was received.
fn json_content_type(headers: &HeaderMap) -> Result<&str, String> {
    let Some(value) = headers.get(header::CONTENT_TYPE) else {
        return Err("<none>".into());
    };
    let Ok(text) = value.to_str() else {
        return Err("<not a valid header value>".into());
    };
    let media_type = text.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("application/json") {
        Ok(text)
    } else {
        Err(text.to_owned())
    }
}

/// The body, at most [`MAX_BODY_BYTES`] of JSON; an empty body is `{}`.
async fn read_json_body(headers: &HeaderMap, mut body: Body) -> Result<Bytes, Answer> {
    let limit_mib = MAX_BODY_BYTES / (1024 * 1024);
    let too_large = || {
        refuse(
            StatusCode::PAYLOAD_TOO_LARGE,
            "FUNCTION_INVOKE_REQUEST_TOO_LARGE",
            format!("The request body is larger than the {limit_mib} MiB limit."),
        )
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > u64::try_from(MAX_BODY_BYTES).unwrap_or(u64::MAX)) {
        return Err(too_large());
    }
    let mut collected = BytesMut::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| {
            refuse(
                StatusCode::BAD_REQUEST,
                "FUNCTION_INVOKE_INVALID_BODY",
                format!("The request body could not be read: {error}"),
            )
        })?;
        if let Ok(data) = frame.into_data() {
            if collected.len() + data.len() > MAX_BODY_BYTES {
                return Err(too_large());
            }
            collected.extend_from_slice(&data);
        }
    }
    if collected.iter().all(u8::is_ascii_whitespace) {
        return Ok(Bytes::from_static(b"{}"));
    }
    if let Err(error) = serde_json::from_slice::<serde::de::IgnoredAny>(&collected) {
        return Err(refuse(
            StatusCode::BAD_REQUEST,
            "FUNCTION_INVOKE_INVALID_BODY",
            format!(
                "The request body is not valid JSON ({error}). Send the function's parameters as \
                 a JSON object, for example {{\"name\": \"Ada\"}}."
            ),
        ));
    }
    Ok(collected.freeze())
}

/// `250 seconds`, or the exact duration when it isn't whole seconds.
fn describe_timeout(timeout: Duration) -> String {
    if timeout.subsec_nanos() == 0 {
        format!("{} seconds", timeout.as_secs())
    } else {
        format!("{timeout:?}")
    }
}

/// A function name for the log: as is when valid, else escaped and shortened.
fn shown(name: &str) -> String {
    if validate_name(name).is_ok() {
        name.to_owned()
    } else {
        let escaped: String = name.escape_debug().take(64).collect();
        format!("\"{escaped}\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_deployed_rule() {
        assert!(validate_name("get_Backlog2").is_ok());
        assert!(validate_name(&"a".repeat(128)).is_ok());
        assert!(
            validate_name(&"a".repeat(129))
                .unwrap_err()
                .contains("maximum length of 128")
        );
        assert!(
            validate_name("get-backlog")
                .unwrap_err()
                .contains("invalid")
        );
        assert!(validate_name("é").is_err());
        assert!(validate_name("").is_err());
        assert_eq!(shown("bad\nname"), "\"bad\\nname\"");
    }

    #[test]
    fn only_json_media_types_are_accepted() {
        let headers = |value: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(value));
            headers
        };
        assert_eq!(
            json_content_type(&headers("Application/JSON; charset=utf-8")),
            Ok("Application/JSON; charset=utf-8")
        );
        assert_eq!(
            json_content_type(&headers("text/plain")),
            Err("text/plain".into())
        );
        assert_eq!(
            json_content_type(&headers("application/json-patch+json")),
            Err("application/json-patch+json".into())
        );
        assert_eq!(json_content_type(&HeaderMap::new()), Err("<none>".into()));
    }
}
