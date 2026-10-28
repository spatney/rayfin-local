//! Planning: a validated root field becomes a plan of what to select, filter, order and page,
//! with authorization decided per field (DAB's `SqlQueryStructure`, `GQLFilterParser`,
//! `ProcessGqlOrderByArg` and `SqlPaginationUtil`, plus Hot Chocolate's `@authorize`).
//!
//! The SQL is built from the plan by [`crate::exec`]; errors carry DAB's messages and codes.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use async_graphql_parser::types::{ExecutableDocument, Field, Selection, SelectionSet};
use async_graphql_parser::{Pos, Positioned};
use rl_db::codec;
use rl_model::{Action, ActionPermission, AppModel, Caller, Entity};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value as Json};

use crate::DataApiSettings;
use crate::access;
use crate::coerce::{self, Input, Variables};
use crate::cursor;
use crate::error::{GraphQlError, codes, messages};
use crate::policy;
use crate::schema::{Resolver, Schema, TypeRef};
use crate::sql::{CmpOp, Operand, Pred, escape_like};
use crate::value::{PKind, Source};

/// Relationship filters nested deeper than this fail (DAB `MAX_NESTED_FILTER_DEPTH`).
const MAX_FILTER_DEPTH: usize = 20;

/// What the planner needs for one request.
pub struct Ctx<'a> {
    pub model: &'a AppModel,
    pub schema: &'a Schema,
    pub caller: &'a Caller,
    pub settings: &'a DataApiSettings,
    pub doc: &'a ExecutableDocument,
    pub source: &'a Source<'a>,
    pub variables: &'a Map<String, Json>,
    pub var_types: &'a HashMap<String, (TypeRef, bool)>,
    aliases: Cell<usize>,
}

impl<'a> Ctx<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: &'a AppModel,
        schema: &'a Schema,
        caller: &'a Caller,
        settings: &'a DataApiSettings,
        doc: &'a ExecutableDocument,
        source: &'a Source<'a>,
        variables: &'a Map<String, Json>,
        var_types: &'a HashMap<String, (TypeRef, bool)>,
    ) -> Self {
        Self {
            model,
            schema,
            caller,
            settings,
            doc,
            source,
            variables,
            var_types,
            aliases: Cell::new(0),
        }
    }

    /// A fresh table alias (`t0`, `t1`, ...).
    pub fn alias(&self) -> String {
        let n = self.aliases.get();
        self.aliases.set(n + 1);
        format!("t{n}")
    }

    fn role(&self) -> &'static str {
        self.caller.role()
    }

    /// The coerced arguments of a field (validated already).
    pub fn arguments(
        &self,
        def: &crate::schema::FieldDef,
        field: &Field,
    ) -> Result<HashMap<String, Input>, GraphQlError> {
        let vars = Variables {
            types: self.var_types,
            values: Some(self.variables),
        };
        let mut out = HashMap::new();
        for (name, value) in &field.arguments {
            let Some(arg) = def.args.get(name.node.as_str()) else {
                continue;
            };
            let literal = self
                .source
                .value_at(value.pos)
                .map_err(|e| GraphQlError::new(e).at(value.pos))?;
            let input =
                coerce::literal(self.schema, &arg.ty, &literal, None, &vars).map_err(|_| {
                    GraphQlError::new(
                        "The specified argument value does not match the argument type.",
                    )
                    .at(value.pos)
                })?;
            out.insert(name.node.to_string(), input);
        }
        Ok(out)
    }
}

/// Fields of one response key, merged (`CollectFields`).
pub struct Collected<'d> {
    pub key: String,
    pub fields: Vec<&'d Positioned<Field>>,
}

impl<'d> Collected<'d> {
    pub fn name(&self) -> &str {
        self.fields[0].node.name.node.as_str()
    }

    pub fn pos(&self) -> Pos {
        self.fields[0].pos
    }

    /// The selection sets of every merged field.
    pub fn sub_sets(&self) -> Vec<&'d SelectionSet> {
        self.fields
            .iter()
            .map(|f| &f.node.selection_set.node)
            .collect()
    }
}

