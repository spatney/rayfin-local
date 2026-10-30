//! Mutations on a connector's source, as DAB's SQL engines run them: `create{E}(item:)`,
//! `update{E}(<key>, item:)` and `delete{E}(<key>)`, each root field in its own transaction.
//!
//! The checks are the data API's (`crate::mutation`): the role's grant for the action and its
//! fields, then the action's policy in the statement's `WHERE`. Then, by [`Target`]:
//!
//! - **SQL database**: `INSERT`/`UPDATE … OUTPUT INSERTED.<key>`, and the row read back by that
//!   key with the read grant and policy, as `_by_pk` reads it. A delete reads the row first.
//! - **Warehouse**: no `OUTPUT` (DWSQL has none); the answer is `DbOperationResult { result:
//!   "success" }` once a row was written.
//!
//! No row written: a create refused by its policy answers DAB's `DatabasePolicyFailure`, an
//! update `ItemNotFound`, a delete `null`. Database errors roll back and answer
//! `DatabaseOperationFailed` with the server's message as `extensions.detail`.

use async_graphql_parser::Pos;
use async_graphql_parser::types::SelectionSet;
use rl_db::codec;
use rl_model::{Action, AppModel, Entity};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value as Json};

use super::Target;
use super::backend::{SqlBackend, SqlError, SqlTransaction, Statement, Value};
use super::emit::{self, Emitter, column_ref, ident, source};
use crate::access;
use crate::error::{GraphQlError, codes, messages};
use crate::exec::{self, Outcome};
use crate::mutation::{
    DB_OPERATION_RESULT, DB_OPERATION_RESULT_FIELD, INSERT_REFUSED, MUTATION, granted, item_values,
    located, read_denied,
};
use crate::plan::{self, Collected, Ctx, ObjectPlan};
use crate::policy;
use crate::schema::Resolver;
use crate::sql::{CmpOp, Operand, Pred};

pub(crate) struct Planned {
    authentication_required: bool,
    roots: Vec<Root>,
}

struct Root {
    key: String,
    kind: RootKind,
}

enum RootKind {
    Typename,
    Failed(GraphQlError),
    Write(Box<Write>),
}

/// One root field's write.
struct Write {
    entity: usize,
    action: Action,
    field: String,
    pos: Pos,
    /// The write: an `INSERT`/`UPDATE` with `OUTPUT INSERTED.<key>` (SQL database), a plain
    /// write (Warehouse), or for an update without fields, a `SELECT` of the key that checks the
    /// row exists.
    write: Statement,
    /// The write returns the key of the row it wrote (rows), else a count.
    returns_keys: bool,
    /// What the field answers.
    answer: Answer,
    /// The update's `ItemNotFound` message.
    not_found: String,
}

enum Answer {
    /// The row read back by its key: the plan of the read and its read policy. A delete reads
    /// it before deleting (`before`).
    Row {
        plan: Box<ObjectPlan>,
        policy: Option<Pred>,
        before: Option<Statement>,
    },
    /// The role may not read the entity (DAB's read-denied error once the write is done).
    ReadDenied,
    /// The row can't be read back (the write is undone).
    Unreadable(GraphQlError),
    /// `DbOperationResult`: the selection's response keys and field names.
    Status(Vec<(String, String)>),
}

/// Plans the root fields of a mutation (the `Mutation` type exists: validation checked it).
pub(crate) fn plan<'d>(ctx: &Ctx<'d>, selection: &'d SelectionSet, target: Target) -> Planned {
    let fields = plan::collect_fields(ctx, MUTATION, &[selection]);
    let authentication_required = !ctx.caller.is_authenticated()
        && exec::requires_authentication(ctx.model, ctx.schema, MUTATION, &fields);
    let roots = if authentication_required {
        Vec::new()
    } else {
        fields
            .iter()
            .map(|field| plan_root(ctx, field, target))
            .collect()
    };
    Planned {
        authentication_required,
        roots,
    }
}

