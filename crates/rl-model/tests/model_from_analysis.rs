//! `AppModel::from_analysis` on the golden analyses of explorer and the `basic` fixture
//! (`tests/fixtures/*.analysis.json`, produced by `js/analyze.mjs`).

use rl_model::*;
use serde_json::{Value, json};

fn basic_json() -> Value {
    serde_json::from_str(include_str!("fixtures/basic.analysis.json")).unwrap()
}

fn basic() -> AppModel {
    AppModel::from_analysis(&basic_json()).unwrap()
}

fn explorer() -> AppModel {
    let json = serde_json::from_str(include_str!("fixtures/explorer.analysis.json")).unwrap();
    AppModel::from_analysis(&json).unwrap()
}

fn eq(left: Operand, right: Operand) -> PolicyExpr {
    PolicyExpr::Compare {
        op: CompareOp::Eq,
        left,
        right,
    }
}

fn ne(left: Operand, right: Operand) -> PolicyExpr {
    PolicyExpr::Compare {
        op: CompareOp::Ne,
        left,
        right,
    }
}

fn claim(name: &str) -> Operand {
    Operand::Claim(name.into())
}

fn field(name: &str) -> Operand {
    Operand::Field(name.into())
}

fn string(value: &str) -> Operand {
    Operand::Literal(Literal::String(value.into()))
}

#[test]
fn explorer_threads_and_messages() {
    let model = explorer();
    assert_eq!(model.core_version.as_deref(), Some("1.36.2"));
    assert_eq!(model.dialect, Dialect::Mssql);
    let names: Vec<_> = model.entities.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["AskMessage", "AskThread"]);

    let thread = model.entity("AskThread").unwrap();
    assert_eq!(thread.table, "AskThreads");
    assert_eq!(thread.graphql.list_query, "askThreads");
    assert_eq!(thread.graphql.by_pk_query, "askThread_by_pk");
    assert_eq!(thread.graphql.create_mutation, "createAskThread");
    assert_eq!(thread.graphql.filter_input(), "AskThreadFilterInput");
    let title = thread.field("title").unwrap();
    assert_eq!(
        title.kind,
        FieldKind::Text {
            min_length: Some(1),
            max_length: Some(200),
            pattern: None
        }
    );
    assert_eq!(title.db_type, "NVARCHAR(200)");
    assert!(thread.field("responseId").unwrap().nullable);
    let messages = thread.relationship("messages").unwrap();
    assert_eq!(messages.cardinality, Cardinality::Many);
    assert_eq!(messages.target, "AskMessage");
    assert_eq!(messages.source_columns, ["id"]);
    assert_eq!(messages.target_columns, ["thread_id"]);

    let message = model.entity("AskMessage").unwrap();
    assert_eq!(message.primary_key_column().name, "id");
    let thread_id = message.column("thread_id").unwrap();
    assert_eq!(thread_id.kind, FieldKind::Uuid);
    assert_eq!(thread_id.origin, ColumnOrigin::Declared);
    assert!(!thread_id.nullable);
    assert_eq!(
        message.foreign_keys,
        [ForeignKey {
            name: "FK_AskMessages_thread_id".into(),
            columns: vec!["thread_id".into()],
            referenced_table: "AskThreads".into(),
            referenced_columns: vec!["id".into()],
            referenced_entity: Some("AskThread".into()),
        }]
    );
    let to_thread = message.relationship("thread").unwrap();
    assert_eq!(to_thread.cardinality, Cardinality::One);
    assert_eq!(to_thread.source_columns, ["thread_id"]);
    assert!(!to_thread.nullable && !to_thread.target_is_system);
    assert_eq!(
        message.field("role").unwrap().kind,
        FieldKind::Set {
            values: vec!["user".into(), "assistant".into()],
            allowed: vec!["user".into(), "assistant".into()],
        }
    );
    assert_eq!(message.field("createdAt").unwrap().kind, FieldKind::Date);

    for entity in &model.entities {
        assert_eq!(entity.permissions.len(), 1, "{}", entity.name);
        let grants: Vec<_> = entity.grants("authenticated", Action::Read).collect();
        assert_eq!(grants.len(), 1);
        assert_eq!(grants[0].action, Action::All);
        let policy = grants[0].policy.as_ref().unwrap();
        assert_eq!(policy.text, "@claims.sub eq @item.user_id");
        assert_eq!(policy.expr, eq(claim("sub"), field("user_id")));
        assert!(entity.grants("anonymous", Action::Read).next().is_none());
    }
    assert_eq!(model.root_field("askMessages").unwrap().1, RootField::List);
    assert_eq!(
        model.root_field("deleteAskThread").unwrap().0.name,
        "AskThread"
    );
    assert!(model.root_field("askThread").is_none());
    assert!(model.storage.is_none());
}

