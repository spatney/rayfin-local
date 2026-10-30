//! Category A connectors end to end against a real SQL Server: the `inventory` connector of the
//! `connectors` fixture app, its routes, statements and answers. Set `RAYFIN_LOCAL_TEST_MSSQL`
//! to `host,port;user;password` (for example a `mcr.microsoft.com/mssql/server:2022-latest`
//! container); without it the tests are skipped with a note. Each test creates its own database
//! and drops it.

mod common;

use axum::Router;
use axum::http::StatusCode;
use common::sql::{Database, HAMMER, RAKE, SAW, admin, app, server};
use common::{json_body, post};
use serde_json::{Value, json};
/// POSTs a GraphQL document as `bearer`; returns the JSON answer (HTTP 200).
async fn graphql(app: &Router, bearer: Option<&str>, query: &str) -> Value {
    let (status, _, body) = post(
        app,
        "/connectors/inventory/graphql",
        bearer,
        json!({ "query": query }),
    )
    .await;
    let body = json_body(&body);
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn queries_read_the_source_with_roles_and_policies() {
    let Some(server) = server() else {
        return;
    };
    let database = Database::create(&server, "queries").await;
    let app = app(&server, &database.name, "fabric-sqldatabase", None);

    // Anonymous callers read what the anonymous role includes.
    let body = graphql(
        &app,
        None,
        "{ categories(orderBy: {name: ASC}) { items { categoryId name } } }",
    )
    .await;
    assert_eq!(
        body,
        json!({"data": {"categories": {"items": [
            {"categoryId": 2, "name": "Garden"},
            {"categoryId": 1, "name": "Tools"},
        ]}}})
    );
    let body = graphql(&app, None, "{ categories { items { name description } } }").await;
    assert_eq!(
        body["errors"][0]["extensions"]["code"],
        "AUTH_NOT_AUTHORIZED"
    );
    assert_eq!(
        body["errors"][0]["path"],
        json!(["categories", "items", 0, "description"])
    );

    // Values come out in DAB's formats; relationships nest to-one objects and to-many connections.
    let body = graphql(
        &app,
        Some("alice"),
        &format!(
            r#"{{ product_by_pk(productId: "{HAMMER}") {{ productId name price inStock createdAt
              category {{ name }} stockLevels(orderBy: {{warehouseCode: ASC}}) {{ items {{
              warehouseCode quantity }} }} }} }}"#
        ),
    )
    .await;
    assert_eq!(
        body,
        json!({"data": {"product_by_pk": {
            "productId": HAMMER, "name": "Hammer", "price": 12.5, "inStock": true,
            "createdAt": "2024-01-15T10:30:00.123Z",
            "category": {"name": "Tools"},
            "stockLevels": {"items": [
                {"warehouseCode": "NYC", "quantity": 0},
                {"warehouseCode": "SEA", "quantity": 5},
            ]},
        }}})
    );

    // Filters (text with LIKE wildcards and quotes, `in`, relationships), order and pages.
    let body = graphql(
        &app,
        Some("alice"),
        r#"{ products(filter: {name: {contains: "50%"}}) { items { name } } }"#,
    )
    .await;
    assert_eq!(
        body["data"]["products"]["items"],
        json!([{"name": "Saw \"Pro\" 50%"}])
    );
    let page = |after: &str| {
        format!(
            r#"{{ products(first: 2, orderBy: {{price: DESC}}{after}, filter: {{category: {{name:
               {{eq: "tools"}}}}, or: [{{inStock: {{eq: true}}}}, {{price: {{gte: 30}}}}]}})
               {{ items {{ name price }} hasNextPage endCursor }} }}"#
        )
    };
    let body = graphql(&app, Some("alice"), &page("")).await;
    let products = &body["data"]["products"];
    assert_eq!(
        products["items"],
        json!([{"name": "Saw \"Pro\" 50%", "price": 30.0}, {"name": "Hammer", "price": 12.5}])
    );
    assert_eq!(products["hasNextPage"], false);
    let body = graphql(
        &app,
        Some("alice"),
        &page("").replace("first: 2", "first: 1"),
    )
    .await;
    let cursor = body["data"]["products"]["endCursor"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(body["data"]["products"]["hasNextPage"], true);
    let body = graphql(
        &app,
        Some("alice"),
        &page(&format!(r#", after: "{cursor}""#)),
    )
    .await;
    assert_eq!(
        body["data"]["products"]["items"],
        json!([{"name": "Hammer", "price": 12.5}])
    );

    // A composite key, and a keyless entity whose dates carry offsets.
    let body = graphql(
        &app,
        Some("alice"),
        &format!(
            r#"{{ stockLevel_by_pk(productId: "{HAMMER}", warehouseCode: "NYC") {{ quantity
              product {{ name }} }} }}"#
        ),
    )
    .await;
    assert_eq!(
        body,
        json!({"data": {"stockLevel_by_pk": {"quantity": 0, "product": {"name": "Hammer"}}}})
    );
    let body = graphql(
        &app,
        Some("alice"),
        "{ auditEntries(orderBy: {at: ASC}) { items { at action detail } } }",
    )
    .await;
    assert_eq!(
        body["data"]["auditEntries"]["items"],
        json!([
            {"at": "2024-01-15T08:30:00.000Z", "action": "login", "detail": null},
            {"at": "2024-01-16T00:00:00.000Z", "action": "logout", "detail": "bye"},
        ])
    );

    // Anonymous callers may not read the excluded field; signed-in-only entities need a token.
    let body = graphql(&app, None, "{ products { items { name ownerEmail } } }").await;
    assert_eq!(
        body["errors"][0]["extensions"]["code"],
        "AUTH_NOT_AUTHORIZED"
    );
    let (status, ..) = post(
        &app,
        "/connectors/inventory/graphql",
        None,
        json!({ "query": "{ auditEntries { items { action } } }" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // An entity out of step with its table: DAB's error, the server's message as its detail.
    admin(
        &server,
        &database.name,
        "ALTER TABLE dbo.categories DROP COLUMN description",
    )
    .await;
    let body = graphql(
        &app,
        Some("alice"),
        "{ categories { items { name description } } }",
    )
    .await;
    let error = &body["errors"][0];
    assert_eq!(
        error["extensions"]["code"], "DatabaseOperationFailed",
        "{body}"
    );
    assert_eq!(
        error["message"],
        "While processing your request the database ran into an error."
    );
    assert!(
        error["extensions"]["detail"]
            .as_str()
            .unwrap()
            .contains("Invalid column name 'description'"),
        "{body}"
    );
    assert_eq!(error["path"], json!(["categories"]));

    database.drop().await;
}

#[tokio::test]
async fn mutations_on_a_sql_database_return_the_row() {
    let Some(server) = server() else {
        return;
    };
    let database = Database::create(&server, "mutations").await;
    let app = app(&server, &database.name, "fabric-sqldatabase", None);

    // Server-generated columns are optional and come back filled in.
    let body = graphql(
        &app,
        Some("alice"),
        r#"mutation { createProduct(item: {categoryId: 2, name: "Shovel", price: 19.99,
             inStock: true, ownerEmail: "alice@contoso.com"}) { productId name price createdAt
             category { name } } }"#,
    )
    .await;
    let created = &body["data"]["createProduct"];
    assert_eq!(created["name"], "Shovel", "{body}");
    assert_eq!(created["price"], 19.99);
    assert_eq!(created["category"], json!({"name": "Garden"}));
    let id = created["productId"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 36);
    assert!(created["createdAt"].as_str().unwrap().ends_with('Z'));

    // Update and delete run under their policy: only the owner matches.
    let update = |id: &str| {
        format!(
            r#"mutation {{ updateProduct(productId: "{id}", item: {{price: 13}}) {{ price }} }}"#
        )
    };
    let body = graphql(&app, Some("alice"), &update(HAMMER)).await;
    assert_eq!(body, json!({"data": {"updateProduct": {"price": 13.0}}}));
    let body = graphql(&app, Some("bob"), &update(HAMMER)).await;
    assert_eq!(body["errors"][0]["extensions"]["code"], "ItemNotFound");
    assert_eq!(
        body["errors"][0]["message"],
        format!("Could not find item with <productId: {HAMMER}>")
    );
    let body = graphql(
        &app,
        Some("alice"),
        &format!(r#"mutation {{ deleteProduct(productId: "{id}") {{ name }} }}"#),
    )
    .await;
    assert_eq!(body, json!({"data": {"deleteProduct": {"name": "Shovel"}}}));
    let body = graphql(
        &app,
        Some("alice"),
        &format!(r#"{{ product_by_pk(productId: "{id}") {{ name }} }}"#),
    )
    .await;
    assert_eq!(body, json!({"data": {"product_by_pk": null}}));

    // Composite keys address the row; constraint violations are DAB's database error.
    let create = format!(
        r#"mutation {{ createStockLevel(item: {{productId: "{RAKE}", warehouseCode: "NYC",
             quantity: 3}}) {{ warehouseCode quantity }} }}"#
    );
    let body = graphql(&app, Some("alice"), &create).await;
    assert_eq!(
        body,
        json!({"data": {"createStockLevel": {"warehouseCode": "NYC", "quantity": 3}}})
    );
    let body = graphql(&app, Some("alice"), &create).await;
    assert_eq!(
        body["errors"][0]["extensions"]["code"],
        "DatabaseOperationFailed"
    );
    assert!(
        body["errors"][0]["extensions"]["detail"]
            .as_str()
            .unwrap()
            .contains("PRIMARY KEY"),
        "{body}"
    );
    let body = graphql(
        &app,
        Some("alice"),
        &format!(
            r#"mutation {{ updateStockLevel(productId: "{RAKE}", warehouseCode: "NYC",
                 item: {{quantity: 4}}) {{ quantity }} }}"#
        ),
    )
    .await;
    assert_eq!(body, json!({"data": {"updateStockLevel": {"quantity": 4}}}));
    let body = graphql(
        &app,
        Some("alice"),
        &format!(
            r#"mutation {{ deleteStockLevel(productId: "{RAKE}", warehouseCode: "NYC") {{
                 quantity }} }}"#
        ),
    )
    .await;
    assert_eq!(body, json!({"data": {"deleteStockLevel": {"quantity": 4}}}));

    // A role without the action never reaches the database.
    let body = graphql(
        &app,
        None,
        r#"mutation { createCategory(item: {name: "x"}) { name } }"#,
    )
    .await;
    assert!(body["errors"][0]["message"].as_str().is_some(), "{body}");

    database.drop().await;
}

