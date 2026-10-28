//! `/connector-invoke` and `/connectors/{name}/graphql` against a mock Power BI.

mod common;

use std::sync::Arc;

use axum::http::{StatusCode, header};
use common::*;
use rl_connectors::router;
use serde_json::{Value, json};

fn app(power_bi: &str) -> axum::Router {
    router(
        config(power_bi),
        Arc::new(FakeCallers),
        FakeTokens::with_token(DEV_TOKEN),
    )
}

#[tokio::test]
async fn streams_power_bis_arrow_answer_unchanged() {
    let power_bi = MockPowerBi::start().await;
    let tokens = FakeTokens::with_token(DEV_TOKEN);
    let app = router(
        config(&power_bi.base_url),
        Arc::new(FakeCallers),
        tokens.clone(),
    );

    let input = json!({ "query": ROW_QUERY });
    let (status, headers, body) = post(
        &app,
        "/connector-invoke/sales",
        Some(SESSION),
        execute_query(input),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(&body[..], ROW);
    assert_eq!(headers[header::CONTENT_TYPE], ARROW);
    assert_eq!(headers["requestid"], "pbi-row");

    let [request] = power_bi.requests().try_into().unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.path,
        format!("/v1.0/myorg/groups/{WORKSPACE}/datasets/{ITEM}/executeDaxQueries")
    );
    assert_eq!(request.header("authorization"), "Bearer dev-token");
    assert_eq!(
        request.header("accept"),
        "application/vnd.apache.arrow.stream, application/json"
    );
    assert_eq!(request.header("content-type"), "application/json");
    assert!(uuid::Uuid::parse_str(request.header("requestid")).is_ok());
    assert!(!request.header("activityid").is_empty());
    assert_eq!(request.body, json!({ "query": ROW_QUERY }));
    assert_eq!(
        *tokens.scopes.lock().unwrap(),
        ["https://analysis.windows.net/powerbi/api/.default"]
    );
}

