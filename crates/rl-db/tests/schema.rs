//! The entity DDL on the fixture apps: what it declares and how the constraints behave.

use rl_db::codec::{self, ConstraintKind};
use rl_db::{Db, create_table_sql, schema_sql};
use rl_model::{AppModel, FieldKind};
use rusqlite::{Connection, params};
use serde_json::json;

fn model(json: &str) -> AppModel {
    AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap()
}

fn basic() -> AppModel {
    model(include_str!(
        "../../rl-model/tests/fixtures/basic.analysis.json"
    ))
}

fn explorer() -> AppModel {
    model(include_str!(
        "../../rl-model/tests/fixtures/explorer.analysis.json"
    ))
}

fn database(model: &AppModel) -> Db {
    let db = Db::open_in_memory().unwrap();
    let sql = schema_sql(model).unwrap();
    db.call_blocking(|conn| conn.execute_batch(&sql)).unwrap();
    db
}

fn violation(conn: &Connection, sql: &str) -> ConstraintKind {
    let Err(error) = conn.execute_batch(sql) else {
        panic!("no violation: {sql}")
    };
    codec::constraint_violation(&error)
        .unwrap_or_else(|| panic!("{sql}: {error}"))
        .kind
}

fn strings(conn: &Connection, sql: &str) -> Vec<String> {
    conn.prepare(sql)
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn ddl_of_the_fixture_apps() {
    let model = basic();
    let todo = create_table_sql(model.entity("Todo").unwrap()).unwrap();
    for expected in [
        "CREATE TABLE \"Todos\" (",
        "\"id\" TEXT COLLATE NOCASE NOT NULL DEFAULT (lower(hex(randomblob(4)))",
        "\"status\" TEXT COLLATE NOCASE NOT NULL DEFAULT 'it''s new'",
        "\"isPublic\" INTEGER NOT NULL DEFAULT 0",
        "\"cost\" REAL NOT NULL",
        "\"dueDate\" TEXT,",
        "CONSTRAINT \"PK_Todos\" PRIMARY KEY (\"id\")",
        "CONSTRAINT \"FK_Todos_category_id\" FOREIGN KEY (\"category_id\") REFERENCES \
         \"Categories\" (\"id\") ON DELETE NO ACTION ON UPDATE NO ACTION",
        "CONSTRAINT \"CK_Todos_status\" CHECK (\"status\" IN ('its new', 'doing', 'done', \
         'its archived'))",
        "CONSTRAINT \"CK_Todos_title_minlen\" CHECK (length(rtrim(\"title\", ' ')) >= 1)",
        "CONSTRAINT \"CK_Todos_title_maxlen\" CHECK (length(\"title\") <= 200)",
        "CONSTRAINT \"CK_Todos_contactEmail_email\" CHECK (\"contactEmail\" LIKE '_%@_%._%' AND \
         instr(\"contactEmail\", ' ') = 0)",
        "CREATE UNIQUE INDEX \"UQ_Todos_slug\" ON \"Todos\" (\"slug\");",
        "CREATE INDEX \"IX_Todos_category_id\" ON \"Todos\" (\"category_id\");",
    ] {
        assert!(todo.contains(expected), "missing {expected}\n{todo}");
    }
    // No range CHECK without `cli-minor-fixes` (the IR has none).
    assert!(!todo.contains("_range"), "{todo}");

    // The system `User` has no table: the column stays, indexed, without a foreign key.
    let people = create_table_sql(model.entity("Person").unwrap()).unwrap();
    assert!(!people.contains("REFERENCES"), "{people}");
    assert!(
        people.contains("CREATE INDEX \"IX_People_account_id\""),
        "{people}"
    );
    assert!(
        people.contains("\"memberSince\" TEXT NOT NULL DEFAULT '2024-01-01T00:00:00.0000000Z'")
    );
    assert!(people.contains("\"rate\" REAL NOT NULL DEFAULT 9.99"));

    let category = create_table_sql(model.entity("Category").unwrap()).unwrap();
    assert!(
        category.contains("\"display_label\" TEXT COLLATE NOCASE,"),
        "{category}"
    );
    assert!(
        category.contains("REFERENCES \"Categories\" (\"id\")"),
        "{category}"
    );

    for model in [basic(), explorer()] {
        let db = database(&model);
        let tables = db.call_blocking(|conn| {
            strings(
                conn,
                "SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name",
            )
        });
        let mut expected: Vec<String> = model.entities.iter().map(|e| e.table.clone()).collect();
        expected.sort();
        assert_eq!(tables, expected);
    }
    let explorer_sql = schema_sql(&explorer()).unwrap();
    assert!(explorer_sql.contains("CREATE INDEX \"IX_AskMessages_thread_id\""));
    assert!(explorer_sql.contains("\"role\" IN ('user', 'assistant')"));
}

#[test]
fn constraints_behave_like_fabric() {
    let db = database(&basic());
    db.call_blocking(|conn| {
        conn.execute_batch("INSERT INTO Categories (name) VALUES ('Work')")
            .unwrap();
        let category: String = conn
            .query_row("SELECT id FROM Categories", [], |row| row.get(0))
            .unwrap();
        // The id is a generated lowercase version 4 uuid.
        assert_eq!(
            codec::encode(&FieldKind::Uuid, &json!(category.to_uppercase())).unwrap(),
            category.clone().into()
        );
        assert_eq!(&category[14..15], "4");
        assert!("89ab".contains(&category[19..20]));

        let titled = |title: &str, columns: &str, values: &str| {
            format!(
                "INSERT INTO Todos (user_id, title, cost, createdAt{columns}) \
                 VALUES ('u1', {title}, 1.5, '2024-01-01T00:00:00.0000000Z'{values})"
            )
        };
        let todo = |columns: &str, values: &str| titled("'Write tests'", columns, values);
        // The `@set` default keeps its quote but the CHECK allows only the stripped values: on
        // Fabric too, a Todo created without a status fails.
        assert_eq!(violation(conn, &todo("", "")), ConstraintKind::Check);

        let cases = [
            (todo(", status", ", 'doing'"), None),
            (todo(", status", ", 'DOING'"), None),
            (todo(", status", ", 'its new'"), None),
            (
                todo(", status", ", 'archived'"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status", ", 'it''s new'"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, priority", ", 'done', 'urgent'"),
                Some(ConstraintKind::Check),
            ),
            (
                titled("NULL", ", status", ", 'done'"),
                Some(ConstraintKind::NotNull),
            ),
            (
                titled("'   '", ", status", ", 'done'"),
                Some(ConstraintKind::Check),
            ),
            (
                titled(&format!("'{}'", "x".repeat(201)), ", status", ", 'done'"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, estimate", ", 'done', 'abc'"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, estimate", ", 'done', 3000000000"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, isPublic", ", 'done', 2"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, dueDate", ", 'done', '2024-01-01'"),
                Some(ConstraintKind::Check),
            ),
            (
                "UPDATE Todos SET cost = 1e16".to_owned(),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, contactEmail", ", 'done', 'a b@c.de'"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, contactEmail", ", 'done', 'nobody'"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(", status, externalRef", ", 'done', 'ABC'"),
                Some(ConstraintKind::Check),
            ),
            (
                todo(
                    ", status, category_id",
                    ", 'done', '00000000-0000-4000-8000-000000000000'",
                ),
                Some(ConstraintKind::ForeignKey),
            ),
        ];
        for (sql, expected) in cases {
            match expected {
                None => conn.execute_batch(&sql).unwrap(),
                Some(kind) => assert_eq!(violation(conn, &sql), kind, "{sql}"),
            }
        }

        // Unique constraints compare text case-insensitively, like Fabric's collation.
        conn.execute_batch(&todo(
            ", status, slug, contactEmail",
            ", 'done', 'my-slug', 'Ann@Example.com'",
        ))
        .unwrap();
        for duplicate in [
            todo(", status, slug", ", 'done', 'MY-SLUG'"),
            todo(", status, contactEmail", ", 'done', 'ann@example.COM'"),
            "INSERT INTO Categories (name) VALUES ('WORK')".to_owned(),
        ] {
            assert_eq!(
                violation(conn, &duplicate),
                ConstraintKind::Unique,
                "{duplicate}"
            );
        }
        // Several NULLs are fine (EF Core filters SQL Server's unique indexes on nullable columns).
        conn.execute_batch(&todo(", status", ", 'done'")).unwrap();

        // Foreign keys do not cascade: a referenced category cannot be deleted.
        conn.execute(
            &todo(", status, category_id", ", 'done', ?1"),
            params![category],
        )
        .unwrap();
        assert_eq!(
            violation(conn, "DELETE FROM Categories"),
            ConstraintKind::ForeignKey
        );

        // `People.account_id` references the system User, which has no table here.
        conn.execute_batch(
            "INSERT INTO People (account_id) VALUES ('5b0c6f1e-1111-4222-8333-444455556666')",
        )
        .unwrap();
        let person: (String, i64, f64, i64, String) = conn
            .query_row(
                "SELECT nickname, level, rate, active, memberSince FROM People",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            person,
            (
                "n/a".into(),
                1,
                9.99,
                1,
                "2024-01-01T00:00:00.0000000Z".into()
            )
        );

        // LEN ignores trailing spaces; the email CHECK carries the minimum length.
        assert_eq!(
            violation(conn, "INSERT INTO Categories (name) VALUES ('A ')"),
            ConstraintKind::Check
        );
        let todo_id: String = conn
            .query_row("SELECT id FROM Todos LIMIT 1", [], |row| row.get(0))
            .unwrap();
        let comment = |email: &str| {
            format!(
                "INSERT INTO Comments (author_id, authorEmail, body, createdAt, todo_id) VALUES \
                 ('u1', '{email}', 'hi', '2024-01-01T00:00:00.0000000Z', '{todo_id}')"
            )
        };
        assert_eq!(violation(conn, &comment("a@b.c")), ConstraintKind::Check);
        conn.execute_batch(&comment("ann@b.co")).unwrap();
    });
}

#[test]
fn text_compares_case_insensitively_and_dates_sort_chronologically() {
    let db = database(&explorer());
    db.call_blocking(|conn| {
        let date = |text: &str| codec::encode(&FieldKind::Date, &json!(text)).unwrap();
        for (title, at) in [
            ("alice", "2024-01-01T09:00:00Z"),
            ("Bob", "2024-01-01T10:00:00+02:00"),
            ("carol", "2024-01-01T07:30:00.5-01:00"),
            ("dave", "2023-12-31"),
        ] {
            conn.execute(
                "INSERT INTO AskThreads (user_id, title, createdAt, updatedAt) \
                 VALUES ('u', ?1, ?2, ?2)",
                params![title, date(at)],
            )
            .unwrap();
        }
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM AskThreads WHERE title = 'ALICE'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            strings(conn, "SELECT title FROM AskThreads ORDER BY title"),
            ["alice", "Bob", "carol", "dave"]
        );
        assert_eq!(
            strings(conn, "SELECT title FROM AskThreads ORDER BY createdAt"),
            ["dave", "Bob", "carol", "alice"]
        );
        let after: i64 = conn
            .query_row(
                "SELECT count(*) FROM AskThreads WHERE createdAt > ?1",
                [date("2024-01-01T08:15:00Z")],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, 2);
    });
}

#[test]
fn reserved_table_names_are_refused() {
    let mut model = explorer();
    model.entities[0].table = "_rl_messages".into();
    let error = schema_sql(&model).unwrap_err().to_string();
    assert!(
        error.contains("_rl_messages") && error.contains("reserved"),
        "{error}"
    );
}
