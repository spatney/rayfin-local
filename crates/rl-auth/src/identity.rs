//! Automatic sign-in while developing, and the identity it signs in as.
//!
//! Apps from Rayfin's templates sign in with `rayfinLocalDev({ autoLogin: true })` from
//! `@microsoft/rayfin-local-dev`: their page asks the Vite dev server for
//! `GET /.rayfin/dev/session-token`, then installs the answer with `signInWithBrokeredToken()`.
//! rayfin-local's own dev server answers that route by asking [`DEV_SESSION_TOKEN_PATH`] here for
//! a session of the [`SignInAs`] identity, with the run's secret in [`DEV_SIGN_IN_SECRET_HEADER`].
//! With the same secret, it asks [`DEV_SESSION_STAMP_PATH`] for the stamp of the sessions this
//! backend accepts, so that its pages forget a stored session of another app or epoch.
//!
//! The answer is the `POST /api/auth/v1/token` body: a session like any other. Refusals are
//! `{"error", "hint"}`, which `fetchRayfinLocalSessionToken()` shows as `<error> <hint>`.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode, header, request::Parts},
    response::{IntoResponse, Response},
};
use parking_lot::{Mutex, RwLock};
use serde_json::json;

use crate::{
    AuthError, AuthService, TokenResponse,
    users::User,
    util::{constant_time_eq, sha256},
};

/// Where rayfin-local's dev server gets automatic sign-ins.
pub const DEV_SESSION_TOKEN_PATH: &str = "/.rayfin-local/dev/session-token";
/// Where rayfin-local's dev server gets [`AuthService::session_stamp`], as `{"stamp"}`.
pub const DEV_SESSION_STAMP_PATH: &str = "/.rayfin-local/dev/session-stamp";
/// The header with the run's secret, which only rayfin-local's own dev server knows.
pub const DEV_SIGN_IN_SECRET_HEADER: &str = "x-rayfin-local-sign-in";
/// The local account the developer identity signs in as when their Entra identity isn't there.
pub const LOCAL_DEVELOPER: &str = "developer@example.com";
/// How long the developer identity stays [`LOCAL_DEVELOPER`] after their Azure sign-in had no token
/// without a prompt, before it asks again: without an Azure sign-in, asking takes a second or more.
const ENTRA_RETRY_AFTER: Duration = Duration::from_secs(5 * 60);

/// Who the app is signed in as automatically.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SignInAs {
    /// The developer: their Entra identity when Entra sign-in is on (the backend is on loopback)
    /// and a token for it comes without a prompt, else [`LOCAL_DEVELOPER`].
    #[default]
    Developer,
    /// A local user, by username or id; created without a password when missing.
    User(String),
    /// Nobody: automatic sign-in answers 401.
    None,
}

/// The state of automatic sign-in.
#[derive(Default)]
pub(crate) struct AutoSignIn {
    identity: RwLock<SignInAs>,
    /// Held while a session is issued for the identity, and taken alone to change it, so that no
    /// session of the previous identity outlives the change.
    switching: tokio::sync::RwLock<()>,
    /// The SHA-256 of the dev server's secret; without one the route stays closed.
    secret: Option<[u8; 32]>,
    /// The user the developer identity signed in as last.
    developer: RwLock<Option<String>>,
    /// Until when the developer identity doesn't ask for their Entra identity again.
    entra_unavailable_until: Mutex<Option<Instant>>,
    /// Whether the log says the developer signs in as [`LOCAL_DEVELOPER`]: once, until they sign
    /// in as their Entra user.
    local_developer_logged: AtomicBool,
}

impl AuthService {
    /// Opens [`DEV_SESSION_TOKEN_PATH`] to loopback requests with `secret` in
    /// [`DEV_SIGN_IN_SECRET_HEADER`].
    pub fn with_dev_sign_in_secret(mut self, secret: &str) -> Self {
        self.auto_sign_in.secret = Some(sha256(secret.as_bytes()));
        self
    }

