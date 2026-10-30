//! The routes: request checks in the order the deployed `/connector-invoke` route applies them,
//! and its answers for the requests it refuses.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use rl_model::{CallerResolver, bearer_token};
use rl_tokens::TokenSource;
use serde_json::{Map, Value, json};

use crate::{
    ConnectorDef, ConnectorsConfig, SqlKind,
    semantic_model::{self, PowerBi, Target},
    sql::SqlConnectors,
};

const FABRIC_SEMANTIC_MODEL: &str = "fabric-semanticmodel";
const EXECUTE_QUERY: &str = "executeQuery";

struct Connectors {
    connectors: Vec<ConnectorDef>,
    callers: Arc<dyn CallerResolver>,
    tokens: Arc<dyn TokenSource>,
    /// `Err` holds why Power BI can't be called (a bad base URL); every query answers it.
    power_bi: Result<PowerBi, String>,
    /// Category A connectors (`/connectors/<name>/graphql`).
    sql: SqlConnectors,
}

impl Connectors {
    fn find(&self, name: &str) -> Option<&ConnectorDef> {
        self.connectors.iter().find(|c| c.name == name)
    }
}

pub(crate) fn router(
    config: ConnectorsConfig,
    callers: Arc<dyn CallerResolver>,
    tokens: Arc<dyn TokenSource>,
) -> Router {
    // reqwest is built without a rustls provider: install ring before building the client. A
    // no-op when the binary already did.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let power_bi = PowerBi::new(&config.powerbi_base_url, config.powerbi_scope);
    if let Err(problem) = &power_bi {
        tracing::error!("{problem}");
    }
    for connector in &config.connectors {
        if !crate::runs_locally(&connector.kind) {
            tracing::info!(
                connector = %connector.name,
                "connector type '{}' isn't supported by rayfin-local yet; its calls answer 501",
                connector.kind
            );
        } else if connector.kind == FABRIC_SEMANTIC_MODEL
            && let Err(problem) = Target::of(connector)
        {
            tracing::warn!(connector = %connector.name, "{problem}");
        }
    }
    let sql = SqlConnectors::new(config.sql, &config.connectors, Arc::clone(&tokens));
    let state = Arc::new(Connectors {
        connectors: config.connectors,
        callers,
        tokens,
        power_bi,
        sql,
    });
    Router::new()
        .route("/connector-invoke/{name}", post(invoke))
        .route("/connectors/{name}/graphql", post(graphql))
        .with_state(state)
}

async fn invoke(
    State(state): State<Arc<Connectors>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let caller = match state.callers.resolve(bearer_token(authorization)) {
        Ok(caller) => caller,
        Err(_) => return invalid_token(),
    };
    let Some(connector) = state.find(&name) else {
        return connector_not_found(&state, &name);
    };
    if SqlKind::parse(&connector.kind).is_some() {
        return service_error(
            StatusCode::NOT_FOUND,
            "CONNECTOR_NOT_FOUND",
            format!(
                "Connector '{name}' is a {} connector, which has no operations to invoke; query \
                 its entities, for example client.connectors.{name}.<Entity>.findMany() \
                 (POST /connectors/{name}/graphql).",
                connector.kind
            ),
        );
    }
    if connector.kind != FABRIC_SEMANTIC_MODEL {
        return not_supported(&connector.kind);
    }
    let (operation, input) = match parse_invoke_body(&body) {
        Ok(request) => request,
        Err(message) => return service_error(StatusCode::BAD_REQUEST, "INVALID_REQUEST", message),
    };
    if let Some(refusal) = operation_refusal(connector, &operation) {
        return refusal;
    }
    if runs_as_signed_in_user(connector, true) && !caller.is_authenticated() {
        return session_required(connector);
    }
    execute_query(&state, connector, &input).await
}

async fn execute_query(state: &Connectors, connector: &ConnectorDef, input: &Value) -> Response {
    let power_bi = match &state.power_bi {
        Ok(power_bi) => power_bi,
        Err(problem) => {
            return service_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONNECTOR_MISCONFIGURED",
                problem.clone(),
            );
        }
    };
    let target = match Target::of(connector) {
        Ok(target) => target,
        Err(problem) => {
            return service_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "CONNECTOR_MISCONFIGURED",
                problem,
            );
        }
    };
    let empty = Map::new();
    let input = input.as_object().unwrap_or(&empty);
    let Some(query) = input
        .get("query")
        .and_then(Value::as_str)
        .filter(|q| !q.is_empty())
    else {
        return service_error(
            StatusCode::BAD_REQUEST,
            "INVALID_INPUT",
            "executeQuery needs `input.query`, a DAX query, for example \
             {\"query\": \"EVALUATE ROW(\\\"x\\\", 1)\"}."
                .to_owned(),
        );
    };
    tracing::debug!(connector = %connector.name, "semantic-model query as the developer");
    semantic_model::execute(
        power_bi,
        state.tokens.as_ref(),
        &connector.name,
        &target,
        query,
        input,
    )
    .await
}

