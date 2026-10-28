//! Sessions: sign-ins as refresh-token families, rotation, sign-out, and the in-memory list of
//! signed-out sessions that access-token verification consults.

use std::{collections::HashMap, fmt, sync::Arc};

use parking_lot::RwLock;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{
    AuthError, AuthService,
    jwt::{AccessClaims, Jwt, LEEWAY_SECONDS, TokenError},
    users::{User, find_user},
    util::{b64url, constant_time_eq, random_bytes, sha256},
};

/// A rotated refresh token presented within this window of the session's last refresh gets a new
/// access token and its own refresh token back (concurrent tabs, retries): the real service's
/// fixed reuse interval.
pub(crate) const REUSE_WINDOW_MS: i64 = 10_000;

const INVALID_OR_MALFORMED: &str = "The refresh token is invalid or malformed";
const INVALID: &str = "The refresh token is invalid";
const EXPIRED: &str = "The session has expired";
const USER_INVALID: &str = "The user account is invalid";

/// The `POST /api/auth/v1/token` success body (OAuth 2.1, camelCase like the real service).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenResponse {
    pub access_token: String,
    /// Always `Bearer`.
    pub token_type: String,
    /// Access-token lifetime in seconds.
    pub expires_in: u64,
    pub refresh_token: Option<String>,
    /// The configured scopes joined with spaces; `null` without scopes.
    pub scope: Option<String>,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("token_type", &self.token_type)
            .field("expires_in", &self.expires_in)
            .field("has_refresh_token", &self.refresh_token.is_some())
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

fn token_response(jwt: &Jwt, access_token: String, refresh_token: String) -> TokenResponse {
    TokenResponse {
        access_token,
        token_type: "Bearer".into(),
        expires_in: u64::try_from(jwt.access_ttl_seconds()).unwrap_or_default(),
        refresh_token: Some(refresh_token),
        scope: jwt.scope().map(str::to_owned),
    }
}

/// Signed-out sessions whose access tokens may not have expired yet.
#[derive(Default)]
pub(crate) struct RevokedSessions(RwLock<HashMap<String, i64>>);

impl RevokedSessions {
    pub(crate) fn contains(&self, session_id: &str) -> bool {
        self.0.read().contains_key(session_id)
    }

    /// Adds sessions revoked at `revoked_at`, dropping entries revoked before `horizon` (all their
    /// access tokens have expired).
    fn add(&self, ids: impl IntoIterator<Item = String>, revoked_at: i64, horizon: i64) {
        let mut revoked = self.0.write();
        revoked.retain(|_, at| *at >= horizon);
        revoked.extend(ids.into_iter().map(|id| (id, revoked_at)));
    }
}