    /// Who the app is signed in as automatically.
    pub fn sign_in_as(&self) -> SignInAs {
        self.auto_sign_in.identity.read().clone()
    }

    /// Sets who the app is signed in as automatically, leaving the sessions as they are.
    pub fn set_sign_in_as(&self, identity: SignInAs) {
        *self.auto_sign_in.identity.write() = identity;
    }

    /// Changes who the app is signed in as automatically and signs out every session, so an app
    /// that reloads signs in again as the new identity. Returns how many sessions ended.
    pub async fn switch_sign_in_as(&self, identity: SignInAs) -> Result<usize, AuthError> {
        let _switching = self.auto_sign_in.switching.write().await;
        self.set_sign_in_as(identity);
        *self.auto_sign_in.entra_unavailable_until.lock() = None;
        self.revoke_every_session().await
    }

    /// The user automatic sign-in signs in as now (created when missing); `None` for nobody.
    pub async fn sign_in_user(&self) -> Result<Option<User>, AuthError> {
        match self.sign_in_as() {
            SignInAs::Developer => self.developer_account().await.map(Some),
            SignInAs::User(user) => self.find_or_create_user(&user).await.map(Some),
            SignInAs::None => Ok(None),
        }
    }

    /// Holds off a switch of identity while a sign-in as the identity is issued, so it belongs to
    /// the identity a switch then signs out.
    pub(crate) async fn identity_unchanged(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.auto_sign_in.switching.read().await
    }

    /// The developer's account: their Entra user when Entra sign-in is on and a token for it
    /// comes without a prompt, else [`LOCAL_DEVELOPER`] (created when missing). After a failure it
    /// asks again only five minutes later, or once the identity is switched.
    pub async fn developer_account(&self) -> Result<User, AuthError> {
        let unavailable = || {
            self.auto_sign_in
                .entra_unavailable_until
                .lock()
                .is_some_and(|until| Instant::now() < until)
        };
        let entra = if self.settings().entra_signin.enabled && !unavailable() {
            self.developer_identity_silently()
                .await
                .inspect_err(|failure| {
                    tracing::debug!(
                        "the developer signs in as {LOCAL_DEVELOPER}: their Azure sign-in has no \
                         token without a prompt ({})",
                        failure.message
                    );
                    *self.auto_sign_in.entra_unavailable_until.lock() =
                        Some(Instant::now() + ENTRA_RETRY_AFTER);
                    let logged = &self.auto_sign_in.local_developer_logged;
                    if !logged.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            "Signing in as the local developer account; sign in with `az login` \
                             to preview as yourself"
                        );
                    }
                })
                .ok()
        } else {
            None
        };
        let user = match entra {
            Some(identity) => {
                self.auto_sign_in
                    .local_developer_logged
                    .store(false, Ordering::Relaxed);
                self.find_or_create_external_user(
                    &identity.object_id,
                    identity.email.as_deref(),
                    identity.name.as_deref(),
                )
                .await?
            }
            None => self.find_or_create_user(LOCAL_DEVELOPER).await?,
        };
        *self.auto_sign_in.developer.write() = Some(user.id.clone());
        Ok(user)
    }

    /// The user the identity signs in as, without signing anyone in: for the developer, the
    /// account it signed in as last in this run (`None` before that); for a user, that user
    /// while it exists.
    pub async fn identity_user(&self) -> Result<Option<User>, AuthError> {
        match self.sign_in_as() {
            SignInAs::Developer => {
                let last = self.auto_sign_in.developer.read().clone();
                match last {
                    Some(id) => self.find_user(&id).await,
                    None => Ok(None),
                }
            }
            SignInAs::User(user) => self.find_user(&user).await,
            SignInAs::None => Ok(None),
        }
    }

    pub(crate) fn forget_developer_account(&self) {
        *self.auto_sign_in.developer.write() = None;
    }

    /// A session for the identity, for the dev server.
    async fn automatic_sign_in(&self, request: &Parts) -> Result<TokenResponse, Refusal> {
        self.check_dev_server(request)?;
        if !self.settings().enabled {
            return Err(Refusal::new(
                StatusCode::NOT_FOUND,
                "Sign-in is turned off for this app.",
                Some("services.auth.enabled is false in rayfin/rayfin.yml."),
            ));
        }
        let _switching = self.auto_sign_in.switching.read().await;
        let user = match self.sign_in_user().await {
            Ok(Some(user)) => user,
            Ok(None) => {
                return Err(Refusal::new(
                    StatusCode::UNAUTHORIZED,
                    "Automatic sign-in is off.",
                    Some("Use the app's sign-in to sign in."),
                ));
            }
            Err(error) => return Err(Refusal::failed(&error)),
        };
        let tokens = self
            .issue_session(&user)
            .await
            .map_err(|error| Refusal::failed(&error))?;
        tracing::debug!(
            developer = user.is_developer(),
            "signed the app in automatically"
        );
        Ok(tokens)
    }

    /// Only rayfin-local's dev server: a loopback peer, no other origin, and the run's secret.
    fn check_dev_server(&self, request: &Parts) -> Result<(), Refusal> {
        let Some(secret) = &self.auto_sign_in.secret else {
            return Err(Refusal::new(
                StatusCode::NOT_FOUND,
                "Automatic sign-in is not available here.",
                Some("It answers the Vite dev server that `rayfin-local serve` starts."),
            ));
        };
        if !from_loopback(request) {
            return Err(Refusal::new(
                StatusCode::FORBIDDEN,
                "Automatic sign-in only answers this machine.",
                None,
            ));
        }
        if has_foreign_origin(request) {
            return Err(Refusal::new(
                StatusCode::FORBIDDEN,
                "Cross-origin automatic sign-in requests are not allowed.",
                None,
            ));
        }
        let presented = request
            .headers
            .get(DEV_SIGN_IN_SECRET_HEADER)
            .map(|value| sha256(value.as_bytes()));
        if !presented.is_some_and(|presented| constant_time_eq(&presented, secret)) {
            return Err(Refusal::new(
                StatusCode::FORBIDDEN,
                "Automatic sign-in only answers rayfin-local's own dev server.",
                None,
            ));
        }
        Ok(())
    }
}

