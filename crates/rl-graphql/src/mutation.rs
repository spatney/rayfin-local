//! Mutations: `create{E}(item:)`, `update{E}(id:, item:)` and `delete{E}(id:)`, the documents the
//! SDK 1.36.2 sends for `create`/`update`/`delete` (its `upsert` is a `_by_pk` read followed by one
//! of them; it never sends nested or multiple creates), run as DAB's `SqlMutationEngine` runs them
//! (micro-fabric compatibility.md §6.8, §6.9, §7.3-§7.5).
//!
//! **Schema** ([`extend_schema`]): a `Mutation` field per entity and action that some role is
//! granted, returning `E` (nullable). `Create{E}Input` and `Update{E}Input` have the columns of `E`
//! (FK columns included, relationships not). On create a column is required when it is NOT NULL
//! without a default, so the generated `id` is optional; on update every column is optional
//! (PATCH: omitted fields are untouched, `null` sets NULL).
//!
//! **Execution**: the root fields run in document order, each in its own transaction.
//!
//! 1. The role needs a grant for the action (else Hot Chocolate's `AUTH_NOT_AUTHORIZED`), and the
//!    grant's fields must allow every `item` field (else `Unauthorized due to one or more fields
//!    in this mutation.`; deletes have no field check).
//! 2. The write, with the action's policy:
//!    - create: `INSERT … SELECT … FROM (<candidate row>) WHERE <policy>`. The candidate row is
//!      the item plus the defaults the database would fill in (a fresh uuid `id`), with the
//!      columns' collations, so the policy sees the values inserted. No row: `Could not insert row
//!      with given values.` (`DatabasePolicyFailure`).
//!    - update: `UPDATE … WHERE id = ? AND <policy>`, on the row before the update. No row:
//!      `Could not find item with <id: …>` (`ItemNotFound`).
//!    - delete: the row is read first (it is the answer), then `DELETE … WHERE id = ? AND
//!      <policy>`. No row: `null` without an error.
//! 3. The row read back by its key with the read grant, policy and field permissions and the
//!    field's selection (relationships included), as `_by_pk` reads it: `null` when the read
//!    policy hides it. A role that may not read the entity has its write committed and gets DAB's
//!    `The mutation operation <field> was successful but …` error.
//!
//! Database errors, constraint violations included, roll the field back and answer DAB's
//! `DatabaseOperationFailed` error without `locations` (as DAB writes server errors), with
//! SQLite's constraint message as `extensions.detail`.

use std::collections::{HashMap, HashSet};

use async_graphql_parser::Pos;
use async_graphql_parser::types::{DocumentOperations, Field, OperationType, SelectionSet};
use indexmap::IndexMap;
use rl_db::{Db, codec};
use rl_model::{Action, ActionPermission, AppModel, Column, Entity};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value as Json};

use crate::access;
use crate::coerce::Input;
use crate::error::{GraphQlError, PathSegment, codes, messages};
use crate::exec::{self, Outcome, Request};
use crate::plan::{self, Collected, Ctx, ObjectPlan};
use crate::policy;
use crate::schema::{
    FieldDef, InputObjectDef, InputValueDef, Mutations, ObjectDef, Resolver, Schema, TypeDef,
    TypeRef,
};
use crate::sql::{CmpOp, Operand, Pred, Sql, quote_ident};
use crate::value::PKind;

/// The mutation root type.
pub const MUTATION: &str = "Mutation";
/// What [`Mutations::OperationResult`] mutations return, and its one field.
pub const DB_OPERATION_RESULT: &str = "DbOperationResult";
pub const DB_OPERATION_RESULT_FIELD: &str = "result";

pub(crate) const FIELDS_NOT_ALLOWED: &str =
    "Unauthorized due to one or more fields in this mutation.";
pub(crate) const INSERT_REFUSED: &str = "Could not insert row with given values.";

pub(crate) fn read_denied(field: &str) -> String {
    format!(
        "The mutation operation {field} was successful but the current user is unauthorized to \
         view the response due to lack of read permissions"
    )
}

