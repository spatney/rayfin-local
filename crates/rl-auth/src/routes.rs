//! The HTTP surface: `/api/auth/v1/*`, JWKS and the project runtime settings, answering with the
//! real service's statuses and bodies (`{"error": code, "message": text}` from its exception
//! middleware, `{"error", "error_description"}` for disabled auth modes, `{"error": text}` from
//! its authentication middleware).

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{any, get, post},
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    AuthError, AuthService, TokenResponse,
    jwt::{AUTHENTICATED_ROLE, AccessClaims, TokenError},
    users::User,
    util::format_timestamp,
};

/// The JSON Web Key Set, at the API root as the SDK expects.
pub const JWKS_PATH: &str = "/.well-known/jwks.json";
/// The project's runtime settings (`auth.getAuthSettings()`).
pub const RUNTIME_SETTINGS_PATH: &str = "/api/projectRuntimeSettings";

const JWKS_CACHE_CONTROL: &str = "public, max-age=3600, stale-while-revalidate=86400";
const PASSWORD_DISABLED: &str = "Password authentication is not enabled for this project";
const PASSWORDLESS_DISABLED: &str = "Passwordless authentication is not enabled for this project";
const EMAIL_DISABLED: &str = "Email verification is not enabled for this project";

type AppState = State<Arc<AuthService>>;

impl AuthService {
    /// The auth routes with their state, for `rl-server` to merge:
    ///
    /// - `POST /api/auth/v1/signup`, `POST /api/auth/v1/token` (`password`, `refresh_token`,
    ///   `authorization_code`), `POST /api/auth/v1/signout`, `POST /api/auth/v1/signout-all`
    /// - [`JWKS_PATH`] and [`RUNTIME_SETTINGS_PATH`]
    /// - `POST /api/auth/v1/brokered/token`: the external Entra exchange, when
    ///   [`crate::EntraSettings::enabled`]
    /// - the sign-in portal under [`crate::PORTAL_PATH`] (the app's `fabricPortalUrl`), with the
    ///   local Fabric host at [`crate::EMBED_HOST_PATH`]
    /// - [`crate::DEV_SESSION_TOKEN_PATH`]: automatic sign-in, for rayfin-local's dev server
    ///   (see [`AuthService::with_dev_sign_in_secret`])
    /// - the routes of features rayfin-local lacks (magic links, email verification, password
    ///   reset email), answering as the real service does when the feature is disabled
    ///
    /// The publishable key is not checked here (`rl-server` does it; see
    /// [`crate::PUBLISHABLE_KEY_EXEMPT_PATHS`]; the portal is under `/.rayfin-local`, which needs
    /// no key either).
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/api/auth/v1/signup", post(signup))
            .route("/api/auth/v1/token", post(token))
            .route("/api/auth/v1/signout", post(signout))
            .route("/api/auth/v1/signout-all", post(signout_all))
            .route("/api/auth/v1/passwordless/send", post(passwordless_send))
            .route("/api/auth/v1/verify-email", get(email_disabled))
            .route(
                "/api/auth/v1/resend-verification-email",
                post(email_disabled),
            )
            .route("/api/auth/v1/reset-password", post(reset_password))
            .route(
                "/api/auth/v1/reset-password/complete",
                post(complete_password_reset),
            )
            .route(
                "/api/auth/v1/brokered/token",
                post(crate::entra::brokered_token),
            )
            .route(
                "/api/auth/v1/brokered/authorize/external",
                any(crate::entra::brokered_authorize_external),
            )
            .route(JWKS_PATH, get(jwks))
            .route(RUNTIME_SETTINGS_PATH, get(runtime_settings))
            .route(
                crate::DEV_SESSION_TOKEN_PATH,
                get(crate::identity::dev_session_token),
            )
            .merge(crate::portal::routes())
            .with_state(self)
    }
}

/// An error answer.
#[derive(Debug)]
pub(crate) struct ApiError {
    status: StatusCode,
    body: Option<Value>,
}

impl ApiError {
    /// The real service's error envelope: `{"error": code, "message": text}`.
    pub(crate) fn new(status: StatusCode, error: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: Some(json!({ "error": error, "message": message.into() })),
        }
    }

    pub(crate) fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    /// A route whose auth mode `rayfin.yml` leaves off.
    pub(crate) fn mode_disabled(description: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            body: Some(json!({ "error": "auth_mode_disabled", "error_description": description })),
        }
    }

    /// The authentication middleware's 401 for a bad bearer token.
    fn unauthenticated(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            body: Some(json!({ "error": message })),
        }
    }

    pub(crate) fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            body: None,
        }
    }

    pub(crate) fn internal(error: &AuthError) -> Self {
        tracing::error!("auth request failed: {error}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_server_error",
            "An internal server error occurred while processing the request.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self.body {
            Some(body) => (self.status, Json(body)).into_response(),
            None => self.status.into_response(),
        }
    }
}

