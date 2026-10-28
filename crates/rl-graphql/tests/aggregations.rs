//! `groupBy` aggregations against the basic model on a real database, as DAB answers them over
//! Fabric SQL: grand totals and groups, every operation, `distinct`, `having`, the read policy and
//! field permissions, paging, and DAB's errors.

mod common;

use common::{App, basic_model};
use rl_graphql::DataApiSettings;
use rl_model::{AppModel, Caller, Dialect};
use serde_json::{Value, json};

const C1: &str = "00000000-0000-4000-8000-0000000000c1";

/// Six todos: alice reads her own five and bob's public one (`T4`), bob his two and alice's
/// public one (`T1`). Returns the app and the callers alice and bob.
async fn todos(model: AppModel, settings: DataApiSettings) -> (App, Caller, Caller) {
    let app = App::with_settings(model, settings).await;
    let (alice, alice_token) = app.user("alice@example.test").await;
    let (bob, bob_token) = app.user("bob@example.test").await;
    let row = |n: u32, user: &str, status: &str, priority: &str, numbers: &str, public: u8| {
        format!(
            "('00000000-0000-4000-8000-00000000000{n}', '{user}', 'todo {n}', '{status}', \
             {priority}, {numbers}, {public}, '2026-01-0{n}T00:00:00.0000000Z', '{C1}')"
        )
    };
    app.seed(&format!(
        "INSERT INTO \"Categories\" (id, name, display_label, sortOrder, parent_id) VALUES
           ('{C1}', 'Home', NULL, 1, NULL);
         INSERT INTO \"Todos\" (id, user_id, title, status, priority, estimate, cost, weight,
           isPublic, createdAt, category_id) VALUES {}, {}, {}, {}, {}, {};",
        row(1, &alice, "doing", "'low'", "3, 1.1, 2.25", 1),
        row(2, &alice, "done", "NULL", "NULL, 10, NULL", 0),
        row(3, &alice, "doing", "'high'", "4, 2.25, 1", 0),
        row(4, &bob, "doing", "'high'", "5, 99.99, NULL", 1),
        row(5, &bob, "done", "'low'", "7, 3, NULL", 0),
        row(6, &alice, "done", "'low'", "3, 0.2, 0.1", 0),
    ));
    let (alice, bob) = (app.caller(Some(&alice_token)), app.caller(Some(&bob_token)));
    (app, alice, bob)
}

async fn basic() -> (App, Caller, Caller) {
    todos(basic_model(), DataApiSettings::default()).await
}

/// `todos.groupBy` of `query`.
async fn groups(app: &App, caller: &Caller, query: &str) -> Value {
    app.run(caller, query).await.data()["todos"]["groupBy"].clone()
}

#[tokio::test]
async fn grand_totals_over_the_callers_rows() {
    let (app, alice, bob) = basic().await;
    let query = "{ todos { groupBy { aggregations {
        n: count(field: estimate) total: sum(field: estimate) mean: avg(field: estimate)
        low: min(field: estimate) high: max(field: estimate) cost: sum(field: cost)
        meanCost: avg(field: cost) maxCost: max(field: cost) meanWeight: avg(field: weight)
        minWeight: min(field: weight)
    } } } }";
    // Fabric SQL: avg(int) is an int (15 / 4); decimal sums keep their scale, decimal averages
    // have 6 decimals.
    assert_eq!(
        groups(&app, &alice, query).await,
        json!([{"aggregations": {
            "n": 4, "total": 15, "mean": 3, "low": 3, "high": 5, "cost": 113.54,
            "meanCost": 22.708, "maxCost": 99.99, "meanWeight": 1.116667, "minWeight": 0.1,
        }}])
    );
    // Bob's rows: his own and alice's public one.
    let query =
        "{ todos { groupBy { aggregations { count(field: estimate) sum(field: cost) } } } }";
    assert_eq!(
        groups(&app, &bob, query).await,
        json!([{"aggregations": {"count": 3, "sum": 104.09}}])
    );
}