/// `GET` [`DEV_SESSION_TOKEN_PATH`].
pub(crate) async fn dev_session_token(
    State(auth): State<Arc<AuthService>>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let response = match auth.automatic_sign_in(&parts).await {
        Ok(tokens) => Json(tokens).into_response(),
        Err(refusal) => refusal.into_response(),
    };
    no_store(response)
}

/// `GET` [`DEV_SESSION_STAMP_PATH`]: `{"stamp"}`, also while the app's sign-in is off, since a
/// page may still hold another app's session.
pub(crate) async fn dev_session_stamp(
    State(auth): State<Arc<AuthService>>,
    request: Request,
) -> Response {
    let (parts, _) = request.into_parts();
    let stamp = match auth.check_dev_server(&parts) {
        Ok(()) => auth.session_stamp().await.map_err(|error| {
            tracing::error!("could not read the session stamp: {error}");
            Refusal::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not read the session stamp.",
                None,
            )
        }),
        Err(refusal) => Err(refusal),
    };
    no_store(match stamp {
        Ok(stamp) => Json(json!({ "stamp": stamp })).into_response(),
        Err(refusal) => refusal.into_response(),
    })
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Why automatic sign-in answered no.
struct Refusal {
    status: StatusCode,
    error: String,
    hint: Option<String>,
}

impl Refusal {
    fn new(status: StatusCode, error: &str, hint: Option<&str>) -> Self {
        Self {
            status,
            error: error.to_owned(),
            hint: hint.map(str::to_owned),
        }
    }