fn plan_root(ctx: &Ctx<'_>, field: &Collected<'_>, target: Target) -> Root {
    let key = field.key.clone();
    if field.name() == "__typename" {
        return Root {
            key,
            kind: RootKind::Typename,
        };
    }
    let def = ctx
        .schema
        .object(MUTATION)
        .and_then(|mutation| mutation.fields.get(field.name()));
    let (def, index, action) = match def.map(|def| (def, def.resolver)) {
        Some((def, Resolver::Create(index))) => (def, index, Action::Create),
        Some((def, Resolver::Update(index))) => (def, index, Action::Update),
        Some((def, Resolver::Delete(index))) => (def, index, Action::Delete),
        _ => {
            let error = GraphQlError::dab(messages::DATABASE_ERROR, codes::UNEXPECTED_ERROR);
            return Root {
                kind: RootKind::Failed(located(error, field.pos(), &key)),
                key,
            };
        }
    };
    let kind = match plan_write(ctx, def, field, index, action, target) {
        Ok(write) => RootKind::Write(Box::new(write)),
        Err(error) => RootKind::Failed(located(error, field.pos(), &key)),
    };
    Root { key, kind }
}

fn plan_write(
    ctx: &Ctx<'_>,
    def: &crate::schema::FieldDef,
    field: &Collected<'_>,
    index: usize,
    action: Action,
    target: Target,
) -> Result<Write, GraphQlError> {
    let model = ctx.model;
    let entity = &model.entities[index];
    let grant = granted(ctx, entity, action)?;
    let node = &field.fields[0].node;
    let args = ctx.arguments(def, node)?;
    let rows = target == Target::SqlDatabase;
    // Statements name the table itself (`[schema].[table]`), so policies refer to its columns.
    let table = source(&entity.table);
    let mut emitter = Emitter::new(model);
    emitter.register(&table, index);
    let mut not_found = String::new();
    let (text, returns_keys) = match action {
        Action::Create => {
            let item = item_values(ctx, entity, grant, node, &args)?;
            let alias = ctx.alias();
            emitter.register(&alias, index);
            let policy = policy::action_filter(entity, ctx.caller, Action::Create, &alias)?;
            (
                insert(&mut emitter, entity, &item, &alias, policy, rows),
                rows && !entity.key_columns().is_empty(),
            )
        }
        Action::Update => {
            let item = item_values(ctx, entity, grant, node, &args)?;
            let policy = policy::action_filter(entity, ctx.caller, Action::Update, &table)?;
            let key = plan::key_predicate(entity, &table, &args)?;
            not_found = format!("Could not find item with <{}>", key_text(entity, &args));
            let filter = emitter.pred(&Pred::all(std::iter::once(key).chain(policy)));
            if item.is_empty() {
                // Nothing to change, but the row must still be found.
                (select_keys(entity, &table, &filter), true)
            } else {
                let sets = item
                    .iter()
                    .map(|(column, value)| {
                        let column = &entity.columns[*column];
                        format!(
                            "{} = {}",
                            ident(&column.name),
                            emitter.value(Some(column), value)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let output = if rows {
                    output_keys(entity)
                } else {
                    String::new()
                };
                (
                    format!("UPDATE {table} SET {sets}{output} WHERE {filter}"),
                    rows,
                )
            }
        }
        _ => {
            let policy = policy::action_filter(entity, ctx.caller, Action::Delete, &table)?;
            let key = plan::key_predicate(entity, &table, &args)?;
            let filter = emitter.pred(&Pred::all(std::iter::once(key).chain(policy)));
            (format!("DELETE FROM {table} WHERE {filter}"), false)
        }
    };
    let write = emitter.finish(text);
    let answer = if rows {
        row_answer(ctx, field, index, action, &args)?
    } else {
        Answer::Status(status_selection(ctx, field))
    };
    Ok(Write {
        entity: index,
        action,
        field: field.name().to_owned(),
        pos: field.pos(),
        write,
        returns_keys,
        answer,
        not_found,
    })
}

/// `INSERT INTO <table> (<item columns>) [OUTPUT INSERTED.<key>] SELECT … FROM (SELECT <every
/// column>) AS <alias> WHERE <create policy>`: the policy sees the candidate row (columns left
/// out are `NULL` there; the database fills in its defaults).
fn insert(
    emitter: &mut Emitter<'_>,
    entity: &Entity,
    item: &[(usize, SqlValue)],
    alias: &str,
    policy: Option<Pred>,
    rows: bool,
) -> String {
    let table = source(&entity.table);
    let candidate = entity
        .columns
        .iter()
        .enumerate()
        .filter(|(_, column)| !column.source.as_ref().is_some_and(|s| s.generated))
        .map(|(index, column)| {
            let value = match item.iter().find(|(c, _)| *c == index) {
                Some((_, value)) => emitter.value(Some(column), value),
                None => "NULL".to_owned(),
            };
            format!("{value} AS {}", ident(&column.name))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let columns = item
        .iter()
        .map(|(column, _)| ident(&entity.columns[*column].name))
        .collect::<Vec<_>>()
        .join(", ");
    let output = if rows && !entity.key_columns().is_empty() {
        output_keys(entity)
    } else {
        String::new()
    };
    let filter = emitter.pred(&policy.unwrap_or(Pred::Const(true)));
    let candidate = if candidate.is_empty() {
        "1 AS [__candidate]".to_owned()
    } else {
        candidate
    };
    if item.is_empty() {
        // Every column takes its default, when the policy allows the row.
        return format!(
            "IF EXISTS (SELECT 1 FROM (SELECT {candidate}) AS {alias} WHERE {filter}) INSERT INTO \
             {table}{output} DEFAULT VALUES"
        );
    }
    let selected = item
        .iter()
        .map(|(column, _)| column_ref(alias, &entity.columns[*column].name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO {table} ({columns}){output} SELECT {selected} FROM (SELECT {candidate}) AS \
         {alias} WHERE {filter}"
    )
}

/// ` OUTPUT INSERTED.[k1], …`.
fn output_keys(entity: &Entity) -> String {
    let keys = entity
        .key_columns()
        .iter()
        .map(|key| format!("INSERTED.{}", ident(&key.name)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(" OUTPUT {keys}")
}

/// `SELECT <key columns> FROM <table> WHERE <filter>`.
fn select_keys(entity: &Entity, table: &str, filter: &str) -> String {
    let keys = entity
        .key_columns()
        .iter()
        .map(|key| column_ref(table, &key.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("SELECT {keys} FROM {table} WHERE {filter}")
}

/// `k1: v1, k2: v2` of the key arguments, for DAB's `ItemNotFound` message.
fn key_text(entity: &Entity, args: &std::collections::HashMap<String, crate::Input>) -> String {
    entity
        .key_columns()
        .iter()
        .map(|key| {
            let value = match args.get(&key.field) {
                Some(crate::Input::Scalar(Json::String(text))) => text.clone(),
                Some(crate::Input::Scalar(json)) => json.to_string(),
                _ => String::new(),
            };
            format!("{}: {value}", key.field)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The row read back (or before a delete), with the read grant, policy and the field's
/// selection.
fn row_answer(
    ctx: &Ctx<'_>,
    field: &Collected<'_>,
    index: usize,
    action: Action,
    args: &std::collections::HashMap<String, crate::Input>,
) -> Result<Answer, GraphQlError> {
    let entity = &ctx.model.entities[index];
    let Some(grant) = access::grant(entity, ctx.caller.role(), Action::Read) else {
        return Ok(Answer::ReadDenied);
    };
    let alias = ctx.alias();
    let read = (|| {
        let policy = policy::read_filter(entity, ctx.caller, &alias)?;
        let plan = plan::object_plan(ctx, index, alias.clone(), &field.sub_sets(), Some(grant))?;
        Ok::<_, GraphQlError>((plan, policy))
    })();
    let (plan, policy) = match read {
        Ok(read) => read,
        // DAB undoes a create or update whose row it can't read back; a delete fails first.
        Err(error) if action != Action::Delete => return Ok(Answer::Unreadable(error)),
        Err(error) => return Err(error),
    };
    if emit::object_uses_group_by(&plan) {
        return Err(super::not_supported());
    }
    let before = (action == Action::Delete)
        .then(|| {
            let key = plan::key_predicate(entity, &alias, args)?;
            let filter = Pred::all(std::iter::once(key).chain(policy.clone()));
            let mut emitter = Emitter::new(ctx.model);
            let text = emitter.object_select(&plan, &filter);
            Ok::<_, GraphQlError>(emitter.finish(text))
        })
        .transpose()?;
    Ok(Answer::Row {
        plan: Box::new(plan),
        policy,
        before,
    })
}

/// The response keys and fields of a `DbOperationResult` selection.
fn status_selection(ctx: &Ctx<'_>, field: &Collected<'_>) -> Vec<(String, String)> {
    plan::collect_fields(ctx, DB_OPERATION_RESULT, &field.sub_sets())
        .iter()
        .map(|selected| (selected.key.clone(), selected.name().to_owned()))
        .collect()
}

/// How a write went.
enum Done {
    /// Committed; for a row answer, the row read back (`None`: hidden by the read policy).
    Applied(Option<Json>),
    /// No row was written: the key or the policy matched none (rolled back).
    NoRow,
    /// Written, then undone because the row can't be read back.
    RolledBack,
    Failed(SqlError),
}

/// Runs a planned mutation: the writes in order, each in its own transaction, then the answer.
pub(crate) async fn execute(
    model: &AppModel,
    planned: Planned,
    backend: &dyn SqlBackend,
) -> Outcome {
    if planned.authentication_required {
        return Outcome::AuthenticationRequired;
    }
    let mut errors = Vec::new();
    let mut data = Map::new();
    for root in planned.roots {
        let value = match root.kind {
            RootKind::Typename => Json::String(MUTATION.into()),
            RootKind::Failed(error) => {
                errors.push(error);
                Json::Null
            }
            RootKind::Write(write) => {
                let done = match run(model, &write, backend).await {
                    Ok(done) => done,
                    Err(error) => Done::Failed(error),
                };
                write.answer(model, &root.key, done, &mut errors)
            }
        };
        data.insert(root.key, value);
    }
    exec::response(Some(Json::Object(data)), errors)
}

async fn run(model: &AppModel, write: &Write, backend: &dyn SqlBackend) -> Result<Done, SqlError> {
    let mut tx = backend.begin().await?;
    let before = match &write.answer {
        Answer::Row {
            before: Some(statement),
            plan,
            ..
        } => read_row(model, plan, tx.as_mut(), statement).await?,
        _ => None,
    };
    let keys = if write.returns_keys {
        let rows = tx.query(&write.write).await?;
        if rows.is_empty() {
            return Ok(Done::NoRow);
        }
        rows
    } else {
        if tx.execute(&write.write).await? == 0 {
            return Ok(Done::NoRow);
        }
        Vec::new()
    };
    let row = match &write.answer {
        Answer::Unreadable(_) => return Ok(Done::RolledBack),
        Answer::Row {
            plan,
            policy,
            before: None,
        } => {
            let entity = &model.entities[write.entity];
            match keys.first() {
                Some(key) => {
                    let statement = read_back(model, entity, plan, policy.as_ref(), key);
                    read_row(model, plan, tx.as_mut(), &statement).await?
                }
                None => None,
            }
        }
        _ => before,
    };
    tx.commit().await?;
    Ok(Done::Applied(row))
}

/// The `SELECT` of the row whose key `OUTPUT` returned.
fn read_back(
    model: &AppModel,
    entity: &Entity,
    plan: &ObjectPlan,
    policy: Option<&Pred>,
    key: &[Value],
) -> Statement {
    let preds = entity
        .key_columns()
        .into_iter()
        .zip(key)
        .map(|(column, value)| {
            Pred::Cmp(
                Operand::column(&plan.alias, &column.name),
                CmpOp::Eq,
                Operand::Value(stored(&column.kind, value)),
            )
        })
        .chain(policy.cloned());
    let mut emitter = Emitter::new(model);
    let text = emitter.object_select(plan, &Pred::all(preds));
    emitter.finish(text)
}

/// A value the database returned, as the storage value [`emit::to_param`] binds back.
fn stored(kind: &rl_model::FieldKind, value: &Value) -> SqlValue {
    let text = |text: &str| codec::parse_text(kind, text).unwrap_or(SqlValue::Null);
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Int(n) => SqlValue::Integer(*n),
        Value::Float(n) => SqlValue::Real(*n),
        Value::Text(value) if codec::is_textual(kind) => SqlValue::Text(value.clone()),
        Value::DateTime(value) => text(&format!("{}Z", value.trim_end_matches('Z'))),
        Value::Decimal(value) | Value::Text(value) | Value::Uuid(value) | Value::Other(value) => {
            text(value)
        }
    }
}

async fn read_row(
    model: &AppModel,
    plan: &ObjectPlan,
    tx: &mut dyn SqlTransaction,
    statement: &Statement,
) -> Result<Option<Json>, SqlError> {
    let rows = tx.query(statement).await?;
    let Some(Value::Text(text)) = rows.first().and_then(|row| row.first()) else {
        return Ok(None);
    };
    let mut json: Json = serde_json::from_str(text).map_err(|error| SqlError::Database {
        number: 0,
        message: format!("the read-back row is not JSON: {error}"),
    })?;
    emit::normalize_object(model, plan, &mut json);
    Ok(Some(json))
}

impl Write {
    /// The field's value once the write ran, adding its errors.
    fn answer(
        self,
        model: &AppModel,
        key: &str,
        done: Done,
        errors: &mut Vec<GraphQlError>,
    ) -> Json {
        let error = match done {
            Done::Failed(error) => super::sql_error(key, error),
            Done::RolledBack => match self.answer {
                Answer::Unreadable(error) => error,
                _ => GraphQlError::dab(messages::DATABASE_ERROR, codes::UNEXPECTED_ERROR),
            },
            Done::NoRow if self.action == Action::Create => {
                GraphQlError::dab(INSERT_REFUSED, codes::DATABASE_POLICY_FAILURE)
            }
            Done::NoRow if self.action == Action::Update => {
                GraphQlError::dab(self.not_found, codes::ITEM_NOT_FOUND)
            }
            // DAB reports a role that may not read once the transaction is over, rows or not.
            Done::NoRow | Done::Applied(_) if matches!(self.answer, Answer::ReadDenied) => {
                GraphQlError::dab(read_denied(&self.field), codes::AUTHORIZATION_CHECK_FAILED)
            }
            Done::NoRow => return Json::Null,
            Done::Applied(row) => match (&self.answer, row) {
                (Answer::Status(selection), _) => return status(selection),
                (Answer::Row { plan, .. }, Some(row)) => {
                    return exec::shape_json_object(model, plan, &row, key, errors)
                        .unwrap_or(Json::Null);
                }
                _ => return Json::Null,
            },
        };
        errors.push(located(error, self.pos, key));
        Json::Null
    }
}

/// The answer of a `DbOperationResult` write.
fn status(selection: &[(String, String)]) -> Json {
    let mut out = Map::new();
    for (key, name) in selection {
        let value = match name.as_str() {
            DB_OPERATION_RESULT_FIELD => Json::String("success".into()),
            _ => Json::String(DB_OPERATION_RESULT.into()),
        };
        out.insert(key.clone(), value);
    }
    Json::Object(out)
}