#[test]
fn basic_columns_cover_every_decorator() {
    let model = basic();
    let todo = model.entity("Todo").unwrap();
    let kind = |name: &str| todo.field(name).unwrap().kind.clone();
    assert_eq!(
        kind("slug"),
        FieldKind::Text {
            min_length: None,
            max_length: Some(80),
            pattern: Some(Pattern {
                source: "^[a-z0-9-]+$".into(),
                flags: "i".into()
            }),
        }
    );
    assert_eq!(
        kind("contactEmail"),
        FieldKind::Email {
            min_length: None,
            max_length: Some(320)
        }
    );
    // rayfin-core strips quotes from @set values when it writes the CHECK constraint.
    let status = todo.field("status").unwrap();
    assert_eq!(
        status.kind,
        FieldKind::Set {
            values: vec![
                "it's new".into(),
                "doing".into(),
                "done".into(),
                "it's archived".into()
            ],
            allowed: vec![
                "its new".into(),
                "doing".into(),
                "done".into(),
                "its archived".into()
            ],
        }
    );
    assert_eq!(status.default, Some(json!("it's new")));
    assert!(todo.field("priority").unwrap().nullable);
    assert_eq!(
        kind("estimate"),
        FieldKind::Int {
            min: Some(0.0),
            max: None
        }
    );
    assert_eq!(
        kind("cost"),
        FieldKind::Decimal {
            precision: 18,
            scale: 2,
            min: None,
            max: None
        }
    );
    assert_eq!(
        kind("weight"),
        FieldKind::Decimal {
            precision: 10,
            scale: 4,
            min: Some(0.0),
            max: Some(99999.0)
        }
    );
    assert_eq!(todo.field("isPublic").unwrap().default, Some(json!(false)));
    assert_eq!(kind("dueDate"), FieldKind::Date);
    let unique: Vec<_> = todo
        .columns
        .iter()
        .filter(|c| c.unique)
        .map(|c| c.field.as_str())
        .collect();
    assert_eq!(unique, ["slug", "contactEmail", "externalRef"]);
    assert_eq!(
        todo.field("category_id").unwrap().origin,
        ColumnOrigin::Declared
    );
    assert!(todo.relationship("category").unwrap().nullable);

    let category = model.entity("Category").unwrap();
    let label = category.field("label").unwrap();
    assert_eq!(label.name, "display_label");
    assert_eq!(category.column("display_label"), Some(label));
    let sort_order = category.field("sortOrder").unwrap();
    assert_eq!(sort_order.default, Some(json!(0)));
    assert_eq!(sort_order.kind.graphql_scalar(), "Int");
    assert!(sort_order.kind.is_numeric());
    let parent = category.relationship("parent").unwrap();
    assert_eq!(
        (parent.target.as_str(), parent.nullable),
        ("Category", true)
    );
    assert_eq!(
        category.column("parent_id").unwrap().origin,
        ColumnOrigin::Relationship {
            relationship: "parent".into()
        }
    );

    let comment = model.entity("Comment").unwrap();
    let todo_id = comment.column("todo_id").unwrap();
    assert!(!todo_id.nullable);
    assert_eq!(
        todo_id.origin,
        ColumnOrigin::Relationship {
            relationship: "todo".into()
        }
    );
    assert_eq!(
        comment.field("authorEmail").unwrap().kind,
        FieldKind::Email {
            min_length: Some(6),
            max_length: Some(320)
        }
    );
}

#[test]
fn basic_inferred_columns_names_and_system_references() {
    let model = basic();
    let milestone = model.entity("Milestone").unwrap();
    assert_eq!(
        milestone.column("id").unwrap().origin,
        ColumnOrigin::ImplicitPrimaryKey
    );
    let project_id = milestone.column("Project_id").unwrap();
    assert!(!project_id.nullable);
    assert_eq!(
        project_id.origin,
        ColumnOrigin::InverseRelationship {
            entity: "Project".into(),
            relationship: "milestones".into()
        }
    );
    assert_eq!(
        milestone.foreign_keys[0].referenced_entity.as_deref(),
        Some("Project")
    );
    // No @role: the generated config grants authenticated everything.
    assert_eq!(milestone.permissions.len(), 1);
    assert_eq!(milestone.permissions[0].role, "authenticated");
    assert_eq!(milestone.permissions[0].actions[0].action, Action::All);
    assert!(milestone.permissions[0].actions[0].policy.is_none());

    let person = model.entity("Person").unwrap();
    assert_eq!(person.table, "People");
    assert_eq!(person.graphql.list_query, "people");
    assert_eq!(person.graphql.by_pk_query, "person_by_pk");
    assert_eq!(model.entity_by_table("People").unwrap().name, "Person");
    let account = person.relationship("account").unwrap();
    assert!(account.target_is_system);
    assert_eq!(account.target, "User");
    assert_eq!(person.foreign_keys[0].referenced_table, "Users");
    assert_eq!(person.foreign_keys[0].referenced_entity, None);
    let defaults: Vec<_> = person
        .columns
        .iter()
        .filter_map(|c| c.default.clone())
        .collect();
    assert_eq!(
        defaults,
        [
            json!("n/a"),
            json!(1),
            json!(9.99),
            json!(true),
            json!("2024-01-01T00:00:00.000Z")
        ]
    );
}

