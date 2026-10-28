//! Signing in with a Microsoft Entra identity:
//!
//! - `POST /api/auth/v1/brokered/token`, the external Entra exchange: `Authorization: Bearer
//!   <Entra access token>` in, a session out (the `POST /token` success body). The Vite
//!   plugin's auto-login (`@microsoft/rayfin-local-dev`) and `signInWithEntraToken` call it; they
//!   read 400 as "not enabled", 401 as "authentication failed", 403 and 404.
//! - "Continue as <you>" on the sign-in page, with the developer's own Power BI token from the
//!   [`TokenSource`] (see [`AuthService::with_entra`]).
//!
//! An exchanged token must be RS256-signed by one of Microsoft's keys, unexpired (5 minutes of
//! leeway), for an accepted audience (Power BI or Fabric by default), issued by the tenant in its
//! `tid` (an allowed one when the settings list some), and a user's (delegated: it has `scp`).
//! Users are keyed by `oid`; the `email` claim is the UPN.

mod keys;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use rl_tokens::{Audience, TokenSource};
use serde_json::{Map, Value};

pub(crate) use keys::EntraKeys;
use keys::KeyError;

use crate::{
    AuthService, TokenResponse,
    jwt::{LEEWAY_SECONDS, decode_object, number_claim},
    routes::{ApiError, bearer},
    users::looks_like_email,
    util::b64url_decode,
};

const NOT_ENABLED: &str = "External Entra exchange is not enabled for this project";

/// The Entra state of the service.
pub(crate) struct Entra {
    /// The developer's tokens, for "Continue as".
    pub(crate) tokens: Option<Arc<dyn TokenSource>>,
    pub(crate) keys: EntraKeys,
    warned: AtomicBool,
}

impl Entra {
    pub(crate) fn new() -> Self {
        Self {
            tokens: None,
            keys: EntraKeys::microsoft(),
            warned: AtomicBool::new(false),
        }
    }
}

/// Who an Entra token belongs to. Keep it out of info-level logs.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct EntraIdentity {
    /// `oid`, lowercase.
    pub(crate) object_id: String,
    /// `tid`, lowercase.
    pub(crate) tenant_id: String,
    /// What the user signs in with: `upn`, else `email`, `preferred_username` or `unique_name`.
    pub(crate) sign_in_name: Option<String>,
    /// The first of those that is an email address: the session's `email` claim.
    pub(crate) email: Option<String>,
    /// `name`.
    pub(crate) name: Option<String>,
}

impl std::fmt::Debug for EntraIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntraIdentity")
            .field("has_email", &self.email.is_some())
            .finish_non_exhaustive()
    }
}

/// Why an Entra token was refused (the messages name no identity).
#[derive(Debug, thiserror::Error)]
pub(crate) enum EntraTokenError {
    #[error("it is not a JWT")]
    Malformed,
    #[error("it is not signed with RS256")]
    Algorithm,
    #[error(transparent)]
    Key(#[from] KeyError),
    #[error("its signature is invalid")]
    Signature,
    #[error("it has expired")]
    Expired,
    #[error("it is not valid yet")]
    NotYetValid,
    #[error("it is for {0}, not one of the accepted audiences ({1})")]
    Audience(String, String),
    #[error("it was not issued by the Microsoft Entra tenant it names")]
    Issuer,
    #[error("its tenant {0} is not one of the allowed tenants")]
    Tenant(String),
    #[error("it is an application's token, not a user's")]
    NotDelegated,
    #[error("it has no {0} claim")]
    MissingClaim(&'static str),
}

/// Why the developer's identity is not available for "Continue as".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeveloperIdentityError {
    pub(crate) message: String,
    pub(crate) hint: Option<String>,
}

pub(crate) async fn brokered_token(
    State(auth): State<Arc<AuthService>>,
    headers: HeaderMap,
) -> Response {
    match auth.exchange_entra_token(&headers).await {
        Ok(tokens) => Json(tokens).into_response(),
        Err(error) => error.into_response(),
    }
}

pub(crate) async fn brokered_authorize_external(State(auth): State<Arc<AuthService>>) -> Response {
    auth.brokered_unavailable().into_response()
}

impl AuthService {
    /// What the brokered routes answer when they are off: the SDK reads 400 as "not enabled"
    /// and 404 as "not available".
    fn brokered_unavailable(&self) -> ApiError {
        if !self.settings.enabled {
            return ApiError::not_found();
        }
        if !(self.settings.fabric_enabled && self.settings.external_entra_exchange) {
            return ApiError::mode_disabled(NOT_ENABLED);
        }
        ApiError::not_found()
    }

