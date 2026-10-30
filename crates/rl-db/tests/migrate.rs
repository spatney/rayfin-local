//! Entity schema migrations on explorer's model and variations of it.

use rl_db::migrate::{self, StepClass};
use rl_db::{Db, LossReason, MigrationStep, SchemaError, apply};
use rl_model::{AppModel, Column, ColumnOrigin, Entity, FieldKind};
use rusqlite::params;
use serde_json::{Value, json};

fn explorer() -> AppModel {
    let json = include_str!("../../rl-model/tests/fixtures/explorer.analysis.json");
    AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap()
}

fn entity<'a>(model: &'a mut AppModel, name: &str) -> &'a mut Entity {
    model.entities.iter_mut().find(|e| e.name == name).unwrap()
}

fn column(name: &str, kind: FieldKind, nullable: bool, default: Option<Value>) -> Column {
    Column {
        name: name.into(),
        field: name.into(),
        kind,
        db_type: String::new(),
        nullable,
        default,
        primary_key: false,
        unique: false,
        origin: ColumnOrigin::Declared,
        source: None,
    }
}

fn text() -> FieldKind {
    FieldKind::Text {
        min_length: None,
        max_length: None,
        pattern: None,
    }
}

/// Inserts a thread (and a message in it), returning the thread id.
fn seed(db: &Db, title: &str, response: Option<&str>) -> String {
    db.call_blocking(|conn| {
        let id: String = conn
            .query_row(
                "INSERT INTO AskThreads (user_id, title, responseId, createdAt, updatedAt)
                 VALUES ('u1', ?1, ?2, '2024-01-01T00:00:00.0000000Z',
                         '2024-01-01T00:00:00.0000000Z') RETURNING id",
                params![title, response],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO AskMessages (user_id, thread_id, role, content, createdAt)
             VALUES ('u1', ?1, 'user', 'hello', '2024-01-01T00:00:00.0000000Z')",
            [&id],
        )
        .unwrap();
        id
    })
}

fn query<T: rusqlite::types::FromSql>(db: &Db, sql: &str) -> Vec<T> {
    db.call_blocking(|conn| {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    })
}

#[tokio::test]
async fn creates_then_does_nothing_and_adds_entities() {
    let db = Db::open_in_memory().unwrap();
    let mut threads_only = explorer();
    threads_only.entities.retain(|e| e.name == "AskThread");
    entity(&mut threads_only, "AskThread").relationships.clear();

    let report = apply(&db, &threads_only, false).await.unwrap();
    assert_eq!(report.steps.len(), 1);
    assert!(matches!(&report.steps[0], MigrationStep::CreateTable(t) if t.name == "AskThreads"));
    assert!(
        apply(&db, &threads_only, false)
            .await
            .unwrap()
            .steps
            .is_empty()
    );

    let model = explorer();
    let report = apply(&db, &model, false).await.unwrap();
    let steps: Vec<String> = report.steps.iter().map(ToString::to_string).collect();
    assert_eq!(steps, ["create table `AskMessages`"]);
    assert!(apply(&db, &model, false).await.unwrap().steps.is_empty());
    seed(&db, "first", None);

    let stored = migrate::stored_schema(&db).await.unwrap().unwrap();
    assert!(migrate::plan(Some(&stored), &model).unwrap().is_empty());
    assert_eq!(migrate::plan(None, &model).unwrap().steps.len(), 2);
}