#[tokio::test]
async fn groups_by_one_and_two_fields() {
    let (app, alice, _) = basic().await;
    let query = "{ todos { groupBy(fields: [status, status]) { fields { status } aggregations {
        n: count(field: estimate) total: sum(field: estimate) cost: sum(field: cost)
        top: max(field: cost) } } } }";
    assert_eq!(
        groups(&app, &alice, query).await,
        json!([
            {"fields": {"status": "doing"},
             "aggregations": {"n": 3, "total": 12, "cost": 103.34, "top": 99.99}},
            {"fields": {"status": "done"},
             "aggregations": {"n": 1, "total": 3, "cost": 10.2, "top": 10}},
        ])
    );
    let query = "{ todos { groupBy(fields: [status, priority]) {
        __typename fields { p: priority status } aggregations { __typename sum(field: estimate) count(field: estimate) }
    } } }";
    let group = |status: &str, priority: Value, sum: Value, count: u32| {
        json!({
            "__typename": "TodoGroupBy",
            "fields": {"p": priority, "status": status},
            "aggregations": {"__typename": "TodoAggregations", "sum": sum, "count": count},
        })
    };
    assert_eq!(
        groups(&app, &alice, query).await,
        json!([
            group("doing", json!("high"), json!(9), 2),
            group("doing", json!("low"), json!(3), 1),
            group("done", Value::Null, Value::Null, 0),
            group("done", json!("low"), json!(3), 1),
        ])
    );
}

#[tokio::test]
async fn filters_distinct_and_having() {
    let (app, alice, _) = basic().await;
    let run = |query: &'static str| groups(&app, &alice, query);
    // `filter` selects rows before grouping.
    assert_eq!(
        run(
            r#"{ todos(filter: { priority: { eq: "low" } }) { groupBy(fields: [status]) {
            fields { status } aggregations { sum(field: estimate) } } } }"#
        )
        .await,
        json!([
            {"fields": {"status": "doing"}, "aggregations": {"sum": 3}},
            {"fields": {"status": "done"}, "aggregations": {"sum": 3}},
        ])
    );
    assert_eq!(
        run("{ todos { groupBy { aggregations {
            n: count(field: estimate, distinct: true) total: sum(field: estimate, distinct: true)
            mean: avg(field: estimate, distinct: true) all: count(field: estimate, distinct: false)
        } } } }")
        .await,
        json!([{"aggregations": {"n": 3, "total": 12, "mean": 4, "all": 4}}])
    );
    // `having` filters groups on the aggregated value; every aggregation's `having` applies.
    let having = |filter: &str| {
        format!(
            "{{ todos {{ groupBy(fields: [status]) {{ fields {{ status }} aggregations {{
               total: sum(field: estimate, having: {filter}) }} }} }} }}"
        )
    };
    let statuses = |groups: Value| -> Vec<Value> {
        groups
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["fields"]["status"].clone())
            .collect()
    };
    for (filter, expected) in [
        ("{ gt: 5 }", vec!["doing"]),
        ("{ gte: 3, lt: 12 }", vec!["done"]),
        ("{ in: [3, 12] }", vec!["doing", "done"]),
        ("{ neq: 12 }", vec!["done"]),
        ("{ eq: 12.0 }", vec!["doing"]),
        ("{ isNull: true }", vec![]),
        ("{ isNull: false }", vec!["doing", "done"]),
        ("{ gt: null }", vec![]),
        ("{ }", vec![]),
    ] {
        let response = app.run(&alice, &having(filter)).await;
        assert_eq!(
            statuses(response.data()["todos"]["groupBy"].clone()),
            expected,
            "having: {filter}"
        );
    }
    assert_eq!(
        run("{ todos { groupBy(fields: [status]) { fields { status } aggregations {
            a: sum(field: estimate, having: { gt: 2 }) b: max(field: cost, having: { lt: 50 }) } } } }")
        .await,
        json!([{"fields": {"status": "done"}, "aggregations": {"a": 3, "b": 10}}])
    );
    // A grand total that fails its `having` is no group at all.
    assert_eq!(
        run("{ todos { groupBy { aggregations { sum(field: estimate, having: { gt: 100 }) } } } }")
            .await,
        json!([])
    );
}

#[tokio::test]
async fn empty_sets_and_nested_connections() {
    let (app, alice, _) = basic().await;
    // People is empty: the grand total is still one group.
    let query = "{ people { groupBy { aggregations {
        count(field: level) sum(field: level) avg(field: rate) min(field: rate) max(field: level)
    } } } }";
    assert_eq!(
        app.run(&alice, query).await.data()["people"]["groupBy"],
        json!([{"aggregations": {
            "count": 0, "sum": null, "avg": null, "min": null, "max": null,
        }}])
    );
    let query = "{ people { groupBy(fields: [level]) { aggregations { count(field: level) } } } }";
    assert_eq!(
        app.run(&alice, query).await.data()["people"]["groupBy"],
        json!([])
    );
    // A to-many relationship groups its own rows, under the target's policy.
    let query = "{ categories { items { name todos { groupBy(fields: [status]) {
        fields { status } aggregations { count(field: estimate) } } } } } }";
    assert_eq!(
        app.run(&alice, query).await.items("categories"),
        [json!({"name": "Home", "todos": {"groupBy": [
            {"fields": {"status": "doing"}, "aggregations": {"count": 3}},
            {"fields": {"status": "done"}, "aggregations": {"count": 1}},
        ]}})]
    );
}

