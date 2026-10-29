//! [`CallerResolver`]: bearer tokens of the other routes (data, functions, connectors, storage).

use rl_model::{Caller, CallerError, CallerResolver, Session};

use crate::AuthService;

impl CallerResolver for AuthService {
    /// No token: anonymous. Otherwise the token must be one of this service's access tokens
    /// (signature, `kid`, `alg`, expiry with 5 minutes of leeway, a lifetime no longer than the
    /// current one, `iss`, `aud`) whose session has not been signed out. Sign-outs made by this
    /// process are rejected at once, as on the real service; those made by another process (the
    /// CLI's `users delete` and `users reset-password`) within 2 seconds.
    fn resolve(&self, bearer: Option<&str>) -> Result<Caller, CallerError> {
        let Some(token) = bearer else {
            return Ok(Caller::anonymous());
        };
        let verified = self
            .verify_session_token(token)
            .map_err(|_| CallerError::InvalidToken)?;
        Ok(Caller::authenticated(Session {
            user_id: verified.sub,
            email: verified.email,
            claims: verified.claims,
            access_token: token.to_owned(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::service;

    #[tokio::test]
    async fn resolves_sessions_and_refuses_bad_tokens() {
        let (_dir, auth) = service().await;
        assert_eq!(auth.resolve(None), Ok(Caller::anonymous()));

        let user = auth
            .create_user("alice@example.test", "secret1")
            .await
            .unwrap();
        let tokens = auth.issue_session(&user).await.unwrap();
        let caller = auth.resolve(Some(&tokens.access_token)).unwrap();
        let session = caller.session.as_ref().unwrap();
        assert_eq!(session.user_id, user.id);
        assert_eq!(session.email.as_deref(), Some("alice@example.test"));
        assert_eq!(
            caller.claim("sub").and_then(|v| v.as_str()),
            Some(user.id.as_str())
        );
        assert_eq!(session.access_token, tokens.access_token);

        assert_eq!(auth.resolve(Some("")), Err(CallerError::InvalidToken));
        assert_eq!(
            auth.resolve(Some(&format!("{}x", tokens.access_token))),
            Err(CallerError::InvalidToken)
        );
        auth.advance_clock((3600 + crate::jwt::LEEWAY_SECONDS + 1) * 1000);
        assert_eq!(
            auth.resolve(Some(&tokens.access_token)),
            Err(CallerError::InvalidToken)
        );
    }
}
