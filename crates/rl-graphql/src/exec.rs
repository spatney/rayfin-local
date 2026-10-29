//! Request execution: parse, validate, plan, run one statement per root field, and shape the
//! response as Hot Chocolate does (errors with paths, `null` propagated to the nearest nullable
//! field).
//!
//! Each root field becomes one parameterized SQLite statement that builds its JSON:
//! `json_object` per row (column values through `rl_db::codec::json_expr`), a correlated
//! subquery per to-one relationship, and `json_group_array` over an ordered, limited subquery per
//! to-many relationship (none for a role that may not read the target: the field is then one
//! `AUTH_NOT_AUTHORIZED`, as a root field is). Connection rows are `json_array(item, ...,
//! cursor)`: one object per `items` selection and the row's order-key values when `endCursor`
//! is selected and the role may read every order key (else `endCursor` is `null` and no `after`
//! is valid); the shaper trims the probe row and computes `hasNextPage` and `endCursor`.

use std::collections::HashMap;
use std::sync::Arc;

use async_graphql_parser::Pos;
use async_graphql_parser::types::{DocumentOperations, OperationType};
use rl_db::{Db, codec};
use rl_model::{Action, AppModel, Caller, FieldKind};
use serde_json::{Map, Value as Json, json};

use crate::access;
use crate::cursor::{self, CursorField};
use crate::error::{GraphQlError, PathSegment, codes, errors_body, messages};
use crate::plan::{
    self, Collected, ConnectionField, ConnectionPlan, Ctx, FieldPlan, ObjectPlan, collect_fields,
};
use crate::schema::{Resolver, Schema, TypeRef};
use crate::sql::{CmpOp, Operand, Pred, Sql, column_sql, quote_ident};
use crate::value::Source;
use crate::{DataApiSettings, Snapshot};

/// A GraphQL request (`POST` body or `GET` query string).
#[derive(Clone, Debug, Default)]
pub struct Request {
    pub query: String,
    pub variables: Map<String, Json>,
    pub operation_name: Option<String>,
}

/// What to answer.
#[derive(Debug)]
pub enum Outcome {
    /// A GraphQL response body and its HTTP status (200, or 400 as Hot Chocolate/DAB use it).
    Graphql { status: u16, body: Json },
    /// No token, and an operation `anonymous` may not run but `authenticated` may: 401, so a
    /// restored session refreshes and replays (micro-fabric compatibility.md §7.6).
    AuthenticationRequired,
}

/// Documents nested deeper than this are refused before parsing (the parser recurses).
pub(crate) const MAX_DOCUMENT_DEPTH: usize = 200;

