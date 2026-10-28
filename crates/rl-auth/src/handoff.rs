//! Handoff codes: the sign-in portal issues one after a sign-in, and the `authorization_code`
//! grant redeems it (`verificationCode`, `codeType: "fabric_handoff"`), with the real service's
//! checks and answers (its passwordless-code service, which also redeems Fabric handoffs).

use axum::http::StatusCode;
use rusqlite::{OptionalExtension, params};

use crate::{
    AuthError, AuthService, TokenResponse,
    routes::{ApiError, Fields},
    users::{User, find_user},
    util::{b64url, constant_time_eq, random_bytes, sha256, web_origin},
};

/// How long a handoff code can be redeemed. The SDK redeems it as soon as the portal posts it.
pub(crate) const HANDOFF_CODE_TTL_MS: i64 = 2 * 60 * 1000;

/// A sign-in request from the portal: the app's PKCE challenge and where the code goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HandoffRequest {
    /// `base64url(SHA-256(code_verifier))` (S256).
    pub(crate) code_challenge: String,
    /// The app's origin, serialized as browsers do: the portal posts the code there, and the app
    /// redeems it with that origin as `redirectUri`.
    pub(crate) return_origin: String,
    /// The app's nonce for this sign-in, echoed back.
    pub(crate) state: String,
}

/// Why a code was refused: the real service's codes and messages, all answered with 401.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rejection {
    Unknown,
    Used,
    Expired,
    RedirectMismatch,
    VerifierMismatch,
}

impl From<Rejection> for ApiError {
    fn from(rejection: Rejection) -> Self {
        let (code, message) = match rejection {
            Rejection::Unknown => ("INVALID_CODE", "Verification code is invalid"),
            Rejection::Used => (
                "CODE_ALREADY_USED",
                "Verification code has already been used",
            ),
            Rejection::Expired => ("CODE_EXPIRED", "Verification code has expired"),
            Rejection::RedirectMismatch => (
                "REDIRECT_URI_MISMATCH",
                "Redirect URI does not match the original request",
            ),
            Rejection::VerifierMismatch => (
                "INVALID_CODE_VERIFIER",
                "PKCE code_verifier validation failed",
            ),
        };
        ApiError::new(StatusCode::UNAUTHORIZED, code, message)
    }
}

/// A verification-code exchange, validated as far as the request shape goes.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CodeExchange {
    pub(crate) verification_code: String,
    pub(crate) code_verifier: String,
    pub(crate) redirect_uri: String,
    /// `fabric_handoff` for the broker popup; absent for magic links.
    pub(crate) code_type: Option<String>,
}