    async fn exchange_entra_token(&self, headers: &HeaderMap) -> Result<TokenResponse, ApiError> {
        if !(self.settings.enabled && self.settings.entra_signin.enabled) {
            return Err(self.brokered_unavailable());
        }
        self.warn_about_fabric_flags();
        let Some(token) = bearer(headers) else {
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_token",
                "An Entra access token is required (Authorization: Bearer <token>).",
            ));
        };
        let identity = match self.verify_entra_token(token).await {
            Ok(identity) => identity,
            Err(EntraTokenError::Key(KeyError::Unavailable(reason))) => {
                tracing::warn!("could not check an Entra token: {reason}");
                return Err(ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "keys_unavailable",
                    reason,
                ));
            }
            Err(error) => {
                tracing::warn!("refused an Entra token exchange: the token {error}");
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "invalid_token",
                    format!("The Entra token was refused: {error}."),
                ));
            }
        };
        let user = self
            .find_or_create_external_user(
                &identity.object_id,
                identity.email.as_deref(),
                identity.name.as_deref(),
            )
            .await
            .map_err(|error| ApiError::internal(&error))?;
        self.issue_session(&user)
            .await
            .map_err(|error| ApiError::internal(&error))
    }

    /// Locally the exchange works without `externalEntraExchange`; say once that Fabric won't.
    fn warn_about_fabric_flags(&self) {
        if self.settings.fabric_enabled && self.settings.external_entra_exchange {
            return;
        }
        if !self.entra.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "rayfin-local accepts Entra token exchanges (POST /api/auth/v1/brokered/token, \
                 which the Vite plugin's auto-login uses), but Fabric only does when \
                 rayfin/rayfin.yml sets services.auth.fabric.enabled and \
                 services.auth.fabric.externalEntraExchange to true: set them before you deploy"
            );
        }
    }

    /// Validates an Entra access token (see the module docs) and says whose it is.
    pub(crate) async fn verify_entra_token(
        &self,
        token: &str,
    ) -> Result<EntraIdentity, EntraTokenError> {
        let mut parts = token.split('.');
        let (Some(header_part), Some(payload_part), Some(signature_part), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(EntraTokenError::Malformed);
        };
        let header = decode_object(header_part).map_err(|_| EntraTokenError::Malformed)?;
        if header.get("alg").and_then(Value::as_str) != Some("RS256") {
            return Err(EntraTokenError::Algorithm);
        }
        let kid = text_claim(&header, "kid").ok_or(EntraTokenError::MissingClaim("kid"))?;
        let key = self.entra.keys.key(kid).await?;
        let signature = b64url_decode(signature_part).ok_or(EntraTokenError::Malformed)?;
        let signing_input = &token[..header_part.len() + 1 + payload_part.len()];
        if !key.verify(signing_input.as_bytes(), &signature) {
            return Err(EntraTokenError::Signature);
        }

        let claims = decode_object(payload_part).map_err(|_| EntraTokenError::Malformed)?;
        let now = self.now_ms() / 1000;
        let exp = number_claim(&claims, "exp").ok_or(EntraTokenError::MissingClaim("exp"))?;
        if now > exp.saturating_add(LEEWAY_SECONDS) {
            return Err(EntraTokenError::Expired);
        }
        if number_claim(&claims, "nbf").is_some_and(|nbf| nbf > now.saturating_add(LEEWAY_SECONDS))
        {
            return Err(EntraTokenError::NotYetValid);
        }
        self.check_audience(&claims)?;
        let tenant = text_claim(&claims, "tid").ok_or(EntraTokenError::MissingClaim("tid"))?;
        let issuer = text_claim(&claims, "iss").ok_or(EntraTokenError::MissingClaim("iss"))?;
        let pinned_elsewhere = key
            .tenant
            .as_deref()
            .is_some_and(|pinned| !pinned.eq_ignore_ascii_case(tenant));
        if !issued_by(issuer, tenant) || pinned_elsewhere {
            return Err(EntraTokenError::Issuer);
        }
        if !self.tenant_allowed(tenant) {
            return Err(EntraTokenError::Tenant(tenant.to_owned()));
        }
        if text_claim(&claims, "scp").is_none() || text_claim(&claims, "idtyp") == Some("app") {
            return Err(EntraTokenError::NotDelegated);
        }
        identity_from_claims(&claims)
    }

    fn check_audience(&self, claims: &Map<String, Value>) -> Result<(), EntraTokenError> {
        let accepted = &self.settings.entra_signin.audiences;
        let is_accepted = |aud: &str| {
            accepted.iter().any(|ok| {
                ok.trim_end_matches('/')
                    .eq_ignore_ascii_case(aud.trim_end_matches('/'))
            })
        };
        let audiences: Vec<&str> = match claims.get("aud") {
            Some(Value::String(aud)) => vec![aud],
            Some(Value::Array(auds)) => auds.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        };
        if audiences.iter().any(|aud| is_accepted(aud)) {
            return Ok(());
        }
        let shown = if audiences.is_empty() {
            "no audience".to_owned()
        } else {
            audiences.join(", ")
        };
        Err(EntraTokenError::Audience(shown, accepted.join(", ")))
    }

    fn tenant_allowed(&self, tenant: &str) -> bool {
        let allowed = &self.settings.entra_signin.allowed_tenants;
        allowed.is_empty() || allowed.iter().any(|ok| ok.eq_ignore_ascii_case(tenant))
    }

    /// Whether the sign-in page offers "Continue as <you>".
    pub(crate) fn entra_portal_enabled(&self) -> bool {
        self.settings.entra_signin.enabled && self.entra.tokens.is_some()
    }

    /// The developer's identity, from a Power BI token minted for them (no signature check: the
    /// token comes from their own Azure sign-in, not from a client).
    pub(crate) async fn developer_identity(&self) -> Result<EntraIdentity, DeveloperIdentityError> {
        let Some(tokens) = &self.entra.tokens else {
            return Err(DeveloperIdentityError {
                message: "rayfin-local was started without access to your Azure sign-in".into(),
                hint: None,
            });
        };
        let token = tokens
            .token(Audience::PowerBi.default_scope())
            .await
            .map_err(|error| DeveloperIdentityError {
                message: error.message,
                hint: error.hint,
            })?;
        let not_a_user = || DeveloperIdentityError {
            message: "your Azure sign-in is not a user account".into(),
            hint: Some("Sign in with your user account: `az login`.".into()),
        };
        let claims = rl_tokens::jwt_claims(&token.token).ok_or_else(not_a_user)?;
        let identity = identity_from_claims(&claims).map_err(|_| not_a_user())?;
        if !self.tenant_allowed(&identity.tenant_id) {
            return Err(DeveloperIdentityError {
                message: format!(
                    "your Azure sign-in is for tenant {}, which this app does not accept",
                    identity.tenant_id
                ),
                hint: Some("Sign in to an allowed tenant: `az login --tenant <tenant id>`.".into()),
            });
        }
        Ok(identity)
    }
}