/// A request body's fields, looked up case-insensitively like the real service's model binding.
#[derive(Default)]
pub(crate) struct Fields(Map<String, Value>);

impl Fields {
    pub(crate) fn json(body: &[u8]) -> Result<Self, ApiError> {
        if body.iter().all(u8::is_ascii_whitespace) {
            return Err(ApiError::invalid_request(
                "A non-empty request body is required.",
            ));
        }
        match serde_json::from_slice(body) {
            Ok(Value::Object(fields)) => Ok(Self(fields)),
            _ => Err(ApiError::invalid_request(
                "The request body must be a JSON object.",
            )),
        }
    }

    fn form(body: &[u8]) -> Self {
        Self(
            url::form_urlencoded::parse(body)
                .map(|(name, value)| (name.into_owned(), Value::String(value.into_owned())))
                .collect(),
        )
    }

    /// A string field; `None` when absent or `null`.
    pub(crate) fn text(&self, name: &str) -> Result<Option<&str>, ApiError> {
        let value = self.0.get(name).or_else(|| {
            self.0
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
        });
        match value {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(ApiError::invalid_request(format!(
                "The {name} field must be a string."
            ))),
        }
    }

    /// A string field that is present and not blank.
    pub(crate) fn non_blank(&self, name: &str) -> Result<Option<&str>, ApiError> {
        Ok(self.text(name)?.filter(|text| !text.trim().is_empty()))
    }
}

pub(crate) fn respond<T: Serialize>(result: Result<T, ApiError>) -> Response {
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error.into_response(),
    }
}

pub(crate) fn bearer(headers: &HeaderMap) -> Option<&str> {
    rl_model::bearer_token(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    )
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("json"))
}

async fn signup(State(auth): AppState, body: Bytes) -> Response {
    respond(auth.signup(&body).await)
}

async fn token(State(auth): AppState, body: Bytes) -> Response {
    respond(auth.token(&body).await)
}

async fn signout(State(auth): AppState, headers: HeaderMap, body: Bytes) -> Response {
    match auth.signout(&headers, &body).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => error.into_response(),
    }
}

async fn signout_all(State(auth): AppState, headers: HeaderMap) -> Response {
    respond(auth.signout_all(&headers).await)
}

async fn passwordless_send(State(auth): AppState) -> Response {
    respond::<Value>(
        auth.require_enabled()
            .and(Err(ApiError::mode_disabled(PASSWORDLESS_DISABLED))),
    )
}

async fn email_disabled(State(auth): AppState) -> Response {
    respond::<Value>(
        auth.require_enabled()
            .and(Err(ApiError::mode_disabled(EMAIL_DISABLED))),
    )
}

async fn reset_password(State(auth): AppState, body: Bytes) -> Response {
    respond(auth.reset_password(&body))
}

async fn complete_password_reset(State(auth): AppState, body: Bytes) -> Response {
    respond::<Value>(auth.complete_password_reset(&body))
}