/// Category A connectors (`fabric-sqldatabase`, `fabric-warehouse`, `fabric-sqlanalytics`):
/// DAB-style GraphQL over the connector's Fabric SQL source (`crate::sql`).
async fn graphql(
    State(state): State<Arc<Connectors>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let caller = match state.callers.resolve(bearer_token(authorization)) {
        Ok(caller) => caller,
        Err(_) => return invalid_token(),
    };
    match state.find(&name) {
        None => connector_not_found(&state, &name),
        Some(connector) if connector.kind == FABRIC_SEMANTIC_MODEL => service_error(
            StatusCode::NOT_FOUND,
            "CONNECTOR_NOT_FOUND",
            format!(
                "Connector '{name}' is a fabric-semanticmodel connector, which has no GraphQL \
                 endpoint; call its operations, for example client.connectors.{name}.executeQuery()."
            ),
        ),
        Some(connector) => match state.sql.graphql(&name, &caller, &body).await {
            Some(response) => response,
            None => not_supported(&connector.kind),
        },
    }
}

/// `{"operation": "<name>", "input": <value>}`, as `SemanticConnectorClient` posts it.
fn parse_invoke_body(body: &[u8]) -> Result<(String, Value), String> {
    let Ok(Value::Object(mut fields)) = serde_json::from_slice::<Value>(body) else {
        return Err("The request body must be a JSON object: \
                    {\"operation\": \"executeQuery\", \"input\": {\"query\": \"...\"}}."
            .to_owned());
    };
    let Some(Value::String(operation)) = fields.remove("operation") else {
        return Err(
            "The request body needs an `operation` string, such as \"executeQuery\".".into(),
        );
    };
    Ok((operation, fields.remove("input").unwrap_or(Value::Null)))
}

/// Why `requested` can't run: it must be declared in `rayfin.yml` (all of the type's when none
/// are), matched case-insensitively as `rayfin connector invoke` does, and be one the type has.
fn operation_refusal(connector: &ConnectorDef, requested: &str) -> Option<Response> {
    let declared: Vec<&str> = if connector.operations.is_empty() {
        vec![EXECUTE_QUERY]
    } else {
        connector.operations.iter().map(String::as_str).collect()
    };
    if !declared.iter().any(|d| d.eq_ignore_ascii_case(requested)) {
        return Some(service_error(
            StatusCode::BAD_REQUEST,
            "OPERATION_NOT_ALLOWED",
            format!(
                "Operation '{requested}' is not declared for connector '{}'. Declared operations: \
                 [{}]. Add it to the connector's `operations:` list in rayfin/rayfin.yml.",
                connector.name,
                declared.join(", ")
            ),
        ));
    }
    (!requested.eq_ignore_ascii_case(EXECUTE_QUERY)).then(|| {
        service_error(
            StatusCode::BAD_REQUEST,
            "OPERATION_NOT_ALLOWED",
            format!(
                "Operation '{requested}' isn't an operation of fabric-semanticmodel connectors; \
                 the only one is {EXECUTE_QUERY}."
            ),
        )
    })
}

/// Delegated connectors run as the signed-in user: semantic models by default
/// (`delegated_by_default`), Category A connectors when `auth.type` says so (they default to
/// `application`).
pub(crate) fn runs_as_signed_in_user(connector: &ConnectorDef, delegated_by_default: bool) -> bool {
    match connector.auth_type.as_deref() {
        None => delegated_by_default,
        Some(auth) => auth.eq_ignore_ascii_case("delegated"),
    }
}

/// A service refusal, shaped like the deployed route's (`code`, `message`, `httpStatusCode`). The
/// SDK throws it as `NetworkError(message, status)`.
pub(crate) fn service_error(status: StatusCode, code: &str, message: String) -> Response {
    tracing::debug!(status = status.as_u16(), code, "connector request refused");
    let body = json!({ "code": code, "message": message, "httpStatusCode": status.as_u16() });
    (status, Json(body)).into_response()
}

/// A bearer token that is malformed, expired or revoked. 401 makes the SDK refresh the session
/// and replay the call once.
fn invalid_token() -> Response {
    let body = json!({
        "error": "invalid_token",
        "error_description": "The access token is invalid or has expired.",
    });
    let challenge = HeaderValue::from_static("Bearer error=\"invalid_token\"");
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, challenge)],
        Json(body),
    )
        .into_response()
}

/// No signed-in user for a delegated connector: 401, as deployed, so a restored session refreshes
/// and replays the call.
pub(crate) fn session_required(connector: &ConnectorDef) -> Response {
    service_error(
        StatusCode::UNAUTHORIZED,
        "DELEGATED_SESSION_EXPIRED",
        format!(
            "Sign in to use connector '{}': its queries run as the signed-in user (delegated \
             auth).",
            connector.name
        ),
    )
}

fn connector_not_found(state: &Connectors, name: &str) -> Response {
    let declared: Vec<&str> = state.connectors.iter().map(|c| c.name.as_str()).collect();
    let declared = if declared.is_empty() {
        " It declares no connectors.".to_owned()
    } else {
        format!(" Declared connectors: [{}].", declared.join(", "))
    };
    service_error(
        StatusCode::NOT_FOUND,
        "CONNECTOR_NOT_FOUND",
        format!("Connector '{name}' is not declared in rayfin/rayfin.yml.{declared}"),
    )
}

/// 501 for connector types rayfin-local doesn't run yet. There is no `error_description`, so the
/// SDK surfaces `message` verbatim.
fn not_supported(kind: &str) -> Response {
    let body = json!({
        "error": "not_supported",
        "message": format!(
            "Connector type '{kind}' isn't supported by rayfin-local yet; deploy with \
             `npx rayfin up` to use it."
        ),
    });
    (StatusCode::NOT_IMPLEMENTED, Json(body)).into_response()
}