/// The spec's `CollectFields` over several selection sets of type `parent` (`@skip`/`@include`
/// applied, fragments inlined).
pub fn collect_fields<'d>(
    ctx: &Ctx<'d>,
    parent: &str,
    sets: &[&'d SelectionSet],
) -> Vec<Collected<'d>> {
    let mut out: Vec<Collected<'d>> = Vec::new();
    let mut visited = HashSet::new();
    for set in sets {
        collect_into(ctx, parent, set, &mut out, &mut visited);
    }
    out
}

fn collect_into<'d>(
    ctx: &Ctx<'d>,
    parent: &str,
    set: &'d SelectionSet,
    out: &mut Vec<Collected<'d>>,
    visited: &mut HashSet<String>,
) {
    for selection in &set.items {
        if !included(ctx, selection.node.directives()) {
            continue;
        }
        match &selection.node {
            Selection::Field(field) => {
                let key = field.node.response_key().node.to_string();
                match out.iter_mut().find(|c| c.key == key) {
                    Some(collected) => collected.fields.push(field),
                    None => out.push(Collected {
                        key,
                        fields: vec![field],
                    }),
                }
            }
            Selection::FragmentSpread(spread) => {
                let name = spread.node.fragment_name.node.as_str();
                let Some(fragment) = ctx.doc.fragments.get(name) else {
                    continue;
                };
                if fragment.node.type_condition.node.on.node == parent
                    && visited.insert(name.to_owned())
                {
                    collect_into(ctx, parent, &fragment.node.selection_set.node, out, visited);
                }
            }
            Selection::InlineFragment(inline) => {
                let on = inline
                    .node
                    .type_condition
                    .as_ref()
                    .map(|c| c.node.on.node.as_str());
                if on.is_none_or(|on| on == parent) {
                    collect_into(ctx, parent, &inline.node.selection_set.node, out, visited);
                }
            }
        }
    }
}

/// `@skip(if:)` / `@include(if:)`.
fn included(
    ctx: &Ctx<'_>,
    directives: &[Positioned<async_graphql_parser::types::Directive>],
) -> bool {
    for directive in directives {
        let name = directive.node.name.node.as_str();
        let condition = directive
            .node
            .get_argument("if")
            .and_then(|value| ctx.source.value_at(value.pos).ok())
            .and_then(|value| match value.kind {
                PKind::Boolean(b) => Some(b),
                PKind::Variable(var) => ctx.variables.get(&var).and_then(Json::as_bool),
                _ => None,
            })
            .unwrap_or(false);
        if (name == "skip" && condition) || (name == "include" && !condition) {
            return false;
        }
    }
    true
}

/// The selection on one entity object (a row).
#[derive(Debug)]
pub struct ObjectPlan {
    pub entity: usize,
    /// The table alias of the row.
    pub alias: String,
    pub fields: Vec<FieldPlan>,
}

#[derive(Debug)]
pub enum FieldPlan {
    Typename {
        key: String,
        name: String,
    },
    Column {
        key: String,
        column: usize,
        non_null: bool,
        pos: Pos,
    },
    /// A field the role may not read: Hot Chocolate's `AUTH_NOT_AUTHORIZED` on every row.
    Forbidden {
        key: String,
        non_null: bool,
        pos: Pos,
    },
    ToOne {
        key: String,
        relationship: usize,
        non_null: bool,
        child: ObjectPlan,
        /// The join with the parent row, and the target's read policy (none for a role without
        /// read access, whose fields are all forbidden).
        predicate: Pred,
        pos: Pos,
    },
    ToMany {
        key: String,
        relationship: usize,
        connection: Box<ConnectionPlan>,
        pos: Pos,
    },
}

/// A `{E}Connection`: a page of rows.
#[derive(Debug)]
pub struct ConnectionPlan {
    pub entity: usize,
    pub alias: String,
    pub type_name: String,
    pub fields: Vec<ConnectionField>,
    /// One per `items` selection, all over the same rows.
    pub items: Vec<ObjectPlan>,
    /// Filter, read policy, keyset and (nested) the join with the parent row.
    pub predicate: Pred,
    pub order: Vec<OrderKey>,
    pub limit: u64,
    /// `hasNextPage` or `endCursor` is selected: one extra row is read.
    pub probe: bool,
    /// `endCursor` is selected: rows carry their order-key values.
    pub cursor: bool,
}

