//! T-SQL text for planned root fields: each one statement that builds its JSON with string
//! functions, as DAB's DWSQL query builder does, because Fabric Warehouse and SQL analytics
//! endpoints refuse `FOR JSON` in subqueries.
//!
//! - A row is `CAST(N'{' AS NVARCHAR(MAX)) + N'"key":' + ISNULL(<value>, N'null') + … + N'}'`;
//!   each value is the column as JSON text ([`json_value`]), `STRING_ESCAPE`d for text.
//! - A to-one relationship is an `OUTER APPLY (SELECT TOP (1) <row> …)`, a to-many one an
//!   `OUTER APPLY` that `STRING_AGG`s the rows of an ordered, limited derived table, so the
//!   correlation with the parent row stays inside the apply.
//! - Connection rows are `[<item>, …, <order-key values or null>]`, as the SQLite statements
//!   build them, so `exec`'s shaper reads both.
//! - Values from the plan (filters, keys, cursors, policies) are SQLite storage values; they are
//!   bound as parameters of the type of the column they meet ([`Emitter::value`]).
//!
//! Dates come out as SQL Server writes them (`CONVERT` style 126/127) and are normalized to DAB's
//! format afterwards ([`normalize_connection`], [`normalize_object`]).

use std::collections::HashMap;

use rl_db::codec;
use rl_model::connector::split_source;
use rl_model::{AppModel, Column, FieldKind};
use rusqlite::types::Value as SqlValue;
use serde_json::Value as Json;

use super::backend::{Param, Statement};
use crate::plan::{ConnectionPlan, FieldPlan, ObjectPlan};
use crate::sql::{CmpOp, Operand, Pred};

/// `[name]`.
pub(crate) fn ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// `[schema].[object]` of a DAB source such as `dbo.orders`.
pub(crate) fn source(table: &str) -> String {
    let (schema, object) = split_source(table);
    format!("{}.{}", ident(schema), ident(object))
}

/// `alias.[column]`.
pub(crate) fn column_ref(alias: &str, column: &str) -> String {
    format!("{alias}.{}", ident(column))
}

/// An `N'…'` literal.
pub(crate) fn literal(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

/// The SQL type of a connector column, lowercase: the database's (`metadata.json`), else the
/// DAB `x-schema` type without its length.
fn data_type(column: &Column) -> String {
    match &column.source {
        Some(source) => source.data_type.clone(),
        None => column
            .db_type
            .split('(')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase(),
    }
}

/// A T-SQL expression giving `column` (at `sql`) as JSON text, `NULL` when it is `NULL`. Dates
/// keep the database's precision and format (normalized in Rust).
pub(crate) fn json_value(column: &Column, sql: &str) -> String {
    let quoted = |inner: String| format!("N'\"' + {inner} + N'\"'");
    let ty = data_type(column);
    match &column.kind {
        FieldKind::Boolean => {
            format!(
                "CASE WHEN {sql} IS NULL THEN NULL WHEN {sql} = 0 THEN N'false' ELSE N'true' END"
            )
        }
        FieldKind::Int { .. } => format!("CONVERT(NVARCHAR(40), {sql})"),
        FieldKind::Decimal { .. } => match ty.as_str() {
            "float" | "real" => format!("CONVERT(NVARCHAR(40), {sql}, 3)"),
            "money" | "smallmoney" => format!("CONVERT(NVARCHAR(40), {sql}, 2)"),
            _ => format!("CONVERT(NVARCHAR(60), {sql})"),
        },
        FieldKind::Date => match ty.as_str() {
            "date" => quoted(format!("CONVERT(NVARCHAR(10), {sql}, 23)")),
            "datetimeoffset" => quoted(format!("CONVERT(NVARCHAR(40), {sql}, 127)")),
            "time" => quoted(format!("CONVERT(NVARCHAR(20), {sql})")),
            _ => quoted(format!(
                "CONVERT(NVARCHAR(40), CAST({sql} AS DATETIME2(7)), 126) + N'Z'"
            )),
        },
        FieldKind::Uuid if matches!(ty.as_str(), "uniqueidentifier" | "") => {
            quoted(format!("LOWER(CONVERT(NVARCHAR(36), {sql}))"))
        }
        FieldKind::Uuid
        | FieldKind::Text { .. }
        | FieldKind::Email { .. }
        | FieldKind::Set { .. } => quoted(format!(
            "STRING_ESCAPE(CONVERT(NVARCHAR(MAX), {sql}), 'json')"
        )),
    }
}

/// Builds one statement: its text from the methods below, its parameters as they bind.
pub(crate) struct Emitter<'m> {
    model: &'m AppModel,
    params: Vec<Param>,
    /// Table alias → entity, for the type of the values compared with its columns.
    aliases: HashMap<String, usize>,
    applies: usize,
}