#[tokio::test]
async fn sends_power_bi_only_the_options_the_input_defines() {
    let power_bi = MockPowerBi::start().await;
    let app = app(&power_bi.base_url);

    let inputs = [
        json!({
            "query": "EVALUATE A", "culture": "en-US", "schemaOnly": false, "queryTimeout": 30,
            "resultSetRowCountLimit": 10, "workspaceId": "elsewhere", "other": 1,
        }),
        json!({ "query": "EVALUATE B", "resultSetRowCountLimit": 0, "culture": null }),
        json!({ "query": "EVALUATE C", "resultSetRowCountLimit": "10" }),
    ];
    for input in inputs {
        let (status, ..) = post(
            &app,
            "/connector-invoke/sales",
            Some(SESSION),
            execute_query(input),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    let bodies: Vec<Value> = power_bi.requests().into_iter().map(|r| r.body).collect();
    assert_eq!(
        bodies,
        [
            json!({
                "query": "EVALUATE A", "culture": "en-US", "schemaOnly": false,
                "queryTimeout": 30, "resultSetRowCountLimit": 10,
            }),
            json!({ "query": "EVALUATE B" }),
            json!({ "query": "EVALUATE C" }),
        ]
    );
    // The requests ids are fresh, the activity id is the process's.
    let requests = power_bi.requests();
    assert_ne!(
        requests[0].header("requestid"),
        requests[1].header("requestid")
    );
    assert_eq!(
        requests[0].header("activityid"),
        requests[1].header("activityid")
    );
}

#[tokio::test]
async fn my_workspace_models_have_no_groups_segment() {
    let power_bi = MockPowerBi::start().await;
    let app = app(&power_bi.base_url);

    // `mine` lists no operations: every operation of the type is allowed.
    let input = json!({ "query": ROW_QUERY });
    let (status, ..) = post(
        &app,
        "/connector-invoke/mine",
        Some(SESSION),
        execute_query(input),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let [request] = power_bi.requests().try_into().unwrap();
    assert_eq!(
        request.path,
        format!("/v1.0/myorg/datasets/{ITEM}/executeDaxQueries")
    );
}

/// The `responseError` and `requestId` of a failed-query envelope, after checking the envelope.
fn failed_envelope(
    status: StatusCode,
    headers: &axum::http::HeaderMap,
    body: &[u8],
) -> (Value, String) {
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    let mut envelope: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(envelope["status"], "Failed");
    assert_eq!(envelope["errors"], json!([]));
    assert_eq!(envelope["output"]["tables"], json!([]));
    let request_id = envelope["output"]["requestId"].as_str().unwrap().to_owned();
    assert_eq!(headers["requestid"], request_id.as_str());
    (envelope["output"]["responseError"].take(), request_id)
}

#[tokio::test]
async fn power_bi_failures_answer_the_direct_paths_envelope() {
    let power_bi = MockPowerBi::start().await;
    let app = app(&power_bi.base_url);
    let access_hint = "Sign in again, and confirm you have at least Viewer access";
    let empty_body_hint = "The request was rejected before Power BI evaluated model access";

    let cases = [
        (
            "EVALUATE BadRequest",
            json!({
                "message": "Bad Request", "category": "api", "code": "BadRequest",
                "details": BAD_REQUEST_BODY,
            }),
            None,
            "pbi-400",
        ),
        (
            "EVALUATE Unauthorized",
            json!({
                "message": UNAUTHORIZED_BODY, "category": "api", "code": "401",
                "details": UNAUTHORIZED_BODY,
            }),
            Some(access_hint),
            "pbi-401",
        ),
        (
            "EVALUATE Forbidden",
            json!({
                "message": "Power BI returned HTTP 403 with an empty body.", "category": "api",
                "code": "403",
            }),
            Some(empty_body_hint),
            "pbi-403",
        ),
        (
            "EVALUATE Missing",
            json!({
                "message": NOT_FOUND_BODY, "category": "api", "code": "404",
                "details": NOT_FOUND_BODY,
            }),
            None,
            "pbi-404",
        ),
        (
            "EVALUATE Throttled",
            json!({
                "message": THROTTLED_BODY, "category": "api", "code": "429",
                "details": THROTTLED_BODY,
            }),
            None,
            "",
        ),
    ];
    for (query, expected, hint, expected_request_id) in cases {
        let input = json!({ "query": query });
        let (status, headers, body) = post(
            &app,
            "/connector-invoke/sales",
            Some(SESSION),
            execute_query(input),
        )
        .await;
        let (mut error, request_id) = failed_envelope(status, &headers, &body);

        let recovery_hint = error.as_object_mut().unwrap().remove("recoveryHint");
        match hint {
            Some(hint) => assert!(recovery_hint.unwrap().as_str().unwrap().starts_with(hint)),
            None => assert_eq!(recovery_hint, None, "{query}"),
        }
        assert_eq!(error, expected, "{query}");
        if expected_request_id.is_empty() {
            // Power BI sent no id: the answer carries the one rayfin-local sent.
            let sent = power_bi.requests().pop().unwrap();
            assert_eq!(request_id, sent.header("requestid"));
        } else {
            assert_eq!(request_id, expected_request_id);
        }
    }
}

#[tokio::test]
async fn tokens_without_a_dataset_scope_fall_back_to_execute_queries() {
    let power_bi = MockPowerBi::start().await;
    let az_token = az_token();
    let app = router(
        config(&power_bi.base_url),
        Arc::new(FakeCallers),
        FakeTokens::with_token(&az_token),
    );

    let input = json!({ "query": ROW_QUERY, "culture": "en-US" });
    let (status, headers, body) = post(
        &app,
        "/connector-invoke/sales",
        Some(SESSION),
        execute_query(input),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(headers["requestid"], "pbi-json-row");
    assert_eq!(
        json_body(&body),
        json!({
            "status": "Succeeded",
            "output": { "tables": [{ "rows": [{ "[x]": 1 }] }], "requestId": "pbi-json-row" },
            "errors": [],
        })
    );

    // Once refused, later queries go straight to executeQueries.
    let input = json!({ "query": "EVALUATE Nope" });
    let (_, _, body) = post(
        &app,
        "/connector-invoke/sales",
        Some(SESSION),
        execute_query(input),
    )
    .await;
    assert_eq!(
        json_body(&body)["output"],
        json!({
            "tables": [],
            "requestId": "pbi-json-nope",
            "queryError": {
                "message": "Query (1, 10) Failed to resolve name 'Nope'. It is not a valid table, \
                            variable, or function name.",
                "code": "3241803821",
            },
        })
    );

    let requests = power_bi.requests();
    let operations: Vec<&str> = requests
        .iter()
        .map(|r| r.path.rsplit('/').next().unwrap())
        .collect();
    assert_eq!(
        operations,
        ["executeDaxQueries", "executeQueries", "executeQueries"]
    );
    assert_eq!(
        requests[1].path,
        format!("/v1.0/myorg/groups/{WORKSPACE}/datasets/{ITEM}/executeQueries")
    );
    assert_eq!(
        requests[1].body,
        json!({
            "queries": [{ "query": ROW_QUERY }],
            "serializerSettings": { "includeNulls": true },
        })
    );
    assert_eq!(
        requests[1].header("authorization"),
        format!("Bearer {az_token}")
    );
    assert_eq!(requests[1].header("accept"), "application/json");
}

#[tokio::test]
async fn token_failures_are_api_errors_with_the_token_sources_hint() {
    let power_bi = MockPowerBi::start().await;
    let tokens = FakeTokens::failing("az isn't signed in", "Run `az login`, then retry.");
    let app = router(config(&power_bi.base_url), Arc::new(FakeCallers), tokens);

    let input = json!({ "query": ROW_QUERY });
    let (status, headers, body) = post(
        &app,
        "/connector-invoke/sales",
        Some(SESSION),
        execute_query(input),
    )
    .await;

    let (error, request_id) = failed_envelope(status, &headers, &body);
    assert_eq!(
        error,
        json!({
            "message": "rayfin-local couldn't get a Power BI token for the developer: \
                        az isn't signed in",
            "category": "api",
            "recoveryHint": "Run `az login`, then retry.",
        })
    );
    assert!(uuid::Uuid::parse_str(&request_id).is_ok());
    assert!(power_bi.requests().is_empty());
}

#[tokio::test]
async fn an_unreachable_power_bi_is_a_network_error() {
    let app = app(&closed_origin().await);

    let input = json!({ "query": ROW_QUERY });
    let (status, headers, body) = post(
        &app,
        "/connector-invoke/sales",
        Some(SESSION),
        execute_query(input),
    )
    .await;

    let (error, request_id) = failed_envelope(status, &headers, &body);
    assert_eq!(error["category"], "network");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("Couldn't reach Power BI at http://127.0.0.1:")
    );
    let details = error["details"].as_str().unwrap();
    assert!(details.starts_with("hostname=127.0.0.1, port=") && details.ends_with("kind=connect"));
    assert!(uuid::Uuid::parse_str(&request_id).is_ok());
}

#[tokio::test]
async fn a_success_that_is_not_arrow_is_an_unknown_error() {
    let power_bi = MockPowerBi::start().await;
    let app = app(&power_bi.base_url);

    let input = json!({ "query": "EVALUATE Html" });
    let (status, headers, body) = post(
        &app,
        "/connector-invoke/sales",
        Some(SESSION),
        execute_query(input),
    )
    .await;

    let (error, request_id) = failed_envelope(status, &headers, &body);
    assert_eq!(error["category"], "unknown");
    assert_eq!(request_id, "pbi-html");
}

#[tokio::test]
async fn refused_requests_never_reach_power_bi() {
    let power_bi = MockPowerBi::start().await;
    let app = app(&power_bi.base_url);
    let query = execute_query(json!({ "query": ROW_QUERY }));

    let (status, _, body) =
        post(&app, "/connector-invoke/nope", Some(SESSION), query.clone()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json_body(&body)["code"], "CONNECTOR_NOT_FOUND");
    assert_eq!(
        json_body(&body)["message"],
        "Connector 'nope' is not declared in rayfin/rayfin.yml. Declared connectors: \
         [sales, mine, telemetry, warehouse]."
    );

    let command = json!({ "operation": "executeCommand", "input": { "query": ROW_QUERY } });
    let (status, _, body) = post(&app, "/connector-invoke/sales", Some(SESSION), command).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["code"], "OPERATION_NOT_ALLOWED");
    assert!(json_body(&body)["message"].as_str().unwrap().starts_with(
        "Operation 'executeCommand' is not declared for connector 'sales'. Declared operations: \
         [executeQuery]."
    ));

    // Delegated: an anonymous caller gets 401 so a restored session refreshes and replays.
    let (status, _, body) = post(&app, "/connector-invoke/sales", None, query.clone()).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json_body(&body)["code"], "DELEGATED_SESSION_EXPIRED");

    let (status, headers, body) = post(
        &app,
        "/connector-invoke/sales",
        Some("expired"),
        query.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        "Bearer error=\"invalid_token\""
    );
    assert_eq!(
        json_body(&body),
        json!({
            "error": "invalid_token",
            "error_description": "The access token is invalid or has expired.",
        })
    );

    for input in [json!(null), json!({ "query": "" }), json!({ "query": 1 })] {
        let (status, _, body) = post(
            &app,
            "/connector-invoke/sales",
            Some(SESSION),
            execute_query(input),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(json_body(&body)["code"], "INVALID_INPUT");
    }

    let (status, _, body) = post(
        &app,
        "/connector-invoke/sales",
        Some(SESSION),
        json!(["executeQuery"]),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json_body(&body)["code"], "INVALID_REQUEST");

    assert!(power_bi.requests().is_empty());
}

#[tokio::test]
async fn other_connector_types_answer_501() {
    let power_bi = MockPowerBi::start().await;
    let app = app(&power_bi.base_url);

    let query = execute_query(json!({ "query": "StormEvents | take 1" }));
    let (status, _, body) = post(&app, "/connector-invoke/telemetry", Some(SESSION), query).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        json_body(&body),
        json!({
            "error": "not_supported",
            "message": "Connector type 'kusto' isn't supported by rayfin-local yet; deploy with \
                        `npx rayfin up` to use it.",
        })
    );

    let graphql = json!({ "query": "{ products { items { id } } }" });
    let (status, _, body) = post(
        &app,
        "/connectors/warehouse/graphql",
        Some(SESSION),
        graphql.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
        json_body(&body)["message"],
        "Connector type 'fabric-warehouse' isn't supported by rayfin-local yet; deploy with \
         `npx rayfin up` to use it."
    );

    let (status, ..) = post(&app, "/connectors/nope/graphql", Some(SESSION), graphql).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(power_bi.requests().is_empty());
}
