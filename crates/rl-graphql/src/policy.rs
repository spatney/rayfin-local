//! `@role` policies (DAB `policy.database`) compiled to SQL predicates.
//!
//! DAB substitutes the caller's claims into the policy and adds it to the statement's `WHERE`,
//! so a policy filters rows with SQL semantics. Here [`compile`] turns a [`PolicyExpr`] into a
//! [`Pred`] with the claims (and literals) bound as parameters, converted to the storage format
//! of the field they are compared with (`rl_db::codec`): a claim compared with a uuid field is
//! parsed as a uuid, a `Date` literal as a date-time, `true` becomes `1`, and so on. A value that
//! does not convert matches nothing, as a `NULL` would.
//!
//! - **read** (this crate): [`read_filter`] gives the predicate ANDed into every `SELECT` of the
//!   entity (lists, by-key reads, nested relationships, relationship filters).
//! - **update/delete** (mutations): `compile` the grant's policy against the table alias and
//!   add it to the statement's `WHERE`.
//! - **create** (mutations): `compile` with a row source that returns the candidate values
//!   ([`Operand::Value`]) instead of columns, and evaluate the predicate (`SELECT <pred>`)
//!   before inserting.
//!
//! A policy that reads a claim the caller does not have fails the whole operation with DAB's
//! [`PolicyError::MissingClaim`] (`AuthorizationCheckFailed`), anonymous callers included.

use rl_db::codec;
use rl_model::{Action, Caller, Column, CompareOp, Entity, FieldKind, Literal, Operand as Op};
use rl_model::{Policy, PolicyExpr};
use rusqlite::types::Value as SqlValue;
use serde_json::Value as Json;

use crate::access;
use crate::error::{GraphQlError, codes, messages};
use crate::sql::{CmpOp, Operand, Pred};

/// Why a policy cannot be applied for this caller.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// The policy reads `claims.<0>`, which the caller's token does not have.
    #[error("{}", messages::MISSING_CLAIMS)]
    MissingClaim(String),
    /// The policy names a field the entity does not have (the model rejects this).
    #[error("the policy reads `item.{0}`, which is not a field of the entity")]
    UnknownField(String),
}

impl From<PolicyError> for GraphQlError {
    fn from(error: PolicyError) -> Self {
        match error {
            PolicyError::MissingClaim(_) => {
                GraphQlError::dab(messages::MISSING_CLAIMS, codes::AUTHORIZATION_CHECK_FAILED)
            }
            PolicyError::UnknownField(_) => {
                GraphQlError::dab(messages::DATABASE_ERROR, codes::UNEXPECTED_ERROR)
            }
        }
    }
}

/// The read predicate of `entity` for `caller` against the rows of table alias `alias`:
/// `Ok(None)` when the role may read every row. Callers check first that the role may read the
/// entity at all ([`access::grant`]).
pub fn read_filter(
    entity: &Entity,
    caller: &Caller,
    alias: &str,
) -> Result<Option<Pred>, PolicyError> {
    action_filter(entity, caller, Action::Read, alias)
}

/// The predicate of the grant `caller` acts under for `action`, against table alias `alias`.
pub fn action_filter(
    entity: &Entity,
    caller: &Caller,
    action: Action,
    alias: &str,
) -> Result<Option<Pred>, PolicyError> {
    let Some(policy) = access::grant(entity, caller.role(), action).and_then(|g| g.policy.as_ref())
    else {
        return Ok(None);
    };
    compile_policy(policy, entity, caller, &|column: &Column| {
        Operand::column(alias, &column.name)
    })
    .map(Some)
}

/// [`compile`] for a model [`Policy`].
pub fn compile_policy(
    policy: &Policy,
    entity: &Entity,
    caller: &Caller,
    row: &dyn Fn(&Column) -> Operand,
) -> Result<Pred, PolicyError> {
    compile(&policy.expr, entity, caller, row)
}