#[tokio::test]
async fn warehouses_answer_a_status_and_analytics_endpoints_only_read() {
    let Some(server) = server() else {
        return;
    };
    let database = Database::create(&server, "dialects").await;

    let warehouse = app(&server, &database.name, "fabric-warehouse", None);
    let body = graphql(
        &warehouse,
        Some("alice"),
        &format!(
            r#"mutation {{ updateProduct(productId: "{HAMMER}", item: {{name: "Mallet"}}) {{
                 result __typename }} }}"#
        ),
    )
    .await;
    assert_eq!(
        body,
        json!({"data": {"updateProduct": {"result": "success", "__typename": "DbOperationResult"}}})
    );
    let body = graphql(
        &warehouse,
        Some("bob"),
        &format!(r#"mutation {{ deleteProduct(productId: "{HAMMER}") {{ result }} }}"#),
    )
    .await;
    assert_eq!(body, json!({"data": {"deleteProduct": null}}));

    let analytics = app(&server, &database.name, "fabric-sqlanalytics", None);
    let body = graphql(
        &analytics,
        Some("alice"),
        &format!(r#"{{ product_by_pk(productId: "{SAW}") {{ name }} }}"#),
    )
    .await;
    assert_eq!(
        body,
        json!({"data": {"product_by_pk": {"name": "Saw \"Pro\" 50%"}}})
    );
    let (status, _, body) = post(
        &analytics,
        "/connectors/inventory/graphql",
        Some("alice"),
        json!({ "query": "mutation { deleteProduct(productId: \"x\") { name } }" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(json_body(&body)["errors"][0]["message"].is_string());

    database.drop().await;
}

#[tokio::test]
async fn an_unreachable_database_says_what_to_check() {
    let Some(server) = server() else {
        return;
    };
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let app = app(&server, "nowhere", "fabric-sqldatabase", Some(closed));
    let body = graphql(&app, Some("alice"), "{ categories { items { name } } }").await;
    let message = body["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.contains("Couldn't connect to the database of connector 'inventory'"),
        "{message}"
    );
    assert_eq!(body["data"], Value::Null);
}
