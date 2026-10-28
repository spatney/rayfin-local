//! DAB's aggregations (micro-fabric compatibility.md §6.10): `{E}Connection.groupBy(fields:)`
//! answers one `{E}GroupBy { fields, aggregations }` per group of the connection's rows (its
//! `filter` and the read policy apply), with `sum`/`avg`/`min`/`max`/`count` over the numeric
//! fields, `distinct` and `having`.
//!
//! The schema is DAB's (`GraphQLSchemaCreator`, `SchemaConverter.GenerateAggregationTypeForEntity`,
//! `EnumTypeBuilder`, `QueryBuilder.GenerateReturnType`); planning follows
//! `SqlQueryStructure.ProcessGroupByField` (field permissions, `fields` limited to the grouped
//! fields, `groupBy` and `items` exclusive) and the SQL `BaseSqlQueryBuilder`
//! (`op([DISTINCT] col)`, `GROUP BY`, `HAVING`, `TOP first`).
//!
//! Values are what Fabric SQL computes, written as Hot Chocolate writes a `Float` (`count`: `Int`):
//!
//! | Operation | `@int()` | `@decimal(p,s)` |
//! | --- | --- | --- |
//! | `sum` | the sum | the exact sum (scale `s`) |
//! | `avg` | `int`: the sum divided by the count, truncated toward zero | `decimal(38, max(s,6))`, rounded |
//! | `min`, `max` | the value | the value |
//! | `count` | the non-null values | the non-null values |
//!
//! `sum`/`avg`/`min`/`max` are `null` over no non-null values. Without `fields` there is one group
//! (the grand total), even over no rows. Groups are ordered by `orderBy`, which may only name
//! grouped fields, then by the grouped fields ascending (DAB 1.x leaves the order to SQL Server),
//! and paged as rows are: `first` (100 by default: DAB's `TOP`), `hasNextPage`, and `endCursor`
//! and `after` over the group keys. On the `postgresql` dialect `avg` is the exact mean.
//!
//! Differences: an `@int()` `sum`/`avg` past 2^31 overflows on SQL Server (a database error) but
//! not here, and decimal aggregates are exact there but doubles here (see `rl_db::codec`).

use indexmap::IndexMap;
use rl_model::{ActionPermission, AppModel, Dialect, Entity, FieldKind};
use rusqlite::types::Value as SqlValue;
use serde_json::{Map, Value as Json};

use crate::access;
use crate::coerce::Input;
use crate::cursor::{self, CursorField};
use crate::error::{GraphQlError, codes, messages};
use crate::plan::{Collected, ConnectionField, ConnectionPlan, Ctx, OrderKey, collect_fields};
use crate::schema::{self, EnumDef, InputValueDef, ObjectDef, Resolver, Schema, TypeDef, TypeRef};
use crate::sql::{Sql, column_sql, quote_ident};

/// `{E}Connection.groupBy`.
pub const GROUP_BY: &str = "groupBy";

/// DAB's aggregation fields, in its order: name, type, description.
const OPERATIONS: [(&str, &str, &str); 5] = [
    ("max", "Float", "Maximum value for numeric fields"),
    ("min", "Float", "Minimum value for numeric fields"),
    ("avg", "Float", "Average value"),
    ("sum", "Float", "Sum of values"),
    ("count", "Int", "Count of numeric values"),
];