#[derive(Debug)]
pub enum ConnectionField {
    Items {
        key: String,
        index: usize,
    },
    EndCursor {
        key: String,
    },
    HasNextPage {
        key: String,
    },
    Typename {
        key: String,
    },
    /// `groupBy`: the connection's rows are groups (`crate::aggregate`).
    GroupBy {
        key: String,
        plan: Box<crate::aggregate::GroupByPlan>,
    },
}

/// One `ORDER BY` key.
#[derive(Clone, Debug, PartialEq)]
pub struct OrderKey {
    pub column: usize,
    pub desc: bool,
}

/// DAB's page size for `first` (`RuntimeConfig.GetPaginationLimit`).
pub fn page_size(first: Option<i64>, settings: &DataApiSettings) -> Result<u64, GraphQlError> {
    let max = i64::from(settings.max_page_size);
    match first {
        None => Ok(u64::from(settings.default_page_size)),
        Some(-1) => Ok(u64::from(settings.max_page_size)),
        Some(n) if n >= 1 && n <= max => Ok(n as u64),
        Some(n) => Err(GraphQlError::dab(
            format!(
                "Invalid number of items requested, first argument must be either -1 or a \
                 positive number within the max page size limit of {max}. Actual value: {n}"
            ),
            codes::BAD_REQUEST,
        )),
    }
}

/// The plan of an entity object selection; `grant` is the role's read grant on the entity
/// (`None`: every field is forbidden).
pub fn object_plan(
    ctx: &Ctx<'_>,
    entity_index: usize,
    alias: String,
    sets: &[&SelectionSet],
    grant: Option<&ActionPermission>,
) -> Result<ObjectPlan, GraphQlError> {
    let entity = &ctx.model.entities[entity_index];
    let type_name = &entity.graphql.type_name;
    let object = ctx
        .schema
        .object(type_name)
        .ok_or_else(|| GraphQlError::dab(messages::DATABASE_ERROR, codes::UNEXPECTED_ERROR))?;
    let mut fields = Vec::new();
    for collected in collect_fields(ctx, type_name, sets) {
        let key = collected.key.clone();
        let pos = collected.pos();
        if collected.name() == "__typename" {
            fields.push(FieldPlan::Typename {
                key,
                name: type_name.clone(),
            });
            continue;
        }
        let Some(def) = object.fields.get(collected.name()) else {
            continue;
        };
        let non_null = def.ty.is_non_null();
        let Some(grant) = grant else {
            fields.push(FieldPlan::Forbidden { key, non_null, pos });
            continue;
        };
        match def.resolver {
            Resolver::Column(_, column) => {
                if access::allows_field(grant, &entity.columns[column].field) {
                    fields.push(FieldPlan::Column {
                        key,
                        column,
                        non_null,
                        pos,
                    });
                } else {
                    fields.push(FieldPlan::Forbidden { key, non_null, pos });
                }
            }
            Resolver::ToOne(_, relationship, target) => {
                let target_entity = &ctx.model.entities[target];
                let target_grant = access::grant(target_entity, ctx.role(), Action::Read);
                let child_alias = ctx.alias();
                let rel = &entity.relationships[relationship];
                let mut predicate = vec![join_pred(
                    ctx.model,
                    entity_index,
                    rel,
                    &alias,
                    target_entity,
                    &child_alias,
                )];
                if target_grant.is_some()
                    && let Some(policy) =
                        policy::read_filter(target_entity, ctx.caller, &child_alias)?
                {
                    predicate.push(policy);
                }
                let child = object_plan(
                    ctx,
                    target,
                    child_alias,
                    &collected.sub_sets(),
                    target_grant,
                )?;
                fields.push(FieldPlan::ToOne {
                    key,
                    relationship,
                    non_null,
                    child,
                    predicate: Pred::all(predicate),
                    pos,
                });
            }
            Resolver::ToMany(_, relationship, target) => {
                let args = ctx.arguments(def, &collected.fields[0].node)?;
                let join = Some((entity_index, relationship, alias.as_str()));
                let connection = connection_plan(ctx, target, &collected.sub_sets(), &args, join)?;
                fields.push(FieldPlan::ToMany {
                    key,
                    relationship,
                    connection: Box::new(connection),
                    pos,
                });
            }
            _ => {}
        }
    }
    Ok(ObjectPlan {
        entity: entity_index,
        alias,
        fields,
    })
}