impl<'m> Emitter<'m> {
    pub(crate) fn new(model: &'m AppModel) -> Self {
        Self {
            model,
            params: Vec::new(),
            aliases: HashMap::new(),
            applies: 0,
        }
    }

    pub(crate) fn finish(self, text: String) -> Statement {
        Statement {
            text,
            params: self.params,
        }
    }

    /// Records that `alias` is a row of `entity`.
    pub(crate) fn register(&mut self, alias: &str, entity: usize) {
        self.aliases.insert(alias.to_owned(), entity);
    }

    /// Binds `param`; returns its placeholder (cast for date-times).
    pub(crate) fn bind(&mut self, param: Param) -> String {
        let datetime = matches!(param, Param::DateTime(_));
        self.params.push(param);
        let placeholder = format!("@P{}", self.params.len());
        if datetime {
            format!("CAST({placeholder} AS DATETIME2(7))")
        } else {
            placeholder
        }
    }

    /// Binds a storage `value` as a parameter of the type of `column`.
    pub(crate) fn value(&mut self, column: Option<&Column>, value: &SqlValue) -> String {
        let param = to_param(column.map(|c| &c.kind), value);
        self.bind(param)
    }

    fn column_of(&self, operand: &Operand) -> Option<&'m Column> {
        let Operand::Column { alias, column } = operand else {
            return None;
        };
        let entity = &self.model.entities[*self.aliases.get(alias)?];
        entity.column(column)
    }

    fn operand(&mut self, operand: &Operand, other: &Operand) -> String {
        match operand {
            Operand::Column { alias, column } => column_ref(alias, column),
            Operand::Value(value) => {
                let column = self.column_of(other);
                self.value(column, value)
            }
        }
    }

    /// A predicate as T-SQL.
    pub(crate) fn pred(&mut self, pred: &Pred) -> String {
        match pred {
            Pred::Const(true) => "1 = 1".to_owned(),
            Pred::Const(false) => "1 = 0".to_owned(),
            Pred::And(preds) | Pred::Or(preds) => {
                let and = matches!(pred, Pred::And(_));
                if preds.is_empty() {
                    return if and { "1 = 1" } else { "1 = 0" }.to_owned();
                }
                let parts: Vec<String> = preds.iter().map(|p| self.pred(p)).collect();
                format!("({})", parts.join(if and { " AND " } else { " OR " }))
            }
            Pred::Cmp(left, op, right) => {
                let null_value = |o: &Operand| matches!(o, Operand::Value(SqlValue::Null));
                if *op == CmpOp::Is && (null_value(left) || null_value(right)) {
                    let operand = if null_value(left) { right } else { left };
                    let sql = self.operand(operand, operand);
                    return format!("({sql} IS NULL)");
                }
                let l = self.operand(left, right);
                let r = self.operand(right, left);
                let op = match op {
                    CmpOp::Eq | CmpOp::Is => "=",
                    CmpOp::Ne => "<>",
                    CmpOp::Gt => ">",
                    CmpOp::Ge => ">=",
                    CmpOp::Lt => "<",
                    CmpOp::Le => "<=",
                };
                format!("({l} {op} {r})")
            }
            Pred::Like {
                operand,
                pattern,
                negated,
            } => {
                let mut sql = self.operand(operand, operand);
                if self
                    .column_of(operand)
                    .is_some_and(|c| c.kind == FieldKind::Uuid)
                {
                    sql = format!("CONVERT(NVARCHAR(36), {sql})");
                }
                let pattern = self.bind(Param::Text(pattern.clone()));
                let not = if *negated { "NOT " } else { "" };
                format!("({sql} {not}LIKE {pattern} ESCAPE '\\')")
            }
            Pred::IsNull { operand, negated } => {
                let sql = self.operand(operand, operand);
                let not = if *negated { "NOT " } else { "" };
                format!("({sql} IS {not}NULL)")
            }
            Pred::In { operand, values } => {
                let sql = self.operand(operand, operand);
                let column = self.column_of(operand);
                let values: Vec<String> = values.iter().map(|v| self.value(column, v)).collect();
                format!("({sql} IN ({}))", values.join(", "))
            }
            Pred::Exists { table, alias, pred } => {
                if let Some(index) = self.model.entities.iter().position(|e| &e.table == table) {
                    self.register(alias, index);
                }
                let inner = self.pred(pred);
                format!(
                    "EXISTS (SELECT 1 FROM {} AS {alias} WHERE {inner})",
                    source(table)
                )
            }
        }
    }

    fn apply_alias(&mut self) -> String {
        self.applies += 1;
        format!("[a{}]", self.applies)
    }

    /// `ORDER BY` keys of a connection.
    fn order_by(&self, plan: &ConnectionPlan) -> String {
        let entity = &self.model.entities[plan.entity];
        plan.order
            .iter()
            .map(|key| {
                let column = column_ref(&plan.alias, &entity.columns[key.column].name);
                format!("{column} {}", if key.desc { "DESC" } else { "ASC" })
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// A connection's rows: `SELECT TOP (n) <row> AS [j] … ORDER BY …`, numbered (`[n]`) for
    /// aggregating them in order. A keyless entity read without `orderBy` has no order.
    pub(crate) fn connection_select(&mut self, plan: &ConnectionPlan, numbered: bool) -> String {
        self.register(&plan.alias, plan.entity);
        let mut applies = String::new();
        let row = self.row_json(plan, &mut applies);
        let predicate = self.pred(&plan.predicate);
        let order = self.order_by(plan);
        let limit = plan.limit + u64::from(plan.probe);
        let number = match (numbered, order.is_empty()) {
            (false, _) => String::new(),
            (true, false) => format!(", ROW_NUMBER() OVER (ORDER BY {order}) AS [n]"),
            (true, true) => ", ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS [n]".to_owned(),
        };
        let order_by = if order.is_empty() {
            String::new()
        } else {
            format!(" ORDER BY {order}")
        };
        format!(
            "SELECT TOP ({limit}) {row} AS [j]{number} FROM {} AS {}{applies} WHERE \
             {predicate}{order_by}",
            source(&self.model.entities[plan.entity].table),
            plan.alias
        )
    }

    /// The row of `plan` matched by `filter` (a by-key read).
    pub(crate) fn object_select(&mut self, plan: &ObjectPlan, filter: &Pred) -> String {
        self.register(&plan.alias, plan.entity);
        let mut applies = String::new();
        let object = self.object_json(plan, &mut applies);
        let predicate = self.pred(filter);
        format!(
            "SELECT TOP (1) {object} AS [j] FROM {} AS {}{applies} WHERE {predicate}",
            source(&self.model.entities[plan.entity].table),
            plan.alias
        )
    }

    /// `[<item>, …, <cursor>]` of a connection row.
    fn row_json(&mut self, plan: &ConnectionPlan, applies: &mut String) -> String {
        let mut parts = Vec::with_capacity(plan.items.len() + 1);
        for item in &plan.items {
            parts.push(self.object_json(item, applies));
        }
        if plan.cursor {
            let entity = &self.model.entities[plan.entity];
            let values: Vec<String> = plan
                .order
                .iter()
                .map(|key| {
                    let column = &entity.columns[key.column];
                    let sql = column_ref(&plan.alias, &column.name);
                    format!("ISNULL({}, N'null')", json_value(column, &sql))
                })
                .collect();
            parts.push(format!(
                "N'[' + {} + N']'",
                if values.is_empty() {
                    "N''".to_owned()
                } else {
                    values.join(" + N',' + ")
                }
            ));
        } else {
            parts.push("N'null'".to_owned());
        }
        format!(
            "(CAST(N'[' AS NVARCHAR(MAX)) + {} + N']')",
            parts.join(" + N',' + ")
        )
    }

    /// `{"key": value, …}` of an object selection (forbidden fields and `__typename` are the
    /// shaper's).
    fn object_json(&mut self, plan: &ObjectPlan, applies: &mut String) -> String {
        self.register(&plan.alias, plan.entity);
        let entity = &self.model.entities[plan.entity];
        let mut members = Vec::new();
        for field in &plan.fields {
            match field {
                FieldPlan::Column { key, column, .. } => {
                    let column = &entity.columns[*column];
                    let sql = column_ref(&plan.alias, &column.name);
                    members.push((
                        key,
                        format!("ISNULL({}, N'null')", json_value(column, &sql)),
                    ));
                }
                FieldPlan::ToOne {
                    key,
                    child,
                    predicate,
                    ..
                } => {
                    self.register(&child.alias, child.entity);
                    let mut child_applies = String::new();
                    let object = self.object_json(child, &mut child_applies);
                    let predicate = self.pred(predicate);
                    let apply = self.apply_alias();
                    applies.push_str(&format!(
                        " OUTER APPLY (SELECT TOP (1) {object} AS [j] FROM {} AS \
                         {}{child_applies} WHERE {predicate}) AS {apply}",
                        source(&self.model.entities[child.entity].table),
                        child.alias
                    ));
                    members.push((key, format!("ISNULL({apply}.[j], N'null')")));
                }
                FieldPlan::ToMany {
                    key, connection, ..
                } => {
                    let rows = self.connection_select(connection, true);
                    let apply = self.apply_alias();
                    let x = format!("[x{}]", self.applies);
                    applies.push_str(&format!(
                        " OUTER APPLY (SELECT CAST(N'[' AS NVARCHAR(MAX)) + STRING_AGG({x}.[j], \
                         N',') WITHIN GROUP (ORDER BY {x}.[n]) + N']' AS [j] FROM ({rows}) AS \
                         {x}) AS {apply}"
                    ));
                    members.push((key, format!("ISNULL({apply}.[j], N'[]')")));
                }
                FieldPlan::Typename { .. } | FieldPlan::Forbidden { .. } => {}
            }
        }
        if members.is_empty() {
            return "CAST(N'{}' AS NVARCHAR(MAX))".to_owned();
        }
        let body = members
            .iter()
            .enumerate()
            .map(|(i, (key, value))| {
                let separator = if i == 0 { "" } else { "," };
                format!("{} + {value}", literal(&format!("{separator}\"{key}\":")))
            })
            .collect::<Vec<_>>()
            .join(" + ");
        format!("(CAST(N'{{' AS NVARCHAR(MAX)) + {body} + N'}}')")
    }
}

/// A storage value (`rl_db::codec`) as a parameter for a column of `kind` (`None`: unknown).
pub(crate) fn to_param(kind: Option<&FieldKind>, value: &SqlValue) -> Param {
    match (kind, value) {
        (_, SqlValue::Null) => Param::Null,
        (Some(FieldKind::Date), SqlValue::Text(text)) => {
            Param::DateTime(text.trim_end_matches('Z').to_owned())
        }
        (Some(FieldKind::Boolean), SqlValue::Integer(n)) => Param::Bool(*n != 0),
        (Some(FieldKind::Decimal { .. }), SqlValue::Real(n)) => decimal_param(*n),
        (Some(FieldKind::Decimal { .. }), SqlValue::Integer(n)) => Param::Decimal {
            value: i128::from(*n),
            scale: 0,
        },
        (_, SqlValue::Integer(n)) => Param::Int(*n),
        (_, SqlValue::Real(n)) => Param::Float(*n),
        (_, SqlValue::Text(text)) => Param::Text(text.clone()),
        (_, SqlValue::Blob(bytes)) => Param::Text(String::from_utf8_lossy(bytes).into_owned()),
    }
}

/// A double as the exact decimal of its shortest representation (the digits a client wrote).
fn decimal_param(value: f64) -> Param {
    let text = format!("{value}");
    let (sign, digits) = match text.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, text.as_str()),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let fraction = &fraction[..fraction.len().min(37)];
    match format!("{whole}{fraction}").parse::<i128>() {
        Ok(mantissa) if value.is_finite() && whole.len() + fraction.len() <= 38 => Param::Decimal {
            value: sign * mantissa,
            scale: fraction.len() as u8,
        },
        _ => Param::Float(value),
    }
}

/// Whether a connection (or one nested in its items) aggregates (`groupBy`).
pub(crate) fn uses_group_by(plan: &ConnectionPlan) -> bool {
    crate::aggregate::is_group_by(plan) || plan.items.iter().any(object_uses_group_by)
}

pub(crate) fn object_uses_group_by(plan: &ObjectPlan) -> bool {
    plan.fields.iter().any(|field| match field {
        FieldPlan::ToOne { child, .. } => object_uses_group_by(child),
        FieldPlan::ToMany { connection, .. } => uses_group_by(connection),
        _ => false,
    })
}

/// Rewrites the dates of a connection row (`[items…, cursor]`) as DAB writes them, and its
/// cursor's at the stored precision.
pub(crate) fn normalize_connection(model: &AppModel, plan: &ConnectionPlan, row: &mut Json) {
    let Some(values) = row.as_array_mut() else {
        return;
    };
    for (item, value) in plan.items.iter().zip(values.iter_mut()) {
        normalize_object(model, item, value);
    }
    let entity = &model.entities[plan.entity];
    if let Some(Json::Array(cursor)) = values.get_mut(plan.items.len()) {
        for (key, value) in plan.order.iter().zip(cursor.iter_mut()) {
            if entity.columns[key.column].kind == FieldKind::Date {
                normalize_date(value, false);
            }
        }
    }
}

/// Rewrites the dates of an object as DAB writes them.
pub(crate) fn normalize_object(model: &AppModel, plan: &ObjectPlan, object: &mut Json) {
    let entity = &model.entities[plan.entity];
    let Some(fields) = object.as_object_mut() else {
        return;
    };
    for field in &plan.fields {
        match field {
            FieldPlan::Column { key, column, .. }
                if entity.columns[*column].kind == FieldKind::Date =>
            {
                if let Some(value) = fields.get_mut(key) {
                    normalize_date(value, true);
                }
            }
            FieldPlan::ToOne { key, child, .. } => {
                if let Some(value) = fields.get_mut(key) {
                    normalize_object(model, child, value);
                }
            }
            FieldPlan::ToMany {
                key, connection, ..
            } => {
                if let Some(Json::Array(rows)) = fields.get_mut(key) {
                    for row in rows {
                        normalize_connection(model, connection, row);
                    }
                }
            }
            _ => {}
        }
    }
}

/// A date as DAB writes it (`output`) or as the cursor carries it (100 ns); left alone when it
/// isn't a date-time (a `time` column).
fn normalize_date(value: &mut Json, output: bool) {
    if let Json::String(text) = value
        && let Some(stored) = codec::normalize_datetime(text)
    {
        *text = if output {
            codec::datetime_output(&stored)
        } else {
            stored
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_and_sources_are_bracketed() {
        assert_eq!(ident("a]b"), "[a]]b]");
        assert_eq!(source("sales.order items"), "[sales].[order items]");
        assert_eq!(source("orders"), "[dbo].[orders]");
        assert_eq!(literal("it's"), "N'it''s'");
    }

    #[test]
    fn decimals_bind_exactly() {
        assert_eq!(
            decimal_param(12.105),
            Param::Decimal {
                value: 12105,
                scale: 3
            }
        );
        assert_eq!(
            decimal_param(-0.5),
            Param::Decimal {
                value: -5,
                scale: 1
            }
        );
        assert_eq!(
            decimal_param(100.0),
            Param::Decimal {
                value: 100,
                scale: 0
            }
        );
        assert_eq!(decimal_param(1e300), Param::Float(1e300));
    }

    #[test]
    fn storage_values_bind_as_their_columns_type() {
        let date = FieldKind::Date;
        assert_eq!(
            to_param(
                Some(&date),
                &SqlValue::Text("2024-01-02T03:04:05.1234567Z".into())
            ),
            Param::DateTime("2024-01-02T03:04:05.1234567".into())
        );
        assert_eq!(
            to_param(Some(&FieldKind::Boolean), &SqlValue::Integer(1)),
            Param::Bool(true)
        );
        assert_eq!(to_param(None, &SqlValue::Integer(7)), Param::Int(7));
        assert_eq!(
            to_param(Some(&FieldKind::Uuid), &SqlValue::Text("ab".into())),
            Param::Text("ab".into())
        );
    }
}