/// Adds `groupBy` to the entity's `{E}Connection` and the `{E}ScalarFields`,
/// `{E}NumericAggregateFields`, `{E}Aggregations` and `{E}GroupBy` types, from its object type
/// (both already in `schema`). Without numeric fields there is no `aggregations`.
pub fn add_types(schema: &mut Schema, entity: &Entity) {
    let type_name = entity.graphql.type_name.as_str();
    let Some(object) = schema.object(type_name) else {
        return;
    };
    let columns: Vec<(String, String)> = object
        .fields
        .values()
        .filter(|f| matches!(f.resolver, Resolver::Column(..)))
        .map(|f| (f.name.clone(), f.ty.name().to_owned()))
        .collect();
    if columns.is_empty() {
        return;
    }
    let scalar_fields = entity.graphql.scalar_fields_enum();
    insert(
        schema,
        TypeDef::Enum(EnumDef {
            name: scalar_fields.clone(),
            description: Some(format!("Scalar fields available in {type_name}")),
            values: columns.iter().map(|(name, _)| name.clone()).collect(),
        }),
    );

    let group_by = entity.graphql.group_by_type();
    let mut group_fields = IndexMap::new();
    let fields = schema::field(
        "fields",
        Some(&format!("Grouped fields from {type_name}")),
        TypeRef::named(type_name),
        Resolver::Extension,
    );
    group_fields.insert(fields.name.clone(), fields);
    let numeric: Vec<&(String, String)> = columns
        .iter()
        .filter(|(_, scalar)| matches!(scalar.as_str(), "Int" | "Decimal"))
        .collect();
    if !numeric.is_empty() {
        let numeric_fields = entity.graphql.numeric_aggregate_fields_enum();
        insert(
            schema,
            TypeDef::Enum(EnumDef {
                name: numeric_fields.clone(),
                description: Some(format!("Fields available for aggregation in {type_name}")),
                values: numeric.iter().map(|(name, _)| name.clone()).collect(),
            }),
        );
        // The highest-precision numeric type's filter (`SchemaConverter.GetCommonFilterInputType`).
        let having = if numeric.iter().any(|(_, scalar)| scalar == "Decimal") {
            schema::scalar_filter_name("Decimal")
        } else {
            schema::scalar_filter_name("Int")
        };
        let aggregations = entity.graphql.aggregations_type();
        let fields = OPERATIONS
            .iter()
            .map(|(op, ty, description)| {
                let mut def = schema::field(
                    op,
                    Some(description),
                    TypeRef::named(*ty),
                    Resolver::Extension,
                );
                let args = [
                    schema::input(
                        "field",
                        "Field to aggregate on",
                        TypeRef::named(&numeric_fields).non_null(),
                    ),
                    schema::input(
                        "having",
                        "Filter criteria for aggregation",
                        TypeRef::named(&having),
                    ),
                    InputValueDef {
                        default: Some("false".into()),
                        ..schema::input(
                            "distinct",
                            "Whether to aggregate on distinct values",
                            TypeRef::named("Boolean"),
                        )
                    },
                ];
                def.args = args.into_iter().map(|a| (a.name.clone(), a)).collect();
                (op.to_string(), def)
            })
            .collect();
        insert(
            schema,
            TypeDef::Object(ObjectDef {
                name: aggregations.clone(),
                description: Some(format!("Aggregation type for {type_name}")),
                fields,
            }),
        );
        let field = schema::field(
            "aggregations",
            Some(&format!("Aggregations for {type_name}")),
            TypeRef::named(&aggregations),
            Resolver::Extension,
        );
        group_fields.insert(field.name.clone(), field);
    }
    insert(
        schema,
        TypeDef::Object(ObjectDef {
            name: group_by.clone(),
            description: Some(format!("GroupBy type for {type_name}")),
            fields: group_fields,
        }),
    );

    let connection = entity.graphql.connection_type();
    if let Some(TypeDef::Object(connection)) = schema.types.get_mut(&connection) {
        let mut def = schema::field(
            GROUP_BY,
            Some("Group results by specified fields"),
            TypeRef::named(&group_by).non_null().list().non_null(),
            Resolver::Extension,
        );
        let arg = schema::input(
            "fields",
            "Fields to group by",
            TypeRef::named(&scalar_fields).non_null().list(),
        );
        def.args.insert(arg.name.clone(), arg);
        connection.fields.insert(def.name.clone(), def);
    }
}

fn insert(schema: &mut Schema, def: TypeDef) {
    schema.types.insert(def.name().to_owned(), def);
}

/// One `groupBy` selection.
#[derive(Debug)]
pub struct GroupByPlan {
    pub entity: usize,
    /// The grouped columns (`fields:`), without repeats, in order.
    pub group: Vec<usize>,
    /// The groups' order (set by [`order`]).
    pub order: Vec<OrderKey>,
    pub selections: Vec<GroupSelection>,
    /// The `having` of every aggregation, all of which a group must pass.
    pub having: Vec<Having>,
}

