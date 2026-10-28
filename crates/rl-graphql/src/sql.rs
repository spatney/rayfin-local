//! SQL building blocks: parameterized SQL text, predicates and their rendering.
//!
//! Every statement is written front to back into one [`Sql`], so the `?` placeholders and
//! [`Sql::params`] stay in step. Identifiers are quoted with `rl_db`'s quoting; values are always
//! bound, never inlined.

use rusqlite::types::Value as SqlValue;

pub use rl_db::schema::quote_ident;

/// SQL text with its positional parameters.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sql {
    pub text: String,
    pub params: Vec<SqlValue>,
}

impl Sql {
    pub fn push(&mut self, text: &str) -> &mut Self {
        self.text.push_str(text);
        self
    }

    /// Appends a `?` bound to `value`.
    pub fn param(&mut self, value: SqlValue) -> &mut Self {
        self.text.push('?');
        self.params.push(value);
        self
    }

    /// Appends a string literal as a bound parameter (JSON keys, for instance).
    pub fn text_param(&mut self, value: &str) -> &mut Self {
        self.param(SqlValue::Text(value.to_owned()))
    }
}

/// `alias."column"`.
pub fn column_sql(alias: &str, column: &str) -> String {
    format!("{alias}.{}", quote_ident(column))
}

/// One side of a comparison.
#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    /// A stored column of a table alias (`t0."user_id"`).
    Column { alias: String, column: String },
    /// A bound value, already in the column's storage format.
    Value(SqlValue),
}

impl Operand {
    pub fn column(alias: &str, column: &str) -> Self {
        Self::Column {
            alias: alias.to_owned(),
            column: column.to_owned(),
        }
    }

    fn render(&self, sql: &mut Sql) {
        match self {
            Self::Column { alias, column } => {
                sql.push(&column_sql(alias, column));
            }
            Self::Value(value) => {
                sql.param(value.clone());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    /// Null-safe equality (`IS`).
    Is,
}

impl CmpOp {
    fn sql(self) -> &'static str {
        match self {
            Self::Eq => " = ",
            Self::Ne => " != ",
            Self::Gt => " > ",
            Self::Ge => " >= ",
            Self::Lt => " < ",
            Self::Le => " <= ",
            Self::Is => " IS ",
        }
    }
}

/// A boolean SQL predicate. Comparisons follow SQL semantics (a `NULL` operand is never true),
/// as on Fabric SQL; text columns compare with their `NOCASE` collation.
#[derive(Clone, Debug, PartialEq)]
pub enum Pred {
    Const(bool),
    And(Vec<Pred>),
    Or(Vec<Pred>),
    Cmp(Operand, CmpOp, Operand),
    /// `LIKE` with `\` as the escape character; `pattern` is already escaped.
    Like {
        operand: Operand,
        pattern: String,
        negated: bool,
    },
    IsNull {
        operand: Operand,
        negated: bool,
    },
    In {
        operand: Operand,
        values: Vec<SqlValue>,
    },
    /// `EXISTS (SELECT 1 FROM table AS alias WHERE pred)`.
    Exists {
        table: String,
        alias: String,
        pred: Box<Pred>,
    },
}

impl Pred {
    /// `AND` of `preds`, dropping `true`s; `true` when empty.
    pub fn all(preds: impl IntoIterator<Item = Pred>) -> Pred {
        let preds: Vec<_> = preds
            .into_iter()
            .filter(|p| *p != Pred::Const(true))
            .collect();
        if preds.contains(&Pred::Const(false)) {
            return Pred::Const(false);
        }
        match preds.len() {
            0 => Pred::Const(true),
            1 => preds.into_iter().next().unwrap(),
            _ => Pred::And(preds),
        }
    }

    pub fn render(&self, sql: &mut Sql) {
        match self {
            Self::Const(true) => {
                sql.push("1");
            }
            Self::Const(false) => {
                sql.push("0");
            }
            Self::And(preds) | Self::Or(preds) => {
                if preds.is_empty() {
                    sql.push(if matches!(self, Self::And(_)) {
                        "1"
                    } else {
                        "0"
                    });
                    return;
                }
                let joiner = if matches!(self, Self::And(_)) {
                    " AND "
                } else {
                    " OR "
                };
                sql.push("(");
                for (i, pred) in preds.iter().enumerate() {
                    if i > 0 {
                        sql.push(joiner);
                    }
                    pred.render(sql);
                }
                sql.push(")");
            }
            Self::Cmp(left, op, right) => {
                sql.push("(");
                left.render(sql);
                sql.push(op.sql());
                right.render(sql);
                sql.push(")");
            }
            Self::Like {
                operand,
                pattern,
                negated,
            } => {
                sql.push("(");
                operand.render(sql);
                sql.push(if *negated { " NOT LIKE " } else { " LIKE " });
                sql.text_param(pattern);
                sql.push(" ESCAPE '\\')");
            }
            Self::IsNull { operand, negated } => {
                sql.push("(");
                operand.render(sql);
                sql.push(if *negated {
                    " IS NOT NULL)"
                } else {
                    " IS NULL)"
                });
            }
            Self::In { operand, values } => {
                sql.push("(");
                operand.render(sql);
                sql.push(" IN (");
                for (i, value) in values.iter().enumerate() {
                    if i > 0 {
                        sql.push(", ");
                    }
                    sql.param(value.clone());
                }
                sql.push("))");
            }
            Self::Exists { table, alias, pred } => {
                sql.push("EXISTS (SELECT 1 FROM ")
                    .push(&quote_ident(table))
                    .push(" AS ")
                    .push(alias)
                    .push(" WHERE ");
                pred.render(sql);
                sql.push(")");
            }
        }
    }
}

/// DAB's `EscapeLikeString`: `\`, `%`, `[`, `]` and `_` are escaped with `\` (SQLite's `LIKE`
/// treats `\[` as a literal `[` under `ESCAPE '\'`).
pub fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if matches!(ch, '\\' | '%' | '[' | ']' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_predicates_with_parameters_in_order() {
        let pred = Pred::And(vec![
            Pred::Cmp(
                Operand::column("t0", "title"),
                CmpOp::Eq,
                Operand::Value(SqlValue::Text("a".into())),
            ),
            Pred::Or(vec![
                Pred::Like {
                    operand: Operand::column("t0", "slug"),
                    pattern: format!("%{}%", escape_like("50%_[x]\\")),
                    negated: true,
                },
                Pred::In {
                    operand: Operand::column("t0", "n"),
                    values: vec![SqlValue::Integer(1), SqlValue::Integer(2)],
                },
            ]),
        ]);
        let mut sql = Sql::default();
        pred.render(&mut sql);
        assert_eq!(
            sql.text,
            "((t0.\"title\" = ?) AND ((t0.\"slug\" NOT LIKE ? ESCAPE '\\') OR (t0.\"n\" IN (?, ?))))"
        );
        assert_eq!(
            sql.params[1],
            SqlValue::Text("%50\\%\\_\\[x\\]\\\\%".into())
        );
        assert_eq!(sql.params.len(), 4);
    }
}
