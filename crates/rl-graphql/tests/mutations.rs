//! Mutations against the golden models on a real database: inputs, the policies and field
//! permissions of each action, the row read back, constraint errors and statuses, as DAB answers
//! them.

mod common;

use common::{App, basic_model, explorer_model};
use rl_model::{Action, ActionPermission, AppModel, Caller, RolePermission};
use serde_json::{Value, json};

const C1: &str = "00000000-0000-4000-8000-0000000000c1";
const T1: &str = "00000000-0000-4000-8000-000000000001";
const T2: &str = "00000000-0000-4000-8000-000000000002";
const T3: &str = "00000000-0000-4000-8000-000000000003";
const MISSING: &str = "00000000-0000-4000-8000-0000000000ff";

/// The basic fixture with a category, todos of alice (T1 with a comment, T2) and bob (T3).
async fn basic_with(model: AppModel) -> (App, Caller, String) {
    let app = App::new(model).await;
    let (alice, token) = app.user("alice@example.test").await;
    app.seed(&format!(
        "INSERT INTO \"Categories\" (id, name, display_label, sortOrder, parent_id) VALUES
           ('{C1}', 'Home', 'House', 1, NULL);
         INSERT INTO \"Todos\" (id, user_id, title, status, estimate, cost, isPublic, createdAt)
           VALUES
           ('{T1}', '{alice}', 'Buy milk', 'doing', 3, 1.5, 1, '2026-01-01T00:00:00.0000000Z'),
           ('{T2}', '{alice}', 'Walk dog', 'done', NULL, 10, 0, '2026-01-02T00:00:00.0000000Z'),
           ('{T3}', 'bob-user-id', 'PAY bills', 'doing', 5, 99.99, 0,
            '2026-01-03T00:00:00.0000000Z');
         INSERT INTO \"Comments\" (id, author_id, authorEmail, body, createdAt, todo_id) VALUES
           ('00000000-0000-4000-8000-0000000000a1', '{alice}', 'alice@example.test', 'first',
            '2026-01-05T00:00:00.0000000Z', '{T1}');"
    ));
    let caller = app.caller(Some(&token));
    (app, caller, alice)
}

async fn basic() -> (App, Caller, String) {
    basic_with(basic_model()).await
}

fn count(app: &App, sql: &str) -> i64 {
    app.db
        .call_blocking(|conn| conn.query_row(sql, [], |row| row.get(0)))
        .unwrap()
}

/// A random (version 4) uuid in the stored, lowercase form.
fn is_new_uuid(value: &Value) -> bool {
    let id = value.as_str().unwrap_or("").as_bytes();
    id.len() == 36
        && id.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            14 => *c == b'4',
            19 => b"89ab".contains(c),
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(c),
        })
}

/// The grants of `role` on entity `entity` of `model`.
fn grants<'m>(model: &'m mut AppModel, entity: &str, role: &str) -> &'m mut Vec<ActionPermission> {
    let entity = model
        .entities
        .iter_mut()
        .find(|e| e.name == entity)
        .unwrap();
    let role = entity
        .permissions
        .iter_mut()
        .find(|p| p.role == role)
        .unwrap();
    &mut role.actions
}

fn grant(action: Action) -> ActionPermission {
    ActionPermission {
        action,
        fields: None,
        policy: None,
    }
}