/// Runs a request for `caller` against the model snapshot.
pub async fn execute(
    snapshot: Arc<Snapshot>,
    db: &Db,
    caller: &Caller,
    settings: &DataApiSettings,
    request: Request,
) -> Outcome {
    if nesting_depth(&request.query) > MAX_DOCUMENT_DEPTH {
        return syntax_error("The GraphQL document is nested too deeply.", Pos::default());
    }
    let doc = match async_graphql_parser::parse_query(&request.query) {
        Ok(doc) => doc,
        Err(error) => {
            let pos = error.positions().next().unwrap_or_default();
            let message = match &error {
                async_graphql_parser::Error::Syntax { message, .. } => message
                    .lines()
                    .last()
                    .unwrap_or(message)
                    .trim_start_matches("= ")
                    .to_owned(),
                other => other.to_string(),
            };
            return syntax_error(&message, pos);
        }
    };
    let source = Source::new(&request.query);
    let schema = &snapshot.schema;
    let errors = crate::validate::validate(schema, &doc, &source, settings.allow_introspection);
    if !errors.is_empty() {
        return response(None, errors);
    }
    let op = match select_operation(&doc, request.operation_name.as_deref()) {
        Ok(op) => op,
        Err(error) => return response(None, vec![error]),
    };
    let variables = match crate::validate::coerce_variables(schema, op, &source, &request.variables)
    {
        Ok(variables) => variables,
        Err(error) => return response(None, vec![error]),
    };
    let var_types: HashMap<String, (TypeRef, bool)> = op
        .variable_definitions
        .iter()
        .map(|d| {
            let ty = TypeRef::from_parsed(&d.node.var_type.node);
            (
                d.node.name.node.to_string(),
                (ty, d.node.default_value.is_some()),
            )
        })
        .collect();

    let model = &snapshot.model;
    // Validation refused operations whose root type the schema lacks (subscriptions).
    if op.ty == OperationType::Mutation {
        let planned = {
            let ctx = Ctx::new(
                model, schema, caller, settings, &doc, &source, &variables, &var_types,
            );
            crate::mutation::plan(&ctx, &op.selection_set.node)
        };
        return crate::mutation::execute(model, db, planned).await;
    }
    let roots = {
        let ctx = Ctx::new(
            model, schema, caller, settings, &doc, &source, &variables, &var_types,
        );
        let fields = collect_fields(&ctx, &schema.query_type, &[&op.selection_set.node]);
        if !caller.is_authenticated()
            && requires_authentication(model, schema, &schema.query_type, &fields)
        {
            return Outcome::AuthenticationRequired;
        }
        fields
            .iter()
            .map(|field| plan_root(&ctx, field))
            .collect::<Vec<_>>()
    };

    let statements: Vec<Sql> = roots.iter().filter_map(|r| r.sql.clone()).collect();
    let results = if statements.is_empty() {
        Ok(Vec::new())
    } else {
        db.call(move |conn| {
            statements
                .iter()
                .map(|sql| run(conn, sql))
                .collect::<Vec<_>>()
        })
        .await
    };
    let mut results = match results {
        Ok(results) => results.into_iter(),
        Err(error) => {
            tracing::warn!("the data API could not reach the database: {error}");
            Vec::new().into_iter()
        }
    };

    let mut shaper = Shaper {
        model,
        errors: Vec::new(),
    };
    let mut data = Map::new();
    let mut data_null = false;
    for root in roots {
        let path = PathSegment::Key(root.key.clone());
        let rows = match &root.sql {
            Some(_) => match results.next() {
                Some(Ok(rows)) => Ok(rows),
                Some(Err(error)) => Err(error),
                None => Err("the database is unavailable".to_owned()),
            },
            None => Ok(Vec::new()),
        };
        let value = match (root.kind, rows) {
            (RootKind::Typename, _) => Some(json!("Query")),
            (RootKind::Failed(error), _) => {
                shaper.errors.push(error.at(root.pos).with_path(vec![path]));
                None
            }
            (_, Err(error)) => {
                tracing::warn!(field = %root.key, "data API statement failed: {error}");
                let error = GraphQlError::dab(
                    codec::DATABASE_ERROR_MESSAGE,
                    codes::DATABASE_OPERATION_FAILED,
                );
                shaper.errors.push(error.with_path(vec![path]));
                None
            }
            (RootKind::List(plan), Ok(rows)) => {
                let rows: Vec<Json> = rows.iter().map(|r| parse_row(r)).collect();
                shaper.connection(&plan, &rows, &mut vec![path])
            }
            (RootKind::ByPk(plan), Ok(rows)) => match rows.first() {
                None => Some(Json::Null),
                Some(row) => shaper.object(&plan, &parse_row(row), &mut vec![path]),
            },
        };
        match value {
            Some(value) => {
                data.insert(root.key, value);
            }
            None if root.nullable => {
                data.insert(root.key, Json::Null);
            }
            None => data_null = true,
        }
    }
    let data = if data_null {
        Json::Null
    } else {
        Json::Object(data)
    };
    response(Some(data), shaper.errors)
}

fn syntax_error(message: &str, pos: Pos) -> Outcome {
    let error = GraphQlError::new(message).at(pos).code(codes::SYNTAX_ERROR);
    Outcome::Graphql {
        status: 400,
        body: errors_body(&[error]),
    }
}

/// The response body (`errors` first, as Hot Chocolate writes it); 400 when DAB's status-code
/// middleware would set it (a `DatabaseInputError`).
pub(crate) fn response(data: Option<Json>, errors: Vec<GraphQlError>) -> Outcome {
    let status = if errors
        .iter()
        .any(|e| e.extensions.get("code") == Some(&json!(codes::DATABASE_INPUT_ERROR)))
    {
        400
    } else {
        200
    };
    let mut body = Map::new();
    if !errors.is_empty() {
        body.insert(
            "errors".into(),
            Json::Array(errors.iter().map(GraphQlError::to_json).collect()),
        );
    }
    if let Some(data) = data {
        body.insert("data".into(), data);
    }
    Outcome::Graphql {
        status,
        body: Json::Object(body),
    }
}