/// A field of `{E}GroupBy`.
#[derive(Debug)]
pub enum GroupSelection {
    Typename { key: String, name: String },
    Fields { key: String, fields: Vec<Selected> },
    Aggregations { key: String, fields: Vec<Selected> },
}

/// A field of `fields` (a grouped column) or `aggregations`.
#[derive(Debug)]
pub enum Selected {
    Typename { key: String, name: String },
    Column { key: String, column: usize },
    Aggregate { key: String, aggregate: Aggregate },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Max,
    Min,
    Avg,
    Sum,
    Count,
}

impl Operation {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "max" => Self::Max,
            "min" => Self::Min,
            "avg" => Self::Avg,
            "sum" => Self::Sum,
            "count" => Self::Count,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Min => "min",
            Self::Avg => "avg",
            Self::Sum => "sum",
            Self::Count => "count",
        }
    }
}

/// `op([DISTINCT] column)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Aggregate {
    pub operation: Operation,
    pub column: usize,
    pub distinct: bool,
}

/// An aggregation's `having`: every condition holds; none (`{}`, or only `null`s) is false, as
/// DAB's empty predicate chain is.
#[derive(Debug)]
pub struct Having {
    pub aggregate: Aggregate,
    pub conditions: Vec<Condition>,
}

#[derive(Debug)]
pub enum Condition {
    /// The SQL operator (` > `) and the value.
    Compare(&'static str, SqlValue),
    /// `isNull: true` (`IS NULL`) or `false` (`IS NOT NULL`).
    IsNull(bool),
    In(Vec<SqlValue>),
}

fn unexpected() -> GraphQlError {
    GraphQlError::dab(messages::DATABASE_ERROR, codes::UNEXPECTED_ERROR)
}

fn column_index(entity: &Entity, field: &str) -> Option<usize> {
    entity.columns.iter().position(|c| c.field == field)
}

/// DAB refuses a connection selecting both `groupBy` and `items`
/// (`BaseQueryStructure.ExtractQueryField`).
pub fn check_selections(selections: &[Collected<'_>]) -> Result<(), GraphQlError> {
    let selects = |name: &str| selections.iter().any(|c| c.name() == name);
    if selects(GROUP_BY) && selects("items") {
        return Err(GraphQlError::dab(
            "Cannot have both groupBy and items in the same query",
            codes::BAD_REQUEST,
        ));
    }
    Ok(())
}

/// The plan of a `groupBy` selection on a connection over `entity_index`; `grant` is the role's
/// read grant (`None`: every field is forbidden).
pub fn plan(
    ctx: &Ctx<'_>,
    entity_index: usize,
    grant: Option<&ActionPermission>,
    collected: &Collected<'_>,
) -> Result<GroupByPlan, GraphQlError> {
    let entity = &ctx.model.entities[entity_index];
    let type_name = entity.graphql.type_name.as_str();
    let readable = |field: &str| grant.is_some_and(|g| access::allows_field(g, field));
    let def = ctx
        .schema
        .object(&entity.graphql.connection_type())
        .and_then(|c| c.fields.get(GROUP_BY))
        .ok_or_else(unexpected)?;
    let args = ctx.arguments(def, &collected.fields[0].node)?;
    let mut group = Vec::new();
    let names = match args.get("fields") {
        Some(Input::List(names)) => names.as_slice(),
        _ => &[],
    };
    for name in names {
        let Input::Enum(name) = name else {
            continue;
        };
        let Some(column) = column_index(entity, name) else {
            continue;
        };
        if !readable(name) {
            return Err(GraphQlError::dab(
                format!("Access forbidden to field '{name}' referenced in the groupBy argument."),
                codes::AUTHORIZATION_CHECK_FAILED,
            ));
        }
        if !group.contains(&column) {
            group.push(column);
        }
    }

    let group_by_type = entity.graphql.group_by_type();
    let aggregations_type = entity.graphql.aggregations_type();
    let mut selections = Vec::new();
    let mut having = Vec::new();
    for selection in collect_fields(ctx, &group_by_type, &collected.sub_sets()) {
        let key = selection.key.clone();
        match selection.name() {
            "__typename" => selections.push(GroupSelection::Typename {
                key,
                name: group_by_type.clone(),
            }),
            "fields" => {
                let mut fields = Vec::new();
                for field in collect_fields(ctx, type_name, &selection.sub_sets()) {
                    let column = column_index(entity, field.name()).filter(|c| group.contains(c));
                    let Some(column) = column else {
                        return Err(GraphQlError::dab(
                            "Groupby fields in selection must match the fields in the groupby \
                             argument.",
                            codes::BAD_REQUEST,
                        ));
                    };
                    fields.push(Selected::Column {
                        key: field.key.clone(),
                        column,
                    });
                }
                selections.push(GroupSelection::Fields { key, fields });
            }
            "aggregations" => {
                let object = ctx
                    .schema
                    .object(&aggregations_type)
                    .ok_or_else(unexpected)?;
                let mut fields = Vec::new();
                for field in collect_fields(ctx, &aggregations_type, &selection.sub_sets()) {
                    let key = field.key.clone();
                    if field.name() == "__typename" {
                        fields.push(Selected::Typename {
                            key,
                            name: aggregations_type.clone(),
                        });
                        continue;
                    }
                    let (Some(operation), Some(def)) = (
                        Operation::parse(field.name()),
                        object.fields.get(field.name()),
                    ) else {
                        continue;
                    };
                    let args = ctx.arguments(def, &field.fields[0].node)?;
                    let Some(Input::Enum(name)) = args.get("field") else {
                        continue;
                    };
                    let Some(column) = column_index(entity, name) else {
                        continue;
                    };
                    if !readable(name) {
                        return Err(GraphQlError::dab(
                            format!(
                                "Access forbidden to field '{name}' referenced in the \
                                 aggregation function '{}'.",
                                operation.name()
                            ),
                            codes::AUTHORIZATION_CHECK_FAILED,
                        ));
                    }
                    let aggregate = Aggregate {
                        operation,
                        column,
                        distinct: matches!(
                            args.get("distinct"),
                            Some(Input::Scalar(Json::Bool(true)))
                        ),
                    };
                    if let Some(filter) = args.get("having").filter(|h| !h.is_null()) {
                        having.push(Having {
                            aggregate,
                            conditions: conditions(filter)?,
                        });
                    }
                    fields.push(Selected::Aggregate { key, aggregate });
                }
                selections.push(GroupSelection::Aggregations { key, fields });
            }
            _ => {}
        }
    }
    Ok(GroupByPlan {
        entity: entity_index,
        group,
        order: Vec::new(),
        selections,
        having,
    })
}

/// A `having` filter (`FieldFilterParser.Parse` on the aggregated value).
fn conditions(filter: &Input) -> Result<Vec<Condition>, GraphQlError> {
    let mut out = Vec::new();
    for (op, value, _) in filter.as_object().unwrap_or_default() {
        if value.is_null() {
            continue;
        }
        let compare = |sql_op: &'static str| {
            Ok::<_, GraphQlError>(Condition::Compare(sql_op, number(value)?))
        };
        out.push(match op.as_str() {
            "eq" => compare(" = ")?,
            "neq" => compare(" != ")?,
            "gt" => compare(" > ")?,
            "gte" => compare(" >= ")?,
            "lt" => compare(" < ")?,
            "lte" => compare(" <= ")?,
            "isNull" => Condition::IsNull(matches!(value, Input::Scalar(Json::Bool(true)))),
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
                    .map(number)
                    .collect::<Result<Vec<_>, _>>()?;
                if values.is_empty() {
                    continue;
                }
                Condition::In(values)
            }
            _ => continue,
        });
    }
    Ok(out)
}