/// The plan of a connection over `entity_index`. `join` is the parent row of a nested
/// connection: (parent entity, relationship, parent alias).
pub fn connection_plan(
    ctx: &Ctx<'_>,
    entity_index: usize,
    sets: &[&SelectionSet],
    args: &HashMap<String, Input>,
    join: Option<(usize, usize, &str)>,
) -> Result<ConnectionPlan, GraphQlError> {
    let entity = &ctx.model.entities[entity_index];
    let alias = ctx.alias();
    let grant = access::grant(entity, ctx.role(), Action::Read);
    let first = match args.get("first") {
        Some(Input::Scalar(Json::Number(n))) => n.as_i64(),
        _ => None,
    };
    let limit = page_size(first, ctx.settings)?;

    let type_name = entity.graphql.connection_type();
    let mut fields = Vec::new();
    let mut items = Vec::new();
    let (mut probe, mut cursor) = (false, false);
    let selections = collect_fields(ctx, &type_name, sets);
    crate::aggregate::check_selections(&selections)?;
    for collected in selections {
        let key = collected.key.clone();
        match collected.name() {
            "items" => {
                let plan = object_plan(
                    ctx,
                    entity_index,
                    alias.clone(),
                    &collected.sub_sets(),
                    grant,
                )?;
                fields.push(ConnectionField::Items {
                    key,
                    index: items.len(),
                });
                items.push(plan);
            }
            "endCursor" => {
                probe = true;
                cursor = true;
                fields.push(ConnectionField::EndCursor { key });
            }
            "hasNextPage" => {
                probe = true;
                fields.push(ConnectionField::HasNextPage { key });
            }
            "__typename" => fields.push(ConnectionField::Typename { key }),
            crate::aggregate::GROUP_BY => {
                let plan = crate::aggregate::plan(ctx, entity_index, grant, &collected)?;
                fields.push(ConnectionField::GroupBy {
                    key,
                    plan: Box::new(plan),
                });
            }
            _ => {}
        }
    }

    let mut preds = Vec::new();
    if let Some((parent, relationship, parent_alias)) = join {
        let rel = &ctx.model.entities[parent].relationships[relationship];
        preds.push(join_pred(
            ctx.model,
            parent,
            rel,
            parent_alias,
            entity,
            &alias,
        ));
    }
    if grant.is_some()
        && let Some(policy) = policy::read_filter(entity, ctx.caller, &alias)?
    {
        preds.push(policy);
    }
    if let Some(filter) = args.get("filter").filter(|f| !f.is_null()) {
        preds.push(filter_object(ctx, entity_index, &alias, grant, filter, 0)?);
    }
    let group_order = crate::aggregate::order(entity, &mut fields, args.get("orderBy"), grant)?;
    let grouped = group_order.is_some();
    let order = match group_order {
        Some(order) => order,
        None => order_keys(entity, args.get("orderBy"), grant)?,
    };
    if let Some(Input::Scalar(Json::String(after))) = args.get("after") {
        preds.push(keyset(entity, &alias, &order, after, !grouped)?);
    }
    Ok(ConnectionPlan {
        entity: entity_index,
        alias,
        type_name,
        fields,
        items,
        predicate: Pred::all(preds),
        order,
        limit,
        probe,
        cursor,
    })
}

/// `target.<target column> = source.<source column>` for each column pair of `rel`.
pub fn join_pred(
    model: &AppModel,
    source: usize,
    rel: &rl_model::Relationship,
    source_alias: &str,
    target: &Entity,
    target_alias: &str,
) -> Pred {
    let source = &model.entities[source];
    let column = |entity: &Entity, name: &str| {
        entity
            .column(name)
            .or_else(|| entity.field(name))
            .map_or_else(|| name.to_owned(), |c| c.name.clone())
    };
    Pred::all(
        rel.source_columns
            .iter()
            .zip(&rel.target_columns)
            .map(|(s, t)| {
                Pred::Cmp(
                    Operand::column(target_alias, &column(target, t)),
                    CmpOp::Eq,
                    Operand::column(source_alias, &column(source, s)),
                )
            }),
    )
}