fn select_operation<'d>(
    doc: &'d async_graphql_parser::types::ExecutableDocument,
    name: Option<&str>,
) -> Result<&'d async_graphql_parser::types::OperationDefinition, GraphQlError> {
    match (&doc.operations, name) {
        (DocumentOperations::Single(op), None) => Ok(&op.node),
        (DocumentOperations::Single(op), Some(_)) => Ok(&op.node),
        (DocumentOperations::Multiple(ops), Some(name)) => {
            ops.get(name).map(|op| &op.node).ok_or_else(|| {
                GraphQlError::new(format!("The specified operation `{name}` cannot be found."))
            })
        }
        (DocumentOperations::Multiple(ops), None) if ops.len() == 1 => {
            Ok(&ops.values().next().unwrap().node)
        }
        (DocumentOperations::Multiple(_), None) => Err(GraphQlError::new(
            "The operation name can only be omitted if there is only one operation in a GraphQL \
             document.",
        )),
    }
}

/// The deepest `{`/`[`/`(` nesting outside strings and comments.
pub(crate) fn nesting_depth(query: &str) -> usize {
    let (mut depth, mut max) = (0usize, 0usize);
    let mut chars = query.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '#' => {
                while chars.peek().is_some_and(|c| *c != '\n' && *c != '\r') {
                    chars.next();
                }
            }
            '"' => {
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => {
                            chars.next();
                        }
                        '"' => break,
                        _ => {}
                    }
                }
            }
            '{' | '[' | '(' => {
                depth += 1;
                max = max.max(depth);
            }
            '}' | ']' | ')' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    max
}