fn number(value: &Input) -> Result<SqlValue, GraphQlError> {
    match value {
        Input::Scalar(Json::Number(n)) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .ok_or_else(unexpected),
        _ => Err(unexpected()),
    }
}

/// Sets the groups' order of every `groupBy` of a connection, after its filter (as DAB's
/// `ProcessGqlOrderByArg` runs): the `orderBy` keys, which must be readable grouped fields, then
/// the other grouped fields ascending. Returns the first `groupBy`'s order (the page's cursor),
/// `None` without `groupBy`.
pub fn order(
    entity: &Entity,
    fields: &mut [ConnectionField],
    order_by: Option<&Input>,
    grant: Option<&ActionPermission>,
) -> Result<Option<Vec<OrderKey>>, GraphQlError> {
    let mut first = None;
    for field in fields {
        let ConnectionField::GroupBy { plan, .. } = field else {
            continue;
        };
        let mut keys: Vec<OrderKey> = Vec::new();
        for (name, value, _) in order_by.and_then(Input::as_object).unwrap_or_default() {
            let Input::Enum(direction) = value else {
                continue;
            };
            let Some(column) = column_index(entity, name) else {
                continue;
            };
            if !grant.is_some_and(|g| access::allows_field(g, name)) {
                return Err(GraphQlError::dab(
                    messages::ORDER_BY_FIELD_FORBIDDEN,
                    codes::AUTHORIZATION_CHECK_FAILED,
                ));
            }
            if !plan.group.contains(&column) {
                return Err(GraphQlError::dab(
                    format!("OrderBy field '{name}' must be present in the groupBy fields."),
                    codes::BAD_REQUEST,
                ));
            }
            keys.push(OrderKey {
                column,
                desc: direction == "DESC",
            });
        }
        for &column in &plan.group {
            if !keys.iter().any(|k| k.column == column) {
                keys.push(OrderKey {
                    column,
                    desc: false,
                });
            }
        }
        plan.order.clone_from(&keys);
        first.get_or_insert(keys);
    }
    Ok(first)
}