/// `https://sts.windows.net/{tid}/` (v1 tokens) or `https://login.microsoftonline.com/{tid}/v2.0`.
fn issued_by(issuer: &str, tenant: &str) -> bool {
    issuer.eq_ignore_ascii_case(&format!("https://sts.windows.net/{tenant}/"))
        || issuer.eq_ignore_ascii_case(&format!("https://login.microsoftonline.com/{tenant}/v2.0"))
}

fn identity_from_claims(claims: &Map<String, Value>) -> Result<EntraIdentity, EntraTokenError> {
    let object_id = text_claim(claims, "oid").ok_or(EntraTokenError::MissingClaim("oid"))?;
    let tenant_id = text_claim(claims, "tid").ok_or(EntraTokenError::MissingClaim("tid"))?;
    let names: Vec<&str> = ["upn", "email", "preferred_username", "unique_name"]
        .into_iter()
        .filter_map(|name| text_claim(claims, name))
        .collect();
    Ok(EntraIdentity {
        object_id: object_id.to_ascii_lowercase(),
        tenant_id: tenant_id.to_ascii_lowercase(),
        sign_in_name: names.first().map(|name| (*name).to_owned()),
        email: names
            .iter()
            .find(|name| looks_like_email(name))
            .map(|name| (*name).to_owned()),
        name: text_claim(claims, "name").map(str::to_owned),
    })
}