#[tokio::test]
async fn creates_fill_defaults_and_check_the_create_policy() {
    let (app, alice, alice_id) = basic().await;
    let create = |user: &str, title: &str| {
        format!(
            "mutation {{ createTodo(item: {{ user_id: \"{user}\", title: \"{title}\", \
             status: \"doing\", cost: 12.5, createdAt: \"2026-03-01T10:00:00Z\", \
             category_id: \"{}\" }}) {{ id title status isPublic cost done category {{ name }} }} }}",
            C1.to_uppercase()
        )
    };
    let response = app.run(&alice, &create(&alice_id, "Plan trip")).await;
    let todo = &response.data()["createTodo"];
    assert!(is_new_uuid(&todo["id"]), "{todo}");
    assert_eq!(
        todo,
        &json!({"id": todo["id"], "title": "Plan trip", "status": "doing", "isPublic": false,
                "cost": 12.5, "done": null, "category": {"name": "Home"}})
    );
    let query = format!(
        "{{ todo_by_pk(id: \"{}\") {{ title }} }}",
        todo["id"].as_str().unwrap()
    );
    let response = app.run(&alice, &query).await;
    assert_eq!(
        response.data(),
        &json!({"todo_by_pk": {"title": "Plan trip"}})
    );

    // The create policy (`claims.sub eq item.user_id`) sees the inserted values, compared as the
    // stored column compares them (case-insensitively).
    let response = app
        .run(&alice, &create(&alice_id.to_uppercase(), "Shouted"))
        .await;
    assert_eq!(response.data()["createTodo"]["title"], "Shouted");
    let response = app.run(&alice, &create("bob-user-id", "Forged")).await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body["data"], json!({"createTodo": null}));
    assert_eq!(
        response.errors(),
        [json!({
            "message": "Could not insert row with given values.",
            "locations": [{"line": 1, "column": 12}],
            "path": ["createTodo"],
            "extensions": {"code": "DatabasePolicyFailure"}
        })]
    );
    assert_eq!(
        count(&app, "SELECT count(*) FROM Todos WHERE title = 'Forged'"),
        0
    );

    // An explicit key is kept (in the stored, lowercase form).
    let response = app
        .run(
            &alice,
            &format!(
                "mutation {{ createTodo(item: {{ id: \"{}\", user_id: \"{alice_id}\", \
                 title: \"Mine\", status: \"done\", cost: 0, createdAt: \"2026-03-01T10:00:00Z\" \
                 }}) {{ id }} }}",
                MISSING.to_uppercase()
            ),
        )
        .await;
    assert_eq!(response.data(), &json!({"createTodo": {"id": MISSING}}));
}