#[test]
fn basic_permissions_rules_and_policies() {
    let model = basic();
    let todo = model.entity("Todo").unwrap();
    let anonymous = todo.role("anonymous").unwrap();
    assert_eq!(anonymous.actions.len(), 1);
    let read = &anonymous.actions[0];
    let fields = read.fields.as_ref().unwrap();
    assert!(fields.allows("title") && !fields.allows("secretNote"));
    assert_eq!(read.policy.as_ref().unwrap().text, "@item.isPublic eq true");

    let authenticated = todo.role("authenticated").unwrap();
    let actions: Vec<_> = authenticated.actions.iter().map(|a| a.action).collect();
    assert_eq!(
        actions,
        [Action::Update, Action::Delete, Action::Read, Action::Create]
    );
    let read = todo.grants("authenticated", Action::Read).next().unwrap();
    let fields = read.fields.as_ref().unwrap();
    assert_eq!(fields.include, ["*"]);
    assert!(fields.allows("title") && !fields.allows("secretNote"));
    let update = &authenticated.actions[0].policy.as_ref().unwrap().expr;
    assert_eq!(
        *update,
        PolicyExpr::And(vec![
            eq(field("user_id"), claim("sub")),
            PolicyExpr::Or(vec![
                PolicyExpr::Or(vec![
                    ne(field("status"), string("it's archived")),
                    eq(field("estimate"), Operand::Literal(Literal::Null)),
                ]),
                ne(
                    field("estimate"),
                    Operand::Literal(Literal::Number(0.into()))
                ),
            ]),
        ])
    );
    let comment = model.entity("Comment").unwrap();
    let create = comment
        .grants("authenticated", Action::Create)
        .next()
        .unwrap();
    assert_eq!(
        create.policy.as_ref().unwrap().expr.claims(),
        ["email", "sub", "role"]
    );

    let project = model.entity("Project").unwrap();
    let anonymous = &project.role("anonymous").unwrap().actions[0];
    let PolicyExpr::And(args) = &anonymous.policy.as_ref().unwrap().expr else {
        panic!("expected and")
    };
    // A Date literal reaches the AST as its ISO string; the DAB text keeps it unquoted.
    assert_eq!(
        args[0],
        ne(field("startsOn"), string("2000-01-01T00:00:00.000Z"))
    );

    // Without Date literals, the tree renders back to the exact DAB policy string.
    for entity in &model.entities {
        for grant in entity.permissions.iter().flat_map(|p| &p.actions) {
            if let Some(policy) = &grant.policy
                && !policy.text.contains("2000-01-01")
            {
                assert_eq!(policy.expr.to_string(), policy.text);
            }
        }
    }
}

#[test]
fn basic_storage_folders() {
    let model = basic();
    let storage = model.storage.as_ref().unwrap();
    assert_eq!(storage.schema_version, 1);
    let names: Vec<_> = storage.folders.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["todo-attachments", "user-avatars"]);

    let attachments = model.storage_folder("todo-attachments").unwrap();
    assert_eq!(attachments.display_name, "Attachments");
    assert_eq!(attachments.on_conflict, OnConflict::Overwrite);
    assert_eq!(attachments.max_size, Some(5 * 1024 * 1024));
    assert_eq!(
        attachments.allowed_content_types,
        ["image/*", "application/pdf"]
    );
    assert_eq!(
        attachments.fields,
        [
            StorageField {
                name: "team_id".into(),
                kind: StorageFieldType::String,
                nullable: false
            },
            StorageField {
                name: "pages".into(),
                kind: StorageFieldType::Number,
                nullable: true
            },
        ]
    );
    let delete = &attachments.rules[0];
    assert_eq!(delete.actions, [Action::Delete]);
    assert_eq!(
        delete.check,
        Some(PolicyExpr::And(vec![
            eq(claim("sub"), field("owner_id")),
            ne(field("team_id"), Operand::Literal(Literal::Null)),
        ]))
    );
    assert_eq!(
        attachments.rules[1].actions,
        [Action::Create, Action::Read, Action::Update]
    );

    let avatars = model.storage_folder("user-avatars").unwrap();
    assert_eq!(avatars.on_conflict, OnConflict::Error);
    assert_eq!(avatars.max_size, None);
    assert!(avatars.rules[0].check.is_none());
}

