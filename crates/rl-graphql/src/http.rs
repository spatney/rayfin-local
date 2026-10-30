//! `POST /graphql` and `GET /graphql` (Hot Chocolate serves both; `GET` runs queries only: a
//! mutation is `405`, "The specified operation kind is not allowed.").
//!
//! HTTP status policy (micro-fabric compatibility.md §2.5, §7.6, and Hot Chocolate's request
//! parser): GraphQL validation and execution errors are `200` (the SDK detects
//! `AggregationNotSupportedError` only on 200), except DAB's `DatabaseInputError` (`400`); an
//! unreadable request (empty, not JSON, no `query`, a syntax error) is `400` with an `errors`
//! envelope; a rejected bearer token is `401 invalid_token`, and a token-less request for data
//! only signed-in users may read is `401 authentication_required`, so the SDK refreshes the
//! session and replays the call. The publishable key is checked upstream (`rl-server`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rl_model::{Caller, bearer_token};
use serde_json::{Map, Value as Json, json};

use crate::DataApi;
use crate::error::{GraphQlError, codes, errors_body, messages};
use crate::exec::{Outcome, Request};

pub(crate) async fn post(
    State(api): State<Arc<DataApi>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let caller = match resolve(&api, &headers) {
        Ok(caller) => caller,
        Err(response) => return *response,
    };
    let request = match read_request(&body) {
        Ok(request) => request,
        Err(response) => return *response,
    };
    respond(api.execute(&caller, request).await)
}

/// The request in a `POST` body, or the `400` answer to an unreadable one.
pub(crate) fn read_request(body: &[u8]) -> Result<Request, Box<Response>> {
    parse_body(body)
        .map_err(|error| Box::new(graphql(StatusCode::BAD_REQUEST, errors_body(&[error]))))
}

pub(crate) async fn get(
    State(api): State<Arc<DataApi>>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let caller = match resolve(&api, &headers) {
        Ok(caller) => caller,
        Err(response) => return *response,
    };
    let Some(query) = params.get("query").filter(|q| !q.is_empty()) else {
        return graphql(StatusCode::BAD_REQUEST, errors_body(&[query_missing()]));
    };
    let variables = match params
        .get("variables")
        .map(|v| serde_json::from_str::<Json>(v))
    {
        None | Some(Ok(Json::Null)) => Map::new(),
        Some(Ok(Json::Object(variables))) => variables,
        Some(_) => {
            let error = GraphQlError::new("The variables parameter is not a JSON object.")
                .code(codes::REQUEST_INVALID);
            return graphql(StatusCode::BAD_REQUEST, errors_body(&[error]));
        }
    };
    let request = Request {
        query: query.clone(),
        variables,
        operation_name: params.get("operationName").cloned(),
    };
    if crate::mutation::selects_mutation(&request) {
        let error = GraphQlError::new("The specified operation kind is not allowed.");
        return graphql(StatusCode::METHOD_NOT_ALLOWED, errors_body(&[error]));
    }
    respond(api.execute(&caller, request).await)
}

fn resolve(api: &DataApi, headers: &HeaderMap) -> Result<Caller, Box<Response>> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    api.callers
        .resolve(bearer_token(authorization))
        .map_err(|_| Box::new(invalid_token()))
}

/// `401 invalid_token`: the SDK refreshes the session and replays the call.
pub(crate) fn invalid_token() -> Response {
    unauthorized(
        "Bearer error=\"invalid_token\"",
        json!({
            "error": "invalid_token",
            "error_description": "The access token is invalid or has expired.",
        }),
    )
}

fn query_missing() -> GraphQlError {
    GraphQlError::new("Either the parameter query or the parameter id has to be set.")
        .code(codes::QUERY_MISSING)
}

/// The request body, as Hot Chocolate's request parser reads it.
fn parse_body(body: &[u8]) -> Result<Request, GraphQlError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(GraphQlError::new("The GraphQL request is empty.").code(codes::REQUEST_INVALID));
    }
    let json: Json = serde_json::from_slice(body).map_err(|e| {
        GraphQlError::new(format!("The request body is not valid JSON: {e}."))
            .code(codes::SYNTAX_ERROR)
    })?;
    let Json::Object(mut object) = json else {
        return Err(GraphQlError::new("Invalid GraphQL Request.").code(codes::REQUEST_INVALID));
    };
    let query = match object.remove("query") {
        Some(Json::String(query)) if !query.is_empty() => query,
        _ => return Err(query_missing()),
    };
    let variables = match object.remove("variables") {
        None | Some(Json::Null) => Map::new(),
        Some(Json::Object(variables)) => variables,
        Some(_) => {
            return Err(
                GraphQlError::new("The variables of a GraphQL request must be an object.")
                    .code(codes::REQUEST_INVALID),
            );
        }
    };
    let operation_name = match object.remove("operationName") {
        Some(Json::String(name)) => Some(name),
        _ => None,
    };
    Ok(Request {
        query,
        variables,
        operation_name,
    })
}

pub(crate) fn respond(outcome: Outcome) -> Response {
    match outcome {
        Outcome::Graphql { status, body } => {
            graphql(StatusCode::from_u16(status).unwrap_or(StatusCode::OK), body)
        }
        Outcome::AuthenticationRequired => unauthorized(
            "Bearer",
            json!({
                "error": "authentication_required",
                "error_description": "Sign in to access this data.",
                "errors": [{
                    "message": messages::NOT_AUTHORIZED,
                    "extensions": {"code": codes::AUTH_NOT_AUTHENTICATED},
                }],
            }),
        ),
    }
}

fn graphql(status: StatusCode, body: Json) -> Response {
    let mut response = (status, body.to_string()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    response
}

fn unauthorized(challenge: &'static str, body: Json) -> Response {
    let mut response = graphql(StatusCode::UNAUTHORIZED, body);
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static(challenge),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_bodies_are_read_as_hot_chocolate_reads_them() {
        let request =
            parse_body(br#"{"query":"{ a }","variables":{"x":1},"operationName":null}"#).unwrap();
        assert_eq!(request.query, "{ a }");
        assert_eq!(request.variables["x"], 1);
        assert!(request.operation_name.is_none());

        let code = |body: &[u8]| parse_body(body).unwrap_err().extensions["code"].clone();
        assert_eq!(code(b"  "), json!("HC0009"));
        assert_eq!(code(b"{nope"), json!("HC0011"));
        assert_eq!(code(br#"{"variables":{}}"#), json!("HC0013"));
        assert_eq!(
            code(br#"{"query":"{ a }","variables":[1]}"#),
            json!("HC0009")
        );
    }
}