    fn failed(error: &AuthError) -> Self {
        tracing::error!("automatic sign-in failed: {error}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: "Automatic sign-in failed.".to_owned(),
            hint: Some(format!("rayfin-local could not sign the app in: {error}.")),
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let body = json!({ "error": self.error, "hint": self.hint });
        (self.status, Json(body)).into_response()
    }
}

/// Whether the request comes from this machine.
fn from_loopback(request: &Parts) -> bool {
    request
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| is_loopback_ip(peer.ip()))
}

/// `127.0.0.0/8`, `::1` and IPv4-mapped loopback addresses.
pub(crate) fn is_loopback_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// An `Origin` other than the backend's own: a page elsewhere, which never needs this route.
fn has_foreign_origin(request: &Parts) -> bool {
    let Some(origin) = request.headers.get(header::ORIGIN) else {
        return false;
    };
    let host = request
        .headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let own = origin.to_str().is_ok_and(|origin| {
        host.is_some_and(|host| {
            ["http", "https"]
                .iter()
                .any(|scheme| origin.eq_ignore_ascii_case(&format!("{scheme}://{host}")))
        })
    });
    !own
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use axum::body::Body;
    use rl_model::CallerResolver;
    use rl_tokens::{AccessToken, Audience, BoxFuture, TokenError, TokenSource};
    use serde_json::Value;

    use super::*;
    use crate::{
        AuthSettings,
        entra::tests::{entra_settings, user_claims},
        routes::tests::{Answer, App, app_from},
        tests::{APP_ID, service_with},
        util::b64url,
    };

    const SECRET: &str = "the-run's-dev-server-secret";
    const LOOPBACK: Option<[u8; 4]> = Some([127, 0, 0, 1]);

    async fn app(settings: AuthSettings) -> App {
        let (dir, auth) = service_with(settings).await;
        app_from(dir, auth.with_dev_sign_in_secret(SECRET))
    }

    /// A request for a session from `peer` (an IPv4 address).
    fn request(
        secret: Option<&str>,
        peer: Option<[u8; 4]>,
        origin: Option<&str>,
    ) -> axum::http::Request<Body> {
        request_to(DEV_SESSION_TOKEN_PATH, secret, peer, origin)
    }

    /// A `GET path` from `peer` (an IPv4 address).
    fn request_to(
        path: &str,
        secret: Option<&str>,
        peer: Option<[u8; 4]>,
        origin: Option<&str>,
    ) -> axum::http::Request<Body> {
        let mut request = axum::http::Request::get(path).header(header::HOST, "localhost:7171");
        if let Some(secret) = secret {
            request = request.header(DEV_SIGN_IN_SECRET_HEADER, secret);
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(ip) = peer {
            request = request.extension(ConnectInfo(SocketAddr::from((ip, 50123))));
        }
        request.body(Body::empty()).unwrap()
    }

    /// The dev server's request.
    async fn sign_in(app: &App) -> Answer {
        app.send(request(Some(SECRET), LOOPBACK, None)).await
    }

    /// The user an answer's access token belongs to.
    async fn user_of(app: &App, answer: &Answer) -> User {
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        let token = answer.body["accessToken"].as_str().unwrap();
        let user_id = app
            .auth
            .resolve(Some(token))
            .unwrap()
            .session
            .unwrap()
            .user_id;
        app.auth.find_user(&user_id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn the_developer_gets_a_session_the_sdk_installs() {
        let app = app(AuthSettings::new(APP_ID)).await;
        assert_eq!(app.auth.identity_user().await.unwrap(), None);
        let first = sign_in(&app).await;
        assert_eq!(first.headers[header::CACHE_CONTROL], "no-store");
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
        assert_eq!(first.body["tokenType"], "Bearer");
        assert!(first.body["refreshToken"].is_string());
        let developer = user_of(&app, &first).await;
        assert_eq!(developer.username, LOCAL_DEVELOPER);
        assert!(developer.is_developer() && !developer.has_password);
        assert_eq!(
            app.auth.identity_user().await.unwrap(),
            Some(developer.clone())
        );

        let again = sign_in(&app).await;
        assert_eq!(user_of(&app, &again).await.id, developer.id);
        assert_eq!(app.auth.list_users().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn only_the_dev_server_on_this_machine_gets_sessions() {
        let app = app(AuthSettings::new(APP_ID)).await;
        let refused = [
            (
                "remote",
                request(Some(SECRET), Some([192, 168, 1, 20]), None),
            ),
            ("wrong secret", request(Some("guess"), LOOPBACK, None)),
            ("no secret", request(None, LOOPBACK, None)),
            ("no peer", request(Some(SECRET), None, None)),
            (
                "from a page",
                request(Some(SECRET), LOOPBACK, Some("http://localhost:5173")),
            ),
        ];
        for (case, refused) in refused {
            let answer = app.send(refused).await;
            assert_eq!(
                answer.status,
                StatusCode::FORBIDDEN,
                "{case}: {}",
                answer.body
            );
            assert!(answer.body["error"].is_string(), "{case}");
            assert!(answer.body.get("accessToken").is_none(), "{case}");
        }
        assert!(app.auth.list_users().await.unwrap().is_empty());
        let own_origin = request(Some(SECRET), LOOPBACK, Some("http://localhost:7171"));
        assert_eq!(app.send(own_origin).await.status, StatusCode::OK);

        let (dir, closed) = service_with(AuthSettings::new(APP_ID)).await;
        let closed = app_from(dir, closed);
        assert_eq!(sign_in(&closed).await.status, StatusCode::NOT_FOUND);
        let mut settings = AuthSettings::new(APP_ID);
        settings.enabled = false;
        let off = sign_in(&app_with_identity(settings, SignInAs::Developer).await).await;
        assert_eq!(off.status, StatusCode::NOT_FOUND);
        assert_eq!(off.body["error"], "Sign-in is turned off for this app.");
    }

    async fn app_with_identity(settings: AuthSettings, identity: SignInAs) -> App {
        let app = app(settings).await;
        app.auth.set_sign_in_as(identity);
        app
    }

    #[tokio::test]
    async fn a_named_user_is_created_and_nobody_is_refused() {
        let app = app_with_identity(
            AuthSettings::new(APP_ID),
            SignInAs::User("Ana@Example.com".into()),
        )
        .await;
        assert_eq!(app.auth.identity_user().await.unwrap(), None);
        let ana = user_of(&app, &sign_in(&app).await).await;
        assert_eq!(ana.username, "Ana@Example.com");
        assert!(!ana.has_password && !ana.is_developer());
        assert_eq!(app.auth.identity_user().await.unwrap(), Some(ana));

        app.auth.set_sign_in_as(SignInAs::None);
        let answer = sign_in(&app).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED);
        assert_eq!(answer.body["error"], "Automatic sign-in is off.");
        assert_eq!(answer.body["hint"], "Use the app's sign-in to sign in.");
        assert_eq!(app.auth.identity_user().await.unwrap(), None);
    }

    #[tokio::test]
    async fn only_the_dev_server_gets_the_session_stamp_even_with_sign_in_off() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.enabled = false;
        let app = app(settings).await;
        let stamp = |secret, origin| request_to(DEV_SESSION_STAMP_PATH, secret, LOOPBACK, origin);
        let answer = app.send(stamp(Some(SECRET), None)).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        assert_eq!(answer.headers[header::CACHE_CONTROL], "no-store");
        let expected = app.auth.session_stamp().await.unwrap();
        assert_eq!(answer.body, json!({ "stamp": expected }));
        let page = Some("http://localhost:5173");
        for refused in [
            stamp(None, None),
            stamp(Some("guess"), None),
            stamp(Some(SECRET), page),
        ] {
            assert_eq!(app.send(refused).await.status, StatusCode::FORBIDDEN);
        }
    }

    #[tokio::test]
    async fn switching_the_identity_signs_everyone_out() {
        let app = app(AuthSettings::new(APP_ID)).await;
        let developer_session = sign_in(&app).await;
        let bob = app.auth.create_user("bob", "secret1").await.unwrap();
        let bob_session = app.auth.issue_session(&bob).await.unwrap();
        let host_token = app.auth.issue_access_token(&bob).await.unwrap();
        assert_eq!(host_token.refresh_token, None);

        let ended = app
            .auth
            .switch_sign_in_as(SignInAs::User("bob".into()))
            .await
            .unwrap();
        assert_eq!(ended, 3);
        for token in [
            developer_session.body["accessToken"].as_str().unwrap(),
            &bob_session.access_token,
            &host_token.access_token,
        ] {
            assert!(app.auth.resolve(Some(token)).is_err());
        }
        assert_eq!(user_of(&app, &sign_in(&app).await).await.id, bob.id);
        assert_eq!(app.auth.sign_in_as(), SignInAs::User("bob".into()));
    }

    /// The developer's Power BI token, only without a prompt.
    struct SilentTokens {
        result: Result<Value, TokenError>,
        asked: AtomicUsize,
    }

    impl SilentTokens {
        fn new(result: Result<Value, TokenError>) -> Arc<Self> {
            Arc::new(Self {
                result,
                asked: AtomicUsize::new(0),
            })
        }
    }

    impl TokenSource for SilentTokens {
        fn token<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
            panic!("automatic sign-in asked for a token that may prompt");
        }

        fn token_silently<'a>(
            &'a self,
            scope: &'a str,
        ) -> BoxFuture<'a, Result<AccessToken, TokenError>> {
            assert_eq!(scope, Audience::PowerBi.default_scope());
            self.asked.fetch_add(1, Ordering::SeqCst);
            let result = self.result.clone().map(|claims| AccessToken {
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

    async fn entra_app(tokens: Arc<SilentTokens>) -> App {
        let (dir, auth) = service_with(entra_settings()).await;
        app_from(dir, auth.with_entra(tokens).with_dev_sign_in_secret(SECRET))
    }

    #[tokio::test]
    async fn the_developer_is_their_entra_user_when_a_token_comes_without_a_prompt() {
        let app = entra_app(SilentTokens::new(Ok(user_claims()))).await;
        let developer = user_of(&app, &sign_in(&app).await).await;
        assert_eq!(developer.email.as_deref(), Some("ada@contoso.com"));
        assert!(developer.entra_oid.is_some() && developer.is_developer());

        let failure = TokenError {
            scope: Audience::PowerBi.default_scope().into(),
            message: "the Azure CLI is not signed in".into(),
            hint: None,
        };
        let tokens = SilentTokens::new(Err(failure));
        let app = entra_app(tokens.clone()).await;
        let developer = user_of(&app, &sign_in(&app).await).await;
        assert_eq!(developer.username, LOCAL_DEVELOPER);
        assert_eq!(user_of(&app, &sign_in(&app).await).await.id, developer.id);
        assert_eq!(
            tokens.asked.load(Ordering::SeqCst),
            1,
            "asked again at once"
        );
        app.auth
            .switch_sign_in_as(SignInAs::Developer)
            .await
            .unwrap();
        assert_eq!(user_of(&app, &sign_in(&app).await).await.id, developer.id);
        assert_eq!(
            tokens.asked.load(Ordering::SeqCst),
            2,
            "not asked after a switch"
        );
    }

    #[test]
    fn loopback_peers() {
        for ip in ["127.0.0.1", "127.4.5.6", "::1", "::ffff:127.0.0.1"] {
            assert!(is_loopback_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["192.168.1.20", "0.0.0.0", "::", "::ffff:10.0.0.1"] {
            assert!(!is_loopback_ip(ip.parse().unwrap()), "{ip}");
        }
    }
}