/// Adds the `Mutation` type, and the input types it uses, for the entities some role may write.
/// [`Mutations::OperationResult`] fields return `DbOperationResult`; [`Mutations::None`] adds
/// none. Columns the database generates (identity, computed and server-managed columns of a
/// connector's source) are left out of the inputs, and those with a database default are optional
/// on create.
pub(crate) fn extend_schema(schema: &mut Schema, model: &AppModel) {
    let style = schema.options.mutations;
    if style == Mutations::None {
        return;
    }
    let returns = |e: &str| match style {
        Mutations::OperationResult => DB_OPERATION_RESULT.to_owned(),
        _ => e.to_owned(),
    };
    let mut fields = IndexMap::new();
    let mut inputs = Vec::new();
    for (index, entity) in model.entities.iter().enumerate() {
        let names = &entity.graphql;
        if schema.entity_types.get(&names.type_name) != Some(&index) {
            continue;
        }
        let Some(object) = schema.object(&names.type_name) else {
            continue;
        };
        // DAB derives the inputs from the fields of `E`: the columns some role may read.
        let columns: Vec<&Column> = object
            .fields
            .values()
            .filter_map(|field| match field.resolver {
                Resolver::Column(_, column) => Some(&entity.columns[column]),
                _ => None,
            })
            .filter(|column| !column.source.as_ref().is_some_and(|s| s.generated))
            .collect();
        let granted = |action| {
            [access::ANONYMOUS, access::AUTHENTICATED]
                .iter()
                .any(|role| access::grant(entity, role, action).is_some())
        };
        let keys = entity.key_columns();
        let e = &names.type_name;
        if granted(Action::Create) {
            let input = input_object(
                &names.create_input(),
                &format!("Input type for creating {e}"),
                &columns,
                |column| {
                    !column.nullable
                        && column.default.is_none()
                        && !codec::is_generated_key(column)
                        && !column.source.as_ref().is_some_and(|s| s.has_default)
                },
            );
            let item = argument(
                "item",
                &format!("Input representing all the fields for creating {e}"),
                TypeRef::named(&input.name).non_null(),
            );
            inputs.push(input);
            let field = mutation_field(
                &names.create_mutation,
                &format!("Creates a new {e}"),
                &returns(e),
                Resolver::Create(index),
                vec![item],
            );
            fields.insert(field.name.clone(), field);
        }
        if granted(Action::Update) && !keys.is_empty() {
            let input = input_object(
                &names.update_input(),
                &format!("Input type for updating {e}"),
                &columns,
                |_| false,
            );
            let item = argument(
                "item",
                &format!("Input representing all the fields for updating {e}"),
                TypeRef::named(&input.name).non_null(),
            );
            inputs.push(input);
            let mut args = key_arguments(entity, "updated");
            args.push(item);
            let field = mutation_field(
                &names.update_mutation,
                &format!("Updates a {e}"),
                &returns(e),
                Resolver::Update(index),
                args,
            );
            fields.insert(field.name.clone(), field);
        }
        if granted(Action::Delete) && !keys.is_empty() {
            let field = mutation_field(
                &names.delete_mutation,
                &format!("Delete a {e}"),
                &returns(e),
                Resolver::Delete(index),
                key_arguments(entity, "deleted"),
            );
            fields.insert(field.name.clone(), field);
        }
    }
    if fields.is_empty() {
        return;
    }
    for input in inputs {
        schema
            .types
            .insert(input.name.clone(), TypeDef::InputObject(input));
    }
    if style == Mutations::OperationResult {
        let result = FieldDef {
            name: DB_OPERATION_RESULT_FIELD.into(),
            description: Some("Contains result for mutation execution".into()),
            args: IndexMap::new(),
            ty: TypeRef::named("String"),
            resolver: Resolver::Extension,
        };
        let object = ObjectDef {
            name: DB_OPERATION_RESULT.into(),
            description: Some(
                "Represents the result of a mutation operation on the database.".into(),
            ),
            fields: IndexMap::from([(result.name.clone(), result)]),
        };
        schema
            .types
            .insert(DB_OPERATION_RESULT.into(), TypeDef::Object(object));
    }
    let mutation = ObjectDef {
        name: MUTATION.into(),
        description: None,
        fields,
    };
    schema
        .types
        .insert(MUTATION.into(), TypeDef::Object(mutation));
    schema.mutation_type = Some(MUTATION.into());
}