/// Whether a token-less request runs a root field of type `root` (an entity action) that
/// `anonymous` may not run but `authenticated` may.
pub(crate) fn requires_authentication(
    model: &AppModel,
    schema: &Schema,
    root: &str,
    fields: &[Collected<'_>],
) -> bool {
    let Some(root) = schema.object(root) else {
        return false;
    };
    fields.iter().any(|field| {
        let (index, action) = match root.fields.get(field.name()).map(|f| f.resolver) {
            Some(Resolver::List(index) | Resolver::ByPk(index)) => (index, Action::Read),
            Some(Resolver::Create(index)) => (index, Action::Create),
            Some(Resolver::Update(index)) => (index, Action::Update),
            Some(Resolver::Delete(index)) => (index, Action::Delete),
            _ => return false,
        };
        let entity = &model.entities[index];
        access::grant(entity, access::ANONYMOUS, action).is_none()
            && access::grant(entity, access::AUTHENTICATED, action).is_some()
    })
}

struct Root {
    key: String,
    pos: Pos,
    nullable: bool,
    kind: RootKind,
    sql: Option<Sql>,
}

enum RootKind {
    Typename,
    List(ConnectionPlan),
    ByPk(ObjectPlan),
    Failed(GraphQlError),
}

fn plan_root(ctx: &Ctx<'_>, field: &Collected<'_>) -> Root {
    let mut root = Root {
        key: field.key.clone(),
        pos: field.pos(),
        nullable: true,
        kind: RootKind::Typename,
        sql: None,
    };
    if field.name() == "__typename" {
        return root;
    }
    let def = ctx
        .schema
        .object(&ctx.schema.query_type)
        .and_then(|q| q.fields.get(field.name()));
    let Some(def) = def else {
        root.kind = RootKind::Failed(GraphQlError::new(
            "Introspection is not supported by rayfin-local; set allowIntrospection to false.",
        ));
        return root;
    };
    root.nullable = !def.ty.is_non_null();
    let planned = match def.resolver {
        Resolver::List(index) => {
            plan_list(ctx, def, field, index).map(|(plan, sql)| (RootKind::List(plan), sql))
        }
        Resolver::ByPk(index) => {
            plan_by_pk(ctx, def, field, index).map(|(plan, sql)| (RootKind::ByPk(plan), sql))
        }
        _ => Err(GraphQlError::dab(
            messages::DATABASE_ERROR,
            codes::UNEXPECTED_ERROR,
        )),
    };
    match planned {
        Ok((kind, sql)) => {
            root.kind = kind;
            root.sql = Some(sql);
        }
        Err(error) => root.kind = RootKind::Failed(error),
    }
    root
}

fn plan_list(
    ctx: &Ctx<'_>,
    def: &crate::schema::FieldDef,
    field: &Collected<'_>,
    index: usize,
) -> Result<(ConnectionPlan, Sql), GraphQlError> {
    let entity = &ctx.model.entities[index];
    let Some(grant) = access::grant(entity, ctx.caller.role(), Action::Read) else {
        return Err(GraphQlError::not_authorized());
    };
    let args = ctx.arguments(def, &field.fields[0].node)?;
    let plan = plan::connection_plan(ctx, index, grant, &field.sub_sets(), &args, None)?;
    let mut sql = Sql::default();
    sql.push("SELECT ");
    if crate::aggregate::is_group_by(&plan) {
        crate::aggregate::connection_sql(&mut sql, ctx.model, &plan);
        return Ok((plan, sql));
    }
    item_array(&mut sql, ctx.model, &plan);
    from_where_order(&mut sql, ctx.model, &plan);
    Ok((plan, sql))
}

fn plan_by_pk(
    ctx: &Ctx<'_>,
    def: &crate::schema::FieldDef,
    field: &Collected<'_>,
    index: usize,
) -> Result<(ObjectPlan, Sql), GraphQlError> {
    let entity = &ctx.model.entities[index];
    let Some(grant) = access::grant(entity, ctx.caller.role(), Action::Read) else {
        return Err(GraphQlError::not_authorized());
    };
    let args = ctx.arguments(def, &field.fields[0].node)?;
    let pk = entity.primary_key_column();
    let alias = ctx.alias();
    let id = match args.get(&pk.field) {
        Some(crate::coerce::Input::Scalar(json)) => codec::encode_operand(&pk.kind, json)
            .map_err(|e| GraphQlError::dab(e.to_string(), codes::BAD_REQUEST))?,
        _ => rusqlite::types::Value::Null,
    };
    let mut preds = vec![Pred::Cmp(
        Operand::column(&alias, &pk.name),
        CmpOp::Eq,
        Operand::Value(id),
    )];
    if let Some(policy) = crate::policy::read_filter(entity, ctx.caller, &alias)? {
        preds.push(policy);
    }
    let plan = plan::object_plan(ctx, index, alias.clone(), &field.sub_sets(), Some(grant))?;
    let mut sql = Sql::default();
    sql.push("SELECT ");
    object_json(&mut sql, ctx.model, &plan);
    sql.push(" FROM ")
        .push(&quote_ident(&entity.table))
        .push(" AS ")
        .push(&alias)
        .push(" WHERE ");
    Pred::all(preds).render(&mut sql);
    Ok((plan, sql))
}

/// `json_object('key', value, ...)` of an object selection.
pub(crate) fn object_json(sql: &mut Sql, model: &AppModel, plan: &ObjectPlan) {
    let entity = &model.entities[plan.entity];
    sql.push("json_object(");
    let mut first = true;
    let mut key = |sql: &mut Sql, key: &str| {
        if !std::mem::take(&mut first) {
            sql.push(", ");
        }
        sql.push(&rl_db::schema::quote_literal(key)).push(", ");
    };
    for field in &plan.fields {
        match field {
            FieldPlan::Column {
                key: name, column, ..
            } => {
                let column = &entity.columns[*column];
                key(sql, name);
                sql.push(&codec::json_expr(
                    &column.kind,
                    &column_sql(&plan.alias, &column.name),
                ));
            }
            FieldPlan::ToOne {
                key: name,
                child,
                predicate,
                ..
            } => {
                key(sql, name);
                sql.push("json((SELECT ");
                object_json(sql, model, child);
                sql.push(" FROM ")
                    .push(&quote_ident(&model.entities[child.entity].table))
                    .push(" AS ")
                    .push(&child.alias)
                    .push(" WHERE ");
                predicate.render(sql);
                sql.push("))");
            }
            FieldPlan::ToMany {
                key: name,
                connection,
                ..
            } => {
                key(sql, name);
                if crate::aggregate::is_group_by(connection) {
                    // The connection's one row (groups), as the rows array the shaper reads.
                    sql.push("json_array(");
                    crate::aggregate::connection_sql(sql, model, connection);
                    sql.push(")");
                    continue;
                }
                sql.push("json((SELECT json_group_array(json(x.j) ORDER BY x.n) FROM (SELECT ");
                item_array(sql, model, connection);
                sql.push(" AS j, row_number() OVER (ORDER BY ");
                order_by(sql, model, connection);
                sql.push(") AS n");
                from_where_order(sql, model, connection);
                sql.push(") AS x))");
            }
            FieldPlan::Typename { .. } | FieldPlan::Forbidden { .. } => {}
        }
    }
    sql.push(")");
}

/// `json_array(<item selection>..., <order-key values or NULL>)` of a connection row.
fn item_array(sql: &mut Sql, model: &AppModel, plan: &ConnectionPlan) {
    let entity = &model.entities[plan.entity];
    sql.push("json_array(");
    for item in &plan.items {
        object_json(sql, model, item);
        sql.push(", ");
    }
    if plan.cursor {
        sql.push("json_array(");
        for (i, key) in plan.order.iter().enumerate() {
            if i > 0 {
                sql.push(", ");
            }
            let column = &entity.columns[key.column];
            let column_sql = column_sql(&plan.alias, &column.name);
            // Dates and decimals keep their stored precision, so the cursor row compares equal.
            sql.push(&match column.kind {
                FieldKind::Boolean => codec::json_expr(&column.kind, &column_sql),
                _ => column_sql,
            });
        }
        sql.push(")");
    } else {
        sql.push("NULL");
    }
    sql.push(")");
}

fn order_by(sql: &mut Sql, model: &AppModel, plan: &ConnectionPlan) {
    let entity = &model.entities[plan.entity];
    for (i, key) in plan.order.iter().enumerate() {
        if i > 0 {
            sql.push(", ");
        }
        sql.push(&column_sql(&plan.alias, &entity.columns[key.column].name))
            .push(if key.desc { " DESC" } else { " ASC" });
    }
}

fn from_where_order(sql: &mut Sql, model: &AppModel, plan: &ConnectionPlan) {
    let entity = &model.entities[plan.entity];
    sql.push(" FROM ")
        .push(&quote_ident(&entity.table))
        .push(" AS ")
        .push(&plan.alias)
        .push(" WHERE ");
    plan.predicate.render(sql);
    sql.push(" ORDER BY ");
    order_by(sql, model, plan);
    let limit = plan.limit + u64::from(plan.probe);
    sql.push(&format!(" LIMIT {limit}"));
}

fn run(conn: &mut rusqlite::Connection, sql: &Sql) -> Result<Vec<String>, String> {
    let mut statement = conn.prepare_cached(&sql.text).map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(sql.params.iter()), |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| e.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn parse_row(row: &str) -> Json {
    serde_json::from_str(row).unwrap_or(Json::Null)
}

/// The object `row` of root field `key` (a mutation's row read back), adding its errors; `None`
/// when a non-null field of it is `null`.
pub(crate) fn shape_object(
    model: &AppModel,
    plan: &ObjectPlan,
    row: &str,
    key: &str,
    errors: &mut Vec<GraphQlError>,
) -> Option<Json> {
    let mut shaper = Shaper {
        model,
        errors: std::mem::take(errors),
    };
    let mut path = vec![PathSegment::Key(key.to_owned())];
    let value = shaper.object(plan, &parse_row(row), &mut path);
    *errors = shaper.errors;
    value
}

/// Turns raw rows into the response, with Hot Chocolate's errors and `null` propagation.
struct Shaper<'a> {
    model: &'a AppModel,
    errors: Vec<GraphQlError>,
}

impl Shaper<'_> {
    fn error_at(&mut self, error: GraphQlError, path: &[PathSegment], key: &str, pos: Pos) {
        let mut path = path.to_vec();
        path.push(PathSegment::Key(key.to_owned()));
        self.errors.push(error.at(pos).with_path(path));
    }

    fn non_null(&mut self, path: &[PathSegment], key: &str, pos: Pos) {
        let error = GraphQlError::dab(messages::NON_NULL, codes::NON_NULL_VIOLATION);
        self.error_at(error, path, key, pos);
    }

    /// The object, or `None` when a non-null field of it is `null` (it becomes `null` itself).
    fn object(
        &mut self,
        plan: &ObjectPlan,
        raw: &Json,
        path: &mut Vec<PathSegment>,
    ) -> Option<Json> {
        let mut out = Map::new();
        let mut complete = true;
        for field in &plan.fields {
            match field {
                FieldPlan::Typename { key, name } => {
                    out.insert(key.clone(), Json::String(name.clone()));
                }
                FieldPlan::Column {
                    key, non_null, pos, ..
                } => {
                    let value = raw.get(key).cloned().unwrap_or(Json::Null);
                    if value.is_null() && *non_null {
                        self.non_null(path, key, *pos);
                        complete = false;
                    }
                    out.insert(key.clone(), value);
                }
                FieldPlan::Forbidden { key, non_null, pos } => {
                    self.error_at(GraphQlError::not_authorized(), path, key, *pos);
                    complete &= !non_null;
                    out.insert(key.clone(), Json::Null);
                }
                FieldPlan::ToOne {
                    key,
                    non_null,
                    child,
                    pos,
                    ..
                } => {
                    let value = match raw.get(key).filter(|v| !v.is_null()) {
                        None => {
                            if *non_null {
                                self.non_null(path, key, *pos);
                            }
                            None
                        }
                        Some(object) => {
                            path.push(PathSegment::Key(key.clone()));
                            let value = self.object(child, object, path);
                            path.pop();
                            value
                        }
                    };
                    complete &= value.is_some() || !non_null;
                    out.insert(key.clone(), value.unwrap_or(Json::Null));
                }
                FieldPlan::ToMany {
                    key, connection, ..
                } => {
                    let rows = raw
                        .get(key)
                        .and_then(Json::as_array)
                        .cloned()
                        .unwrap_or_default();
                    path.push(PathSegment::Key(key.clone()));
                    let value = self.connection(connection, &rows, path);
                    path.pop();
                    out.insert(key.clone(), value.unwrap_or(Json::Null));
                }
            }
        }
        complete.then_some(Json::Object(out))
    }

    fn connection(
        &mut self,
        plan: &ConnectionPlan,
        rows: &[Json],
        path: &mut Vec<PathSegment>,
    ) -> Option<Json> {
        if crate::aggregate::is_group_by(plan) {
            return Some(crate::aggregate::shape(self.model, plan, rows));
        }
        let limit = usize::try_from(plan.limit).unwrap_or(usize::MAX);
        let more = rows.len() > limit;
        let rows = &rows[..rows.len().min(limit)];
        let mut out = Map::new();
        let mut complete = true;
        for field in &plan.fields {
            match field {
                ConnectionField::Items { key, index } => {
                    path.push(PathSegment::Key(key.clone()));
                    let mut items = Vec::with_capacity(rows.len());
                    let mut all = true;
                    for (i, row) in rows.iter().enumerate() {
                        path.push(PathSegment::Index(i));
                        let raw = row.get(*index).cloned().unwrap_or(Json::Null);
                        match self.object(&plan.items[*index], &raw, path) {
                            Some(item) => items.push(item),
                            None => all = false,
                        }
                        path.pop();
                    }
                    path.pop();
                    complete &= all;
                    out.insert(
                        key.clone(),
                        if all { Json::Array(items) } else { Json::Null },
                    );
                }
                ConnectionField::HasNextPage { key } => {
                    out.insert(key.clone(), Json::Bool(more));
                }
                ConnectionField::EndCursor { key } => {
                    let cursor = rows
                        .last()
                        .filter(|_| more && plan.cursor)
                        .map(|row| self.cursor(plan, row));
                    out.insert(key.clone(), cursor.map_or(Json::Null, Json::String));
                }
                ConnectionField::Typename { key } => {
                    out.insert(key.clone(), Json::String(plan.type_name.clone()));
                }
                ConnectionField::GroupBy { .. } => {}
            }
        }
        complete.then_some(Json::Object(out))
    }

    fn cursor(&self, plan: &ConnectionPlan, row: &Json) -> String {
        let entity = &self.model.entities[plan.entity];
        let values = row
            .get(plan.items.len())
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let fields: Vec<CursorField> = plan
            .order
            .iter()
            .zip(values)
            .map(|(key, value)| CursorField {
                field: entity.columns[key.column].field.clone(),
                value,
                direction: i64::from(key.desc),
            })
            .collect();
        cursor::encode(&entity.name, &fields)
    }
}