/// Compiles `expr` for `caller`; `row` gives the SQL operand of an `@item.<field>` (a column of
/// the statement's table alias, or a candidate value).
pub fn compile(
    expr: &PolicyExpr,
    entity: &Entity,
    caller: &Caller,
    row: &dyn Fn(&Column) -> Operand,
) -> Result<Pred, PolicyError> {
    match expr {
        PolicyExpr::And(args) | PolicyExpr::Or(args) => {
            let preds = args
                .iter()
                .map(|arg| compile(arg, entity, caller, row))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(if matches!(expr, PolicyExpr::And(_)) {
                Pred::all(preds)
            } else {
                Pred::Or(preds)
            })
        }
        PolicyExpr::Compare { op, left, right } => {
            let left = side(left, entity, caller)?;
            let right = side(right, entity, caller)?;
            let cmp = match op {
                CompareOp::Eq => CmpOp::Eq,
                CompareOp::Ne => CmpOp::Ne,
            };
            Ok(match (left, right) {
                (Side::Field(a), Side::Field(b)) => Pred::Cmp(row(a), cmp, row(b)),
                (Side::Field(column), Side::Value(value)) => match stored(&column.kind, &value) {
                    Some(value) => Pred::Cmp(row(column), cmp, Operand::Value(value)),
                    None => Pred::Const(false),
                },
                (Side::Value(value), Side::Field(column)) => match stored(&column.kind, &value) {
                    Some(value) => Pred::Cmp(Operand::Value(value), cmp, row(column)),
                    None => Pred::Const(false),
                },
                (Side::Value(a), Side::Value(b)) => Pred::Const(constants_compare(&a, &b, cmp)),
            })
        }
    }
}

enum Side<'m> {
    Field(&'m Column),
    Value(Json),
}

fn side<'m>(operand: &Op, entity: &'m Entity, caller: &Caller) -> Result<Side<'m>, PolicyError> {
    Ok(match operand {
        Op::Field(name) => Side::Field(
            entity
                .field(name)
                .ok_or_else(|| PolicyError::UnknownField(name.clone()))?,
        ),
        Op::Claim(name) => Side::Value(
            caller
                .claim(name)
                .cloned()
                .ok_or_else(|| PolicyError::MissingClaim(name.clone()))?,
        ),
        Op::Literal(literal) => Side::Value(match literal {
            Literal::Null => Json::Null,
            Literal::Bool(value) => Json::Bool(*value),
            Literal::Number(value) => Json::Number(value.clone()),
            Literal::String(value) => Json::String(value.clone()),
        }),
    })
}

/// A claim or literal in the storage format of a column of `kind`; `None` when it cannot be one
/// (or is `null`), so the comparison is never true.
fn stored(kind: &FieldKind, value: &Json) -> Option<SqlValue> {
    let converted = match value {
        Json::String(text) if !codec::is_textual(kind) => codec::parse_text(kind, text).ok(),
        Json::String(_) => codec::encode_operand(kind, value).ok(),
        Json::Number(_) | Json::Bool(_) => codec::encode_operand(kind, value)
            .or_else(|_| codec::parse_text(kind, &value.to_string()))
            .ok(),
        Json::Null | Json::Array(_) | Json::Object(_) => None,
    };
    converted.filter(|value| *value != SqlValue::Null)
}