/// A `{E}FilterInput` object (`GQLFilterParser.Parse`): fields AND-ed; a field whose value is
/// `null` is skipped; an object that yields no predicate is false, as DAB's empty chain is.
pub fn filter_object(
    ctx: &Ctx<'_>,
    entity_index: usize,
    alias: &str,
    grant: Option<&ActionPermission>,
    filter: &Input,
    depth: usize,
) -> Result<Pred, GraphQlError> {
    if depth > MAX_FILTER_DEPTH {
        return Err(GraphQlError::dab(
            format!(
                "The provided GraphQL filter exceeds the maximum allowed nesting depth of \
                 {MAX_FILTER_DEPTH}."
            ),
            codes::DATABASE_INPUT_ERROR,
        ));
    }
    let entity = &ctx.model.entities[entity_index];
    let Some(fields) = filter.as_object() else {
        return Ok(Pred::Const(false));
    };
    let mut preds = Vec::new();
    for (name, value, _) in fields {
        if value.is_null() {
            continue;
        }
        if name == "and" || name == "or" {
            let items: Vec<&Input> = match value {
                Input::List(items) => items.iter().filter(|i| !i.is_null()).collect(),
                other => vec![other],
            };
            let parts = items
                .into_iter()
                .map(|item| filter_object(ctx, entity_index, alias, grant, item, depth))
                .collect::<Result<Vec<_>, _>>()?;
            preds.push(match (name.as_str(), parts.is_empty()) {
                (_, true) => Pred::Const(false),
                ("and", false) => Pred::And(parts),
                _ => Pred::Or(parts),
            });
        } else if let Some(column) = entity.field(name) {
            if !grant.is_some_and(|g| access::allows_field(g, name)) {
                return Err(GraphQlError::dab(
                    messages::FILTER_FIELD_FORBIDDEN,
                    codes::AUTHORIZATION_CHECK_FAILED,
                ));
            }
            preds.push(scalar_filter(alias, column, value)?);
        } else if let Some(rel_index) = entity.relationships.iter().position(|r| &r.name == name) {
            preds.push(relationship_filter(
                ctx,
                entity_index,
                alias,
                rel_index,
                value,
                depth,
            )?);
        }
    }
    Ok(if preds.is_empty() {
        Pred::Const(false)
    } else {
        Pred::all(preds)
    })
}

fn relationship_filter(
    ctx: &Ctx<'_>,
    entity_index: usize,
    alias: &str,
    rel_index: usize,
    value: &Input,
    depth: usize,
) -> Result<Pred, GraphQlError> {
    let entity = &ctx.model.entities[entity_index];
    let rel = &entity.relationships[rel_index];
    let target_index = ctx
        .model
        .entities
        .iter()
        .position(|e| e.name == rel.target)
        .ok_or_else(|| GraphQlError::dab(messages::DATABASE_ERROR, codes::UNEXPECTED_ERROR))?;
    let target = &ctx.model.entities[target_index];
    let Some(target_grant) = access::grant(target, ctx.role(), Action::Read) else {
        return Err(GraphQlError::dab(
            messages::FILTER_ENTITY_FORBIDDEN,
            codes::AUTHORIZATION_CHECK_FAILED,
        ));
    };
    let fields = value.as_object().unwrap_or_default();
    let mut preds = Vec::new();
    // The `isNull` superset on a to-one filter: the FK column is (not) null.
    if let Some((_, Input::Scalar(Json::Bool(is_null)), _)) =
        fields.iter().find(|(name, _, _)| name == "isNull")
    {
        for source in &rel.source_columns {
            let column = entity.column(source).map_or(source.as_str(), |c| &c.name);
            preds.push(Pred::IsNull {
                operand: Operand::column(alias, column),
                negated: !is_null,
            });
        }
    }
    let rest: Vec<_> = fields
        .iter()
        .filter(|(name, _, _)| name != "isNull")
        .cloned()
        .collect();
    if !rest.is_empty() || preds.is_empty() {
        let target_alias = ctx.alias();
        let mut inner = vec![join_pred(
            ctx.model,
            entity_index,
            rel,
            alias,
            target,
            &target_alias,
        )];
        if let Some(policy) = policy::read_filter(target, ctx.caller, &target_alias)? {
            inner.push(policy);
        }
        inner.push(filter_object(
            ctx,
            target_index,
            &target_alias,
            Some(target_grant),
            &Input::Object(rest),
            depth + 1,
        )?);
        preds.push(Pred::Exists {
            table: target.table.clone(),
            alias: target_alias,
            pred: Box::new(Pred::all(inner)),
        });
    }
    Ok(Pred::all(preds))
}