#[tokio::test]
async fn additive_columns_keep_the_rows() {
    let db = Db::open_in_memory().unwrap();
    let mut model = explorer();
    apply(&db, &model, false).await.unwrap();
    let thread = seed(&db, "first", Some("r1"));

    let threads = entity(&mut model, "AskThread");
    threads.columns.push(column("notes", text(), true, None));
    let int = FieldKind::Int {
        min: None,
        max: None,
    };
    threads
        .columns
        .push(column("rank", int, false, Some(json!(3))));
    let plan = migrate::plan(migrate::stored_schema(&db).await.unwrap().as_ref(), &model).unwrap();
    assert!(plan.steps.iter().all(|s| s.class() == StepClass::Additive));
    let report = apply(&db, &model, false).await.unwrap();
    let steps: Vec<String> = report.steps.iter().map(ToString::to_string).collect();
    assert_eq!(
        steps,
        [
            "add column `AskThreads.notes`",
            "add column `AskThreads.rank`"
        ]
    );
    let rows: Vec<(String, Option<String>, i64)> = db.call_blocking(|conn| {
        conn.prepare("SELECT id, notes, rank FROM AskThreads")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    });
    assert_eq!(rows, [(thread, None, 3)]);
    assert!(apply(&db, &model, false).await.unwrap().steps.is_empty());
}

#[tokio::test]
async fn required_columns_without_default_need_force_when_rows_exist() {
    let db = Db::open_in_memory().unwrap();
    let model = explorer();
    apply(&db, &model, false).await.unwrap();
    let mut required = model.clone();
    entity(&mut required, "AskThread")
        .columns
        .push(column("summary", text(), false, None));

    // An empty table takes it without force.
    let report = apply(&db, &required, false).await.unwrap();
    assert_eq!(report.steps[0].class(), StepClass::Destructive);
    assert!(report.forced.is_empty());

    apply(&db, &model, true).await.unwrap();
    seed(&db, "first", None);
    let error = apply(&db, &required, false).await.unwrap_err();
    let SchemaError::DataLoss { losses } = &error else {
        panic!("{error}")
    };
    assert_eq!(losses.len(), 1);
    assert_eq!(
        (
            losses[0].table.as_str(),
            losses[0].column.as_deref(),
            losses[0].rows
        ),
        ("AskThreads", Some("summary"), 1)
    );
    let message = error.to_string();
    assert!(message.contains("AskThreads.summary") && message.contains("--force-schema"));
    assert!(message.contains("rayfin-local reset"), "{message}");

    let report = apply(&db, &required, true).await.unwrap();
    assert_eq!(
        report.forced[0].reason,
        LossReason::Placeholder { value: "''".into() }
    );
    assert_eq!(query::<String>(&db, "SELECT summary FROM AskThreads"), [""]);
}

#[tokio::test]
async fn dropped_columns_need_force_and_keep_the_other_data() {
    let db = Db::open_in_memory().unwrap();
    let model = explorer();
    apply(&db, &model, false).await.unwrap();
    let first = seed(&db, "first", Some("r1"));
    let second = seed(&db, "second", None);

    let mut dropped = model.clone();
    entity(&mut dropped, "AskThread")
        .columns
        .retain(|c| c.name != "responseId");
    let error = apply(&db, &dropped, false).await.unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("AskThreads.responseId: the column is dropped with its 1 value(s)"),
        "{message}"
    );
    // Nothing changed.
    assert_eq!(
        query::<Option<String>>(&db, "SELECT responseId FROM AskThreads ORDER BY title"),
        [Some("r1".to_owned()), None]
    );

    let report = apply(&db, &dropped, true).await.unwrap();
    assert_eq!(report.forced.len(), 1);
    assert!(report.steps[0].to_string().contains("drop responseId"));
    assert_eq!(
        query::<String>(
            &db,
            "SELECT id || ':' || title FROM AskThreads ORDER BY title"
        ),
        [format!("{first}:first"), format!("{second}:second")]
    );
    let columns = query::<String>(&db, "SELECT name FROM pragma_table_info('AskThreads')");
    assert!(!columns.contains(&"responseId".to_owned()), "{columns:?}");

    // The rebuilt parent still enforces its non-cascading foreign keys, with indexes back.
    db.call_blocking(|conn| {
        assert!(conn.execute("DELETE FROM AskThreads", []).is_err());
        assert!(
            conn.execute(
                "INSERT INTO AskMessages (user_id, thread_id, role, content, createdAt) VALUES
                 ('u1', '00000000-0000-4000-8000-000000000000', 'user', 'x',
                  '2024-01-01T00:00:00.0000000Z')",
                [],
            )
            .is_err()
        );
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    });
    assert!(query::<String>(&db, "SELECT \"table\" FROM pragma_foreign_key_check").is_empty());
    assert_eq!(
        query::<String>(
            &db,
            "SELECT name FROM sqlite_schema WHERE type = 'index' AND name LIKE 'IX_%'"
        ),
        ["IX_AskMessages_thread_id"]
    );
    assert!(apply(&db, &dropped, false).await.unwrap().steps.is_empty());
}

