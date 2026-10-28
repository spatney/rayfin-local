//! `@role` policies as a typed expression tree.
//!
//! The 1.36.2 policy DSL (`@microsoft/rayfin-core` `policy.js`) builds `ComparisonExpression`s
//! (`eq`, and `ne` from `.neq()`) over `claims.sub|email|role`, `item.<field>` and literals
//! (string, number, boolean, `Date`, `null`), combined by `LogicalExpression`s (`and`, `or`).
//! There is no `not` and no ordering comparison. rayfin-core's `serializeCheckToAst` turns that
//! tree into a JSON "check" AST, which [`PolicyExpr::from_check`] reads; anything outside that
//! grammar is an error, never ignored.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A boolean policy expression.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyExpr {
    /// True when every operand is (at least two operands).
    And(Vec<PolicyExpr>),
    /// True when any operand is (at least two operands).
    Or(Vec<PolicyExpr>),
    /// `left op right`. DAB compares with SQL semantics: a `null` operand never matches, so
    /// `@item.x eq null` is never true (use the result as a SQL predicate as-is).
    Compare {
        op: CompareOp,
        left: Operand,
        right: Operand,
    },
}

/// A comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    /// `eq`.
    Eq,
    /// `ne` (the DSL's `.neq()`; `neq` in the check AST).
    Ne,
}

/// One side of a comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operand {
    /// A claim of the caller's token (`@claims.sub`); 1.36.2 offers `sub`, `email` and `role`.
    Claim(String),
    /// A field of the row (`@item.user_id`), by its exposed (GraphQL) name.
    Field(String),
    /// A constant.
    Literal(Literal),
}

/// A constant in a policy.
///
/// A `Date` in the DSL arrives as its ISO-8601 string (as in the storage check AST); the policy
/// text keeps it unquoted (`@item.due ne 2020-01-01T00:00:00.000Z`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Literal {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
}

/// Why a check AST node is outside the 1.36.2 policy grammar.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{reason}")]
pub struct PolicyAstError {
    pub reason: String,
}

fn unsupported(reason: impl Into<String>) -> PolicyAstError {
    PolicyAstError {
        reason: reason.into(),
    }
}

impl PolicyExpr {
    /// Reads a rayfin-core check AST (`CheckNode`): `{op: "and"|"or", args: [node, node, ...]}`
    /// or `{op: "eq"|"neq", lhs: ref, rhs: ref | literal}`, where a ref is `{claim}` or
    /// `{field}`.
    pub fn from_check(node: &Value) -> Result<Self, PolicyAstError> {
        let object = node
            .as_object()
            .ok_or_else(|| unsupported(format!("expected a policy node, found {node}")))?;
        let op = object
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| unsupported(format!("policy node without an `op`: {node}")))?;
        match op {
            "and" | "or" => {
                expect_keys(object, &["op", "args"], node)?;
                let args = object
                    .get("args")
                    .and_then(Value::as_array)
                    .ok_or_else(|| unsupported(format!("`{op}` without `args`: {node}")))?;
                if args.len() < 2 {
                    return Err(unsupported(format!(
                        "`{op}` needs at least two operands: {node}"
                    )));
                }
                let args = args
                    .iter()
                    .map(Self::from_check)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(if op == "and" {
                    Self::And(args)
                } else {
                    Self::Or(args)
                })
            }
            "eq" | "neq" => {
                expect_keys(object, &["op", "lhs", "rhs"], node)?;
                let left = reference(&object["lhs"])?.ok_or_else(|| {
                    unsupported(format!(
                        "the left side of `{op}` must be a claim or a field: {node}"
                    ))
                })?;
                let right = match reference(&object["rhs"])? {
                    Some(reference) => reference,
                    None => Operand::Literal(literal(&object["rhs"])?),
                };
                let op = if op == "eq" {
                    CompareOp::Eq
                } else {
                    CompareOp::Ne
                };
                Ok(Self::Compare { op, left, right })
            }
            other => Err(unsupported(format!(
                "unknown policy operator `{other}` (rayfin-local supports eq, neq, and, or): {node}"
            ))),
        }
    }

    /// Every `item.<field>` the expression reads.
    pub fn fields(&self) -> Vec<&str> {
        let mut fields = Vec::new();
        self.visit_operands(&mut |operand| {
            if let Operand::Field(name) = operand {
                fields.push(name.as_str());
            }
        });
        fields
    }

    /// Every `claims.<name>` the expression reads.
    pub fn claims(&self) -> Vec<&str> {
        let mut claims = Vec::new();
        self.visit_operands(&mut |operand| {
            if let Operand::Claim(name) = operand {
                claims.push(name.as_str());
            }
        });
        claims
    }

    fn visit_operands<'a>(&'a self, visit: &mut impl FnMut(&'a Operand)) {
        match self {
            Self::And(args) | Self::Or(args) => {
                for arg in args {
                    arg.visit_operands(visit);
                }
            }
            Self::Compare { left, right, .. } => {
                visit(left);
                visit(right);
            }
        }
    }
}