/// A `<Scalar>FilterInput` (`FieldFilterParser.Parse`): operators AND-ed, `null` operands skipped,
/// none left → false.
fn scalar_filter(
    alias: &str,
    column: &rl_model::Column,
    ops: &Input,
) -> Result<Pred, GraphQlError> {
    let operand = || Operand::column(alias, &column.name);
    let stored = |value: &Input| -> Result<SqlValue, GraphQlError> {
        let Input::Scalar(json) = value else {
            return Err(GraphQlError::dab(
                messages::DATABASE_ERROR,
                codes::BAD_REQUEST,
            ));
        };
        codec::encode_operand(&column.kind, json)
            .map_err(|e| GraphQlError::dab(e.to_string(), codes::BAD_REQUEST))
    };
    let text = |value: &Input| -> Result<String, GraphQlError> {
        match stored(value)? {
            SqlValue::Text(text) => Ok(text),
            other => Err(GraphQlError::dab(
                format!("unexpected operand {other:?}"),
                codes::BAD_REQUEST,
            )),
        }
    };
    let mut preds = Vec::new();
    for (op, value, _) in ops.as_object().unwrap_or_default() {
        if value.is_null() {
            continue;
        }
        let cmp = |op: CmpOp| -> Result<Pred, GraphQlError> {
            Ok(Pred::Cmp(operand(), op, Operand::Value(stored(value)?)))
        };
        let like = |pattern: String, negated: bool| Pred::Like {
            operand: operand(),
            pattern,
            negated,
        };
        preds.push(match op.as_str() {
            "eq" => cmp(CmpOp::Eq)?,
            "neq" => cmp(CmpOp::Ne)?,
            "gt" => cmp(CmpOp::Gt)?,
            "gte" => cmp(CmpOp::Ge)?,
            "lt" => cmp(CmpOp::Lt)?,
            "lte" => cmp(CmpOp::Le)?,
            "contains" => like(format!("%{}%", escape_like(&text(value)?)), false),
            "notContains" => like(format!("%{}%", escape_like(&text(value)?)), true),
            "startsWith" => like(format!("{}%", escape_like(&text(value)?)), false),
            "endsWith" => like(format!("%{}", escape_like(&text(value)?)), false),
            "isNull" => Pred::IsNull {
                operand: operand(),
                negated: !matches!(value, Input::Scalar(Json::Bool(true))),
            },
            "in" => {
                let items = match value {
                    Input::List(items) => items.as_slice(),
                    other => std::slice::from_ref(other),
                };
                if items.len() > 100 {
                    return Err(GraphQlError::dab(
                        messages::IN_TOO_MANY_VALUES,
                        codes::BAD_REQUEST,
                    ));
                }
                let values = items
                    .iter()
                    .filter(|i| !i.is_null())
                    .map(stored)
                    .collect::<Result<Vec<_>, _>>()?;
                if values.is_empty() {
                    continue;
                }
                Pred::In {
                    operand: operand(),
                    values,
                }
            }
            _ => continue,
        });
    }
    Ok(if preds.is_empty() {
        Pred::Const(false)
    } else {
        Pred::all(preds)
    })
}