#[tokio::test]
async fn inputs_follow_dab_create_and_update_input_types() {
    let (app, alice, alice_id) = basic().await;
    // Required: NOT NULL without a default.
    let response = app
        .run(
            &alice,
            "mutation { createTodo(item: { title: \"x\" }) { id } }",
        )
        .await;
    assert_eq!(
        response.messages(),
        ["`user_id` is a required field and cannot be null."]
    );
    // Only the fields of `Todo` (readable by some role) are inputs: never `secretNote`.
    let response = app
        .run(
            &alice,
            &format!(
                "mutation {{ updateTodo(id: \"{T1}\", item: {{ secretNote: \"s\" }}) {{ id }} }}"
            ),
        )
        .await;
    assert_eq!(
        response.messages(),
        ["The specified input object field `secretNote` does not exist."]
    );
    // Unknown mutation fields are validation errors (DAB has no `patch` for SQL).
    let response = app
        .run(&alice, "mutation { patchCategory(id: \"x\") { id } }")
        .await;
    assert_eq!(
        response.messages(),
        ["The field `patchCategory` does not exist on the type `Mutation`."]
    );
    let response = app.run(&alice, "mutation { __typename }").await;
    assert_eq!(response.data(), &json!({"__typename": "Mutation"}));
    // Variables: a whole item, and an item field the request leaves unset (absent, not null).
    let mut variables = serde_json::Map::new();
    variables.insert(
        "item".into(),
        json!({"user_id": alice_id, "title": "From variables", "status": "doing", "cost": 1,
               "createdAt": "2026-03-01T00:00:00Z", "estimate": 4}),
    );
    let response = app
        .run_with(
            &alice,
            "mutation($item: CreateTodoInput!) { createTodo(item: $item) { id estimate } }",
            variables,
        )
        .await;
    let id = response.data()["createTodo"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let response = app
        .run(
            &alice,
            &format!(
                "mutation($e: Int) {{ updateTodo(id: \"{id}\", item: {{ title: \"Renamed\", \
                 estimate: $e }}) {{ title estimate }} }}"
            ),
        )
        .await;
    assert_eq!(
        response.data(),
        &json!({"updateTodo": {"title": "Renamed", "estimate": 4}})
    );
}

#[tokio::test]
async fn updates_are_partial_and_limited_to_the_policy_rows() {
    let (app, alice, _) = basic().await;
    let update = |id: &str, item: &str| {
        format!(
            "mutation {{ updateTodo(id: \"{id}\", item: {{ {item} }}) {{ id title done status \
             estimate }} }}"
        )
    };
    let response = app
        .run(&alice, &update(T1, "title: \"Buy oat milk\", done: true"))
        .await;
    assert_eq!(
        response.data(),
        &json!({"updateTodo": {"id": T1, "title": "Buy oat milk", "done": true,
                               "status": "doing", "estimate": 3}})
    );
    // `null` sets NULL; an empty item changes nothing.
    let response = app.run(&alice, &update(T1, "estimate: null")).await;
    assert_eq!(response.data()["updateTodo"]["estimate"], Value::Null);
    let response = app.run(&alice, &update(T1, "")).await;
    assert_eq!(response.data()["updateTodo"]["title"], "Buy oat milk");

    // `cost` is not among the update grant's fields.
    let response = app.run(&alice, &update(T1, "title: \"x\", cost: 3")).await;
    assert_eq!(
        response.messages(),
        ["Unauthorized due to one or more fields in this mutation."]
    );
    assert_eq!(response.codes(), ["AuthorizationCheckFailed"]);
    assert_eq!(response.body["data"], json!({"updateTodo": null}));

    // Bob's todo is outside the update policy, like a missing one.
    let response = app.run(&alice, &update(T3, "title: \"mine\"")).await;
    assert_eq!(
        response.errors(),
        [json!({
            "message": format!("Could not find item with <id: {T3}>"),
            "locations": [{"line": 1, "column": 12}],
            "path": ["updateTodo"],
            "extensions": {"code": "ItemNotFound"}
        })]
    );
    let response = app
        .run(&alice, &update(&MISSING.to_uppercase(), "title: \"x\""))
        .await;
    assert_eq!(
        response.messages(),
        [format!("Could not find item with <id: {MISSING}>")]
    );
    assert_eq!(
        count(&app, "SELECT count(*) FROM Todos WHERE title = 'PAY bills'"),
        1
    );
}

#[tokio::test]
async fn deletes_answer_the_row_before_deleting_it() {
    let (app, alice, _) = basic().await;
    let delete = |id: &str| format!("mutation {{ deleteTodo(id: \"{id}\") {{ id title }} }}");
    let response = app.run(&alice, &delete(T2)).await;
    assert_eq!(
        response.data(),
        &json!({"deleteTodo": {"id": T2, "title": "Walk dog"}})
    );
    assert_eq!(
        count(
            &app,
            &format!("SELECT count(*) FROM Todos WHERE id = '{T2}'")
        ),
        0
    );
    // Bob's todo (outside the delete policy) and a missing one: `null`, no error.
    for id in [T3, MISSING] {
        let response = app.run(&alice, &delete(id)).await;
        assert_eq!(response.data(), &json!({"deleteTodo": null}));
    }
    assert_eq!(
        count(
            &app,
            &format!("SELECT count(*) FROM Todos WHERE id = '{T3}'")
        ),
        1
    );
    // T1 has a comment: the foreign key (no cascade) refuses, and nothing is deleted.
    let response = app.run(&alice, &delete(T1)).await;
    assert_eq!(
        response.errors(),
        [json!({
            "message": "While processing your request the database ran into an error.",
            "path": ["deleteTodo"],
            "extensions": {"code": "DatabaseOperationFailed",
                           "detail": "FOREIGN KEY constraint failed"}
        })]
    );
    assert_eq!(response.body["data"], json!({"deleteTodo": null}));
    assert_eq!(
        count(
            &app,
            &format!("SELECT count(*) FROM Todos WHERE id = '{T1}'")
        ),
        1
    );
}

#[tokio::test]
async fn constraint_violations_answer_dab_database_error() {
    let (app, alice, _) = basic().await;
    // Unique names compare case-insensitively, as on Fabric.
    let response = app
        .run(
            &alice,
            "mutation { createCategory(item: { name: \"HOME\" }) { id } }",
        )
        .await;
    assert_eq!(response.codes(), ["DatabaseOperationFailed"]);
    assert_eq!(
        response.errors()[0]["extensions"]["detail"],
        "UNIQUE constraint failed: Categories.name"
    );
    assert!(response.errors()[0].get("locations").is_none());
    // NOT NULL.
    let response = app
        .run(
            &alice,
            &format!(
                "mutation {{ updateCategory(id: \"{C1}\", item: {{ name: null }}) {{ id }} }}"
            ),
        )
        .await;
    assert_eq!(
        response.messages(),
        ["While processing your request the database ran into an error."]
    );
    // The default (`sortOrder: 0`) is filled in, and the role reads the row back.
    let response = app
        .run(
            &alice,
            "mutation { createCategory(item: { name: \"Garden\" }) { name sortOrder } }",
        )
        .await;
    assert_eq!(
        response.data(),
        &json!({"createCategory": {"name": "Garden", "sortOrder": 0}})
    );
}

#[tokio::test]
async fn reading_back_follows_the_read_permission() {
    let mut model = basic_model();
    // Anonymous may create categories but not read them, and create comments, whose
    // `authorEmail` it may not read.
    *grants(&mut model, "Category", "anonymous") = vec![grant(Action::Create)];
    grants(&mut model, "Comment", "anonymous").push(grant(Action::Create));
    // Alice may create todos for anyone, but reads only hers and the public ones.
    for action in grants(&mut model, "Todo", "authenticated") {
        if action.action == Action::Create {
            action.policy = None;
        }
    }
    let (app, alice, _) = basic_with(model).await;
    let anonymous = Caller::anonymous();

    let response = app
        .run(
            &anonymous,
            "mutation { createCategory(item: { name: \"Garden\" }) { id } }",
        )
        .await;
    assert_eq!(
        response.messages(),
        [
            "The mutation operation createCategory was successful but the current user is \
          unauthorized to view the response due to lack of read permissions"
        ]
    );
    assert_eq!(response.codes(), ["AuthorizationCheckFailed"]);
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM Categories WHERE name = 'Garden'"
        ),
        1
    );

    let comment = |selection: &str| {
        format!(
            "mutation {{ createComment(item: {{ author_id: \"guest\", authorEmail: \
             \"guest@example.test\", body: \"{selection}\", createdAt: \"2026-02-01T00:00:00Z\", \
             todo_id: \"{T1}\" }}) {{ {selection} }} }}"
        )
    };
    let response = app.run(&anonymous, &comment("body")).await;
    assert_eq!(response.data(), &json!({"createComment": {"body": "body"}}));
    // A field it may write but not read: written, then the read error, null up to the field.
    let response = app.run(&anonymous, &comment("body authorEmail")).await;
    assert_eq!(response.body["data"], json!({"createComment": null}));
    assert_eq!(response.codes(), ["AUTH_NOT_AUTHORIZED"]);
    assert_eq!(
        response.errors()[0]["path"],
        json!(["createComment", "authorEmail"])
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM Comments WHERE author_id = 'guest'"
        ),
        2
    );

    // Written, but hidden by the read policy: null without an error.
    let response = app
        .run(
            &alice,
            "mutation { createTodo(item: { user_id: \"bob-user-id\", title: \"For bob\", \
             status: \"doing\", cost: 1, createdAt: \"2026-03-01T00:00:00Z\" }) { id } }",
        )
        .await;
    assert_eq!(response.data(), &json!({"createTodo": null}));
    assert_eq!(
        count(&app, "SELECT count(*) FROM Todos WHERE title = 'For bob'"),
        1
    );
}