fn argument(name: &str, description: &str, ty: TypeRef) -> InputValueDef {
    InputValueDef {
        name: name.into(),
        description: Some(description.into()),
        ty,
        default: None,
        accepts_is_null: false,
    }
}

/// The primary key arguments: `id: UUID!` for data entities, one per key column for a
/// connector's.
fn key_arguments(entity: &Entity, done: &str) -> Vec<InputValueDef> {
    entity
        .key_columns()
        .into_iter()
        .map(|key| {
            argument(
                &key.field,
                &format!("One of the ids of the item being {done}"),
                TypeRef::named(key.kind.graphql_scalar()).non_null(),
            )
        })
        .collect()
}

fn mutation_field(
    name: &str,
    description: &str,
    type_name: &str,
    resolver: Resolver,
    args: Vec<InputValueDef>,
) -> FieldDef {
    FieldDef {
        name: name.into(),
        description: Some(description.into()),
        args: args
            .into_iter()
            .map(|arg| (arg.name.clone(), arg))
            .collect(),
        ty: TypeRef::named(type_name),
        resolver,
    }
}

/// `Create{E}Input`/`Update{E}Input`: one field per column, of the column's scalar.
fn input_object(
    name: &str,
    description: &str,
    columns: &[&Column],
    required: impl Fn(&Column) -> bool,
) -> InputObjectDef {
    let fields = columns
        .iter()
        .map(|column| {
            let scalar = TypeRef::named(column.kind.graphql_scalar());
            let ty = if required(column) {
                scalar.non_null()
            } else {
                scalar
            };
            let description = format!("Input for field {} on type {name}", column.field);
            (
                column.field.clone(),
                argument(&column.field, &description, ty),
            )
        })
        .collect();
    InputObjectDef {
        name: name.into(),
        description: Some(description.into()),
        fields,
    }
}

/// A mutation operation, planned: what the database thread runs and how to answer.
pub(crate) struct Planned {
    /// No token, and a field `anonymous` may not run but `authenticated` may (HTTP 401).
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
    /// Taken by the database thread.
    job: Option<Job>,
    /// The field's schema name (DAB's read-denied message names it).
    field: String,
    pos: Pos,
    action: Action,
    /// The update's `ItemNotFound` message.
    not_found: String,
    /// The plan of the row read back; `None` when the role may not read the entity.
    read: Option<ObjectPlan>,
    /// Why the row cannot be read back (the write is then undone).
    read_error: Option<GraphQlError>,
}

/// The statements of one write, run in one transaction.
struct Job {
    write: Sql,
    read: ReadJob,
}

enum ReadJob {
    /// The role may not read the entity.
    None,
    /// The row is read before the write (delete).
    Before(Sql),
    /// The row is read after the write (create, update).
    After(Sql),
    /// The row cannot be read back: the write is undone.
    Rollback,
}

/// How a write went.
enum Done {
    /// Applied and committed, with the row read back (`None`: hidden, or not read).
    Applied(Option<String>),
    /// No row was written: the key or the policy matched none (rolled back).
    NoRow,
    /// Applied, then undone because the row cannot be read back.
    RolledBack,
    /// A database error (rolled back), with the constraint violation's message if it is one.
    Failed(Option<String>),
}

/// Plans the root fields of a mutation (the `Mutation` type exists: validation checked it).
pub(crate) fn plan<'d>(ctx: &Ctx<'d>, selection: &'d SelectionSet) -> Planned {
    let fields = plan::collect_fields(ctx, MUTATION, &[selection]);
    let authentication_required = !ctx.caller.is_authenticated()
        && exec::requires_authentication(ctx.model, ctx.schema, MUTATION, &fields);
    let roots = if authentication_required {
        Vec::new()
    } else {
        fields.iter().map(|field| plan_root(ctx, field)).collect()
    };
    Planned {
        authentication_required,
        roots,
    }
}