async fn jwks(State(auth): AppState) -> Response {
    match auth.require_enabled() {
        Ok(()) => (
            [(header::CACHE_CONTROL, JWKS_CACHE_CONTROL)],
            Json(auth.jwks()),
        )
            .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn runtime_settings(State(auth): AppState) -> Json<Value> {
    Json(auth.settings().runtime_settings())
}

impl AuthService {
    /// Every auth route answers 404 when `services.auth.enabled` is off.
    pub(crate) fn require_enabled(&self) -> Result<(), ApiError> {
        if self.settings().enabled {
            Ok(())
        } else {
            Err(ApiError::not_found())
        }
    }

    fn require_password_mode(&self) -> Result<(), ApiError> {
        self.require_enabled()?;
        if self.settings().password_enabled {
            Ok(())
        } else {
            Err(ApiError::mode_disabled(PASSWORD_DISABLED))
        }
    }

    /// Checks a bearer token like the real service's authentication middleware.
    fn authenticate(&self, token: &str) -> Result<AccessClaims, ApiError> {
        self.verify_session_token(token)
            .map_err(|error| match error {
                TokenError::Revoked => ApiError::unauthenticated("Session is no longer valid"),
                _ => ApiError::unauthenticated("Invalid authentication token"),
            })
    }

    async fn signup(&self, body: &[u8]) -> Result<Value, ApiError> {
        self.require_password_mode()?;
        if !self.settings().signup_enabled {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "signup_disabled",
                "Sign-up is turned off for this app. Ask the developer for an account (`rayfin-local \
                 users add`).",
            ));
        }
        let fields = Fields::json(body)?;
        let email = fields
            .non_blank("email")?
            .ok_or_else(|| ApiError::invalid_request("Email is required"))?;
        let password = fields
            .non_blank("password")?
            .ok_or_else(|| ApiError::invalid_request("Password is required and cannot be empty"))?;
        match self.create_user(email, password).await {
            Ok(user) => Ok(signup_response(&user)),
            Err(AuthError::UsernameTaken) => Err(ApiError::new(
                StatusCode::CONFLICT,
                "email_already_registered",
                "Email is already registered",
            )),
            Err(AuthError::InvalidInput(message)) => Err(ApiError::invalid_request(message)),
            Err(error) => Err(ApiError::internal(&error)),
        }
    }

    /// `POST /token`. It ignores `Authorization`: the SDK sends its stale token along.
    async fn token(&self, body: &[u8]) -> Result<TokenResponse, ApiError> {
        self.require_enabled()?;
        let fields = Fields::json(body)?;
        let grant_type = fields
            .non_blank("grantType")?
            .ok_or_else(|| ApiError::invalid_request("The grantType field is required."))?;
        match grant_type.to_ascii_lowercase().as_str() {
            "password" => self.password_grant(&fields).await,
            "refresh_token" => self.refresh_grant(&fields).await,
            "authorization_code" => self.authorization_code_grant(&fields).await,
            _ => Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                format!(
                    "Grant type '{grant_type}' is not supported. Supported types: password, \
                     refresh_token"
                ),
            )),
        }
    }

    async fn password_grant(&self, fields: &Fields) -> Result<TokenResponse, ApiError> {
        let (Some(email), Some(password)) =
            (fields.non_blank("email")?, fields.non_blank("password")?)
        else {
            return Err(ApiError::invalid_request(
                "Email and password are required for password grant",
            ));
        };
        if let Some(user) = self.auto_create(email, password).await? {
            return self
                .issue_session(&user)
                .await
                .map_err(|error| ApiError::internal(&error));
        }
        let user = match self.verify_password(email, password).await {
            Ok(user) => user,
            Err(AuthError::InvalidCredentials) => {
                return Err(ApiError::new(
                    StatusCode::UNAUTHORIZED,
                    "invalid_grant",
                    "Invalid email or password",
                ));
            }
            Err(error) => return Err(ApiError::internal(&error)),
        };
        self.issue_session(&user)
            .await
            .map_err(|error| ApiError::internal(&error))
    }

    /// Creates the account for a password sign-in to an email that has none, when
    /// [`AuthSettings::auto_create_users`] and sign-up allow it. `None` leaves the sign-in to the
    /// usual checks (an existing account, or credentials sign-up would refuse).
    ///
    /// `password.enabled: false` does not stop it, as it does not stop the password grant (it
    /// only closes sign-up and password reset): apps that offer only Fabric sign-in in production
    /// often sign in locally with a fixed dev account.
    async fn auto_create(&self, email: &str, password: &str) -> Result<Option<User>, ApiError> {
        let settings = self.settings();
        if !(settings.auto_create_users && settings.signup_enabled) {
            return Ok(None);
        }
        let exists = self
            .find_user(email)
            .await
            .map_err(|error| ApiError::internal(&error))?
            .is_some();
        if exists {
            return Ok(None);
        }
        match self.create_user(email, password).await {
            Ok(user) => {
                tracing::info!(
                    "Created a local user on its first password sign-in (turn this off with \
                     `[auth] auto_create_users = false` in rayfin-local.toml)"
                );
                Ok(Some(user))
            }
            // Raced with another sign-in, or input sign-up refuses: the usual checks answer.
            Err(AuthError::UsernameTaken | AuthError::InvalidInput(_)) => Ok(None),
            Err(error) => Err(ApiError::internal(&error)),
        }
    }

    async fn refresh_grant(&self, fields: &Fields) -> Result<TokenResponse, ApiError> {
        let refresh_token = fields.non_blank("refreshToken")?.ok_or_else(|| {
            ApiError::invalid_request("refresh_token is required for refresh_token grant")
        })?;
        match self.refresh(refresh_token).await {
            Ok(tokens) => Ok(tokens),
            Err(AuthError::InvalidGrant(message)) => Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "invalid_grant",
                message,
            )),
            Err(error) => Err(ApiError::internal(&error)),
        }
    }

    /// `POST /signout` (RFC 7009): `200` with no body whether or not anything was revoked. The
    /// session of the `token` field (form or JSON; the bearer token when absent) is signed out
    /// when it belongs to the bearer's user.
    async fn signout(&self, headers: &HeaderMap, body: &[u8]) -> Result<(), ApiError> {
        self.require_enabled()?;
        let bearer = bearer(headers);
        let caller = bearer.map(|token| self.authenticate(token)).transpose()?;
        let fields = if is_json(headers) {
            Fields::json(body).unwrap_or_default()
        } else {
            Fields::form(body)
        };
        let token = fields.non_blank("token").ok().flatten().or(bearer);
        let (Some(caller), Some(token)) = (caller, token) else {
            return Ok(());
        };
        let Ok(claims) = self.jwt().verify(token, self.now_ms() / 1000) else {
            return Ok(());
        };
        if claims.sub == caller.sub
            && let Err(error) = self.revoke_session(&claims.sid, &claims.sub).await
        {
            tracing::warn!("could not record a sign-out: {error}");
        }
        Ok(())
    }

    /// `POST /signout-all`: signs out every session of the bearer's user.
    async fn signout_all(&self, headers: &HeaderMap) -> Result<Value, ApiError> {
        self.require_enabled()?;
        let invalid_client =
            || ApiError::new(StatusCode::UNAUTHORIZED, "invalid_client", "invalid_client");
        let token = bearer(headers).ok_or_else(invalid_client)?;
        let claims = self.authenticate(token)?;
        if uuid::Uuid::parse_str(&claims.sub).is_err() {
            return Err(invalid_client());
        }
        let count = self
            .revoke_all_sessions(&claims.sub)
            .await
            .map_err(|error| ApiError::internal(&error))?;
        Ok(json!({ "count": count }))
    }

    /// `POST /reset-password`: the real service with email off answers success without sending
    /// anything; so does rayfin-local.
    fn reset_password(&self, body: &[u8]) -> Result<Value, ApiError> {
        self.require_password_mode()?;
        let fields = Fields::json(body)?;
        fields
            .non_blank("email")?
            .ok_or_else(|| ApiError::invalid_request("Email is required"))?;
        tracing::info!(
            "a password reset was requested; rayfin-local sends no email: set a new password with \
             `rayfin-local users reset-password`"
        );
        Ok(json!({
            "success": true,
            "message": "If an account exists with this email, a reset link has been sent.",
        }))
    }

    /// `POST /reset-password/complete`: rayfin-local never issues reset tokens.
    fn complete_password_reset(&self, body: &[u8]) -> Result<Value, ApiError> {
        self.require_password_mode()?;
        let fields = Fields::json(body)?;
        let missing = |name: &str| -> Result<bool, ApiError> {
            Ok(fields.text(name)?.is_none_or(str::is_empty))
        };
        if missing("token")? || missing("newPassword")? {
            return Err(ApiError::invalid_request(
                "Token and new password are required",
            ));
        }
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_token",
            "Invalid reset token",
        ))
    }
}