#[tokio::test]
async fn a_read_back_that_needs_a_missing_claim_undoes_the_write() {
    let mut model = basic_model();
    let person = model
        .entities
        .iter_mut()
        .find(|e| e.name == "Person")
        .unwrap();
    // Anonymous may create people, and read them under the `claims.sub` policy of `*`.
    let read = ActionPermission {
        action: Action::Read,
        ..person.permissions[0].actions[0].clone()
    };
    person.permissions.insert(
        0,
        RolePermission {
            role: "anonymous".into(),
            actions: vec![grant(Action::Create), read],
        },
    );
    let table = person.table.clone();
    let app = App::new(model).await;
    let response = app
        .run(
            &Caller::anonymous(),
            "mutation { createPerson(item: { nickname: \"zed\" }) { id nickname } }",
        )
        .await;
    assert_eq!(
        response.messages(),
        ["User does not possess all the claims required to perform this operation."]
    );
    assert_eq!(response.codes(), ["AuthorizationCheckFailed"]);
    assert_eq!(count(&app, &format!("SELECT count(*) FROM \"{table}\"")), 0);
}

#[tokio::test]
async fn get_requests_do_not_run_mutations() {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    let (app, _, _) = basic().await;
    let (_, token) = app.user("carol@example.test").await;
    let query = "mutation { createCategory(item: { name: \"Garden\" }) { id } }";
    let query: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let request = Request::get(format!("/graphql?query={query}"))
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let response = app.api.clone().router().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        body["errors"][0]["message"],
        "The specified operation kind is not allowed."
    );
    assert_eq!(
        count(
            &app,
            "SELECT count(*) FROM Categories WHERE name = 'Garden'"
        ),
        0
    );
}