fn plan_root(ctx: &Ctx<'_>, field: &Collected<'_>) -> Root {
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
    let write = match def.map(|def| (def, def.resolver)) {
        Some((def, Resolver::Create(index))) => plan_create(ctx, def, field, index),
        Some((def, Resolver::Update(index))) => plan_update(ctx, def, field, index),
        Some((def, Resolver::Delete(index))) => plan_delete(ctx, def, field, index),
        _ => Err(GraphQlError::dab(
            messages::DATABASE_ERROR,
            codes::UNEXPECTED_ERROR,
        )),
    };
    let kind = match write {
        Ok(write) => RootKind::Write(Box::new(write)),
        Err(error) => RootKind::Failed(located(error, field.pos(), &key)),
    };
    Root { key, kind }
}

fn plan_create(
    ctx: &Ctx<'_>,
    def: &FieldDef,
    field: &Collected<'_>,
    index: usize,
) -> Result<Write, GraphQlError> {
    let entity = &ctx.model.entities[index];
    let grant = granted(ctx, entity, Action::Create)?;
    let node = &field.fields[0].node;
    let args = ctx.arguments(def, node)?;
    let mut row: Vec<Option<SqlValue>> = vec![None; entity.columns.len()];
    for (column, value) in item_values(ctx, entity, grant, node, &args)? {
        row[column] = Some(value);
    }
    for (value, column) in row.iter_mut().zip(&entity.columns) {
        if value.is_none() {
            *value = codec::generated_default(column);
        }
    }
    let alias = ctx.alias();
    let policy = policy::action_filter(entity, ctx.caller, Action::Create, &alias)?;
    let inserted = entity
        .columns
        .iter()
        .zip(&row)
        .filter(|(_, value)| value.is_some())
        .map(|(column, _)| quote_ident(&column.name))
        .collect::<Vec<_>>()
        .join(", ");
    let mut sql = Sql::default();
    sql.push("INSERT INTO ")
        .push(&quote_ident(&entity.table))
        .push(" (")
        .push(&inserted)
        .push(") SELECT ")
        .push(&inserted)
        .push(" FROM (SELECT ");
    for (i, (column, value)) in entity.columns.iter().zip(&row).enumerate() {
        if i > 0 {
            sql.push(", ");
        }
        sql.param(value.clone().unwrap_or(SqlValue::Null));
        // The policy compares the candidate values as it compares the stored columns.
        if codec::is_textual(&column.kind) {
            sql.push(" COLLATE NOCASE");
        }
        sql.push(" AS ").push(&quote_ident(&column.name));
    }
    sql.push(") AS ").push(&alias).push(" WHERE ");
    policy.unwrap_or(Pred::Const(true)).render(&mut sql);
    let key = entity
        .columns
        .iter()
        .zip(&row)
        .find(|(column, _)| column.primary_key)
        .and_then(|(_, value)| value.clone())
        .unwrap_or(SqlValue::Null);
    Ok(write_then_read(
        ctx,
        field,
        index,
        Action::Create,
        sql,
        key,
        String::new(),
    ))
}

fn plan_update(
    ctx: &Ctx<'_>,
    def: &FieldDef,
    field: &Collected<'_>,
    index: usize,
) -> Result<Write, GraphQlError> {
    let entity = &ctx.model.entities[index];
    let grant = granted(ctx, entity, Action::Update)?;
    let node = &field.fields[0].node;
    let args = ctx.arguments(def, node)?;
    let item = item_values(ctx, entity, grant, node, &args)?;
    let (key_text, key) = key_value(entity, &args)?;
    let alias = ctx.alias();
    let policy = policy::action_filter(entity, ctx.caller, Action::Update, &alias)?;
    let pk = entity.primary_key_column();
    let mut sql = Sql::default();
    sql.push("UPDATE ")
        .push(&quote_ident(&entity.table))
        .push(" AS ")
        .push(&alias)
        .push(" SET ");
    if item.is_empty() {
        // Nothing to change, but the row must still be found.
        let pk_sql = quote_ident(&pk.name);
        sql.push(&format!("{pk_sql} = {pk_sql}"));
    }
    for (i, (column, value)) in item.iter().enumerate() {
        if i > 0 {
            sql.push(", ");
        }
        sql.push(&quote_ident(&entity.columns[*column].name))
            .push(" = ")
            .param(value.clone());
    }
    sql.push(" WHERE ");
    by_key(&alias, pk, key.clone(), policy).render(&mut sql);
    let new_key = item
        .iter()
        .find(|(column, _)| entity.columns[*column].primary_key)
        .map_or(key, |(_, value)| value.clone());
    let not_found = format!("Could not find item with <{}: {key_text}>", pk.field);
    Ok(write_then_read(
        ctx,
        field,
        index,
        Action::Update,
        sql,
        new_key,
        not_found,
    ))
}