impl AuthService {
    /// Validates the request like the real service, then redeems the code.
    pub(crate) async fn authorization_code_grant(
        &self,
        fields: &Fields,
    ) -> Result<TokenResponse, ApiError> {
        let code = fields.non_blank("code")?;
        let verification_code = fields.non_blank("verificationCode")?;
        let verification_code = match (code, verification_code) {
            (Some(_), Some(_)) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "AMBIGUOUS_CODE_FIELDS",
                    "Request cannot contain both 'code' and 'verification_code'. Use 'code' for \
                     OAuth authorization codes or 'verification_code' for passwordless magic links.",
                ));
            }
            (None, None) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "MISSING_CODE",
                    "Either 'code' (for OAuth) or 'verification_code' (for passwordless) is required",
                ));
            }
            (Some(_), None) => {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "unsupported_grant_type",
                    "OAuth authorization code grant is not yet implemented. Use verification_code \
                     for passwordless authentication.",
                ));
            }
            (None, Some(verification_code)) => verification_code,
        };
        let code_verifier = fields
            .non_blank("codeVerifier")?
            .ok_or_else(|| ApiError::invalid_request("code_verifier is required for PKCE"))?;
        if !is_valid_code_verifier(code_verifier) {
            return Err(ApiError::invalid_request(
                "code_verifier must be 43-128 characters using unreserved URI charset",
            ));
        }
        let redirect_uri = fields.non_blank("redirectUri")?.ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "MISSING_REDIRECT_URI",
                "redirect_uri is required",
            )
        })?;
        let exchange = CodeExchange {
            verification_code: verification_code.to_owned(),
            code_verifier: code_verifier.to_owned(),
            redirect_uri: redirect_uri.to_owned(),
            code_type: fields.text("codeType")?.map(str::to_owned),
        };
        self.redeem_verification_code(exchange).await
    }

    /// Redeems a handoff code for a session. The code must exist, be unused and unexpired, have
    /// been sent to the origin of `redirect_uri`, and match the PKCE verifier. Refused codes stay
    /// redeemable (the SDK replays a 401 once, after a refresh attempt); a redeemed code is burnt.
    async fn redeem_verification_code(
        &self,
        exchange: CodeExchange,
    ) -> Result<TokenResponse, ApiError> {
        let code_hash = sha256(exchange.verification_code.as_bytes());
        let challenge = b64url(&sha256(exchange.code_verifier.as_bytes()));
        let redirect_origin = web_origin(&exchange.redirect_uri);
        let now = self.now_ms();
        tracing::debug!(
            code_type = exchange.code_type.as_deref().unwrap_or("(none)"),
            "redeeming a verification code"
        );
        let redeemed = self
            .db
            .call(move |conn| -> Result<Result<User, Rejection>, AuthError> {
                let tx = conn.transaction()?;
                let code = tx
                    .query_row(
                        "SELECT user_id, code_challenge, return_origin, expires_at, used_at
                         FROM _rl_auth_handoff_codes WHERE code_hash = ?1",
                        [&code_hash[..]],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, i64>(3)?,
                                row.get::<_, Option<i64>>(4)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((user_id, stored_challenge, return_origin, expires_at, used_at)) = code
                else {
                    return Ok(Err(Rejection::Unknown));
                };
                let rejection = if used_at.is_some() {
                    Some(Rejection::Used)
                } else if expires_at <= now {
                    Some(Rejection::Expired)
                } else if redirect_origin.as_deref() != Some(return_origin.as_str()) {
                    Some(Rejection::RedirectMismatch)
                } else if !constant_time_eq(stored_challenge.as_bytes(), challenge.as_bytes()) {
                    Some(Rejection::VerifierMismatch)
                } else {
                    None
                };
                if let Some(rejection) = rejection {
                    return Ok(Err(rejection));
                }
                tx.execute(
                    "UPDATE _rl_auth_handoff_codes SET used_at = ?1 WHERE code_hash = ?2",
                    params![now, &code_hash[..]],
                )?;
                let user = find_user(&tx, "id = ?1", &user_id)?;
                tx.commit()?;
                Ok(user.ok_or(Rejection::Unknown))
            })
            .await
            .map_err(|error| ApiError::internal(&AuthError::from(error)))?
            .map_err(|error| ApiError::internal(&error))?;
        let user = redeemed.map_err(ApiError::from)?;
        match self.issue_session(&user).await {
            Ok(tokens) => Ok(tokens),
            // Deleted between the redemption and the sign-in.
            Err(AuthError::UserNotFound) => Err(Rejection::Unknown.into()),
            Err(error) => Err(ApiError::internal(&error)),
        }
    }

    /// A single-use code that signs `user` in, for the app that sent `request`. It expires after
    /// [`HANDOFF_CODE_TTL_MS`].
    pub(crate) async fn issue_handoff_code(
        &self,
        user: &User,
        request: &HandoffRequest,
    ) -> Result<String, AuthError> {
        let code = b64url(&random_bytes::<32>()?);
        let code_hash = sha256(code.as_bytes());
        let now = self.now_ms();
        let user_id = user.id.clone();
        let challenge = request.code_challenge.clone();
        let return_origin = request.return_origin.clone();
        self.db
            .call(move |conn| -> Result<(), AuthError> {
                // Expired codes are kept a while, so a late redemption hears "expired".
                conn.execute(
                    "DELETE FROM _rl_auth_handoff_codes WHERE expires_at < ?1",
                    [now - HANDOFF_CODE_TTL_MS],
                )?;
                let inserted = conn.execute(
                    "INSERT INTO _rl_auth_handoff_codes (code_hash, user_id, code_challenge,
                        return_origin, created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        &code_hash[..],
                        user_id,
                        challenge,
                        return_origin,
                        now,
                        now + HANDOFF_CODE_TTL_MS
                    ],
                );
                match inserted {
                    Ok(_) => Ok(()),
                    // The user was deleted meanwhile.
                    Err(rusqlite::Error::SqliteFailure(error, _))
                        if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY =>
                    {
                        Err(AuthError::UserNotFound)
                    }
                    Err(error) => Err(error.into()),
                }
            })
            .await??;
        Ok(code)
    }
}