#[tokio::test]
async fn anonymous_writes_need_a_grant_or_sign_in() {
    let (app, _, _) = basic().await;
    let anonymous = Caller::anonymous();
    // Signed-in users may: 401, so a restored session refreshes and replays.
    for mutation in [
        "mutation { createCategory(item: { name: \"x\" }) { id } }".to_owned(),
        format!("mutation {{ deleteTodo(id: \"{T1}\") {{ id }} }}"),
    ] {
        let response = app.run(&anonymous, &mutation).await;
        assert_eq!(response.status, 401, "{mutation}");
    }
    assert_eq!(count(&app, "SELECT count(*) FROM Todos"), 3);
}

#[tokio::test]
async fn explorer_threads_and_messages_belong_to_their_owner() {
    let app = App::new(explorer_model()).await;
    let (alice_id, alice_token) = app.user("alice@example.test").await;
    let (_, bob_token) = app.user("bob@example.test").await;
    let alice = app.caller(Some(&alice_token));
    let bob = app.caller(Some(&bob_token));
    let thread = |user: &str, title: &str| {
        format!(
            "createAskThread(item: {{ user_id: \"{user}\", title: \"{title}\", responseId: \
             \"resp_1\", createdAt: \"2026-02-01T00:00:00.000Z\", updatedAt: \
             \"2026-02-01T00:00:00.000Z\" }}) {{ id title }}"
        )
    };

    // Each root field is its own transaction: the first is kept when the second fails.
    let response = app
        .run(
            &alice,
            &format!(
                "mutation {{ a: {} b: {} }}",
                thread(&alice_id, "First"),
                thread("someone-else", "Forged")
            ),
        )
        .await;
    assert_eq!(response.body["data"]["b"], Value::Null);
    assert_eq!(
        response.messages(),
        ["Could not insert row with given values."]
    );
    assert_eq!(response.errors()[0]["path"], json!(["b"]));
    let thread_id = response.body["data"]["a"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(is_new_uuid(&json!(thread_id)));

    let message = |thread: &str| {
        format!(
            "mutation {{ createAskMessage(item: {{ user_id: \"{alice_id}\", thread_id: \
             \"{thread}\", role: \"user\", content: \"hello\", createdAt: \
             \"2026-02-01T00:00:01.000Z\" }}) {{ id thread_id thread {{ title }} }} }}"
        )
    };
    let response = app.run(&alice, &message(&thread_id)).await;
    let created = &response.data()["createAskMessage"];
    assert_eq!(created["thread"], json!({"title": "First"}));
    let message_id = created["id"].as_str().unwrap().to_owned();
    // A message in a thread that does not exist.
    let response = app.run(&alice, &message(MISSING)).await;
    assert_eq!(
        response.errors()[0]["extensions"],
        json!({"code": "DatabaseOperationFailed", "detail": "FOREIGN KEY constraint failed"})
    );

    // Bob can neither change nor delete alice's thread.
    let response = app
        .run(
            &bob,
            &format!(
                "mutation {{ updateAskThread(id: \"{thread_id}\", item: {{ title: \"mine\" }}) \
                 {{ id }} }}"
            ),
        )
        .await;
    assert_eq!(response.codes(), ["ItemNotFound"]);
    let delete_thread = format!("mutation {{ deleteAskThread(id: \"{thread_id}\") {{ id }} }}");
    let response = app.run(&bob, &delete_thread).await;
    assert_eq!(response.data(), &json!({"deleteAskThread": null}));

    // An unset variable leaves the field as it is.
    let response = app
        .run(
            &alice,
            &format!(
                "mutation($r: String) {{ updateAskThread(id: \"{thread_id}\", item: {{ title: \
                 \"Renamed\", responseId: $r }}) {{ title responseId }} }}"
            ),
        )
        .await;
    assert_eq!(
        response.data(),
        &json!({"updateAskThread": {"title": "Renamed", "responseId": "resp_1"}})
    );

    // The thread's messages must go first; then the thread.
    let response = app.run(&alice, &delete_thread).await;
    assert_eq!(response.codes(), ["DatabaseOperationFailed"]);
    let response = app
        .run(
            &alice,
            &format!("mutation {{ deleteAskMessage(id: \"{message_id}\") {{ id }} }} "),
        )
        .await;
    assert_eq!(
        response.data(),
        &json!({"deleteAskMessage": {"id": message_id}})
    );
    let response = app.run(&alice, &delete_thread).await;
    assert_eq!(
        response.data(),
        &json!({"deleteAskThread": {"id": thread_id}})
    );

    // Without a token: 401.
    let response = app
        .run(
            &Caller::anonymous(),
            &format!("mutation {{ {} }}", thread(&alice_id, "x")),
        )
        .await;
    assert_eq!(response.status, 401);
}
