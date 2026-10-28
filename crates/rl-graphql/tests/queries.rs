//! Queries against the golden models on a real database: filters, ordering, pagination,
//! relationships, permissions, policies and errors, as DAB answers them.

mod common;

use common::{App, basic_model, explorer_model};
use rl_model::Caller;
use serde_json::{Value, json};

const C1: &str = "00000000-0000-4000-8000-0000000000c1";
const C2: &str = "00000000-0000-4000-8000-0000000000c2";
const T1: &str = "00000000-0000-4000-8000-000000000001";
const T2: &str = "00000000-0000-4000-8000-000000000002";
const T3: &str = "00000000-0000-4000-8000-000000000003";
const T4: &str = "00000000-0000-4000-8000-000000000004";

/// The basic fixture with categories, todos (two users) and comments; alice is signed in.
async fn basic() -> (App, Caller, String) {
    let app = App::new(basic_model()).await;
    let (alice, token) = app.user("alice@example.test").await;
    let bob = "bob-user-id";
    app.seed(&format!(
        "INSERT INTO \"Categories\" (id, name, display_label, sortOrder, parent_id) VALUES
           ('{C1}', 'Home', 'House', 1, NULL), ('{C2}', 'Work', NULL, 2, '{C1}');
         INSERT INTO \"Todos\" (id, user_id, title, slug, contactEmail, status, priority, estimate,
           cost, weight, isPublic, done, createdAt, dueDate, secretNote, category_id) VALUES
           ('{T1}', '{alice}', 'Buy milk', 'buy-milk', 'a@b.co', 'doing', 'low', 3, 1.5, 2.25, 1, 0,
            '2026-01-01T00:00:00.0000000Z', '2026-02-01T10:00:00.0000000Z', 's1', '{C1}'),
           ('{T2}', '{alice}', 'Walk dog', '50%_off', NULL, 'done', NULL, NULL, 10, NULL, 0, 1,
            '2026-01-02T00:00:00.0000000Z', NULL, NULL, '{C2}'),
           ('{T3}', '{bob}', 'PAY bills', NULL, NULL, 'doing', 'high', 5, 99.99, NULL, 1, NULL,
            '2026-01-03T00:00:00.0000000Z', '2026-03-01T00:00:00.0000000Z', NULL, NULL),
           ('{T4}', '{bob}', 'secret plan', NULL, NULL, 'doing', NULL, 0, 0, NULL, 0, NULL,
            '2026-01-04T00:00:00.0000000Z', NULL, NULL, '{C1}');
         INSERT INTO \"Comments\" (id, author_id, authorEmail, body, createdAt, todo_id) VALUES
           ('00000000-0000-4000-8000-0000000000a1', '{alice}', 'alice@example.test', 'first',
            '2026-01-05T00:00:00.0000000Z', '{T1}'),
           ('00000000-0000-4000-8000-0000000000a2', '{alice}', 'alice@example.test', 'second',
            '2026-01-06T00:00:00.0000000Z', '{T1}');"
    ));
    let caller = app.caller(Some(&token));
    (app, caller, alice)
}