fn plan_delete(
    ctx: &Ctx<'_>,
    def: &FieldDef,
    field: &Collected<'_>,
    index: usize,
) -> Result<Write, GraphQlError> {
    let entity = &ctx.model.entities[index];
    granted(ctx, entity, Action::Delete)?;
    let args = ctx.arguments(def, &field.fields[0].node)?;
    let (_, key) = key_value(entity, &args)?;
    // DAB reads the answer before deleting, so its errors come first.
    let (read, plan) = match read_back(ctx, field, index, key.clone())? {
        Some((sql, plan)) => (ReadJob::Before(sql), Some(plan)),
        None => (ReadJob::None, None),
    };
    let alias = ctx.alias();
    let policy = policy::action_filter(entity, ctx.caller, Action::Delete, &alias)?;
    let mut sql = Sql::default();
    sql.push("DELETE FROM ")
        .push(&quote_ident(&entity.table))
        .push(" AS ")
        .push(&alias)
        .push(" WHERE ");
    by_key(&alias, entity.primary_key_column(), key, policy).render(&mut sql);
    Ok(Write {
        job: Some(Job { write: sql, read }),
        field: field.name().to_owned(),
        pos: field.pos(),
        action: Action::Delete,
        not_found: String::new(),
        read: plan,
        read_error: None,
    })
}

/// A create or update, whose answer is the row with primary key `key` read after the write.
fn write_then_read(
    ctx: &Ctx<'_>,
    field: &Collected<'_>,
    index: usize,
    action: Action,
    write: Sql,
    key: SqlValue,
    not_found: String,
) -> Write {
    let (read, plan, read_error) = match read_back(ctx, field, index, key) {
        Ok(Some((sql, plan))) => (ReadJob::After(sql), Some(plan), None),
        Ok(None) => (ReadJob::None, None, None),
        Err(error) => (ReadJob::Rollback, None, Some(error)),
    };
    Write {
        job: Some(Job { write, read }),
        field: field.name().to_owned(),
        pos: field.pos(),
        action,
        not_found,
        read: plan,
        read_error,
    }
}

/// The role's grant for `action`, or Hot Chocolate's authorization error.
pub(crate) fn granted<'m>(
    ctx: &Ctx<'_>,
    entity: &'m Entity,
    action: Action,
) -> Result<&'m ActionPermission, GraphQlError> {
    access::grant(entity, ctx.caller.role(), action).ok_or_else(GraphQlError::not_authorized)
}

/// The `SELECT` of the row with primary key `key` as the role reads it, with the field's
/// selection; `None` when the role may not read the entity.
fn read_back(
    ctx: &Ctx<'_>,
    field: &Collected<'_>,
    index: usize,
    key: SqlValue,
) -> Result<Option<(Sql, ObjectPlan)>, GraphQlError> {
    let entity = &ctx.model.entities[index];
    let Some(grant) = access::grant(entity, ctx.caller.role(), Action::Read) else {
        return Ok(None);
    };
    let alias = ctx.alias();
    let policy = policy::read_filter(entity, ctx.caller, &alias)?;
    let plan = plan::object_plan(ctx, index, alias.clone(), &field.sub_sets(), Some(grant))?;
    let mut sql = Sql::default();
    sql.push("SELECT ");
    exec::object_json(&mut sql, ctx.model, &plan);
    sql.push(" FROM ")
        .push(&quote_ident(&entity.table))
        .push(" AS ")
        .push(&alias)
        .push(" WHERE ");
    by_key(&alias, entity.primary_key_column(), key, policy).render(&mut sql);
    Ok(Some((sql, plan)))
}

/// `alias.pk = key`, and the policy.
fn by_key(alias: &str, pk: &Column, key: SqlValue, policy: Option<Pred>) -> Pred {
    let key = Pred::Cmp(
        Operand::column(alias, &pk.name),
        CmpOp::Eq,
        Operand::Value(key),
    );
    Pred::all(std::iter::once(key).chain(policy))
}