/// The `POST /signup` body. Email verification is off locally, so users are verified at once,
/// as the real service does when email is disabled.
fn signup_response(user: &User) -> Value {
    let created_at = format_timestamp(user.created_at);
    json!({
        "userId": user.id,
        "email": user.username,
        "role": AUTHENTICATED_ROLE,
        "createdAt": created_at,
        "emailVerified": true,
        "emailVerifiedAt": created_at,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use axum::body::Body;
    use http_body_util::BodyExt;
    use rl_model::CallerResolver;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        AuthSettings,
        tests::{APP_ID, service_with},
    };

    pub(crate) struct Answer {
        pub(crate) status: StatusCode,
        pub(crate) headers: HeaderMap,
        pub(crate) body: Value,
        pub(crate) raw: Bytes,
    }

    pub(crate) struct App {
        _dir: tempfile::TempDir,
        pub(crate) auth: Arc<AuthService>,
        router: Router,
    }

    async fn app() -> App {
        app_with(AuthSettings::new(APP_ID)).await
    }

    async fn app_with(settings: AuthSettings) -> App {
        let (dir, auth) = service_with(settings).await;
        app_from(dir, auth)
    }

    pub(crate) fn app_from(dir: tempfile::TempDir, auth: AuthService) -> App {
        let auth = Arc::new(auth);
        App {
            _dir: dir,
            router: Arc::clone(&auth).router(),
            auth,
        }
    }

    impl App {
        pub(crate) async fn send(&self, request: axum::http::Request<Body>) -> Answer {
            let response = self.router.clone().oneshot(request).await.unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            let raw = response.into_body().collect().await.unwrap().to_bytes();
            let body = serde_json::from_slice(&raw).unwrap_or(Value::Null);
            Answer {
                status,
                headers,
                body,
                raw,
            }
        }

        pub(crate) async fn post(&self, path: &str, body: Value) -> Answer {
            self.post_with(path, body, None).await
        }

        async fn post_with(&self, path: &str, body: Value, bearer: Option<&str>) -> Answer {
            let mut request =
                axum::http::Request::post(path).header(header::CONTENT_TYPE, "application/json");
            if let Some(bearer) = bearer {
                request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
            }
            self.send(request.body(Body::from(body.to_string())).unwrap())
                .await
        }

        pub(crate) async fn get(&self, path: &str) -> Answer {
            self.send(axum::http::Request::get(path).body(Body::empty()).unwrap())
                .await
        }

        async fn sign_out(&self, access_token: &str, token_field: &str) -> Answer {
            let request = axum::http::Request::post("/api/auth/v1/signout")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
                .body(Body::from(format!("token={token_field}")))
                .unwrap();
            self.send(request).await
        }

        async fn sign_up(&self, email: &str, password: &str) -> Answer {
            self.post(
                "/api/auth/v1/signup",
                json!({ "email": email, "password": password }),
            )
            .await
        }

        async fn sign_in(&self, email: &str, password: &str) -> Answer {
            self.send(
                axum::http::Request::post("/api/auth/v1/token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer expired.or.garbage")
                    .body(Body::from(
                        json!({ "grantType": "password", "email": email, "password": password })
                            .to_string(),
                    ))
                    .unwrap(),
            )
            .await
        }

        async fn refresh(&self, refresh_token: &str) -> Answer {
            self.post(
                "/api/auth/v1/token",
                json!({ "grantType": "refresh_token", "refreshToken": refresh_token }),
            )
            .await
        }
    }

    fn error(code: &str, message: &str) -> Value {
        json!({ "error": code, "message": message })
    }

    fn text<'a>(answer: &'a Answer, field: &str) -> &'a str {
        answer.body[field].as_str().unwrap()
    }

    #[tokio::test]
    async fn sign_up_then_sign_in_gives_a_session_the_resolver_accepts() {
        let app = app().await;
        let signed_up = app.sign_up("Alice@Example.test", "correct horse").await;
        assert_eq!(signed_up.status, StatusCode::OK);
        let user_id = text(&signed_up, "userId");
        assert!(uuid::Uuid::parse_str(user_id).is_ok());
        assert_eq!(signed_up.body["email"], "Alice@Example.test");
        assert_eq!(signed_up.body["role"], "Authenticated");
        assert_eq!(signed_up.body["emailVerified"], true);
        assert!(text(&signed_up, "createdAt").ends_with("+00:00"));

        let signed_in = app.sign_in("alice@example.TEST", "correct horse").await;
        assert_eq!(signed_in.status, StatusCode::OK);
        assert_eq!(signed_in.body["tokenType"], "Bearer");
        assert_eq!(signed_in.body["expiresIn"], 3600);
        assert_eq!(signed_in.body["scope"], Value::Null);
        let keys: Vec<_> = signed_in
            .body
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
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
        let access_token = text(&signed_in, "accessToken");
        let caller = app.auth.resolve(Some(access_token)).unwrap();
        let session = caller.session.unwrap();
        assert_eq!(session.user_id, user_id);
        assert_eq!(session.email.as_deref(), Some("Alice@Example.test"));
        assert_eq!(session.claims["aud"], APP_ID);

        let refreshed = app.refresh(text(&signed_in, "refreshToken")).await;
        assert_eq!(refreshed.status, StatusCode::OK);
        assert_ne!(
            refreshed.body["refreshToken"],
            signed_in.body["refreshToken"]
        );
        assert!(
            app.auth
                .resolve(Some(text(&refreshed, "accessToken")))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn wrong_credentials_are_an_invalid_grant() {
        let app = app().await;
        app.sign_up("alice", "correct horse").await;
        for (email, password) in [("alice", "wrong horse"), ("nobody", "correct horse")] {
            let answer = app.sign_in(email, password).await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
            assert_eq!(
                answer.body,
                error("invalid_grant", "Invalid email or password")
            );
        }
    }

    #[tokio::test]
    async fn auto_create_signs_in_new_emails_but_not_wrong_passwords() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.auto_create_users = true;
        let app = app_with(settings).await;

        let first = app.sign_in("dev@contoso.com", "LocalDev!Pass123").await;
        assert_eq!(first.status, StatusCode::OK);
        assert!(app.auth.resolve(Some(text(&first, "accessToken"))).is_ok());
        let again = app.sign_in("DEV@contoso.com", "LocalDev!Pass123").await;
        assert_eq!(again.status, StatusCode::OK);

        let wrong = app.sign_in("dev@contoso.com", "not the password").await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            wrong.body,
            error("invalid_grant", "Invalid email or password")
        );
        assert_eq!(app.auth.list_users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_create_respects_sign_up_being_off() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.auto_create_users = true;
        settings.signup_enabled = false;
        let app = app_with(settings).await;
        let answer = app.sign_in("dev@contoso.com", "LocalDev!Pass123").await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert!(app.auth.list_users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn auto_create_works_when_password_auth_is_off() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.auto_create_users = true;
        settings.password_enabled = false;
        let app = app_with(settings).await;

        let first = app.sign_in("dev@contoso.com", "LocalDev!Pass123").await;
        assert_eq!(first.status, StatusCode::OK, "{}", first.body);
        assert!(app.auth.resolve(Some(text(&first, "accessToken"))).is_ok());
        let wrong = app.sign_in("dev@contoso.com", "not the password").await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            wrong.body,
            error("invalid_grant", "Invalid email or password")
        );
        assert_eq!(app.auth.list_users().await.unwrap().len(), 1);
        // Sign-up itself stays closed.
        let signed_up = app.sign_up("other@contoso.com", "secret1").await;
        assert_eq!(signed_up.body["error"], "auth_mode_disabled");
    }

    #[tokio::test]
    async fn signing_up_twice_conflicts() {
        let app = app().await;
        assert_eq!(app.sign_up("alice", "secret1").await.status, StatusCode::OK);
        let again = app.sign_up("ALICE", "secret2").await;
        assert_eq!(again.status, StatusCode::CONFLICT);
        assert_eq!(
            again.body,
            error("email_already_registered", "Email is already registered")
        );
    }

    #[tokio::test]
    async fn malformed_requests_are_invalid_requests() {
        let app = app().await;
        let cases = [
            ("/api/auth/v1/signup", json!({ "email": "alice" })),
            (
                "/api/auth/v1/signup",
                json!({ "email": " ", "password": "secret1" }),
            ),
            (
                "/api/auth/v1/signup",
                json!({ "email": 7, "password": "secret1" }),
            ),
            ("/api/auth/v1/signup", json!(["alice"])),
            (
                "/api/auth/v1/token",
                json!({ "email": "alice", "password": "x" }),
            ),
            (
                "/api/auth/v1/token",
                json!({ "grantType": "password", "email": "alice" }),
            ),
            (
                "/api/auth/v1/token",
                json!({ "grantType": "refresh_token" }),
            ),
        ];
        for (path, body) in cases {
            let answer = app.post(path, body.clone()).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{path} {body}");
            assert_eq!(answer.body["error"], "invalid_request", "{path} {body}");
            assert!(answer.body["message"].is_string());
        }
        let empty = app
            .send(
                axum::http::Request::post("/api/auth/v1/token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(empty.status, StatusCode::BAD_REQUEST);

        let unsupported = app
            .post(
                "/api/auth/v1/token",
                json!({ "grantType": "client_credentials" }),
            )
            .await;
        assert_eq!(unsupported.status, StatusCode::BAD_REQUEST);
        assert_eq!(unsupported.body["error"], "unsupported_grant_type");
    }

    #[tokio::test]
    async fn field_names_match_case_insensitively() {
        let app = app().await;
        app.sign_up("alice", "secret1").await;
        let answer = app
            .post(
                "/api/auth/v1/token",
                json!({ "GrantType": "PASSWORD", "Email": "alice", "PASSWORD": "secret1" }),
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn refresh_tokens_rotate_and_bad_ones_end_the_session() {
        let app = app().await;
        app.sign_up("alice", "secret1").await;
        let signed_in = app.sign_in("alice", "secret1").await;
        let first = text(&signed_in, "refreshToken").to_owned();
        app.auth.advance_clock(crate::sessions::REUSE_WINDOW_MS + 1);
        let second = app.refresh(&first).await;
        assert_eq!(second.status, StatusCode::OK);
        assert_ne!(text(&second, "refreshToken"), first);

        let unknown = app.refresh("not-a-refresh-token").await;
        assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            unknown.body,
            error("invalid_grant", "The refresh token is invalid or malformed")
        );
    }

    #[tokio::test]
    async fn sign_out_ends_only_that_session() {
        let app = app().await;
        app.sign_up("alice", "secret1").await;
        let first = app.sign_in("alice", "secret1").await;
        let second = app.sign_in("alice", "secret1").await;
        let first_access = text(&first, "accessToken");

        let answer = app.sign_out(first_access, first_access).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert!(answer.raw.is_empty());
        assert!(app.auth.resolve(Some(first_access)).is_err());
        let refresh = app.refresh(text(&first, "refreshToken")).await;
        assert_eq!(refresh.status, StatusCode::UNAUTHORIZED);
        assert_eq!(refresh.body["error"], "invalid_grant");
        assert!(app.auth.resolve(Some(text(&second, "accessToken"))).is_ok());

        let again = app.sign_out(first_access, first_access).await;
        assert_eq!(again.status, StatusCode::UNAUTHORIZED);
        assert_eq!(again.body, json!({ "error": "Session is no longer valid" }));
        let garbage = app.sign_out("garbage", "garbage").await;
        assert_eq!(garbage.status, StatusCode::UNAUTHORIZED);
        assert_eq!(
            garbage.body,
            json!({ "error": "Invalid authentication token" })
        );
    }

    #[tokio::test]
    async fn sign_out_all_ends_every_session_of_the_user() {
        let app = app().await;
        app.sign_up("alice", "secret1").await;
        app.sign_up("bob", "secret1").await;
        let first = app.sign_in("alice", "secret1").await;
        let second = app.sign_in("alice", "secret1").await;
        let bob = app.sign_in("bob", "secret1").await;

        let answer = app
            .post_with(
                "/api/auth/v1/signout-all",
                json!({}),
                Some(text(&first, "accessToken")),
            )
            .await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.body, json!({ "count": 2 }));
        assert!(
            app.auth
                .resolve(Some(text(&second, "accessToken")))
                .is_err()
        );
        assert!(app.auth.resolve(Some(text(&bob, "accessToken"))).is_ok());

        let anonymous = app.post("/api/auth/v1/signout-all", json!({})).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
        assert_eq!(anonymous.body, error("invalid_client", "invalid_client"));
    }

    #[tokio::test]
    async fn the_jwks_verifies_issued_tokens() {
        let app = app().await;
        app.sign_up("alice", "secret1").await;
        let token = text(&app.sign_in("alice", "secret1").await, "accessToken").to_owned();

        let answer = app.get(JWKS_PATH).await;
        assert_eq!(answer.status, StatusCode::OK);
        assert_eq!(answer.headers[header::CACHE_CONTROL], JWKS_CACHE_CONTROL);
        let keys = answer.body["keys"].as_array().unwrap();
        assert_eq!(keys.len(), 1);
        let jwk = &keys[0];
        assert_eq!(jwk["kty"], "EC");
        assert_eq!(jwk["crv"], "P-256");
        assert_eq!(jwk["alg"], "ES256");
        assert_eq!(jwk["use"], "sig");

        let (signing_input, signature) = token.rsplit_once('.').unwrap();
        let header: Value = serde_json::from_slice(
            &crate::util::b64url_decode(signing_input.split('.').next().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(header["kid"], jwk["kid"]);
        let mut point = vec![0x04];
        point.extend(crate::util::b64url_decode(jwk["x"].as_str().unwrap()).unwrap());
        point.extend(crate::util::b64url_decode(jwk["y"].as_str().unwrap()).unwrap());
        let public_key = ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            point,
        );
        public_key
            .verify(
                signing_input.as_bytes(),
                &crate::util::b64url_decode(signature).unwrap(),
            )
            .expect("the JWKS key verifies the token");
    }

    #[tokio::test]
    async fn runtime_settings_reflect_rayfin_yml() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.fabric_enabled = true;
        settings.password_enabled = false;
        settings.magic_link_enabled = true;
        settings.access_token_minutes = 30;
        settings.allowed_redirect_uris = vec!["http://localhost:5175".into()];
        settings.custom_claims = vec![("app_version".into(), "1.0.0".into())];
        let app = app_with(settings).await;
        let answer = app.get(RUNTIME_SETTINGS_PATH).await;
        assert_eq!(answer.status, StatusCode::OK);
        let auth = &answer.body["serviceSettings"]["auth"];
        assert_eq!(auth["enabled"], true);
        assert_eq!(auth["password"], json!({ "enabled": false }));
        assert_eq!(
            auth["passwordless"],
            json!({ "magicLink": { "enabled": true } })
        );
        assert_eq!(
            auth["fabric"],
            json!({ "enabled": true, "externalEntraExchange": false })
        );
        assert_eq!(auth["expiryInMinutes"], 30);
        assert_eq!(auth["refreshToken"]["lifetimeInDays"], 90);
        assert_eq!(
            auth["allowedRedirectUris"],
            json!(["http://localhost:5175"])
        );
        assert_eq!(auth["customClaims"], json!({ "app_version": "1.0.0" }));
    }

    #[tokio::test]
    async fn features_rayfin_local_lacks_answer_as_disabled() {
        let app = app().await;
        let disabled = |description: &str| json!({ "error": "auth_mode_disabled", "error_description": description });
        let magic = app
            .post(
                "/api/auth/v1/passwordless/send",
                json!({ "email": "a@b.test" }),
            )
            .await;
        assert_eq!(magic.status, StatusCode::BAD_REQUEST);
        assert_eq!(magic.body, disabled(PASSWORDLESS_DISABLED));
        let verify = app.get("/api/auth/v1/verify-email?token=abc").await;
        assert_eq!(verify.status, StatusCode::BAD_REQUEST);
        assert_eq!(verify.body, disabled(EMAIL_DISABLED));
        let resend = app
            .post(
                "/api/auth/v1/resend-verification-email",
                json!({ "email": "a@b.test" }),
            )
            .await;
        assert_eq!(resend.body, disabled(EMAIL_DISABLED));

        let reset = app
            .post(
                "/api/auth/v1/reset-password",
                json!({ "email": "a@b.test" }),
            )
            .await;
        assert_eq!(reset.status, StatusCode::OK);
        assert_eq!(reset.body["success"], true);
        let complete = app
            .post(
                "/api/auth/v1/reset-password/complete",
                json!({ "token": "abc", "newPassword": "secret2" }),
            )
            .await;
        assert_eq!(complete.status, StatusCode::BAD_REQUEST);
        assert_eq!(complete.body, error("invalid_token", "Invalid reset token"));

        let brokered = app.post("/api/auth/v1/brokered/token", json!({})).await;
        assert_eq!(brokered.status, StatusCode::BAD_REQUEST);
        let external = app.get("/api/auth/v1/brokered/authorize/external").await;
        assert_eq!(external.status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn disabling_password_auth_closes_sign_up_but_not_sign_in() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.password_enabled = false;
        let app = app_with(settings).await;
        let signed_up = app.sign_up("alice", "secret1").await;
        assert_eq!(signed_up.status, StatusCode::BAD_REQUEST);
        assert_eq!(signed_up.body["error"], "auth_mode_disabled");
        assert_eq!(signed_up.body["error_description"], PASSWORD_DISABLED);
        let reset = app
            .post("/api/auth/v1/reset-password", json!({ "email": "alice" }))
            .await;
        assert_eq!(reset.body["error"], "auth_mode_disabled");

        app.auth.create_user("alice", "secret1").await.unwrap();
        assert_eq!(app.sign_in("alice", "secret1").await.status, StatusCode::OK);
    }

    #[tokio::test]
    async fn closed_sign_up_is_forbidden() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.signup_enabled = false;
        let app = app_with(settings).await;
        let answer = app.sign_up("alice", "secret1").await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN);
        assert_eq!(answer.body["error"], "signup_disabled");
    }

    #[tokio::test]
    async fn the_external_entra_exchange_is_not_available_without_entra_sign_in() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.fabric_enabled = true;
        settings.external_entra_exchange = true;
        let app = app_with(settings).await;
        let answer = app.post("/api/auth/v1/brokered/token", json!({})).await;
        assert_eq!(answer.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn authorization_codes_are_validated_then_unknown() {
        let app = app().await;
        let verifier = "v".repeat(43);
        let cases = [
            (json!({}), "MISSING_CODE"),
            (
                json!({ "code": "c", "verificationCode": "v" }),
                "AMBIGUOUS_CODE_FIELDS",
            ),
            (json!({ "code": "c" }), "unsupported_grant_type"),
            (json!({ "verificationCode": "v" }), "invalid_request"),
            (
                json!({ "verificationCode": "v", "codeVerifier": "short" }),
                "invalid_request",
            ),
            (
                json!({ "verificationCode": "v", "codeVerifier": verifier }),
                "MISSING_REDIRECT_URI",
            ),
        ];
        for (mut body, code) in cases {
            body["grantType"] = json!("authorization_code");
            let answer = app.post("/api/auth/v1/token", body.clone()).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{body}");
            assert_eq!(answer.body["error"], code, "{body}");
        }
        let unknown = app
            .post(
                "/api/auth/v1/token",
                json!({
                    "grantType": "authorization_code",
                    "verificationCode": "never-issued",
                    "codeVerifier": verifier,
                    "redirectUri": "http://localhost:5175",
                    "codeType": "fabric_handoff",
                }),
            )
            .await;
        assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
        assert_eq!(unknown.body["error"], "INVALID_CODE");
    }

    #[tokio::test]
    async fn new_settings_apply_at_once_and_keep_the_sessions() {
        let app = app().await;
        app.sign_up("alice", "secret1").await;
        let signed_in = app.sign_in("alice", "secret1").await;
        let access_token = text(&signed_in, "accessToken").to_owned();

        let mut settings = AuthSettings::new(APP_ID);
        settings.password_enabled = false;
        settings.access_token_minutes = 120;
        settings.scopes = vec!["notes.read".into()];
        app.auth.set_settings(settings).unwrap();

        let signed_up = app.sign_up("bob", "secret1").await;
        assert_eq!(signed_up.body["error"], "auth_mode_disabled");
        let runtime = app.get(RUNTIME_SETTINGS_PATH).await;
        assert_eq!(
            runtime.body["serviceSettings"]["auth"]["password"],
            json!({ "enabled": false })
        );
        assert!(app.auth.resolve(Some(&access_token)).is_ok());
        let refreshed = app.refresh(text(&signed_in, "refreshToken")).await;
        assert_eq!(refreshed.status, StatusCode::OK);
        assert_eq!(refreshed.body["expiresIn"], 7200);
        assert_eq!(refreshed.body["scope"], "notes.read");
        assert!(
            app.auth
                .resolve(Some(text(&refreshed, "accessToken")))
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_disabled_auth_service_hides_its_routes() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.enabled = false;
        let app = app_with(settings).await;
        assert_eq!(
            app.sign_up("alice", "secret1").await.status,
            StatusCode::NOT_FOUND
        );
        assert_eq!(app.get(JWKS_PATH).await.status, StatusCode::NOT_FOUND);
        assert_eq!(
            app.post("/api/auth/v1/passwordless/send", json!({}))
                .await
                .status,
            StatusCode::NOT_FOUND
        );
        let settings = app.get(RUNTIME_SETTINGS_PATH).await;
        assert_eq!(settings.status, StatusCode::OK);
        assert_eq!(settings.body["serviceSettings"]["auth"]["enabled"], false);
    }
}