/// Whether the connection selects `groupBy`: its rows are then groups (it cannot select `items`).
pub fn is_group_by(plan: &ConnectionPlan) -> bool {
    plan.fields
        .iter()
        .any(|f| matches!(f, ConnectionField::GroupBy { .. }))
}

/// The connection's only row, as an SQL expression: `json_array(<groups>, ...)` with, per
/// `groupBy`, the array of its groups, each `json_array(<group>, <order-key values or NULL>)`.
pub fn connection_sql(sql: &mut Sql, model: &AppModel, plan: &ConnectionPlan) {
    sql.push("json_array(");
    let mut first = true;
    for field in &plan.fields {
        let ConnectionField::GroupBy { plan: group, .. } = field else {
            continue;
        };
        if !std::mem::take(&mut first) {
            sql.push(", ");
        }
        groups_sql(sql, model, plan, group);
    }
    sql.push(")");
}

fn groups_sql(sql: &mut Sql, model: &AppModel, connection: &ConnectionPlan, plan: &GroupByPlan) {
    let entity = &model.entities[plan.entity];
    let alias = connection.alias.as_str();
    sql.push("json((SELECT json_group_array(json(g.j) ORDER BY g.n) FROM (SELECT json_array(");
    group_json(sql, model, entity, alias, plan);
    sql.push(", ");
    if connection.cursor {
        sql.push("json_array(");
        for (i, key) in plan.order.iter().enumerate() {
            if i > 0 {
                sql.push(", ");
            }
            let column = &entity.columns[key.column];
            let column_sql = column_sql(alias, &column.name);
            // As in the rows' cursors: dates and decimals keep their stored precision.
            sql.push(&match column.kind {
                FieldKind::Boolean => rl_db::codec::json_expr(&column.kind, &column_sql),
                _ => column_sql,
            });
        }
        sql.push(")");
    } else {
        sql.push("NULL");
    }
    sql.push(") AS j, row_number() OVER (");
    if !plan.order.is_empty() {
        sql.push("ORDER BY ");
        order_sql(sql, entity, alias, &plan.order);
    }
    sql.push(") AS n FROM ")
        .push(&quote_ident(&entity.table))
        .push(" AS ")
        .push(alias)
        .push(" WHERE ");
    connection.predicate.render(sql);
    if !plan.group.is_empty() {
        sql.push(" GROUP BY ");
        for (i, column) in plan.group.iter().enumerate() {
            if i > 0 {
                sql.push(", ");
            }
            sql.push(&column_sql(alias, &entity.columns[*column].name));
        }
    }
    if !plan.having.is_empty() {
        sql.push(" HAVING ");
        having_sql(sql, model, entity, alias, &plan.having);
    }
    if !plan.order.is_empty() {
        sql.push(" ORDER BY ");
        order_sql(sql, entity, alias, &plan.order);
    }
    let limit = connection.limit + u64::from(connection.probe);
    sql.push(&format!(" LIMIT {limit}) AS g))"));
}