#[tokio::test]
async fn type_changes_convert_what_they_can() {
    let db = Db::open_in_memory().unwrap();
    let model = explorer();
    apply(&db, &model, false).await.unwrap();
    seed(&db, "a", Some("42"));
    seed(&db, "b", Some("abc"));
    seed(&db, "c", None);

    let mut retyped = model.clone();
    let response = entity(&mut retyped, "AskThread")
        .columns
        .iter_mut()
        .find(|c| c.name == "responseId")
        .unwrap();
    response.kind = FieldKind::Int {
        min: None,
        max: None,
    };
    response.db_type = "INT".into();
    let error = apply(&db, &retyped, false).await.unwrap_err();
    let SchemaError::DataLoss { losses } = &error else {
        panic!("{error}")
    };
    assert_eq!(
        (losses[0].rows, &losses[0].reason),
        (
            1,
            &LossReason::Unconvertible {
                from: "text".into(),
                to: "int".into()
            }
        )
    );
    apply(&db, &retyped, true).await.unwrap();
    assert_eq!(
        query::<Option<i64>>(&db, "SELECT responseId FROM AskThreads ORDER BY title"),
        [Some(42), None, None]
    );
    let kind: Vec<String> = query(
        &db,
        "SELECT typeof(responseId) FROM AskThreads ORDER BY title",
    );
    assert_eq!(kind, ["integer", "null", "null"]);
}

#[tokio::test]
async fn unconvertible_values_are_reported_once() {
    let db = Db::open_in_memory().unwrap();
    let model = explorer();
    apply(&db, &model, false).await.unwrap();
    seed(&db, "a", Some("abc"));
    seed(&db, "b", None);

    // A required int without a default: "abc" cannot be converted, the NULL gets a placeholder.
    let mut retyped = model.clone();
    let response = entity(&mut retyped, "AskThread")
        .columns
        .iter_mut()
        .find(|c| c.name == "responseId")
        .unwrap();
    response.kind = FieldKind::Int {
        min: None,
        max: None,
    };
    response.nullable = false;
    let error = apply(&db, &retyped, false).await.unwrap_err();
    let SchemaError::DataLoss { losses } = &error else {
        panic!("{error}")
    };
    let losses: Vec<_> = losses
        .iter()
        .map(|loss| (loss.rows, &loss.reason))
        .collect();
    assert_eq!(
        losses,
        [
            (
                1,
                &LossReason::Unconvertible {
                    from: "text".into(),
                    to: "int".into()
                }
            ),
            (1, &LossReason::Placeholder { value: "0".into() })
        ]
    );
}

#[tokio::test]
async fn rebuilds_keep_the_nulls_a_column_allows() {
    let db = Db::open_in_memory().unwrap();
    let mut model = explorer();
    entity(&mut model, "AskThread").columns.push(column(
        "archived",
        FieldKind::Boolean,
        true,
        None,
    ));
    apply(&db, &model, false).await.unwrap();
    seed(&db, "a", None);
    seed(&db, "b", None);
    db.call_blocking(|conn| {
        conn.execute("UPDATE AskThreads SET archived = 1 WHERE title = 'b'", [])
            .unwrap()
    });
    let archived = || query::<Option<bool>>(&db, "SELECT archived FROM AskThreads ORDER BY title");

    // The optional field gains a default: the table is rebuilt, without force, and keeps its NULLs.
    entity(&mut model, "AskThread")
        .columns
        .iter_mut()
        .find(|c| c.name == "archived")
        .unwrap()
        .default = Some(json!(false));
    let report = apply(&db, &model, false).await.unwrap();
    assert!(
        matches!(&report.steps[..], [step] if step.class() == StepClass::Rebuild),
        "{:?}",
        report.steps
    );
    assert_eq!(archived(), [None, Some(true)]);

    // So does a rebuild for another field.
    for c in &mut entity(&mut model, "AskThread").columns {
        if let FieldKind::Text { max_length, .. } = &mut c.kind
            && c.name == "title"
        {
            *max_length = Some(400);
        }
    }
    assert!(!apply(&db, &model, false).await.unwrap().steps.is_empty());
    assert_eq!(archived(), [None, Some(true)]);
}