/// The primary key argument: its text (for DAB's messages) and its stored value.
fn key_value(
    entity: &Entity,
    args: &HashMap<String, Input>,
) -> Result<(String, SqlValue), GraphQlError> {
    let pk = entity.primary_key_column();
    let Some(Input::Scalar(json)) = args.get(&pk.field) else {
        return Ok((String::new(), SqlValue::Null));
    };
    let value = codec::encode_operand(&pk.kind, json)
        .map_err(|e| GraphQlError::dab(e.to_string(), codes::BAD_REQUEST))?;
    let text = json
        .as_str()
        .map_or_else(|| json.to_string(), str::to_owned);
    Ok((text, value))
}

/// The `item` argument as (column index, stored value) pairs, in document order, once the grant
/// is known to allow every field.
pub(crate) fn item_values(
    ctx: &Ctx<'_>,
    entity: &Entity,
    grant: &ActionPermission,
    field: &Field,
    args: &HashMap<String, Input>,
) -> Result<Vec<(usize, SqlValue)>, GraphQlError> {
    let Some(item) = args.get("item").and_then(Input::as_object) else {
        return Ok(Vec::new());
    };
    let unset = unset_variables(ctx, field);
    let item: Vec<_> = item
        .iter()
        .filter(|(name, ..)| !unset.contains(name))
        .collect();
    if !item
        .iter()
        .all(|(name, ..)| access::allows_field(grant, name))
    {
        return Err(GraphQlError::dab(
            FIELDS_NOT_ALLOWED,
            codes::AUTHORIZATION_CHECK_FAILED,
        ));
    }
    item.into_iter()
        .filter_map(|(name, input, pos)| {
            let index = entity.columns.iter().position(|c| c.field == *name)?;
            let value = match input {
                Input::Scalar(json) => codec::encode(&entity.columns[index].kind, json)
                    .map_err(|e| GraphQlError::dab(e.to_string(), codes::BAD_REQUEST).at(*pos)),
                _ => Ok(SqlValue::Null),
            };
            Some(value.map(|value| (index, value)))
        })
        .collect()
}

/// The `item` fields given as variables that the request does not provide: Hot Chocolate leaves
/// them out of the input object (so an update does not set them to `null`).
fn unset_variables(ctx: &Ctx<'_>, field: &Field) -> HashSet<String> {
    let Some((_, value)) = field
        .arguments
        .iter()
        .find(|(name, _)| name.node.as_str() == "item")
    else {
        return HashSet::new();
    };
    let Ok(literal) = ctx.source.value_at(value.pos) else {
        return HashSet::new();
    };
    let PKind::Object(fields) = literal.kind else {
        return HashSet::new();
    };
    fields
        .into_iter()
        .filter(|f| matches!(&f.value.kind, PKind::Variable(v) if !ctx.variables.contains_key(v)))
        .map(|f| f.name)
        .collect()
}

/// Whether `request` runs a mutation, which Hot Chocolate refuses over `GET`. Documents that do
/// not parse, or name no single operation, are left to the executor's errors.
pub(crate) fn selects_mutation(request: &Request) -> bool {
    if exec::nesting_depth(&request.query) > exec::MAX_DOCUMENT_DEPTH {
        return false;
    }
    let Ok(doc) = async_graphql_parser::parse_query(&request.query) else {
        return false;
    };
    let op = match (&doc.operations, request.operation_name.as_deref()) {
        (DocumentOperations::Single(op), _) => Some(&op.node),
        (DocumentOperations::Multiple(ops), Some(name)) => ops.get(name).map(|op| &op.node),
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => {
            ops.values().next().map(|op| &op.node)
        }
        (DocumentOperations::Multiple(_), None) => None,
    };
    op.is_some_and(|op| op.ty == OperationType::Mutation)
}