fn text_claim<'a>(claims: &'a Map<String, Value>, name: &str) -> Option<&'a str> {
    claims
        .get(name)?
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, SystemTime};

    use axum::http::{Request, header};
    use ring::{rand::SystemRandom, signature::RsaKeyPair};
    use rl_model::CallerResolver;
    use rl_tokens::{AccessToken, BoxFuture, TokenError};
    use serde_json::json;

    use super::*;
    use crate::{
        AuthSettings,
        routes::tests::{Answer, App, app_from},
        tests::{APP_ID, service_with},
        util::{b64url, unix_now_ms},
    };

    pub(crate) const TRUSTED_KID: &str = "trusted-kid";
    pub(crate) const OTHER_KID: &str = "other-kid";
    const TRUSTED_KEY: &[u8] = include_bytes!("entra/test-rsa-trusted.pk8");
    const OTHER_KEY: &[u8] = include_bytes!("entra/test-rsa-other.pk8");
    const TENANT: &str = "72f988bf-86f1-41af-91ab-2d7cd011db47";
    const OID: &str = "0b2c1a9e-5d3f-4e6a-8b7c-9d0e1f2a3b4c";

    /// The JWK of the trusted test key, under `kid`.
    pub(crate) fn jwk(kid: &str) -> Value {
        let pair = RsaKeyPair::from_pkcs8(TRUSTED_KEY).unwrap();
        let (n, e) = rsa_public_components(pair.public().as_ref());
        json!({ "kty": "RSA", "use": "sig", "kid": kid, "n": b64url(n), "e": b64url(e) })
    }

    /// `RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }`.
    fn rsa_public_components(der: &[u8]) -> (&[u8], &[u8]) {
        fn element(input: &[u8], tag: u8) -> (&[u8], &[u8]) {
            assert_eq!(input[0], tag);
            let (length, start) = match input[1] {
                short if short < 0x80 => (usize::from(short), 2),
                0x81 => (usize::from(input[2]), 3),
                0x82 => ((usize::from(input[2]) << 8) | usize::from(input[3]), 4),
                other => panic!("unexpected DER length byte {other:#x}"),
            };
            (&input[start..start + length], &input[start + length..])
        }
        let (sequence, _) = element(der, 0x30);
        let (n, rest) = element(sequence, 0x02);
        let (e, _) = element(rest, 0x02);
        (n, e)
    }

    fn now() -> i64 {
        unix_now_ms() / 1000
    }

    /// The claims of a Power BI token az would mint for the test user.
    pub(crate) fn user_claims() -> Value {
        json!({
            "aud": "https://analysis.windows.net/powerbi/api",
            "iss": format!("https://sts.windows.net/{TENANT}/"),
            "iat": now() - 60, "nbf": now() - 60, "exp": now() + 3600,
            "name": "Ada Lovelace", "oid": OID, "scp": "user_impersonation",
            "tid": TENANT, "unique_name": "ada@contoso.com", "upn": "ada@contoso.com",
        })
    }

    /// An RS256 JWT over `claims`, signed with `key`, naming `kid`.
    fn sign(claims: &Value, kid: &str, key: &[u8]) -> String {
        let pair = RsaKeyPair::from_pkcs8(key).unwrap();
        let header = json!({ "typ": "JWT", "alg": "RS256", "kid": kid });
        let input = format!(
            "{}.{}",
            b64url(header.to_string().as_bytes()),
            b64url(claims.to_string().as_bytes())
        );
        let mut signature = vec![0; pair.public().modulus_len()];
        pair.sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            input.as_bytes(),
            &mut signature,
        )
        .unwrap();
        format!("{input}.{}", b64url(&signature))
    }

    fn trusted(claims: &Value) -> String {
        sign(claims, TRUSTED_KID, TRUSTED_KEY)
    }

    pub(crate) fn entra_settings() -> AuthSettings {
        let mut settings = AuthSettings::new(APP_ID);
        settings.entra_signin.enabled = true;
        settings
    }

    async fn entra_app(settings: AuthSettings) -> App {
        let (dir, mut auth) = service_with(settings).await;
        auth.entra.keys = EntraKeys::fixed(&json!({ "keys": [jwk(TRUSTED_KID)] }));
        app_from(dir, auth)
    }

    async fn exchange(app: &App, token: Option<&str>) -> Answer {
        let mut request = Request::post("/api/auth/v1/brokered/token");
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        app.send(request.body(axum::body::Body::empty()).unwrap())
            .await
    }

    fn user_id(app: &App, answer: &Answer) -> String {
        let token = answer.body["accessToken"].as_str().unwrap();
        app.auth.verify_session_token(token).unwrap().sub
    }

    #[tokio::test]
    async fn a_valid_token_signs_in_the_user_keyed_by_oid() {
        let app = entra_app(entra_settings()).await;
        let first = exchange(&app, Some(&trusted(&user_claims()))).await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.body);
        let keys: Vec<_> = first.body.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            [
                "accessToken",
                "tokenType",
                "expiresIn",
                "refreshToken",
                "scope"
            ]
        );
        let session = app
            .auth
            .resolve(first.body["accessToken"].as_str())
            .unwrap()
            .session
            .unwrap();
        assert_eq!(session.email.as_deref(), Some("ada@contoso.com"));
        assert_eq!(session.claims["name"], "Ada Lovelace");

        // Within the leeway, with a new display name: the same user.
        let mut claims = user_claims();
        claims["exp"] = json!(now() - 200);
        claims["nbf"] = json!(now() + 200);
        claims["name"] = json!("Ada King");
        let second = exchange(&app, Some(&trusted(&claims))).await;
        assert_eq!(second.status, StatusCode::OK, "{}", second.body);
        assert_eq!(user_id(&app, &second), user_id(&app, &first));
        let session = app
            .auth
            .resolve(second.body["accessToken"].as_str())
            .unwrap()
            .session
            .unwrap();
        assert_eq!(session.claims["name"], "Ada King");
        let users = app.auth.list_users().await.unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].entra_oid.as_deref(), Some(OID));
    }

    #[tokio::test]
    async fn tokens_that_fail_validation_are_refused() {
        let mut settings = entra_settings();
        settings.entra_signin.allowed_tenants = vec![TENANT.to_uppercase()];
        let app = entra_app(settings).await;
        let with = |changes: Value| {
            let mut claims = user_claims();
            let object = claims.as_object_mut().unwrap();
            for (name, value) in changes.as_object().unwrap() {
                if value.is_null() {
                    object.remove(name);
                } else {
                    object.insert(name.clone(), value.clone());
                }
            }
            trusted(&claims)
        };
        let other_tenant = "f8cdef31-a31e-4b4a-93e4-5f571e91255a";
        let other_issuer = format!("https://sts.windows.net/{other_tenant}/");
        let valid = trusted(&user_claims());
        let mut parts = valid.split('.');
        let (head, _, signature) = (parts.next().unwrap(), parts.next(), parts.next().unwrap());
        let mut forged_claims = user_claims();
        forged_claims["oid"] = json!("someone-else");
        let forged_payload = b64url(forged_claims.to_string().as_bytes());
        let unsigned_head = b64url(br#"{"alg":"none","typ":"JWT"}"#);
        let cases = [
            ("expired", with(json!({ "exp": now() - 400 }))),
            ("not yet valid", with(json!({ "nbf": now() + 400 }))),
            (
                "for Microsoft Graph",
                with(json!({ "aud": "https://graph.microsoft.com" })),
            ),
            ("without audience", with(json!({ "aud": null }))),
            (
                "of another tenant",
                with(json!({ "tid": other_tenant, "iss": other_issuer })),
            ),
            (
                "issued by another tenant",
                with(json!({ "iss": other_issuer })),
            ),
            (
                "app-only",
                with(json!({ "scp": null, "roles": ["Tenant.Read.All"] })),
            ),
            ("of an app identity", with(json!({ "idtyp": "app" }))),
            ("without oid", with(json!({ "oid": null }))),
            (
                "signed by another key",
                sign(&user_claims(), TRUSTED_KID, OTHER_KEY),
            ),
            (
                "signed by an unknown key",
                sign(&user_claims(), OTHER_KID, OTHER_KEY),
            ),
            ("forged", format!("{head}.{forged_payload}.{signature}")),
            ("unsigned", format!("{unsigned_head}.{forged_payload}.")),
            ("garbage", "not-a-token".to_owned()),
        ];
        for (case, token) in cases {
            let answer = exchange(&app, Some(&token)).await;
            assert_eq!(
                answer.status,
                StatusCode::UNAUTHORIZED,
                "{case}: {}",
                answer.body
            );
            assert_eq!(answer.body["error"], "invalid_token", "{case}");
        }
        assert_eq!(exchange(&app, None).await.status, StatusCode::UNAUTHORIZED);
        assert!(app.auth.list_users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn keys_pinned_to_another_tenant_cannot_sign_for_this_one() {
        let (dir, mut auth) = service_with(entra_settings()).await;
        let mut pinned = jwk(TRUSTED_KID);
        pinned["issuer"] =
            json!("https://login.microsoftonline.com/9188040d-6c67-4c5b-b112-36a304b66dad/v2.0");
        auth.entra.keys = EntraKeys::fixed(&json!({ "keys": [pinned] }));
        let app = app_from(dir, auth);
        let answer = exchange(&app, Some(&trusted(&user_claims()))).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_disabled_exchange_answers_like_the_real_service() {
        let token = trusted(&user_claims());
        let off = entra_app(AuthSettings::new(APP_ID)).await;
        let answer = exchange(&off, Some(&token)).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
        assert_eq!(answer.body["error"], "auth_mode_disabled");

        let mut settings = entra_settings();
        settings.enabled = false;
        let no_auth = entra_app(settings).await;
        assert_eq!(
            exchange(&no_auth, Some(&token)).await.status,
            StatusCode::NOT_FOUND
        );
    }

    /// A [`TokenSource`] that mints an unsigned token with these claims, or fails.
    pub(crate) struct FakeTokens(pub(crate) Result<Value, TokenError>);

    impl TokenSource for FakeTokens {
        fn token<'a>(&'a self, scope: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
            assert_eq!(scope, Audience::PowerBi.default_scope());
            let result = self.0.clone().map(|claims| AccessToken {
                token: format!(
                    "{}.{}.signature",
                    b64url(br#"{"alg":"RS256","typ":"JWT"}"#),
                    b64url(claims.to_string().as_bytes())
                ),
                expires_at: SystemTime::now() + Duration::from_secs(3600),
            });
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn the_developer_identity_comes_from_their_power_bi_token() {
        let (_dir, auth) = service_with(entra_settings()).await;
        let auth = auth.with_entra(Arc::new(FakeTokens(Ok(user_claims()))));
        assert!(auth.entra_portal_enabled());
        let identity = auth.developer_identity().await.unwrap();
        assert_eq!(identity.object_id, OID);
        assert_eq!(identity.sign_in_name.as_deref(), Some("ada@contoso.com"));
        assert_eq!(identity.email.as_deref(), Some("ada@contoso.com"));
        assert_eq!(identity.name.as_deref(), Some("Ada Lovelace"));

        let mut service_principal = user_claims();
        service_principal.as_object_mut().unwrap().remove("oid");
        let (_dir, auth) = service_with(entra_settings()).await;
        let auth = auth.with_entra(Arc::new(FakeTokens(Ok(service_principal))));
        let error = auth.developer_identity().await.unwrap_err();
        assert!(error.hint.unwrap().contains("az login"));
    }
}