/// The titles `filter` selects for alice (who sees her todos and the public ones).
async fn titles(app: &App, caller: &Caller, filter: &str) -> Vec<String> {
    let query = format!(
        "query {{ todos(filter: {filter}, orderBy: {{ title: ASC }}) {{ items {{ title }} }} }}"
    );
    let response = app.run(caller, &query).await;
    response
        .column("todos", "title")
        .into_iter()
        .map(|t| t.as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn every_operator_of_every_scalar() {
    let (app, alice, _) = basic().await;
    let cases: &[(&str, &[&str])] = &[
        // String: case-insensitive, LIKE wildcards escaped, `neq` excludes NULL.
        (r#"{ title: { eq: "buy MILK" } }"#, &["Buy milk"]),
        (r#"{ slug: { contains: "%_" } }"#, &["Walk dog"]),
        (r#"{ title: { startsWith: "pay" } }"#, &["PAY bills"]),
        (r#"{ title: { endsWith: "DOG" } }"#, &["Walk dog"]),
        (r#"{ title: { notContains: "a" } }"#, &["Buy milk"]),
        (r#"{ slug: { neq: "buy-milk" } }"#, &["Walk dog"]),
        (r#"{ slug: { isNull: true } }"#, &["PAY bills"]),
        (
            r#"{ title: { in: ["walk DOG", "nope", null] } }"#,
            &["Walk dog"],
        ),
        (r#"{ title: { gt: "c" } }"#, &["PAY bills", "Walk dog"]),
        // Int.
        ("{ estimate: { eq: 3 } }", &["Buy milk"]),
        ("{ estimate: { gt: 3 } }", &["PAY bills"]),
        ("{ estimate: { gte: 3 } }", &["Buy milk", "PAY bills"]),
        ("{ estimate: { lt: 5 } }", &["Buy milk"]),
        ("{ estimate: { lte: 5 } }", &["Buy milk", "PAY bills"]),
        ("{ estimate: { neq: 3 } }", &["PAY bills"]),
        ("{ estimate: { isNull: true } }", &["Walk dog"]),
        ("{ estimate: { in: [3, 5] } }", &["Buy milk", "PAY bills"]),
        // Decimal: integer and exponent literals.
        ("{ cost: { gt: 1.5 } }", &["PAY bills", "Walk dog"]),
        ("{ cost: { eq: 10 } }", &["Walk dog"]),
        ("{ cost: { lt: 1e1 } }", &["Buy milk"]),
        ("{ weight: { gte: 2.25, lte: 2.25 } }", &["Buy milk"]),
        // Boolean.
        ("{ isPublic: { eq: false } }", &["Walk dog"]),
        ("{ done: { neq: true } }", &["Buy milk"]),
        ("{ done: { isNull: true } }", &["PAY bills"]),
        ("{ done: { in: [true] } }", &["Walk dog"]),
        // DateTime: any precision and offset, compared as instants.
        (
            r#"{ dueDate: { gt: "2026-02-01T10:00:00.000Z" } }"#,
            &["PAY bills"],
        ),
        (
            r#"{ dueDate: { lte: "2026-02-01T11:00:00+01:00" } }"#,
            &["Buy milk"],
        ),
        (
            r#"{ createdAt: { in: ["2026-01-02T00:00:00Z"] } }"#,
            &["Walk dog"],
        ),
        (
            r#"{ dueDate: { isNull: false } }"#,
            &["Buy milk", "PAY bills"],
        ),
        // UUID: any case.
        (
            "{ id: { eq: \"00000000-0000-4000-8000-000000000001\" } }",
            &["Buy milk"],
        ),
        (
            "{ category_id: { neq: \"00000000-0000-4000-8000-0000000000C1\" } }",
            &["Walk dog"],
        ),
        (
            "{ id: { contains: \"00000000-0000-4000-8000-000000000003\" } }",
            &["PAY bills"],
        ),
        // Combinators, nesting, several operators on one field.
        (
            r#"{ or: [{ title: { eq: "Buy milk" } }, { estimate: { gt: 4 } }] }"#,
            &["Buy milk", "PAY bills"],
        ),
        (
            "{ and: [{ isPublic: { eq: true } }, { or: [{ estimate: { gte: 4 } }, { done: { eq: false } }] }] }",
            &["Buy milk", "PAY bills"],
        ),
        ("{ and: { estimate: { eq: 5 } } }", &["PAY bills"]),
        // DAB: an empty list or object filters everything out.
        ("{ estimate: { in: [] } }", &[]),
        ("{ and: [] }", &[]),
        ("{ title: {} }", &[]),
        // Relationships: EXISTS under the target's read policy; `isNull` on a to-one.
        (r#"{ category: { name: { eq: "home" } } }"#, &["Buy milk"]),
        ("{ category: { isNull: true } }", &["PAY bills"]),
        (r#"{ comments: { body: { eq: "second" } } }"#, &["Buy milk"]),
    ];
    for (filter, expected) in cases {
        assert_eq!(titles(&app, &alice, filter).await, *expected, "{filter}");
    }
}

#[tokio::test]
async fn in_with_more_than_100_values_is_refused() {
    let (app, alice, _) = basic().await;
    let values: Vec<String> = (0..101).map(|n| n.to_string()).collect();
    let query = format!(
        "{{ todos(filter: {{ estimate: {{ in: [{}] }} }}) {{ items {{ id }} }} }}",
        values.join(", ")
    );
    let response = app.run(&alice, &query).await;
    assert_eq!(response.status, 200);
    assert_eq!(
        response.messages(),
        ["IN operator filter object cannot process more than 100 values at a time."]
    );
    assert_eq!(response.codes(), ["BadRequest"]);
    assert_eq!(response.body["data"], Value::Null);
    assert_eq!(response.errors()[0]["path"], json!(["todos"]));
}

#[tokio::test]
async fn order_by_keys_then_the_primary_key() {
    let (app, alice, _) = basic().await;
    let query = "{ todos(orderBy: { priority: DESC, title: ASC }) { items { title } } }";
    let response = app.run(&alice, query).await;
    // DESC puts NULL last, as SQL Server does.
    assert_eq!(
        response.column("todos", "title"),
        [json!("Buy milk"), json!("PAY bills"), json!("Walk dog")]
    );
    let query = "{ todos(orderBy: { estimate: ASC, title: null }) { items { title } } }";
    assert_eq!(
        app.run(&alice, query).await.column("todos", "title"),
        [json!("Walk dog"), json!("Buy milk"), json!("PAY bills")]
    );
    // Without orderBy: the primary key.
    let query = "{ todos { items { id } } }";
    assert_eq!(
        app.run(&alice, query).await.column("todos", "id"),
        [json!(T1), json!(T2), json!(T3)]
    );
}

#[tokio::test]
async fn cursors_page_through_every_row_once() {
    let (app, alice, _) = basic().await;
    for (order, expected) in [
        ("estimate: ASC", ["Walk dog", "Buy milk", "PAY bills"]),
        ("estimate: DESC", ["PAY bills", "Buy milk", "Walk dog"]),
        (
            "dueDate: DESC, cost: ASC",
            ["PAY bills", "Buy milk", "Walk dog"],
        ),
    ] {
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let after_arg = after
                .as_ref()
                .map(|c| format!(", after: \"{c}\""))
                .unwrap_or_default();
            let query = format!(
                "{{ todos(orderBy: {{ {order} }}, first: 1{after_arg}) {{ items {{ title }} \
                 endCursor hasNextPage }} }}"
            );
            let response = app.run(&alice, &query).await;
            let page = &response.data()["todos"];
            seen.extend(
                page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["title"].clone()),
            );
            if page["hasNextPage"] == json!(false) {
                assert_eq!(page["endCursor"], Value::Null);
                break;
            }
            let cursor = page["endCursor"].as_str().unwrap();
            assert!(
                cursor
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c))
            );
            after = Some(cursor.to_owned());
        }
        assert_eq!(seen, expected.map(|t| json!(t)), "{order}");
    }

    // A cursor of another order, or garbage, is refused with DAB's messages.
    let query = "{ todos(orderBy: { estimate: ASC }, first: 1) { endCursor } }";
    let cursor = app.run(&alice, query).await.data()["todos"]["endCursor"]
        .as_str()
        .unwrap()
        .to_owned();
    let query =
        format!("{{ todos(orderBy: {{ title: ASC }}, after: \"{cursor}\") {{ items {{ id }} }} }}");
    assert_eq!(
        app.run(&alice, &query).await.messages(),
        [
            "Could not match order by column title with a column in the pagination token with the same name and direction."
        ]
    );
    let response = app
        .run(&alice, "{ todos(after: \"abc\") { items { id } } }")
        .await;
    assert_eq!(
        response.messages(),
        ["abc is not a valid pagination token."]
    );
    assert_eq!(response.codes(), ["BadRequest"]);
}

#[tokio::test]
async fn first_follows_dab_page_limits() {
    let (app, alice, _) = basic().await;
    let response = app
        .run(&alice, "{ todos(first: 2) { items { id } hasNextPage } }")
        .await;
    assert_eq!(response.items("todos").len(), 2);
    assert_eq!(response.data()["todos"]["hasNextPage"], json!(true));
    let response = app
        .run(&alice, "{ todos(first: -1) { items { id } } }")
        .await;
    assert_eq!(response.items("todos").len(), 3);
    for first in ["0", "-2", "100001"] {
        let query = format!("{{ todos(first: {first}) {{ items {{ id }} }} }}");
        let response = app.run(&alice, &query).await;
        assert_eq!(
            response.messages(),
            [format!(
                "Invalid number of items requested, first argument must be either -1 or a \
                 positive number within the max page size limit of 100000. Actual value: {first}"
            )]
        );
    }
}

#[tokio::test]
async fn relationships_nest_with_their_own_policies() {
    let (app, alice, _) = basic().await;
    let query = "{ todos(orderBy: { title: ASC }) { items { title category { name parent { name } } \
                 comments(orderBy: { body: DESC }) { items { body todo { title } } } } } }";
    let response = app.run(&alice, query).await;
    let items = response.items("todos");
    assert_eq!(
        items[0]["category"],
        json!({"name": "Home", "parent": null})
    );
    assert_eq!(
        items[0]["comments"]["items"],
        json!([
            {"body": "second", "todo": {"title": "Buy milk"}},
            {"body": "first", "todo": {"title": "Buy milk"}},
        ])
    );
    assert_eq!(items[1]["category"], Value::Null);
    assert_eq!(items[1]["comments"], json!({"items": []}));
    assert_eq!(items[2]["category"]["parent"], json!({"name": "Home"}));

    // A to-many from the other side, with arguments and paging fields.
    let query = "{ categories(orderBy: { name: ASC }) { items { name todos(first: 1, orderBy: { title: DESC }) { items { title } hasNextPage } } } }";
    let response = app.run(&alice, query).await;
    let items = response.items("categories");
    // Home has Buy milk (alice's) and secret plan (bob's, hidden by the Todo read policy).
    assert_eq!(
        items[0]["todos"],
        json!({"items": [{"title": "Buy milk"}], "hasNextPage": false})
    );
    assert_eq!(items[1]["todos"]["items"], json!([{"title": "Walk dog"}]));
}

#[tokio::test]
async fn by_pk_is_null_when_missing_or_hidden() {
    let (app, alice, _) = basic().await;
    let query = format!(
        "{{ todo_by_pk(id: \"{}\") {{ id title cost isPublic dueDate }} }}",
        T1.to_uppercase()
    );
    let response = app.run(&alice, &query).await;
    assert_eq!(
        response.data()["todo_by_pk"],
        json!({"id": T1, "title": "Buy milk", "cost": 1.5, "isPublic": true, "dueDate": "2026-02-01T10:00:00.000Z"})
    );
    for id in [T4, "00000000-0000-4000-8000-0000000000ff"] {
        let query = format!("{{ todo_by_pk(id: \"{id}\") {{ id }} }}");
        assert_eq!(
            app.run(&alice, &query).await.data(),
            &json!({"todo_by_pk": null})
        );
    }
    let response = app.run(&alice, "{ todo_by_pk(id: \"nope\") { id } }").await;
    assert_eq!(
        response.messages(),
        ["The specified argument value does not match the argument type."]
    );
    assert!(response.body.get("data").is_none());
}

#[tokio::test]
async fn anonymous_callers_get_the_anonymous_grant() {
    let (app, _, _) = basic().await;
    let anonymous = Caller::anonymous();
    let response = app
        .run(&anonymous, "{ todos { items { id title status } } }")
        .await;
    assert_eq!(response.column("todos", "id"), [json!(T1), json!(T3)]);

    // A non-null field the role may not read: an error per row, null up to `data`.
    let response = app
        .run(&anonymous, "{ todos { items { title cost } } }")
        .await;
    assert_eq!(response.body["data"], Value::Null);
    assert_eq!(
        response.codes(),
        ["AUTH_NOT_AUTHORIZED", "AUTH_NOT_AUTHORIZED"]
    );
    assert_eq!(
        response.errors()[1]["path"],
        json!(["todos", "items", 1, "cost"])
    );
    // A nullable one: the field is null.
    let response = app
        .run(&anonymous, "{ todos { items { title priority } } }")
        .await;
    assert_eq!(response.messages().len(), 2);
    assert_eq!(
        response.body["data"]["todos"]["items"][0],
        json!({"title": "Buy milk", "priority": null})
    );
    let response = app
        .run(
            &anonymous,
            "{ todos(filter: { cost: { gt: 0 } }) { items { id } } }",
        )
        .await;
    assert_eq!(
        response.messages(),
        ["Access forbidden to a field referenced in the filter."]
    );
    assert_eq!(response.codes(), ["AuthorizationCheckFailed"]);
    let response = app
        .run(
            &anonymous,
            "{ todos(orderBy: { cost: ASC }) { items { id } } }",
        )
        .await;
    assert_eq!(
        response.messages(),
        ["Access forbidden to a field referenced in the orderBy argument."]
    );
    // Milestones are for signed-in users only: 401, so a restored session refreshes.
    let response = app.run(&anonymous, "{ milestones { items { id } } }").await;
    assert_eq!(response.status, 401);
    // Nested unreadable entities: every selected field is an error.
    let response = app
        .run(
            &anonymous,
            "{ projects { items { name milestones { items { title } } } } }",
        )
        .await;
    assert_eq!(response.data(), &json!({"projects": {"items": []}}));
}

#[tokio::test]
async fn validation_errors_read_as_hot_chocolate_writes_them() {
    let (app, alice, _) = basic().await;
    let cases = [
        (
            "{ todos { items { nope } } }",
            "The field `nope` does not exist on the type `Todo`.",
        ),
        (
            "{ todos { items { secretNote } } }",
            "The field `secretNote` does not exist on the type `Todo`.",
        ),
        (
            "{ users { items { id } } }",
            "The field `users` does not exist on the type `Query`.",
        ),
        (
            "{ todos(filter: { not: { title: { eq: \"x\" } } }) { items { id } } }",
            "The specified input object field `not` does not exist.",
        ),
        (
            "{ todos(orderBy: { title: ASC, title: DESC }) { items { id } } }",
            "There can be only one input field named `title`.",
        ),
        (
            "{ todos(first: \"2\") { items { id } } }",
            "The specified argument value does not match the argument type.",
        ),
        ("{ todo_by_pk { id } }", "The argument `id` is required."),
        (
            "{ todos(top: 1) { items { id } } }",
            "The argument `top` does not exist.",
        ),
        (
            "{ todos { items { title { x } } } }",
            "Field \"title\" must not have a selection since type \"String!\" has no subfields.",
        ),
        (
            "{ __schema { types { name } } }",
            "Introspection is not allowed for the current request.",
        ),
        (
            "{ todos(filter: { estimate: { eq: 1.5 } }) { items { id } } }",
            "The specified value type of field `eq` does not match the field type.",
        ),
    ];
    for (query, message) in cases {
        let response = app.run(&alice, query).await;
        assert_eq!(response.status, 200, "{query}");
        assert_eq!(response.messages(), [message], "{query}");
        assert!(response.body.get("data").is_none(), "{query}");
    }
    let response = app.run(&alice, "{ __schema { types { name } } }").await;
    assert_eq!(response.codes(), ["HC0046"]);
    let response = app.run(&alice, "{ todos { items { id } }").await;
    assert_eq!(response.status, 400);
    assert_eq!(response.codes(), ["HC0011"]);
}

#[tokio::test]
async fn variables_fragments_aliases_and_directives() {
    let (app, alice, _) = basic().await;
    let query = "query Q($title: String, $first: Int = 5, $skip: Boolean!) {
        a: todos(filter: { title: { eq: $title } }, first: $first) { items { ...F } }
        b: todos(first: 1) { kind: __typename items { id @skip(if: $skip) title } }
        __typename
      }
      fragment F on Todo { id t: title ... on Todo { status } }";
    let variables = json!({"title": "walk dog", "skip": true});
    let response = app
        .run_with(&alice, query, variables.as_object().unwrap().clone())
        .await;
    assert_eq!(
        response.data(),
        &json!({
            "a": {"items": [{"id": T2, "t": "Walk dog", "status": "done"}]},
            "b": {"kind": "TodoConnection", "items": [{"title": "Buy milk"}]},
            "__typename": "Query",
        })
    );
    let response = app
        .run_with(
            &alice,
            query,
            json!({"skip": "yes"}).as_object().unwrap().clone(),
        )
        .await;
    assert_eq!(
        response.messages(),
        ["Variable `skip` got an invalid value."]
    );
}

#[tokio::test]
async fn explorer_threads_are_private_to_their_owner() {
    let app = App::new(explorer_model()).await;
    let (alice, alice_token) = app.user("alice@example.test").await;
    let (bob, bob_token) = app.user("bob@example.test").await;
    let thread = |id: u32, user: &str, title: &str| {
        format!(
            "('00000000-0000-4000-8000-00000000010{id}', '{user}', '{title}', NULL, \
             '2026-01-0{id}T00:00:00.0000000Z', '2026-01-0{id}T00:00:00.0000000Z')"
        )
    };
    app.seed(&format!(
        "INSERT INTO \"AskThreads\" (id, user_id, title, responseId, createdAt, updatedAt) VALUES {}, {}, {};
         INSERT INTO \"AskMessages\" (id, user_id, thread_id, role, content, createdAt) VALUES
           ('00000000-0000-4000-8000-000000000201', '{alice}', '00000000-0000-4000-8000-000000000101',
            'user', 'hello', '2026-01-01T00:00:01.0000000Z'),
           ('00000000-0000-4000-8000-000000000202', '{alice}', '00000000-0000-4000-8000-000000000101',
            'assistant', 'hi!', '2026-01-01T00:00:02.0000000Z');",
        thread(1, &alice, "Alice one"),
        thread(2, &alice, "Alice two"),
        thread(3, &bob, "Bob one"),
    ));
    let alice = app.caller(Some(&alice_token));
    let bob = app.caller(Some(&bob_token));
    let query = "{ askThreads(orderBy: { createdAt: DESC }) { items { title messages { items { role content } } } } }";
    let response = app.run(&alice, query).await;
    assert_eq!(
        response.items("askThreads"),
        [
            json!({"title": "Alice two", "messages": {"items": []}}),
            json!({"title": "Alice one", "messages": {"items": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi!"},
            ]}}),
        ]
    );
    let response = app.run(&bob, query).await;
    assert_eq!(response.column("askThreads", "title"), [json!("Bob one")]);
    let query = "{ askMessages { items { content thread { title } } } }";
    assert_eq!(
        app.run(&bob, query).await.items("askMessages"),
        Vec::<Value>::new()
    );
    assert_eq!(
        app.run(&alice, query).await.items("askMessages")[0],
        json!({"content": "hello", "thread": {"title": "Alice one"}})
    );
    // No token: the SDK must refresh first.
    let response = app.run(&Caller::anonymous(), query).await;
    assert_eq!(response.status, 401);
}

#[tokio::test]
async fn http_status_policy() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let app = App::new(explorer_model()).await;
    let (_, token) = app.user("alice@example.test").await;
    let router = app.api.clone().router();
    let send = |method: &str, uri: &str, auth: Option<String>, body: &str| {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(auth) = auth {
            request = request.header(header::AUTHORIZATION, auth);
        }
        let request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        let router = router.clone();
        async move {
            let response = router.oneshot(request).await.unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            (status, headers, body)
        }
    };
    let query = r#"{"query":"query { askThreads { items { id } } }"}"#;
    let bearer = Some(format!("Bearer {token}"));

    let (status, headers, body) = send("POST", "/graphql", bearer.clone(), query).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    assert_eq!(body, json!({"data": {"askThreads": {"items": []}}}));

    let (status, headers, body) = send("POST", "/graphql", Some("Bearer nope".into()), query).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        headers[header::WWW_AUTHENTICATE],
        "Bearer error=\"invalid_token\""
    );
    assert_eq!(body["error"], "invalid_token");

    let (status, _, body) = send("POST", "/graphql", None, query).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "authentication_required");
    assert_eq!(
        body["errors"][0]["extensions"]["code"],
        "AUTH_NOT_AUTHENTICATED"
    );

    let (status, _, body) = send("POST", "/graphql", bearer.clone(), "{\"query\":\"{ x\"}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["errors"][0]["extensions"]["code"], "HC0011");
    let (status, _, body) = send("POST", "/graphql", bearer.clone(), "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["errors"][0]["message"],
        "Either the parameter query or the parameter id has to be set."
    );
    // Validation errors are 200 (the SDK detects some of them only then).
    let bad = r#"{"query":"{ askThreads { nope } }"}"#;
    let (status, _, body) = send("POST", "/graphql", bearer.clone(), bad).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["errors"][0]["message"],
        "The field `nope` does not exist on the type `AskThreadConnection`."
    );
    // GET serves queries too.
    let uri = "/graphql?query=%7B%20__typename%20%7D";
    let (status, _, body) = send("GET", uri, bearer, "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"data": {"__typename": "Query"}}));
}

#[tokio::test]
async fn set_model_swaps_the_schema_for_new_requests() {
    let app = App::new(explorer_model()).await;
    let (_, token) = app.user("alice@example.test").await;
    let alice = app.caller(Some(&token));
    let query = "{ askThreads { items { id } } }";
    assert_eq!(
        app.run(&alice, query).await.items("askThreads"),
        Vec::<Value>::new()
    );
    app.api.set_model(std::sync::Arc::new(basic_model()));
    assert_eq!(app.api.model().entities.len(), basic_model().entities.len());
    assert_eq!(
        app.run(&alice, query).await.messages(),
        ["The field `askThreads` does not exist on the type `Query`."]
    );
}