/// A comparison of two constants (claims and literals), as Fabric SQL evaluates it: `NULL` never
/// matches and text compares case-insensitively.
fn constants_compare(a: &Json, b: &Json, op: CmpOp) -> bool {
    let text = |value: &Json| match value {
        Json::String(s) => Some(s.to_lowercase()),
        Json::Number(n) => Some(n.to_string()),
        Json::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let equal = match (a, b) {
        (Json::Number(x), Json::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => Some(x == y),
            _ => None,
        },
        _ => match (text(a), text(b)) {
            (Some(x), Some(y)) => Some(x == y),
            _ => None,
        },
    };
    match (equal, op) {
        (Some(equal), CmpOp::Eq) => equal,
        (Some(equal), CmpOp::Ne) => !equal,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use rl_model::{AppModel, Session};
    use serde_json::json;

    use super::*;
    use crate::sql::Sql;

    fn basic() -> AppModel {
        let json = include_str!("../../rl-model/tests/fixtures/basic.analysis.json");
        AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap()
    }

    fn caller(claims: Json) -> Caller {
        let Json::Object(claims) = claims else {
            panic!()
        };
        Caller::authenticated(Session {
            user_id: "u".into(),
            email: None,
            claims,
            access_token: "t".into(),
        })
    }

    fn render(pred: &Pred) -> Sql {
        let mut sql = Sql::default();
        pred.render(&mut sql);
        sql
    }

    #[test]
    fn read_policies_bind_typed_claims_and_literals() {
        let model = basic();
        let todo = model.entity("Todo").unwrap();
        let pred = read_filter(todo, &caller(json!({"sub": "abc"})), "t0")
            .unwrap()
            .unwrap();
        let sql = render(&pred);
        assert_eq!(sql.text, "((? = t0.\"user_id\") OR (t0.\"isPublic\" = ?))");
        assert_eq!(
            sql.params,
            [SqlValue::Text("abc".into()), SqlValue::Integer(1)]
        );

        // Anonymous callers act under the anonymous grant; this one reads no claims.
        let pred = read_filter(todo, &Caller::anonymous(), "t1")
            .unwrap()
            .unwrap();
        assert_eq!(render(&pred).text, "(t1.\"isPublic\" = ?)");

        // A date literal is stored as a fixed-width UTC date; `x ne null` never matches.
        let project = model.entity("Project").unwrap();
        let pred = read_filter(project, &Caller::anonymous(), "t0")
            .unwrap()
            .unwrap();
        let sql = render(&pred);
        assert_eq!(
            sql.text,
            "((t0.\"startsOn\" != ?) AND (t0.\"budget\" != ?))"
        );
        assert_eq!(
            sql.params[0],
            SqlValue::Text("2000-01-01T00:00:00.0000000Z".into())
        );
        let pred = read_filter(project, &caller(json!({"sub": "u1"})), "t0").unwrap();
        assert_eq!(render(&pred.unwrap()).text, "((t0.\"owner_id\" = ?) OR 0)");
    }

    #[test]
    fn missing_claims_and_constant_comparisons() {
        let model = basic();
        let comment = model.entity("Comment").unwrap();
        let grant = access::grant(comment, "authenticated", Action::Create).unwrap();
        let policy = grant.policy.as_ref().unwrap();
        let row = |c: &Column| Operand::column("t0", &c.name);
        let error = compile_policy(policy, comment, &caller(json!({"sub": "u1"})), &row);
        assert_eq!(error, Err(PolicyError::MissingClaim("email".into())));

        // `claims.role eq 'authenticated'` compares two constants, case-insensitively.
        let claims = json!({"sub": "u1", "email": "a@b.c", "role": "Authenticated"});
        let pred = compile_policy(policy, comment, &caller(claims), &row).unwrap();
        let sql = render(&pred);
        assert!(sql.text.ends_with("OR 1))"), "{}", sql.text);

        let anonymous = Caller::anonymous();
        assert_eq!(
            compile_policy(policy, comment, &anonymous, &row),
            Err(PolicyError::MissingClaim("email".into()))
        );
    }

    #[test]
    fn unconvertible_claims_match_nothing() {
        let model = basic();
        let person = model.entity("Person").unwrap();
        // account_id is a uuid; a `sub` that is not one can never be equal to it.
        let pred = read_filter(person, &caller(json!({"sub": "not-a-uuid"})), "t0").unwrap();
        assert_eq!(pred, Some(Pred::Const(false)));
        let id = "0D7F4C1E-9A43-4B8F-B2A6-3F0C5E8D1A22";
        let pred = read_filter(person, &caller(json!({"sub": id})), "t0").unwrap();
        let sql = render(&pred.unwrap());
        assert_eq!(sql.params, [SqlValue::Text(id.to_lowercase())]);
    }
}