fn expect_keys(
    object: &Map<String, Value>,
    keys: &[&str],
    node: &Value,
) -> Result<(), PolicyAstError> {
    if let Some(extra) = object.keys().find(|key| !keys.contains(&key.as_str())) {
        return Err(unsupported(format!(
            "unknown policy node property `{extra}`: {node}"
        )));
    }
    if let Some(missing) = keys.iter().find(|key| !object.contains_key(**key)) {
        return Err(unsupported(format!(
            "policy node without `{missing}`: {node}"
        )));
    }
    Ok(())
}

/// `Some` for `{claim}` / `{field}`, `None` for anything that is not an object.
fn reference(value: &Value) -> Result<Option<Operand>, PolicyAstError> {
    let Some(object) = value.as_object() else {
        return Ok(None);
    };
    let mut entries = object.iter();
    match (entries.next(), entries.next()) {
        (Some((key, Value::String(name))), None) if key == "claim" => {
            Ok(Some(Operand::Claim(name.clone())))
        }
        (Some((key, Value::String(name))), None) if key == "field" => {
            Ok(Some(Operand::Field(name.clone())))
        }
        _ => Err(unsupported(format!(
            "expected `{{\"claim\": ...}}` or `{{\"field\": ...}}`, found {value}"
        ))),
    }
}

fn literal(value: &Value) -> Result<Literal, PolicyAstError> {
    match value {
        Value::Null => Ok(Literal::Null),
        Value::Bool(value) => Ok(Literal::Bool(*value)),
        Value::Number(value) => Ok(Literal::Number(value.clone())),
        Value::String(value) => Ok(Literal::String(value.clone())),
        other => Err(unsupported(format!("unsupported policy literal {other}"))),
    }
}

