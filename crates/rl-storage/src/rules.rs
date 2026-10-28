//! Folder rules: a rule grants its `actions` to its `role` on objects matching its
//! `pathPattern` when its `check` (if any) holds for the caller and the object.
//!
//! `anonymous` rules apply to every caller, `authenticated` rules to signed-in callers; other
//! roles never apply (1.36.2 folders only have `authenticated` rules). A `check` is evaluated like
//! the data path's policies: `eq null` / `ne null` test for a missing or null value, any other
//! comparison with a missing or null value is false, and strings compare case-insensitively
//! (Fabric's collation).

use rl_model::{
    Action, Caller, CompareOp, Literal, Operand, PolicyExpr, StorageFolder, StorageRule,
};
use serde_json::{Map, Value};

use crate::names::glob_match;

/// Whether some rule could grant `action` to `caller` (checks and paths not considered).
pub(crate) fn may(folder: &StorageFolder, caller: &Caller, action: Action) -> bool {
    folder.rules.iter().any(|rule| grants(rule, caller, action))
}

/// Whether a rule grants `action` on `item` (the object's fields, as [`crate::store`] builds
/// them; `path` is the logical path).
pub(crate) fn allows(
    folder: &StorageFolder,
    caller: &Caller,
    action: Action,
    item: &Map<String, Value>,
) -> bool {
    let path = item.get("path").and_then(Value::as_str).unwrap_or_default();
    folder.rules.iter().any(|rule| {
        grants(rule, caller, action)
            && path_matches(&rule.path_pattern, path)
            && rule
                .check
                .as_ref()
                .is_none_or(|check| eval(check, caller, item))
    })
}

fn grants(rule: &StorageRule, caller: &Caller, action: Action) -> bool {
    let role_applies = rule.role.eq_ignore_ascii_case("anonymous")
        || rule.role.eq_ignore_ascii_case(caller.role());
    role_applies
        && rule
            .actions
            .iter()
            .any(|granted| *granted == action || *granted == Action::All)
}

fn path_matches(pattern: &str, path: &str) -> bool {
    pattern.is_empty() || glob_match(pattern, path)
}

fn eval(expr: &PolicyExpr, caller: &Caller, item: &Map<String, Value>) -> bool {
    match expr {
        PolicyExpr::And(operands) => operands.iter().all(|e| eval(e, caller, item)),
        PolicyExpr::Or(operands) => operands.iter().any(|e| eval(e, caller, item)),
        PolicyExpr::Compare { op, left, right } => {
            let is_null_literal =
                |operand: &Operand| matches!(operand, Operand::Literal(Literal::Null));
            let equal = if is_null_literal(left) || is_null_literal(right) {
                let other = if is_null_literal(right) { left } else { right };
                resolve(other, caller, item).is_none()
            } else {
                match (resolve(left, caller, item), resolve(right, caller, item)) {
                    (Some(left), Some(right)) => values_equal(&left, &right),
                    _ => return false,
                }
            };
            match op {
                CompareOp::Eq => equal,
                CompareOp::Ne => !equal,
            }
        }
    }
}

/// An operand's value; `None` for a missing or null one.
fn resolve(operand: &Operand, caller: &Caller, item: &Map<String, Value>) -> Option<Value> {
    let value = match operand {
        Operand::Claim(name) => claim(caller, name),
        Operand::Field(name) => item.get(name).cloned(),
        Operand::Literal(Literal::Null) => None,
        Operand::Literal(Literal::Bool(b)) => Some(Value::Bool(*b)),
        Operand::Literal(Literal::Number(n)) => Some(Value::Number(n.clone())),
        Operand::Literal(Literal::String(s)) => Some(Value::String(s.clone())),
    };
    value.filter(|value| !value.is_null())
}

fn claim(caller: &Caller, name: &str) -> Option<Value> {
    let session = caller.session.as_ref()?;
    match name {
        "sub" => Some(Value::String(session.user_id.clone())),
        "email" => session
            .email
            .clone()
            .map(Value::String)
            .or_else(|| caller.claim(name).cloned()),
        _ => caller.claim(name).cloned(),
    }
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (Value::Bool(a), Value::Bool(b)) => a == b,
        // A multi-valued claim (`roles`) matches when one of its values does.
        (Value::Array(values), other) | (other, Value::Array(values)) => {
            values.iter().any(|value| values_equal(value, other))
        }
        _ => text(left).to_lowercase() == text(right).to_lowercase(),
    }
}

fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use rl_model::{OnConflict, Session};
    use serde_json::json;

    use super::*;

    fn user(sub: &str) -> Caller {
        Caller::authenticated(Session {
            user_id: sub.into(),
            email: None,
            claims: Map::new(),
            access_token: "t".into(),
        })
    }

    fn folder(rules: Vec<StorageRule>) -> StorageFolder {
        StorageFolder {
            name: "docs".into(),
            display_name: "Docs".into(),
            on_conflict: OnConflict::Error,
            max_size: None,
            allowed_content_types: vec![],
            fields: vec![],
            rules,
        }
    }

    fn rule(role: &str, actions: &[Action], check: Option<PolicyExpr>) -> StorageRule {
        StorageRule {
            path_pattern: "**".into(),
            role: role.into(),
            actions: actions.to_vec(),
            check,
        }
    }

    fn compare(op: CompareOp, left: Operand, right: Operand) -> PolicyExpr {
        PolicyExpr::Compare { op, left, right }
    }

    fn item(value: Value) -> Map<String, Value> {
        let Value::Object(mut map) = value else {
            unreachable!()
        };
        map.entry("path").or_insert("a.txt".into());
        map
    }

    #[test]
    fn owner_checks_compare_the_sub_claim_case_insensitively() {
        let owner_only = compare(
            CompareOp::Eq,
            Operand::Claim("sub".into()),
            Operand::Field("owner_id".into()),
        );
        let folder = folder(vec![rule(
            "authenticated",
            &[Action::Read],
            Some(owner_only),
        )]);
        let mine = item(json!({ "owner_id": "User-1" }));
        assert!(allows(&folder, &user("user-1"), Action::Read, &mine));
        assert!(!allows(&folder, &user("user-2"), Action::Read, &mine));
        assert!(!allows(&folder, &user("user-1"), Action::Delete, &mine));
        assert!(!allows(&folder, &Caller::anonymous(), Action::Read, &mine));
        assert!(!allows(
            &folder,
            &user("user-1"),
            Action::Read,
            &item(json!({}))
        ));
    }

    #[test]
    fn null_literals_test_presence_and_other_nulls_never_match() {
        let has_team = compare(
            CompareOp::Ne,
            Operand::Field("team_id".into()),
            Operand::Literal(Literal::Null),
        );
        let no_team = compare(
            CompareOp::Eq,
            Operand::Field("team_id".into()),
            Operand::Literal(Literal::Null),
        );
        let not_owner = compare(
            CompareOp::Ne,
            Operand::Claim("sub".into()),
            Operand::Field("owner_id".into()),
        );
        let caller = user("u1");
        let with_team = item(json!({ "team_id": "t1" }));
        let without_team = item(json!({ "team_id": null }));
        assert!(eval(&has_team, &caller, &with_team));
        assert!(!eval(&has_team, &caller, &without_team));
        assert!(eval(&no_team, &caller, &without_team));
        assert!(!eval(&not_owner, &caller, &without_team));
        let both = PolicyExpr::Or(vec![no_team, has_team]);
        assert!(eval(&both, &caller, &item(json!({}))));
    }

    #[test]
    fn anonymous_rules_apply_to_everyone_and_star_grants_every_action() {
        let folder = folder(vec![
            rule("anonymous", &[Action::Read], None),
            rule("authenticated", &[Action::All], None),
        ]);
        let any = item(json!({}));
        assert!(allows(&folder, &Caller::anonymous(), Action::Read, &any));
        assert!(!allows(&folder, &Caller::anonymous(), Action::Create, &any));
        assert!(allows(&folder, &user("u"), Action::Read, &any));
        assert!(allows(&folder, &user("u"), Action::Delete, &any));
        assert!(may(&folder, &user("u"), Action::Update));
        assert!(!may(&folder, &Caller::anonymous(), Action::Update));
    }
}