fn order_sql(sql: &mut Sql, entity: &Entity, alias: &str, order: &[OrderKey]) {
    for (i, key) in order.iter().enumerate() {
        if i > 0 {
            sql.push(", ");
        }
        sql.push(&column_sql(alias, &entity.columns[key.column].name))
            .push(if key.desc { " DESC" } else { " ASC" });
    }
}

/// `json_object(...)` of a group's selection.
fn group_json(sql: &mut Sql, model: &AppModel, entity: &Entity, alias: &str, plan: &GroupByPlan) {
    let literal = rl_db::schema::quote_literal;
    sql.push("json_object(");
    for (i, selection) in plan.selections.iter().enumerate() {
        if i > 0 {
            sql.push(", ");
        }
        match selection {
            GroupSelection::Typename { key, name } => {
                sql.push(&literal(key)).push(", ").push(&literal(name));
            }
            GroupSelection::Fields { key, fields }
            | GroupSelection::Aggregations { key, fields } => {
                sql.push(&literal(key)).push(", json_object(");
                for (j, field) in fields.iter().enumerate() {
                    if j > 0 {
                        sql.push(", ");
                    }
                    match field {
                        Selected::Typename { key, name } => {
                            sql.push(&literal(key)).push(", ").push(&literal(name));
                        }
                        Selected::Column { key, column } => {
                            let column = &entity.columns[*column];
                            let value = rl_db::codec::json_expr(
                                &column.kind,
                                &column_sql(alias, &column.name),
                            );
                            sql.push(&literal(key)).push(", ").push(&value);
                        }
                        Selected::Aggregate { key, aggregate } => {
                            let value = aggregate_sql(model, entity, alias, aggregate);
                            sql.push(&literal(key)).push(", ").push(&value);
                        }
                    }
                }
                sql.push(")");
            }
        }
    }
    sql.push(")");
}

/// The aggregate as Fabric SQL computes it (see the module docs): SQLite stores decimals as
/// doubles, so results are rounded back to the scale SQL Server's `DECIMAL` arithmetic keeps.
fn aggregate_sql(model: &AppModel, entity: &Entity, alias: &str, aggregate: &Aggregate) -> String {
    let column = &entity.columns[aggregate.column];
    let mut arg = column_sql(alias, &column.name);
    if aggregate.distinct {
        arg = format!("DISTINCT {arg}");
    }
    let scale = match column.kind {
        FieldKind::Decimal { scale, .. } => Some(scale),
        _ => None,
    };
    let rounded = |expr: String| match scale {
        Some(scale) => format!("round({expr}, {scale})"),
        None => expr,
    };
    match aggregate.operation {
        Operation::Count => format!("count({arg})"),
        Operation::Sum => rounded(format!("sum({arg})")),
        Operation::Min => rounded(format!("min({arg})")),
        Operation::Max => rounded(format!("max({arg})")),
        Operation::Avg => match (model.dialect, scale) {
            (Dialect::Mssql, None) => format!("(sum({arg}) / count({arg}))"),
            (Dialect::Mssql, Some(scale)) => format!("round(avg({arg}), {})", scale.max(6)),
            (Dialect::Postgresql, _) => format!("avg({arg})"),
        },
    }
}