/// The DAB policy syntax rayfin-core writes (`(@claims.sub eq @item.user_id) and (...)`).
impl fmt::Display for PolicyExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::And(args) | Self::Or(args) => {
                let word = if matches!(self, Self::And(_)) {
                    "and"
                } else {
                    "or"
                };
                // rayfin-core only builds binary nodes; longer lists render as a left fold.
                let mut rendered = format!("{}", args[0]);
                for arg in &args[1..] {
                    rendered = format!("({rendered}) {word} ({arg})");
                }
                f.write_str(&rendered)
            }
            Self::Compare { op, left, right } => {
                let op = match op {
                    CompareOp::Eq => "eq",
                    CompareOp::Ne => "ne",
                };
                write!(f, "{left} {op} {right}")
            }
        }
    }
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Claim(name) => write!(f, "@claims.{name}"),
            Self::Field(name) => write!(f, "@item.{name}"),
            Self::Literal(Literal::Null) => f.write_str("null"),
            Self::Literal(Literal::Bool(value)) => write!(f, "{value}"),
            Self::Literal(Literal::Number(value)) => write!(f, "{value}"),
            Self::Literal(Literal::String(value)) => write!(f, "'{}'", value.replace('\'', "''")),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_every_node_of_the_dsl() {
        let check = json!({"op": "and", "args": [
            {"op": "eq", "lhs": {"claim": "sub"}, "rhs": {"field": "user_id"}},
            {"op": "or", "args": [
                {"op": "neq", "lhs": {"field": "status"}, "rhs": "it's done"},
                {"op": "eq", "lhs": {"field": "n"}, "rhs": 0.5},
                {"op": "eq", "lhs": {"field": "flag"}, "rhs": true},
                {"op": "eq", "lhs": {"field": "closed"}, "rhs": null},
                {"op": "eq", "lhs": {"field": "mail"}, "rhs": {"claim": "email"}}
            ]}
        ]});
        let expr = PolicyExpr::from_check(&check).unwrap();
        let PolicyExpr::And(args) = &expr else {
            panic!("expected and: {expr:?}")
        };
        assert_eq!(
            args[0],
            PolicyExpr::Compare {
                op: CompareOp::Eq,
                left: Operand::Claim("sub".into()),
                right: Operand::Field("user_id".into()),
            }
        );
        let PolicyExpr::Or(alternatives) = &args[1] else {
            panic!("expected or: {:?}", args[1])
        };
        assert_eq!(alternatives.len(), 5);
        assert!(matches!(
            &alternatives[0],
            PolicyExpr::Compare { op: CompareOp::Ne, right: Operand::Literal(Literal::String(s)), .. }
                if s == "it's done"
        ));
        assert!(matches!(
            &alternatives[3],
            PolicyExpr::Compare {
                right: Operand::Literal(Literal::Null),
                ..
            }
        ));
        assert_eq!(
            expr.fields(),
            ["user_id", "status", "n", "flag", "closed", "mail"]
        );
        assert_eq!(expr.claims(), ["sub", "email"]);
    }

    #[test]
    fn renders_the_dab_policy_syntax() {
        let check = json!({"op": "or", "args": [
            {"op": "eq", "lhs": {"field": "owner"}, "rhs": {"claim": "sub"}},
            {"op": "and", "args": [
                {"op": "neq", "lhs": {"field": "status"}, "rhs": "it's done"},
                {"op": "eq", "lhs": {"field": "flag"}, "rhs": false}
            ]}
        ]});
        let expr = PolicyExpr::from_check(&check).unwrap();
        assert_eq!(
            expr.to_string(),
            "(@item.owner eq @claims.sub) or ((@item.status ne 'it''s done') and (@item.flag eq false))"
        );
    }

    #[test]
    fn rejects_anything_outside_the_grammar() {
        let rejected = [
            json!({"op": "not", "args": [{"op": "eq", "lhs": {"claim": "sub"}, "rhs": "x"}]}),
            json!({"op": "gt", "lhs": {"field": "n"}, "rhs": 1}),
            json!({"op": "and", "args": [{"op": "eq", "lhs": {"claim": "sub"}, "rhs": "x"}]}),
            json!({"op": "eq", "lhs": "x", "rhs": {"claim": "sub"}}),
            json!({"op": "eq", "lhs": {"claim": "sub"}, "rhs": [1]}),
            json!({"op": "eq", "lhs": {"claim": "sub"}, "rhs": {"subquery": "x"}}),
            json!({"op": "eq", "lhs": {"claim": "sub", "field": "x"}, "rhs": 1}),
            json!({"op": "eq", "lhs": {"claim": "sub"}, "rhs": 1, "negate": true}),
            json!({"op": "eq", "lhs": {"claim": "sub"}}),
            json!({"lhs": {"claim": "sub"}, "rhs": 1}),
            json!("@claims.sub eq @item.user_id"),
        ];
        for check in rejected {
            let error = PolicyExpr::from_check(&check).unwrap_err();
            assert!(!error.reason.is_empty(), "{check}");
        }
    }
}