#[tokio::test]
async fn field_permissions_and_dab_errors() {
    let (app, _, _) = basic().await;
    let anonymous = Caller::anonymous();
    // Anonymous reads public todos, without `estimate` or `priority`.
    let errors = [
        (
            "{ todos { groupBy { aggregations { sum(field: estimate) } } } }",
            "Access forbidden to field 'estimate' referenced in the aggregation function 'sum'.",
            "AuthorizationCheckFailed",
        ),
        (
            "{ todos { groupBy(fields: [status, priority]) { fields { status } } } }",
            "Access forbidden to field 'priority' referenced in the groupBy argument.",
            "AuthorizationCheckFailed",
        ),
        (
            "{ todos { groupBy(fields: [status]) { fields { title } } } }",
            "Groupby fields in selection must match the fields in the groupby argument.",
            "BadRequest",
        ),
        (
            "{ todos { groupBy { fields { __typename } } } }",
            "Groupby fields in selection must match the fields in the groupby argument.",
            "BadRequest",
        ),
        (
            "{ todos { items { id } groupBy(fields: [status]) { fields { status } } } }",
            "Cannot have both groupBy and items in the same query",
            "BadRequest",
        ),
        (
            "{ todos(orderBy: { title: ASC }) { groupBy(fields: [status]) { fields { status } } } }",
            "OrderBy field 'title' must be present in the groupBy fields.",
            "BadRequest",
        ),
    ];
    for (query, message, code) in errors {
        let response = app.run(&anonymous, query).await;
        assert_eq!(response.status, 200, "{query}");
        assert_eq!(response.messages(), [message], "{query}");
        assert_eq!(response.codes(), [code], "{query}");
        assert_eq!(response.body["data"], Value::Null, "{query}");
    }
    let response = app
        .run(
            &anonymous,
            "{ todos { groupBy(fields: [status]) { fields { status } } } }",
        )
        .await;
    assert_eq!(
        response.data()["todos"]["groupBy"],
        json!([{"fields": {"status": "doing"}}])
    );
    // Only numeric fields aggregate.
    let response = app
        .run(
            &anonymous,
            "{ todos { groupBy { aggregations { count(field: title) } } } }",
        )
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.errors().len(), 1, "{}", response.body);
    assert!(response.body.get("data").is_none());
}

#[tokio::test]
async fn groups_order_and_page_like_rows() {
    let (app, alice, _) = basic().await;
    let page = |args: String| {
        format!(
            "{{ todos({args}) {{ groupBy(fields: [status]) {{ fields {{ status }} }} hasNextPage \
             endCursor }} }}"
        )
    };
    let response = app
        .run(&alice, &page("orderBy: { status: DESC }".into()))
        .await;
    assert_eq!(
        response.data()["todos"],
        json!({"groupBy": [{"fields": {"status": "done"}}, {"fields": {"status": "doing"}}],
               "hasNextPage": false, "endCursor": null})
    );
    let response = app.run(&alice, &page("first: 1".into())).await;
    let connection = &response.data()["todos"];
    assert_eq!(
        connection["groupBy"],
        json!([{"fields": {"status": "doing"}}])
    );
    assert_eq!(connection["hasNextPage"], true);
    let cursor = connection["endCursor"].as_str().unwrap();
    let response = app
        .run(&alice, &page(format!("first: 1, after: \"{cursor}\"")))
        .await;
    assert_eq!(
        response.data()["todos"],
        json!({"groupBy": [{"fields": {"status": "done"}}], "hasNextPage": false, "endCursor": null})
    );

    // Without `first`, DAB's default page size caps the groups too.
    let settings = DataApiSettings {
        default_page_size: 1,
        ..DataApiSettings::default()
    };
    let (app, alice, _) = todos(basic_model(), settings).await;
    let query = "{ todos { groupBy(fields: [status]) { fields { status } } } }";
    assert_eq!(
        groups(&app, &alice, query).await,
        json!([{"fields": {"status": "doing"}}])
    );
}

#[tokio::test]
async fn postgresql_averages_are_exact() {
    let mut model = basic_model();
    model.dialect = Dialect::Postgresql;
    let (app, alice, _) = todos(model, DataApiSettings::default()).await;
    let query = "{ todos { groupBy { aggregations { avg(field: estimate) } } } }";
    assert_eq!(
        groups(&app, &alice, query).await,
        json!([{"aggregations": {"avg": 3.75}}])
    );
}