impl AuthService {
    /// Signs `user` in: a new session with a refresh token and an access token. The sign-in
    /// portal and the Entra exchange use this after establishing who the user is.
    pub async fn issue_session(&self, user: &User) -> Result<TokenResponse, AuthError> {
        let jwt = Arc::clone(&self.jwt);
        let user_id = user.id.clone();
        let now = self.now_ms();
        let expires_at = now + self.refresh_lifetime_ms();
        self.db
            .call(move |conn| -> Result<TokenResponse, AuthError> {
                let tx = conn.transaction()?;
                // Read the user again so a just-deleted user gets no session and claims are current.
                let user = find_user(&tx, "id = ?1", &user_id)?.ok_or(AuthError::UserNotFound)?;
                let session_id = uuid::Uuid::new_v4().to_string();
                tx.execute(
                    "INSERT INTO _rl_auth_sessions (id, user_id, created_at, expires_at,
                        last_refresh_at)
                     VALUES (?1, ?2, ?3, ?4, ?3)",
                    params![session_id, user.id, now, expires_at],
                )?;
                let refresh_token = insert_refresh_token(&tx, &session_id, now)?;
                let access_token = jwt.mint(&user, &session_id, now / 1000)?;
                tx.commit()?;
                Ok(token_response(&jwt, access_token, refresh_token))
            })
            .await?
    }

    /// The refresh-token grant, with the real service's semantics: the session must be live
    /// (not signed out, within its sliding and absolute lifetimes) and its user must exist. The
    /// newest token rotates; an older token of the session is redeemed too (tabs that have not
    /// caught up), returned as is within [`REUSE_WINDOW_MS`] of the last refresh and rotated
    /// after it. Refused tokens are [`AuthError::InvalidGrant`].
    pub(crate) async fn refresh(&self, refresh_token: &str) -> Result<TokenResponse, AuthError> {
        let Some((selector, verifier_hash)) = parse_refresh_token(refresh_token) else {
            return Err(AuthError::InvalidGrant(INVALID_OR_MALFORMED));
        };
        let selector = selector.to_owned();
        let presented = refresh_token.to_owned();
        let jwt = Arc::clone(&self.jwt);
        let now = self.now_ms();
        let lifetime = self.refresh_lifetime_ms();
        let absolute_lifetime = self
            .settings
            .session_absolute_lifetime_minutes
            .map(|minutes| i64::from(minutes) * 60_000);
        self.db
            .call(move |conn| -> Result<TokenResponse, AuthError> {
                let tx = conn.transaction()?;
                let token = tx
                    .query_row(
                        "SELECT verifier_hash, session_id, rotated_at
                         FROM _rl_auth_refresh_tokens WHERE selector = ?1",
                        [&selector],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Option<i64>>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((stored_hash, session_id, rotated_at)) = token else {
                    return Err(AuthError::InvalidGrant(INVALID_OR_MALFORMED));
                };
                if !constant_time_eq(&stored_hash, &verifier_hash) {
                    return Err(AuthError::InvalidGrant(INVALID_OR_MALFORMED));
                }
                let session = tx
                    .query_row(
                        "SELECT user_id, created_at, expires_at, last_refresh_at
                         FROM _rl_auth_sessions WHERE id = ?1 AND revoked_at IS NULL",
                        [&session_id],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)?,
                                row.get::<_, i64>(2)?,
                                row.get::<_, i64>(3)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((user_id, created_at, expires_at, last_refresh_at)) = session else {
                    return Err(AuthError::InvalidGrant(INVALID));
                };
                let past_absolute =
                    absolute_lifetime.is_some_and(|lifetime| created_at + lifetime < now);
                if expires_at < now || past_absolute {
                    return Err(AuthError::InvalidGrant(EXPIRED));
                }
                let user = find_user(&tx, "id = ?1", &user_id)?
                    .ok_or(AuthError::InvalidGrant(USER_INVALID))?;
                let access_token = jwt.mint(&user, &session_id, now / 1000)?;
                let reuse = rotated_at.is_some() && now - last_refresh_at <= REUSE_WINDOW_MS;
                let refresh_token = if reuse {
                    presented
                } else {
                    tx.execute(
                        "UPDATE _rl_auth_refresh_tokens SET rotated_at = ?1
                         WHERE session_id = ?2 AND rotated_at IS NULL",
                        params![now, session_id],
                    )?;
                    tx.execute(
                        "UPDATE _rl_auth_sessions SET last_refresh_at = ?1, expires_at = ?2
                         WHERE id = ?3",
                        params![now, now + lifetime, session_id],
                    )?;
                    insert_refresh_token(&tx, &session_id, now)?
                };
                tx.commit()?;
                Ok(token_response(&jwt, access_token, refresh_token))
            })
            .await?
    }

    /// Signs out one session of `user_id`; `false` when it was not active.
    pub(crate) async fn revoke_session(
        &self,
        session_id: &str,
        user_id: &str,
    ) -> Result<bool, AuthError> {
        let now = self.now_ms();
        let (sid, uid) = (session_id.to_owned(), user_id.to_owned());
        let changed = self
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE _rl_auth_sessions SET revoked_at = ?1
                     WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
                    params![now, sid, uid],
                )
            })
            .await??;
        if changed == 1 {
            self.forget_sessions(vec![session_id.to_owned()], now);
        }
        Ok(changed == 1)
    }

    /// Signs out every active session of `user_id` and returns how many there were.
    pub async fn revoke_all_sessions(&self, user_id: &str) -> Result<usize, AuthError> {
        let now = self.now_ms();
        let uid = user_id.to_owned();
        let revoked = self
            .db
            .call(move |conn| revoke_user_sessions(conn, &uid, now))
            .await??;
        let count = revoked.len();
        self.forget_sessions(revoked, now);
        Ok(count)
    }

    /// Verifies an access token and that its session has not been signed out.
    pub(crate) fn verify_session_token(&self, token: &str) -> Result<AccessClaims, TokenError> {
        let claims = self.jwt.verify(token, self.now_ms() / 1000)?;
        if self.revoked.contains(&claims.sid) {
            return Err(TokenError::Revoked);
        }
        Ok(claims)
    }

    /// Rejects the access tokens of sessions signed out at `now` from now on.
    pub(crate) fn forget_sessions(&self, session_ids: Vec<String>, now: i64) {
        self.revoked
            .add(session_ids, now, now - self.revocation_window_ms());
    }

    /// Startup: drops sessions that no token can use any more and remembers the recently signed-out
    /// ones, whose access tokens may still be presented.
    pub(crate) async fn load_revoked_sessions(&self) -> Result<(), AuthError> {
        let now = self.now_ms();
        let horizon = now - self.revocation_window_ms();
        let revoked = self
            .db
            .call(move |conn| -> Result<Vec<(String, i64)>, AuthError> {
                conn.execute(
                    "DELETE FROM _rl_auth_sessions WHERE revoked_at < ?1 OR expires_at < ?1",
                    [horizon],
                )?;
                let mut statement = conn.prepare(
                    "SELECT id, revoked_at FROM _rl_auth_sessions WHERE revoked_at IS NOT NULL",
                )?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await??;
        let mut map = self.revoked.0.write();
        map.extend(revoked);
        Ok(())
    }

    fn refresh_lifetime_ms(&self) -> i64 {
        i64::from(self.settings.refresh_token_days) * 86_400_000
    }

    /// How long a signed-out session's access tokens can still be presented.
    fn revocation_window_ms(&self) -> i64 {
        (self.jwt.access_ttl_seconds() + LEEWAY_SECONDS) * 1000
    }
}

/// Signs out the active sessions of `user_id`, returning their ids.
pub(crate) fn revoke_user_sessions(
    conn: &Connection,
    user_id: &str,
    now: i64,
) -> Result<Vec<String>, AuthError> {
    let mut statement = conn.prepare(
        "UPDATE _rl_auth_sessions SET revoked_at = ?1
         WHERE user_id = ?2 AND revoked_at IS NULL RETURNING id",
    )?;
    let ids = statement
        .query_map(params![now, user_id], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()?;
    Ok(ids)
}

/// A new refresh token for the session: `<selector>.<verifier>`, of which only the selector and
/// the verifier's SHA-256 are stored.
fn insert_refresh_token(
    conn: &Connection,
    session_id: &str,
    now: i64,
) -> Result<String, AuthError> {
    let selector = b64url(&random_bytes::<16>()?);
    let verifier = b64url(&random_bytes::<32>()?);
    conn.execute(
        "INSERT INTO _rl_auth_refresh_tokens (selector, verifier_hash, session_id, issued_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![selector, &sha256(verifier.as_bytes())[..], session_id, now],
    )?;
    Ok(format!("{selector}.{verifier}"))
}

fn parse_refresh_token(token: &str) -> Option<(&str, [u8; 32])> {
    let (selector, verifier) = token.split_once('.')?;
    (selector.len() == 22 && verifier.len() == 43).then(|| (selector, sha256(verifier.as_bytes())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::service;

    #[tokio::test]
    async fn refresh_rotates_the_newest_token() {
        let (_dir, auth) = service().await;
        let user = auth.create_user("alice", "secret1").await.unwrap();
        let first = auth.issue_session(&user).await.unwrap();
        let first_refresh = first.refresh_token.clone().unwrap();
        auth.advance_clock(REUSE_WINDOW_MS + 1);

        let second = auth.refresh(&first_refresh).await.unwrap();
        let second_refresh = second.refresh_token.clone().unwrap();
        assert_ne!(second_refresh, first_refresh);
        assert_ne!(second.access_token, first.access_token);
        let claims = auth.verify_session_token(&second.access_token).unwrap();
        assert_eq!(claims.sub, user.id);

        // Within the reuse window an older token gets a new access token and itself back.
        let again = auth.refresh(&first_refresh).await.unwrap();
        assert_eq!(again.refresh_token.as_deref(), Some(first_refresh.as_str()));
        // After the window it is redeemed with a new rotation, like the real service does.
        auth.advance_clock(REUSE_WINDOW_MS + 1);
        let later = auth.refresh(&first_refresh).await.unwrap();
        let later_refresh = later.refresh_token.unwrap();
        assert_ne!(later_refresh, first_refresh);
        assert_ne!(later_refresh, second_refresh);
        // The token that was newest before is now an older one: reused within the window.
        let reused = auth.refresh(&second_refresh).await.unwrap();
        assert_eq!(
            reused.refresh_token.as_deref(),
            Some(second_refresh.as_str())
        );
    }

    #[tokio::test]
    async fn unknown_and_forged_refresh_tokens_are_invalid_grants() {
        let (_dir, auth) = service().await;
        let user = auth.create_user("alice", "secret1").await.unwrap();
        let token = auth
            .issue_session(&user)
            .await
            .unwrap()
            .refresh_token
            .unwrap();
        let (selector, _) = token.split_once('.').unwrap();
        let forged = format!("{selector}.{}", "A".repeat(43));
        for bad in ["", "garbage", forged.as_str()] {
            assert!(
                matches!(
                    auth.refresh(bad).await,
                    Err(AuthError::InvalidGrant(INVALID_OR_MALFORMED))
                ),
                "{bad}"
            );
        }
    }

    #[tokio::test]
    async fn sessions_expire_without_refreshes() {
        let (_dir, auth) = service().await;
        let user = auth.create_user("alice", "secret1").await.unwrap();
        let token = auth
            .issue_session(&user)
            .await
            .unwrap()
            .refresh_token
            .unwrap();
        auth.advance_clock(auth.refresh_lifetime_ms() + 1);
        assert!(matches!(
            auth.refresh(&token).await,
            Err(AuthError::InvalidGrant(EXPIRED))
        ));
    }

    #[tokio::test]
    async fn signing_out_ends_the_session_and_its_access_tokens() {
        let (_dir, auth) = service().await;
        let user = auth.create_user("alice", "secret1").await.unwrap();
        let kept = auth.issue_session(&user).await.unwrap();
        let ended = auth.issue_session(&user).await.unwrap();
        let sid = auth.verify_session_token(&ended.access_token).unwrap().sid;

        assert!(!auth.revoke_session(&sid, "someone-else").await.unwrap());
        assert!(auth.revoke_session(&sid, &user.id).await.unwrap());
        assert!(!auth.revoke_session(&sid, &user.id).await.unwrap());
        assert_eq!(
            auth.verify_session_token(&ended.access_token),
            Err(TokenError::Revoked)
        );
        assert!(matches!(
            auth.refresh(ended.refresh_token.as_deref().unwrap()).await,
            Err(AuthError::InvalidGrant(INVALID))
        ));
        assert!(auth.verify_session_token(&kept.access_token).is_ok());

        assert_eq!(auth.revoke_all_sessions(&user.id).await.unwrap(), 1);
        assert_eq!(
            auth.verify_session_token(&kept.access_token),
            Err(TokenError::Revoked)
        );
    }

    #[tokio::test]
    async fn sessions_survive_a_restart_and_signed_out_ones_stay_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = rl_db::Db::open(&dir.path().join("db.sqlite")).unwrap();
        let settings = crate::AuthSettings::new(crate::tests::APP_ID);
        let keys = dir.path().join("keys");
        let auth = AuthService::new(db.clone(), keys.clone(), settings.clone())
            .await
            .unwrap();
        let user = auth.create_user("alice", "secret1").await.unwrap();
        let ended = auth.issue_session(&user).await.unwrap();
        auth.revoke_all_sessions(&user.id).await.unwrap();
        let kept = auth.issue_session(&user).await.unwrap();

        let restarted = AuthService::new(db, keys, settings).await.unwrap();
        assert_eq!(
            restarted.verify_session_token(&ended.access_token),
            Err(TokenError::Revoked)
        );
        assert!(restarted.verify_session_token(&kept.access_token).is_ok());
        assert!(
            restarted
                .refresh(kept.refresh_token.as_deref().unwrap())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn deleted_users_lose_their_sessions() {
        let (_dir, auth) = service().await;
        let user = auth.create_user("alice", "secret1").await.unwrap();
        let tokens = auth.issue_session(&user).await.unwrap();
        auth.delete_user("alice").await.unwrap();
        assert_eq!(
            auth.verify_session_token(&tokens.access_token),
            Err(TokenError::Revoked)
        );
        assert!(matches!(
            auth.refresh(tokens.refresh_token.as_deref().unwrap()).await,
            Err(AuthError::InvalidGrant(INVALID))
        ));
        assert!(matches!(
            auth.issue_session(&user).await,
            Err(AuthError::UserNotFound)
        ));
    }
}