/// Runs a planned mutation: the writes in order, each in its own transaction, then the answer.
pub(crate) async fn execute(model: &AppModel, db: &Db, mut planned: Planned) -> Outcome {
    if planned.authentication_required {
        return Outcome::AuthenticationRequired;
    }
    let jobs: Vec<Job> = planned
        .roots
        .iter_mut()
        .filter_map(|root| match &mut root.kind {
            RootKind::Write(write) => write.job.take(),
            _ => None,
        })
        .collect();
    let done = if jobs.is_empty() {
        Ok(Vec::new())
    } else {
        db.call(move |conn| jobs.iter().map(|job| run(conn, job)).collect::<Vec<_>>())
            .await
    };
    let mut done = match done {
        Ok(done) => done.into_iter(),
        Err(error) => {
            tracing::warn!("the data API could not reach the database: {error}");
            Vec::new().into_iter()
        }
    };
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
                let done = done.next().unwrap_or(Done::Failed(None));
                write.answer(model, &root.key, done, &mut errors)
            }
        };
        data.insert(root.key, value);
    }
    exec::response(Some(Json::Object(data)), errors)
}

fn run(conn: &mut Connection, job: &Job) -> Done {
    match run_in_transaction(conn, job) {
        Ok(done) => done,
        Err(error) => {
            let violation = codec::constraint_violation(&error);
            match &violation {
                Some(violation) => tracing::debug!("a data API write was refused: {violation:?}"),
                None => tracing::warn!("a data API write failed: {error}"),
            }
            Done::Failed(violation.map(|violation| violation.message))
        }
    }
}

fn run_in_transaction(conn: &mut Connection, job: &Job) -> rusqlite::Result<Done> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let before = match &job.read {
        ReadJob::Before(sql) => read_row(&tx, sql)?,
        _ => None,
    };
    let changed = tx
        .prepare_cached(&job.write.text)?
        .execute(rusqlite::params_from_iter(job.write.params.iter()))?;
    if changed == 0 {
        return Ok(Done::NoRow);
    }
    let row = match &job.read {
        ReadJob::After(sql) => read_row(&tx, sql)?,
        ReadJob::Rollback => return Ok(Done::RolledBack),
        ReadJob::None | ReadJob::Before(_) => before,
    };
    tx.commit()?;
    Ok(Done::Applied(row))
}

fn read_row(conn: &Connection, sql: &Sql) -> rusqlite::Result<Option<String>> {
    conn.prepare_cached(&sql.text)?
        .query_row(rusqlite::params_from_iter(sql.params.iter()), |row| {
            row.get(0)
        })
        .optional()
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
            Done::Failed(detail) => {
                let error =
                    GraphQlError::dab(codec::DATABASE_ERROR_MESSAGE, codec::DATABASE_ERROR_CODE);
                match detail {
                    Some(detail) => error.ext("detail", detail),
                    None => error,
                }
            }
            Done::RolledBack => self.read_error.unwrap_or_else(|| {
                GraphQlError::dab(messages::DATABASE_ERROR, codes::UNEXPECTED_ERROR)
            }),
            Done::NoRow if self.action == Action::Create => {
                GraphQlError::dab(INSERT_REFUSED, codes::DATABASE_POLICY_FAILURE)
            }
            Done::NoRow if self.action == Action::Update => {
                GraphQlError::dab(self.not_found, codes::ITEM_NOT_FOUND)
            }
            // DAB reports a role that may not read once the transaction is over, rows or not.
            Done::NoRow | Done::Applied(_) if self.read.is_none() => {
                GraphQlError::dab(read_denied(&self.field), codes::AUTHORIZATION_CHECK_FAILED)
            }
            Done::NoRow | Done::Applied(None) => return Json::Null,
            Done::Applied(Some(row)) => {
                let Some(plan) = &self.read else {
                    return Json::Null;
                };
                return exec::shape_object(model, plan, &row, key, errors).unwrap_or(Json::Null);
            }
        };
        errors.push(located(error, self.pos, key));
        Json::Null
    }
}

/// `error` at root field `key`, located at the field unless it is a server error (DAB writes
/// those without `locations`) or already located.
pub(crate) fn located(error: GraphQlError, pos: Pos, key: &str) -> GraphQlError {
    let code = error.extensions.get("code").and_then(Json::as_str);
    let server = matches!(
        code,
        Some(codes::DATABASE_OPERATION_FAILED | codes::UNEXPECTED_ERROR)
    );
    let error = if server || !error.locations.is_empty() {
        error
    } else {
        error.at(pos)
    };
    error.with_path(vec![PathSegment::Key(key.to_owned())])
}