/// The `orderBy` keys, then the primary key ascending (the tie-breaker DAB always adds).
pub fn order_keys(
    entity: &Entity,
    order_by: Option<&Input>,
    grant: Option<&ActionPermission>,
) -> Result<Vec<OrderKey>, GraphQlError> {
    let mut keys = Vec::new();
    for (name, value, _) in order_by.and_then(Input::as_object).unwrap_or_default() {
        let Input::Enum(direction) = value else {
            continue;
        };
        let Some(column) = entity.columns.iter().position(|c| &c.field == name) else {
            continue;
        };
        if !grant.is_some_and(|g| access::allows_field(g, name)) {
            return Err(GraphQlError::dab(
                messages::ORDER_BY_FIELD_FORBIDDEN,
                codes::AUTHORIZATION_CHECK_FAILED,
            ));
        }
        keys.push(OrderKey {
            column,
            desc: direction == "DESC",
        });
    }
    for (column, def) in entity.columns.iter().enumerate() {
        if def.primary_key && !keys.iter().any(|k| k.column == column) {
            keys.push(OrderKey {
                column,
                desc: false,
            });
        }
    }
    Ok(keys)
}

/// The rows after the cursor `after` under `order` (DAB's checks and messages). Unlike DAB's
/// `(a > @a) OR (a = @a AND b > @b)`, `NULL`s are placed as the `ORDER BY` places them (first
/// ascending, last descending), so no row is skipped. Cursors of groups carry no primary key
/// (`require_pk` false).
fn keyset(
    entity: &Entity,
    alias: &str,
    order: &[OrderKey],
    after: &str,
    require_pk: bool,
) -> Result<Pred, GraphQlError> {
    let bad = |message: String| GraphQlError::dab(message, codes::BAD_REQUEST);
    let (text, fields) = cursor::decode(after).map_err(|e| bad(e.message()))?;
    let invalid = || bad(cursor::InvalidCursor(text.clone()).message());
    let mut by_name: HashMap<&str, &cursor::CursorField> = HashMap::new();
    for field in &fields {
        if entity.field(&field.field).is_none() {
            return Err(bad(format!(
                "Pagination token is not well formed because {} is not valid.",
                field.field
            )));
        }
        if by_name.insert(&field.field, field).is_some() {
            return Err(invalid());
        }
    }
    let pk = entity.primary_key_column();
    if require_pk && !by_name.contains_key(pk.field.as_str()) {
        return Err(bad(format!(
            "Pagination token is not well formed because it is missing an expected field: {}",
            pk.field
        )));
    }
    let mut values = Vec::with_capacity(order.len());
    for key in order {
        let column = &entity.columns[key.column];
        let direction = i64::from(key.desc);
        let Some(field) = by_name
            .get(column.field.as_str())
            .filter(|f| f.direction == direction)
        else {
            return Err(bad(format!(
                "Could not match order by column {} with a column in the pagination token with \
                 the same name and direction.",
                column.field
            )));
        };
        let value = codec::encode_operand(&column.kind, &field.value).map_err(|_| invalid())?;
        values.push((column, key.desc, value));
    }
    if by_name.len() != order.len() {
        return Err(invalid());
    }
    let mut alternatives = Vec::new();
    for i in 0..values.len() {
        let mut parts = Vec::new();
        for (column, _, value) in &values[..i] {
            let operand = Operand::column(alias, &column.name);
            parts.push(match value {
                SqlValue::Null => Pred::IsNull {
                    operand,
                    negated: false,
                },
                value => Pred::Cmp(operand, CmpOp::Is, Operand::Value(value.clone())),
            });
        }
        let (column, desc, value) = &values[i];
        let operand = Operand::column(alias, &column.name);
        parts.push(match (desc, value) {
            (false, SqlValue::Null) => Pred::IsNull {
                operand,
                negated: true,
            },
            (true, SqlValue::Null) => Pred::Const(false),
            (false, value) => Pred::Cmp(operand, CmpOp::Gt, Operand::Value(value.clone())),
            (true, value) => Pred::Or(vec![
                Pred::Cmp(operand.clone(), CmpOp::Lt, Operand::Value(value.clone())),
                Pred::IsNull {
                    operand,
                    negated: false,
                },
            ]),
        });
        alternatives.push(Pred::all(parts));
    }
    Ok(Pred::Or(alternatives))
}