/// RFC 7636 §4.1: 43-128 characters of `[A-Za-z0-9-._~]`.
pub(crate) fn is_valid_code_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// RFC 7636 §4.2 for S256, as the real service checks it: 43-128 characters of base64url.
pub(crate) fn is_valid_code_challenge(challenge: &str) -> bool {
    (43..=128).contains(&challenge.len())
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        AuthSettings,
        routes::tests::{Answer, App, app_from},
        tests::{APP_ID, service_with},
    };

    const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const ORIGIN: &str = "http://localhost:5175";

    async fn app_with_code() -> (App, User, String) {
        let (dir, auth) = service_with(AuthSettings::new(APP_ID)).await;
        let user = auth
            .create_user("ada@contoso.com", "secret1")
            .await
            .unwrap();
        let request = HandoffRequest {
            code_challenge: b64url(&sha256(VERIFIER.as_bytes())),
            return_origin: ORIGIN.into(),
            state: "state".into(),
        };
        let code = auth.issue_handoff_code(&user, &request).await.unwrap();
        (app_from(dir, auth), user, code)
    }

    async fn redeem(app: &App, code: &str, verifier: &str, redirect_uri: &str) -> Answer {
        app.post(
            "/api/auth/v1/token",
            json!({
                "grantType": "authorization_code",
                "verificationCode": code,
                "codeVerifier": verifier,
                "redirectUri": redirect_uri,
                "codeType": "fabric_handoff",
            }),
        )
        .await
    }

    fn refused(answer: &Answer, code: &str) {
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{}", answer.body);
        assert_eq!(answer.body["error"], code);
        assert!(answer.body["message"].is_string());
    }

    #[tokio::test]
    async fn codes_are_bound_to_the_verifier_and_the_origin_until_redeemed() {
        let (app, user, code) = app_with_code().await;
        let other_verifier = "x".repeat(43);
        refused(
            &redeem(&app, &code, &other_verifier, ORIGIN).await,
            "INVALID_CODE_VERIFIER",
        );
        refused(
            &redeem(&app, &code, VERIFIER, "http://localhost:5176").await,
            "REDIRECT_URI_MISMATCH",
        );
        refused(
            &redeem(&app, &format!("{code}x"), VERIFIER, ORIGIN).await,
            "INVALID_CODE",
        );
        // Refusals leave the code usable; the redirect URI is compared by origin.
        let tokens = redeem(&app, &code, VERIFIER, "http://LOCALHOST:5175/").await;
        assert_eq!(tokens.status, StatusCode::OK, "{}", tokens.body);
        let access_token = tokens.body["accessToken"].as_str().unwrap();
        assert_eq!(
            app.auth.verify_session_token(access_token).unwrap().sub,
            user.id
        );
        refused(
            &redeem(&app, &code, VERIFIER, ORIGIN).await,
            "CODE_ALREADY_USED",
        );
    }

    #[tokio::test]
    async fn codes_expire_and_die_with_their_user() {
        let (app, _, code) = app_with_code().await;
        app.auth.advance_clock(HANDOFF_CODE_TTL_MS);
        refused(&redeem(&app, &code, VERIFIER, ORIGIN).await, "CODE_EXPIRED");

        let (app, _, code) = app_with_code().await;
        app.auth.delete_user("ada@contoso.com").await.unwrap();
        let answer: Answer = redeem(&app, &code, VERIFIER, ORIGIN).await;
        refused(&answer, "INVALID_CODE");
        assert_eq!(answer.body.get("accessToken"), None::<&Value>);
    }

    #[test]
    fn code_verifiers_follow_rfc_7636() {
        assert!(is_valid_code_verifier(&"a".repeat(43)));
        assert!(is_valid_code_verifier(&format!("{}-._~", "Z9".repeat(20))));
        assert!(!is_valid_code_verifier(&"a".repeat(42)));
        assert!(!is_valid_code_verifier(&"a".repeat(129)));
        assert!(!is_valid_code_verifier(&format!("{}+/", "a".repeat(43))));
    }
}