#[test]
fn the_model_round_trips_through_json() {
    let model = basic();
    let json = serde_json::to_value(&model).unwrap();
    let back: AppModel = serde_json::from_value(json).unwrap();
    assert_eq!(back, model);
}

/// The Todo `authenticated` `update` policy record of the basic analysis.
fn todo_update_policy(analysis: &mut Value) -> &mut Value {
    analysis["policies"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["entity"] == "Todo" && p["action"] == "update")
        .unwrap()
}

fn policy_error(analysis: &Value) -> String {
    match AppModel::from_analysis(analysis).unwrap_err() {
        ModelError::Policy {
            entity,
            role,
            action,
            reason,
            ..
        } => {
            assert_eq!(
                (entity.as_str(), role.as_str(), action.as_str()),
                ("Todo", "authenticated", "update")
            );
            reason
        }
        other => panic!("expected a policy error, got {other}"),
    }
}

#[test]
fn policies_outside_the_grammar_fail_loudly() {
    let mut analysis = basic_json();
    let record = todo_update_policy(&mut analysis);
    record.as_object_mut().unwrap().remove("check");
    record["checkError"] = json!("Unsupported policy expression for storage check AST: FieldRef");
    assert!(policy_error(&analysis).contains("FieldRef"));

    let mut analysis = basic_json();
    todo_update_policy(&mut analysis)["check"] =
        json!({"op": "gt", "lhs": {"field": "estimate"}, "rhs": 1});
    assert!(policy_error(&analysis).contains("`gt`"));

    let mut analysis = basic_json();
    todo_update_policy(&mut analysis)["check"] =
        json!({"op": "eq", "lhs": {"field": "nope"}, "rhs": {"claim": "sub"}});
    assert!(policy_error(&analysis).contains("item.nope"));

    let mut analysis = basic_json();
    let policies = analysis["policies"].as_array_mut().unwrap();
    policies.retain(|p| !(p["entity"] == "Todo" && p["action"] == "update"));
    assert!(policy_error(&analysis).contains("no expression tree"));
}

#[test]
fn unknown_decorators_and_shapes_fail_loudly() {
    let mut analysis = basic_json();
    let todo = analysis["entities"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|e| e["name"] == "Todo")
        .unwrap();
    let cost = todo["fields"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|f| f["name"] == "cost")
        .unwrap();
    cost["originalFieldMetadata"]["format"] = json!("money");
    let error = AppModel::from_analysis(&analysis).unwrap_err().to_string();
    assert!(error.contains("Todo") && error.contains("money"), "{error}");

    let mut analysis = basic_json();
    analysis["config"]["entities"]["Category"]["relationships"]["todos"]["linking"] = json!({});
    let error = AppModel::from_analysis(&analysis).unwrap_err().to_string();
    assert!(error.contains("many-to-many"), "{error}");

    let mut analysis = basic_json();
    analysis["config"]["entities"]["Todo"]["permissions"][0]["actions"][0]["policy"] =
        json!({"database": "@item.isPublic eq true", "request": "x"});
    let error = AppModel::from_analysis(&analysis).unwrap_err().to_string();
    assert!(error.contains("policy.database"), "{error}");

    let mut analysis = basic_json();
    analysis["storage"]["config"]["folders"][0]["rules"][0]["check"] =
        json!({"op": "in", "args": []});
    let error = AppModel::from_analysis(&analysis).unwrap_err();
    assert!(matches!(error, ModelError::Storage { .. }), "{error}");

    let error = AppModel::from_analysis(&json!({"dialect": "mssql"})).unwrap_err();
    assert!(matches!(error, ModelError::Malformed(_)), "{error}");
}

#[test]
fn analyze_input_for_a_project() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("fixtures")
        .join("apps")
        .join("basic");
    let input = AnalyzeInput::for_project(&root, Dialect::Mssql, true).to_json();
    assert_eq!(input["dialect"], "mssql");
    assert_eq!(input["storage"], true);
    assert!(
        input["tsconfigPath"]
            .as_str()
            .unwrap()
            .ends_with("tsconfig.json")
    );
    assert!(input["outDir"].as_str().unwrap().contains(".rayfin-local"));
    assert!(input.get("storageDir").is_none());
}