#[tokio::test]
async fn dates_become_text_with_their_full_precision() {
    let db = Db::open_in_memory().unwrap();
    let mut model = explorer();
    entity(&mut model, "AskThread")
        .columns
        .push(column("due", FieldKind::Date, true, None));
    apply(&db, &model, false).await.unwrap();
    seed(&db, "a", None);
    db.call_blocking(|conn| {
        conn.execute(
            "UPDATE AskThreads SET due = '2024-01-01T10:00:00.1234567Z'",
            [],
        )
        .unwrap()
    });

    entity(&mut model, "AskThread")
        .columns
        .iter_mut()
        .find(|c| c.name == "due")
        .unwrap()
        .kind = text();
    let report = apply(&db, &model, false).await.unwrap();
    assert!(report.forced.is_empty());
    assert_eq!(
        query::<String>(&db, "SELECT due FROM AskThreads"),
        ["2024-01-01T10:00:00.1234567Z"]
    );
}

#[tokio::test]
async fn lossless_rebuilds_and_dropped_tables() {
    let db = Db::open_in_memory().unwrap();
    let model = explorer();
    apply(&db, &model, false).await.unwrap();
    seed(&db, "a title of forty characters, give or take", None);

    // A longer title and an optional role rebuild the tables without force.
    let mut relaxed = model.clone();
    for c in &mut entity(&mut relaxed, "AskThread").columns {
        if let FieldKind::Text { max_length, .. } = &mut c.kind
            && c.name == "title"
        {
            *max_length = Some(400);
        }
    }
    entity(&mut relaxed, "AskMessage")
        .columns
        .iter_mut()
        .find(|c| c.name == "role")
        .unwrap()
        .nullable = true;
    let report = apply(&db, &relaxed, false).await.unwrap();
    assert!(
        report.steps.iter().all(|s| s.class() == StepClass::Rebuild),
        "{:?}",
        report.steps
    );
    assert_eq!(query::<i64>(&db, "SELECT count(*) FROM AskMessages"), [1]);

    // A shorter title than the data fails, and changes nothing.
    let mut shorter = relaxed.clone();
    for c in &mut entity(&mut shorter, "AskThread").columns {
        if let FieldKind::Text { max_length, .. } = &mut c.kind
            && c.name == "title"
        {
            *max_length = Some(10);
        }
    }
    let error = apply(&db, &shorter, true).await.unwrap_err();
    assert!(
        matches!(&error, SchemaError::Rebuild { table, reason } if table == "AskThreads" && reason.contains("CK_AskThreads_title_maxlen")),
        "{error}"
    );

    // Removing an entity drops its table, with force when it has rows.
    let mut without_messages = relaxed.clone();
    without_messages.entities.retain(|e| e.name != "AskMessage");
    entity(&mut without_messages, "AskThread")
        .relationships
        .clear();
    let error = apply(&db, &without_messages, false).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("AskMessages: the table is dropped with its 1 row(s)"),
        "{error}"
    );
    apply(&db, &without_messages, true).await.unwrap();
    let tables: Vec<String> = query(
        &db,
        "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE '\\_rl\\_%' ESCAPE '\\'",
    );
    assert_eq!(tables, ["AskThreads"]);
}
