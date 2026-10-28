//! Who a request acts for.
//!
//! `rl-auth` verifies bearer tokens and implements [`CallerResolver`]; the feature crates (data,
//! functions, connectors, storage) take an `Arc<dyn CallerResolver>` and act for the [`Caller`] it
//! returns, so each can be tested with a fake resolver.

use std::fmt;

use serde_json::{Map, Value};

/// A signed-in session, established from a verified Rayfin access token.
#[derive(Clone, PartialEq)]
pub struct Session {
    /// The `sub` claim: the user's stable id (`claims.sub` in `@role` policies).
    pub user_id: String,
    /// The `email` claim, when the session has one (`claims.email` in policies).
    pub email: Option<String>,
    /// Every claim of the verified token, for policies over other claims.
    pub claims: Map<String, Value>,
    /// The raw bearer token, forwarded to functions so `ctx.getDataClient()` acts as the caller.
    pub access_token: String,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The token and identity claims stay out of logs.
        f.debug_struct("Session")
            .field("user_id", &self.user_id)
            .field("has_email", &self.email.is_some())
            .field("claims", &self.claims.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// The identity a request runs as.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caller {
    /// `None` for anonymous requests.
    pub session: Option<Session>,
}

impl Caller {
    /// A request without a bearer token.
    pub fn anonymous() -> Self {
        Self { session: None }
    }

    /// A request with a verified session.
    pub fn authenticated(session: Session) -> Self {
        Self {
            session: Some(session),
        }
    }

    /// Whether the request carries a verified session.
    pub fn is_authenticated(&self) -> bool {
        self.session.is_some()
    }

    /// The DAB system role the request runs under: `authenticated` or `anonymous`.
    pub fn role(&self) -> &'static str {
        if self.is_authenticated() {
            "authenticated"
        } else {
            "anonymous"
        }
    }

    /// A claim of the session's token; `None` for anonymous callers and absent claims.
    pub fn claim(&self, name: &str) -> Option<&Value> {
        self.session.as_ref()?.claims.get(name)
    }
}

/// Why a bearer token was rejected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CallerError {
    /// Malformed, expired, revoked or wrongly signed. Routes answer 401, so the SDK refreshes the
    /// session and retries once.
    #[error("the access token is invalid or has expired")]
    InvalidToken,
}

/// Turns a request's bearer token into a [`Caller`].
pub trait CallerResolver: Send + Sync + 'static {
    /// `bearer` is the token of `Authorization: Bearer <token>`, or `None` when the request has
    /// none (an anonymous caller).
    fn resolve(&self, bearer: Option<&str>) -> Result<Caller, CallerError>;
}

/// The token of an `Authorization` header value (`Bearer <token>`, scheme case-insensitive).
pub fn bearer_token(authorization: Option<&str>) -> Option<&str> {
    let (scheme, token) = authorization?.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_token_accepts_any_scheme_case_and_trims() {
        assert_eq!(bearer_token(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_token(Some("bearer  abc ")), Some("abc"));
        assert_eq!(bearer_token(Some("Basic abc")), None);
        assert_eq!(bearer_token(Some("Bearer ")), None);
        assert_eq!(bearer_token(None), None);
    }

    #[test]
    fn role_follows_the_session() {
        assert_eq!(Caller::anonymous().role(), "anonymous");
        let session = Session {
            user_id: "u1".into(),
            email: None,
            claims: Map::new(),
            access_token: "t".into(),
        };
        let caller = Caller::authenticated(session);
        assert_eq!(caller.role(), "authenticated");
        assert!(caller.claim("sub").is_none());
    }

    #[test]
    fn session_debug_hides_the_token() {
        let session = Session {
            user_id: "u1".into(),
            email: Some("a@b.c".into()),
            claims: Map::new(),
            access_token: "secret-token".into(),
        };
        let printed = format!("{session:?}");
        assert!(!printed.contains("secret-token"));
        assert!(!printed.contains("a@b.c"));
    }
}
