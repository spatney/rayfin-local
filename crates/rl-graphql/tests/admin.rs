//! The rows a host's control API reads and clears (`DataApi::rows`, `clear_rows`, ...), on the
//! basic model: every row whatever the permissions, each value as the data API answers it.

mod common;

use common::{App, basic_model};
use rl_graphql::admin::{AdminError, RowsQuery};
use serde_json::{Value, json};

const C1: &str = "00000000-0000-4000-8000-0000000000c1";
const C2: &str = "00000000-0000-4000-8000-0000000000c2";
const T1: &str = "00000000-0000-4000-8000-000000000001";
const T2: &str = "00000000-0000-4000-8000-000000000002";
const T3: &str = "00000000-0000-4000-8000-000000000003";

/// Categories (one the parent of the other), three todos (bob's T3 private) and a comment.
async fn app() -> (App, String) {
    let app = App::new(basic_model()).await;
    let (alice, token) = app.user("alice@example.test").await;
    app.seed(&format!(
        "INSERT INTO \"Categories\" (id, name, display_label, sortOrder, parent_id) VALUES
           ('{C1}', 'Home', 'House', 1, NULL), ('{C2}', 'Work', NULL, 2, '{C1}');
         INSERT INTO \"Todos\" (id, user_id, title, slug, contactEmail, status, priority, estimate,
           cost, weight, isPublic, done, createdAt, dueDate, secretNote, externalRef, category_id)
         VALUES
           ('{T1}', '{alice}', 'Buy milk', 'buy-milk', 'a@b.co', 'doing', 'low', 3, 1.5, 2.25, 1, 0,
            '2026-01-01T00:00:00.1234567Z', '2026-02-01T10:00:00.0000000Z', 's1',
            '0000000a-0000-4000-8000-00000000000a', '{C1}'),
           ('{T2}', '{alice}', 'Walk dog', NULL, NULL, 'done', NULL, NULL, 10, NULL, 0, 1,
            '2026-01-02T00:00:00.0000000Z', NULL, NULL, NULL, '{C2}'),
           ('{T3}', 'bob', 'Pay bills', NULL, NULL, 'doing', 'high', 5, 99.99, NULL, 0, NULL,
            '2026-01-03T00:00:00.0000000Z', NULL, 'bob only', NULL, '{C1}');
         INSERT INTO \"Comments\" (id, author_id, authorEmail, body, createdAt, todo_id) VALUES
           ('00000000-0000-4000-8000-0000000000a1', '{alice}', 'alice@example.test', 'first',
            '2026-01-05T00:00:00.0000000Z', '{T1}');"
    ));
    (app, token)
}

fn page(order_by: Option<&str>, desc: bool) -> RowsQuery {
    RowsQuery {
        offset: 0,
        limit: 50,
        order_by: order_by.map(str::to_owned),
        desc,
    }
}

#[tokio::test]
async fn every_row_with_the_values_the_data_api_answers() {
    let (app, token) = app().await;
    let rows = app.api.rows("Todo", &page(None, false)).await.unwrap();
    assert_eq!(rows.total, 3);
    let ids: Vec<&str> = rows
        .rows
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [T1, T2, T3], "every row, by primary key");
    let names: Vec<&str> = rows.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names[..3], ["id", "user_id", "title"]);
    let column = |name: &str| rows.columns.iter().find(|c| c.name == name).unwrap();
    assert!(column("id").key && !column("title").key);
    for (name, kind) in [
        ("id", "UUID"),
        ("title", "String"),
        ("estimate", "Int"),
        ("cost", "Decimal"),
        ("isPublic", "Boolean"),
        ("createdAt", "DateTime"),
    ] {
        assert_eq!(column(name).kind, kind, "{name}");
    }
    assert_eq!(rows.rows[2]["secretNote"], "bob only");

    // Alice reads her todos through the data API: the same values, field by field.
    let fields: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| *name != "secretNote")
        .collect();
    let query = format!("query {{ todos {{ items {{ {} }} }} }}", fields.join(" "));
    let answered = app.run(&app.caller(Some(&token)), &query).await;
    let items = answered.items("todos");
    assert_eq!(items.len(), 2, "alice sees her own todos");
    for item in items {
        let row = rows
            .rows
            .iter()
            .find(|row| row["id"] == item["id"])
            .unwrap();
        for field in &fields {
            assert_eq!(row[*field], item[*field], "{field}");
        }
    }
    assert_eq!(rows.rows[0]["isPublic"], json!(true));
    assert_eq!(rows.rows[0]["createdAt"], "2026-01-01T00:00:00.123Z");

    // A renamed column is its field.
    let categories = app.api.rows("Category", &page(None, false)).await.unwrap();
    assert_eq!(categories.rows[0]["label"], "House");
    assert!(categories.columns.iter().all(|c| c.name != "display_label"));
}

#[tokio::test]
async fn pages_follow_the_order_and_limit() {
    let (app, _) = app().await;
    let titles = |rows: &[Value]| -> Vec<String> {
        rows.iter()
            .map(|row| row["title"].as_str().unwrap().to_owned())
            .collect()
    };
    let by_title = app
        .api
        .rows("Todo", &page(Some("title"), true))
        .await
        .unwrap();
    assert_eq!(
        titles(&by_title.rows),
        ["Walk dog", "Pay bills", "Buy milk"]
    );
    let second = app
        .api
        .rows(
            "todo",
            &RowsQuery {
                offset: 1,
                limit: 1,
                ..page(Some("TITLE"), false)
            },
        )
        .await
        .unwrap();
    assert_eq!(titles(&second.rows), ["Pay bills"]);
    assert_eq!(second.total, 3);

    let unknown = app.api.rows("Todo", &page(Some("nope"), false)).await;
    assert!(
        matches!(&unknown, Err(AdminError::UnknownField { field, fields, .. })
            if field == "nope" && fields.starts_with("id, user_id, title")),
        "{unknown:?}"
    );
    assert!(matches!(
        app.api.rows("Nope", &page(None, false)).await,
        Err(AdminError::UnknownEntity(_))
    ));
}

#[tokio::test]
async fn rows_are_counted_and_cleared_unless_other_rows_refer_to_them() {
    let (app, _) = app().await;
    let counts = app.api.row_counts().await.unwrap();
    let count = |name: &str| counts.iter().find(|(entity, _)| entity == name).unwrap().1;
    assert_eq!(
        (count("Category"), count("Todo"), count("Comment")),
        (2, 3, 1)
    );
    assert_eq!(count("Person"), 0);

    let refused = app.api.clear_rows("Category").await.unwrap_err();
    assert!(
        matches!(&refused, AdminError::Referenced { by, .. } if by == &[("Todo".to_owned(), 3)]),
        "{refused:?}"
    );
    assert_eq!(
        refused.to_string(),
        "Category's rows can't be cleared while 3 Todo rows refer to them"
    );
    assert_eq!(app.api.clear_rows("Comment").await.unwrap(), 1);
    assert_eq!(app.api.clear_rows("Todo").await.unwrap(), 3);
    // Categories refer to each other, and go together.
    assert_eq!(app.api.clear_rows("Category").await.unwrap(), 2);
    assert!(
        app.api
            .row_counts()
            .await
            .unwrap()
            .iter()
            .all(|(_, rows)| *rows == 0)
    );
}

#[tokio::test]
async fn clearing_everything_ignores_the_references_between_rows() {
    let (app, _) = app().await;
    assert_eq!(app.api.clear_all_rows().await.unwrap(), 6);
    assert!(
        app.api
            .row_counts()
            .await
            .unwrap()
            .iter()
            .all(|(_, rows)| *rows == 0)
    );
}