fn having_sql(sql: &mut Sql, model: &AppModel, entity: &Entity, alias: &str, having: &[Having]) {
    for (i, having) in having.iter().enumerate() {
        if i > 0 {
            sql.push(" AND ");
        }
        if having.conditions.is_empty() {
            sql.push("0");
            continue;
        }
        let expr = aggregate_sql(model, entity, alias, &having.aggregate);
        sql.push("(");
        for (j, condition) in having.conditions.iter().enumerate() {
            if j > 0 {
                sql.push(" AND ");
            }
            sql.push("(").push(&expr);
            match condition {
                Condition::Compare(op, value) => {
                    sql.push(op).param(value.clone());
                }
                Condition::IsNull(is_null) => {
                    sql.push(if *is_null { " IS NULL" } else { " IS NOT NULL" });
                }
                Condition::In(values) => {
                    sql.push(" IN (");
                    for (k, value) in values.iter().enumerate() {
                        if k > 0 {
                            sql.push(", ");
                        }
                        sql.param(value.clone());
                    }
                    sql.push(")");
                }
            }
            sql.push(")");
        }
        sql.push(")");
    }
}

/// The response of a `groupBy` connection from its row (see [`connection_sql`]): groups past
/// `first` are dropped (the first `groupBy` decides `hasNextPage` and `endCursor`), and integral
/// `Float`s are written as Hot Chocolate writes doubles (`6`, not `6.0`).
pub fn shape(model: &AppModel, plan: &ConnectionPlan, rows: &[Json]) -> Json {
    let row = rows.first().and_then(Json::as_array);
    let limit = usize::try_from(plan.limit).unwrap_or(usize::MAX);
    let mut results: Vec<(&GroupByPlan, &[Json])> = Vec::new();
    for field in &plan.fields {
        if let ConnectionField::GroupBy { plan: group, .. } = field {
            let groups = row
                .and_then(|row| row.get(results.len()))
                .and_then(Json::as_array)
                .map_or(&[][..], Vec::as_slice);
            results.push((group, groups));
        }
    }
    let more = results
        .first()
        .is_some_and(|(_, groups)| groups.len() > limit);
    let mut out = Map::new();
    let mut index = 0;
    for field in &plan.fields {
        match field {
            ConnectionField::GroupBy { key, .. } => {
                let (group, groups) = results[index];
                index += 1;
                let groups = groups
                    .iter()
                    .take(limit)
                    .map(|g| group_value(group, g.get(0).cloned().unwrap_or(Json::Null)))
                    .collect();
                out.insert(key.clone(), Json::Array(groups));
            }
            ConnectionField::HasNextPage { key } => {
                out.insert(key.clone(), Json::Bool(more));
            }
            ConnectionField::EndCursor { key } => {
                let cursor = results
                    .first()
                    .filter(|_| more)
                    .and_then(|(group, groups)| {
                        let last = groups.get(limit.checked_sub(1)?)?;
                        Some(group_cursor(model, group, last.get(1)?))
                    });
                out.insert(key.clone(), cursor.map_or(Json::Null, Json::String));
            }
            ConnectionField::Typename { key } => {
                out.insert(key.clone(), Json::String(plan.type_name.clone()));
            }
            ConnectionField::Items { .. } => {}
        }
    }
    Json::Object(out)
}

fn group_value(plan: &GroupByPlan, mut group: Json) -> Json {
    for selection in &plan.selections {
        let GroupSelection::Aggregations { key, fields } = selection else {
            continue;
        };
        let Some(values) = group.get_mut(key.as_str()) else {
            continue;
        };
        for field in fields {
            if let Selected::Aggregate { key, aggregate } = field
                && aggregate.operation != Operation::Count
                && let Some(value) = values.get_mut(key.as_str())
            {
                *value = float(value.take());
            }
        }
    }
    group
}

/// A `Float` as System.Text.Json writes a double: integral values without a fraction.
fn float(value: Json) -> Json {
    const EXACT: f64 = 9_007_199_254_740_992.0;
    match value.as_f64() {
        Some(f) if value.is_f64() && f.fract() == 0.0 && f.abs() < EXACT => Json::from(f as i64),
        _ => value,
    }
}

fn group_cursor(model: &AppModel, plan: &GroupByPlan, values: &Json) -> String {
    let entity = &model.entities[plan.entity];
    let values = values.as_array().cloned().unwrap_or_default();
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
