//! Effective permissions: the grant a role acts under for an action on an entity.
//!
//! Exactly one role is evaluated per request (`Caller::role()`: `anonymous` or `authenticated`).
//! For a role and action:
//!
//! 1. the role's own entries covering the action; when several do (`read` and `*`), the last one
//!    wins, as DAB's `AuthorizationResolver` overwrites the operation's metadata entry by entry;
//! 2. otherwise, for `authenticated`, the `anonymous` grant for that action (micro-fabric
//!    compatibility.md §7.2: per-action inheritance);
//! 3. otherwise the action is denied.

use rl_model::{Action, ActionPermission, Entity};

pub const ANONYMOUS: &str = "anonymous";
pub const AUTHENTICATED: &str = "authenticated";
const ROLES: [&str; 2] = [ANONYMOUS, AUTHENTICATED];

/// The grant `role` acts under for `action` on `entity`, `None` when denied.
pub fn grant<'m>(entity: &'m Entity, role: &str, action: Action) -> Option<&'m ActionPermission> {
    entity.grants(role, action).last().or_else(|| {
        if role == AUTHENTICATED {
            entity.grants(ANONYMOUS, action).last()
        } else {
            None
        }
    })
}

/// Whether the grant lets the role use the exposed field `field`.
pub fn allows_field(grant: &ActionPermission, field: &str) -> bool {
    grant
        .fields
        .as_ref()
        .is_none_or(|access| access.allows(field))
}

/// Whether some role may read the entity (DAB then exposes its query fields).
pub fn readable_by_anyone(entity: &Entity) -> bool {
    ROLES
        .iter()
        .any(|role| grant(entity, role, Action::Read).is_some())
}

/// Whether some role may read `field` (DAB then exposes it on the entity's type).
pub fn field_readable_by_anyone(entity: &Entity, field: &str) -> bool {
    ROLES.iter().any(|role| {
        grant(entity, role, Action::Read).is_some_and(|grant| allows_field(grant, field))
    })
}

#[cfg(test)]
mod tests {
    use rl_model::AppModel;

    use super::*;

    fn basic() -> AppModel {
        let json = include_str!("../../rl-model/tests/fixtures/basic.analysis.json");
        AppModel::from_analysis(&serde_json::from_str(json).unwrap()).unwrap()
    }

    #[test]
    fn authenticated_falls_back_to_anonymous_per_action() {
        let model = basic();
        let project = model.entity("Project").unwrap();
        let anonymous = grant(project, ANONYMOUS, Action::Read).unwrap();
        let authenticated = grant(project, AUTHENTICATED, Action::Read).unwrap();
        assert_ne!(anonymous.policy, authenticated.policy);

        let milestone = model.entity("Milestone").unwrap();
        assert!(grant(milestone, ANONYMOUS, Action::Read).is_none());
        assert!(grant(milestone, AUTHENTICATED, Action::Delete).is_some());

        let todo = model.entity("Todo").unwrap();
        let anonymous = grant(todo, ANONYMOUS, Action::Read).unwrap();
        assert!(allows_field(anonymous, "title") && !allows_field(anonymous, "cost"));
        assert!(field_readable_by_anyone(todo, "cost"));
        assert!(!field_readable_by_anyone(todo, "secretNote"));
        // Comment: anonymous reads without authorEmail; authenticated has its own read.
        let comment = model.entity("Comment").unwrap();
        let read = grant(comment, AUTHENTICATED, Action::Read).unwrap();
        assert!(allows_field(read, "authorEmail") && read.policy.is_none());
    }
}
