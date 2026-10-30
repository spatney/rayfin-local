//! The sign-in portal: rayfin-local's stand-in for the Fabric portal page that
//! `ensureSignedInWithFabric` opens in a popup, and the JSON endpoints that page calls.
//!
//! [`PORTAL_PATH`] is the app's `fabricPortalUrl` (`VITE_FABRIC_PORTAL_URL`). The SDK opens
//! `<fabricPortalUrl>/secureItemEmbed?workspaceId=…&itemType=AppBackend&itemId=…&extensionPath=/brokeredauth`
//! (`buildBrokerUrl` in `@microsoft/rayfin-auth-provider-fabric` 1.36.2 `brokerUrl.js`) and runs
//! this handshake with the page (`initiateFabricLogin.js`):
//!
//! 1. page → `window.opener`, target `*` (it carries nothing): `{type: 'brokeredAuth.ready'}`.
//! 2. app → page (`event.source`): `{type: 'brokeredAuth.challenge', returnOrigin,
//!    codeChallenge, codeChallengeMethod: 'S256', state}`.
//! 3. The user signs in; the server issues a handoff code bound to the challenge, the return
//!    origin and the user (`POST …/api/signin`, `…/api/signup` or `…/api/entra`).
//! 4. page → `window.opener`, target the return origin: `{type: 'brokeredAuth.handoff',
//!    handoffCode, state}`. The app redeems the code at `POST /api/auth/v1/token`
//!    (`authorization_code`, `codeType: 'fabric_handoff'`) and closes the popup.
//!
//! A cancelled or refused sign-in posts `{type: 'brokeredAuth.error', state, error,
//! errorDescription}` instead, also when the popup is closed (the SDK would otherwise wait for
//! its 5-minute timeout).
//!
//! The JSON endpoints serve the page only: they need its CSRF token (`X-Rayfin-Local-CSRF`) and
//! an `Origin` equal to the portal's own; "Continue as" also needs a loopback `Host`.
//!
//! The portal also hosts apps that sign in with `initEmbeddedAuth`, as Fabric does: see
//! [`embed`] and [`EMBED_HOST_PATH`].

mod embed;

use std::sync::Arc;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use ring::hmac;
use serde_json::{Value, json};

use crate::{
    AuthError, AuthService,
    handoff::{HandoffRequest, is_valid_code_challenge},
    routes::{ApiError, Fields, respond},
    users::User,
    util::{
        b64url, b64url_decode, is_loopback_authority, is_loopback_origin, random_bytes, web_origin,
    },
};

/// Where the sign-in portal lives on the backend: the app's `fabricPortalUrl` is
/// `<api origin>` + this.
pub const PORTAL_PATH: &str = "/.rayfin-local/portal";
/// The local Fabric host: a page that runs the app in a frame, in embedded mode, and signs it
/// in like Fabric does for apps using `initEmbeddedAuth`. `?app=<url>` names the app; without
/// it, the page frames the one set with [`AuthService::set_embed_app_url`].
pub const EMBED_HOST_PATH: &str = "/.rayfin-local/portal/embed";
const API_PATH: &str = "/.rayfin-local/portal/api";
const CSRF_HEADER: &str = "x-rayfin-local-csrf";
const CSRF_CONTEXT: &[u8] = b"rayfin-local portal csrf v1\n";
/// How long a sign-in page can be used; the SDK gives up after 5 minutes anyway.
const CSRF_LIFETIME_MS: i64 = 60 * 60 * 1000;
const MAX_STATE_CHARS: usize = 512;

const PAGE: &str = include_str!("portal/page.html");
const CARD: &str = include_str!("portal/card.html");
const STYLE: &str = include_str!("portal/page.css");
const SCRIPT: &str = include_str!("portal/page.js");

type AppState = State<Arc<AuthService>>;

/// The portal's state: the key of its CSRF tokens (new on every start, so pages from a previous
/// run are refused), and the local Fabric host's.
pub(crate) struct Portal {
    csrf_key: hmac::Key,
    host: embed::Host,
}

impl Portal {
    pub(crate) fn new() -> Result<Self, AuthError> {
        Ok(Self {
            csrf_key: hmac::Key::new(hmac::HMAC_SHA256, &random_bytes::<32>()?),
            host: embed::Host::default(),
        })
    }

    /// `<random>.<expires at ms>.<HMAC>`: each page says how long it can be used.
    fn csrf_token(&self, now_ms: i64, lifetime_ms: i64) -> Result<String, AuthError> {
        let body = format!(
            "{}.{}",
            b64url(&random_bytes::<16>()?),
            now_ms + lifetime_ms
        );
        let tag = hmac::sign(&self.csrf_key, &[CSRF_CONTEXT, body.as_bytes()].concat());
        Ok(format!("{body}.{}", b64url(tag.as_ref())))
    }

    fn csrf_valid(&self, token: &str, now_ms: i64) -> bool {
        let Some((body, tag)) = token.rsplit_once('.') else {
            return false;
        };
        let Some(tag) = b64url_decode(tag) else {
            return false;
        };
        let message = [CSRF_CONTEXT, body.as_bytes()].concat();
        if hmac::verify(&self.csrf_key, &message, &tag).is_err() {
            return false;
        }
        body.split_once('.')
            .and_then(|(_, expires)| expires.parse::<i64>().ok())
            .is_some_and(|expires| now_ms <= expires)
    }
}

/// The page and its JSON endpoints, with `Cache-Control: no-store` and `nosniff` on every answer.
pub(crate) fn routes() -> Router<Arc<AuthService>> {
    Router::new()
        .route(PORTAL_PATH, get(page))
        .route("/.rayfin-local/portal/", get(page))
        .route("/.rayfin-local/portal/secureItemEmbed", get(page))
        .route(
            "/.rayfin-local/portal/groups/{workspace}/appbackends/{item}/brokeredauth",
            get(page),
        )
        .route("/.rayfin-local/portal/api/challenge", post(check_challenge))
        .route("/.rayfin-local/portal/api/whoami", post(whoami))
        .route("/.rayfin-local/portal/api/signin", post(sign_in))
        .route("/.rayfin-local/portal/api/signup", post(sign_up))
        .route("/.rayfin-local/portal/api/entra", post(continue_as_entra))
        .route(EMBED_HOST_PATH, get(embed::host_page))
        .route(
            "/.rayfin-local/portal/api/embed/signin",
            post(embed::sign_in),
        )
        .route(
            "/.rayfin-local/portal/api/embed/signup",
            post(embed::sign_up),
        )
        .route("/.rayfin-local/portal/api/embed/entra", post(embed::entra))
        .route(
            "/.rayfin-local/portal/api/embed/handoff",
            post(embed::handoff),
        )
        .layer(middleware::map_response(protect))
}

async fn protect(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn page(State(auth): AppState) -> Response {
    auth.portal_page()
        .unwrap_or_else(|error| ApiError::internal(&error).into_response())
}

async fn check_challenge(
    State(auth): AppState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    respond(auth.portal_challenge(&headers, &uri, &body))
}

async fn whoami(State(auth): AppState, headers: HeaderMap, uri: Uri) -> Response {
    respond(auth.portal_whoami(&headers, &uri).await)
}

async fn sign_in(State(auth): AppState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    respond(auth.portal_sign_in(&headers, &uri, &body).await)
}

async fn sign_up(State(auth): AppState, headers: HeaderMap, uri: Uri, body: Bytes) -> Response {
    respond(auth.portal_sign_up(&headers, &uri, &body).await)
}

async fn continue_as_entra(
    State(auth): AppState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    respond(auth.portal_entra(&headers, &uri, &body).await)
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, message)
}

fn not_from_the_page() -> ApiError {
    error(
        StatusCode::FORBIDDEN,
        "invalid_portal_request",
        "This request did not come from an open rayfin-local sign-in page, or the page has \
         expired. Close the sign-in window and use the app's Sign in button again.",
    )
}

impl AuthService {
    /// The sign-in page: self-contained HTML whose inline style and script run under a
    /// per-response CSP nonce.
    fn portal_page(&self) -> Result<Response, AuthError> {
        let nonce = b64url(&random_bytes::<16>()?);
        let csrf = self.portal.csrf_token(self.now_ms(), CSRF_LIFETIME_MS)?;
        let settings = self.settings();
        let app_name = settings.app_name.as_deref();
        let title = escape_html(
            &app_name.map_or_else(|| "Sign in".to_owned(), |name| format!("Sign in to {name}")),
        );
        let config = json!({
            "csrf": csrf,
            "api": API_PATH,
            "appName": app_name,
            "authEnabled": settings.enabled,
            "signUp": settings.signup_enabled,
            "entra": self.entra_portal_enabled(),
        });
        let html = fill(
            PAGE,
            &[
                ("nonce", &nonce),
                ("title", &title),
                ("config", &script_json(&config)),
                ("style", STYLE),
                ("card", &fill(CARD, &[("heading", &title)])),
                ("script", SCRIPT),
            ],
        );
        html_page(StatusCode::OK, &page_csp(&nonce, None), html)
    }

    /// The checks of every JSON endpoint: auth is on, and the request comes from the portal's
    /// own page (its CSRF token, and an `Origin` equal to the portal's). Returns the `Host`.
    fn portal_request(&self, headers: &HeaderMap, uri: &Uri) -> Result<String, ApiError> {
        self.require_enabled()?;
        self.page_request(headers, uri)
            .ok_or_else(not_from_the_page)
    }

    /// The `Host` of a request from one of the portal's pages: one with a page's CSRF token,
    /// and an `Origin` equal to the portal's.
    fn page_request(&self, headers: &HeaderMap, uri: &Uri) -> Option<String> {
        let host = request_host(headers, uri)?;
        let same_origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|origin| {
                ["http", "https"]
                    .iter()
                    .any(|scheme| origin.eq_ignore_ascii_case(&format!("{scheme}://{host}")))
            });
        let token = headers
            .get(CSRF_HEADER)
            .and_then(|value| value.to_str().ok());
        (same_origin && token.is_some_and(|token| self.portal.csrf_valid(token, self.now_ms())))
            .then(|| host.to_owned())
    }

    /// The app's challenge, checked like the real broker checks it.
    fn handoff_request(&self, fields: &Fields, host: &str) -> Result<HandoffRequest, ApiError> {
        if fields.text("codeChallengeMethod")? != Some("S256") {
            return Err(ApiError::invalid_request(
                "codeChallengeMethod must be S256",
            ));
        }
        let code_challenge = fields
            .text("codeChallenge")?
            .filter(|challenge| is_valid_code_challenge(challenge))
            .ok_or_else(|| {
                ApiError::invalid_request(
                    "codeChallenge must be 43-128 characters using the base64url charset",
                )
            })?;
        let state = fields
            .text("state")?
            .filter(|state| !state.is_empty() && state.chars().count() <= MAX_STATE_CHARS)
            .ok_or_else(|| {
                ApiError::invalid_request("state is required (at most 512 characters)")
            })?;
        let return_origin = fields
            .non_blank("returnOrigin")?
            .ok_or_else(|| ApiError::invalid_request("returnOrigin is required"))?;
        Ok(HandoffRequest {
            code_challenge: code_challenge.to_owned(),
            return_origin: self.allowed_return_origin(return_origin, host)?,
            state: state.to_owned(),
        })
    }

    /// Codes go to apps on this machine, to the origins of `allowedRedirectUris`, and to the
    /// portal's own origin (the `--static` app).
    fn allowed_return_origin(&self, value: &str, host: &str) -> Result<String, ApiError> {
        let Some(origin) = web_origin(value) else {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "invalid_return_origin",
                "returnOrigin must be the app's http(s) origin, such as http://localhost:5173.",
            ));
        };
        if self.may_sign_in(&origin, Some(host)) {
            return Ok(origin);
        }
        Err(error(
            StatusCode::BAD_REQUEST,
            "invalid_return_origin",
            cannot_sign_in(&origin),
        ))
    }

    /// Whether apps on `origin` (from [`web_origin`]) may be signed in, for a portal reached on
    /// `host`.
    fn may_sign_in(&self, origin: &str, host: Option<&str>) -> bool {
        let own = host.is_some_and(|host| {
            ["http", "https"]
                .iter()
                .filter_map(|scheme| web_origin(&format!("{scheme}://{host}")))
                .any(|own| own == origin)
        });
        let listed = self
            .settings()
            .allowed_redirect_uris
            .iter()
            .filter_map(|uri| web_origin(uri))
            .any(|allowed| allowed == origin);
        is_loopback_origin(origin) || own || listed
    }

    fn portal_challenge(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        let host = self.portal_request(headers, uri)?;
        let request = self.handoff_request(&Fields::json(body)?, &host)?;
        Ok(json!({ "returnOrigin": request.return_origin }))
    }

    async fn portal_sign_in(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        let host = self.portal_request(headers, uri)?;
        let fields = Fields::json(body)?;
        let request = self.handoff_request(&fields, &host)?;
        let user = self.password_user(&fields).await?;
        self.handoff(&user, &request).await
    }

    /// The user whose username and password `fields` carry.
    async fn password_user(&self, fields: &Fields) -> Result<User, ApiError> {
        let (Some(username), Some(password)) = credentials(fields)? else {
            return Err(ApiError::invalid_request(
                "Enter your username and password.",
            ));
        };
        match self.verify_password(username, password).await {
            Ok(user) => Ok(user),
            Err(AuthError::InvalidCredentials) => Err(error(
                StatusCode::UNAUTHORIZED,
                "invalid_credentials",
                "The username or password is wrong.",
            )),
            Err(other) => Err(ApiError::internal(&other)),
        }
    }

    async fn portal_sign_up(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        let host = self.portal_request(headers, uri)?;
        self.require_sign_up()?;
        let fields = Fields::json(body)?;
        let request = self.handoff_request(&fields, &host)?;
        let user = self.new_user(&fields).await?;
        self.handoff(&user, &request).await
    }

    fn require_sign_up(&self) -> Result<(), ApiError> {
        if self.settings().signup_enabled {
            return Ok(());
        }
        Err(error(
            StatusCode::FORBIDDEN,
            "signup_disabled",
            "Creating accounts is turned off for this app. Ask the developer for an account \
             (`rayfin-local users add`).",
        ))
    }

    /// Creates the account whose username and password `fields` carry.
    async fn new_user(&self, fields: &Fields) -> Result<User, ApiError> {
        let (Some(username), Some(password)) = credentials(fields)? else {
            return Err(ApiError::invalid_request(
                "Choose a username and a password.",
            ));
        };
        match self.create_user(username, password).await {
            Ok(user) => Ok(user),
            Err(AuthError::UsernameTaken) => Err(error(
                StatusCode::CONFLICT,
                "user_exists",
                "An account with this username already exists. Sign in instead.",
            )),
            Err(AuthError::InvalidInput(message)) => Err(ApiError::invalid_request(message)),
            Err(other) => Err(ApiError::internal(&other)),
        }
    }

    /// "Continue as" is for the developer at this machine: it needs a loopback `Host`, which
    /// DNS rebinding cannot fake.
    fn require_entra_portal(&self, host: &str) -> Result<(), ApiError> {
        if !self.entra_portal_enabled() {
            return Err(error(
                StatusCode::NOT_FOUND,
                "not_available",
                "Signing in with your Microsoft account is turned off.",
            ));
        }
        if !is_loopback_authority(host) {
            return Err(error(
                StatusCode::FORBIDDEN,
                "invalid_portal_request",
                "Signing in with your Microsoft account only works when the sign-in page is \
                 opened on localhost.",
            ));
        }
        Ok(())
    }

    async fn portal_whoami(&self, headers: &HeaderMap, uri: &Uri) -> Result<Value, ApiError> {
        let host = self.portal_request(headers, uri)?;
        self.require_entra_portal(&host)?;
        Ok(match self.developer_identity().await {
            Ok(identity) => json!({
                "available": true,
                "name": identity.name,
                "username": identity.sign_in_name,
            }),
            Err(error) => json!({
                "available": false,
                "message": error.message,
                "hint": error.hint,
            }),
        })
    }

    async fn portal_entra(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        let host = self.portal_request(headers, uri)?;
        self.require_entra_portal(&host)?;
        let request = self.handoff_request(&Fields::json(body)?, &host)?;
        let user = self.developer_user().await?;
        self.handoff(&user, &request).await
    }

    /// The developer's user, from their Azure sign-in (created on first use).
    async fn developer_user(&self) -> Result<User, ApiError> {
        let identity = self.developer_identity().await.map_err(|failure| {
            let message = match failure.hint {
                Some(hint) => format!(
                    "Could not use your Azure sign-in: {}. {hint}",
                    failure.message
                ),
                None => format!("Could not use your Azure sign-in: {}.", failure.message),
            };
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "entra_unavailable",
                message,
            )
        })?;
        self.find_or_create_external_user(
            &identity.object_id,
            identity.email.as_deref(),
            identity.name.as_deref(),
        )
        .await
        .map_err(|failure| ApiError::internal(&failure))
    }

    async fn handoff(&self, user: &User, request: &HandoffRequest) -> Result<Value, ApiError> {
        let code = self
            .issue_handoff_code(user, request)
            .await
            .map_err(|failure| ApiError::internal(&failure))?;
        Ok(json!({ "handoffCode": code, "state": request.state }))
    }
}

fn credentials(fields: &Fields) -> Result<(Option<&str>, Option<&str>), ApiError> {
    let username = fields.non_blank("username")?;
    let password = fields
        .text("password")?
        .filter(|password| !password.is_empty());
    Ok((username, password))
}

fn cannot_sign_in(origin: &str) -> String {
    format!(
        "{origin} cannot receive sign-ins. rayfin-local signs in apps on this machine \
         (localhost, 127.0.0.1, [::1]) and on the origins listed in \
         services.auth.allowedRedirectUris in rayfin/rayfin.yml: open the app on localhost, or \
         add {origin} there."
    )
}

/// The request's `Host` (`name[:port]`).
fn request_host<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .or_else(|| uri.authority().map(|authority| authority.as_str()))
        .map(str::trim)
        .filter(|host| !host.is_empty())
}

/// The CSP of the portal's pages: only their own nonced style and script run, they only call
/// their own origin, nothing frames them, and they frame `frame_src` only (the Fabric host's
/// app).
fn page_csp(nonce: &str, frame_src: Option<&str>) -> String {
    let frames = frame_src.map_or_else(String::new, |source| format!("frame-src {source}; "));
    format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
         img-src data:; connect-src 'self'; {frames}base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'; require-trusted-types-for 'script'; trusted-types 'none'"
    )
}

fn html_page(status: StatusCode, csp: &str, html: String) -> Result<Response, AuthError> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_SECURITY_POLICY, csp)
        .header(header::X_FRAME_OPTIONS, "DENY")
        .header(header::REFERRER_POLICY, "no-referrer")
        .header("cross-origin-resource-policy", "same-origin")
        .body(Body::from(html))
        .map_err(|_| AuthError::Crypto("could not build the sign-in page"))
}

/// `template` with each `{{name}}` replaced by its value, in one pass (values are not scanned).
fn fill(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + 32 * 1024);
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .expect("the page template's placeholders are closed");
        let value = values
            .iter()
            .find(|(name, _)| *name == &after[..end])
            .map(|(_, value)| *value)
            .expect("the page template's placeholders have values");
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// JSON safe inside `<script type="application/json">`: nothing in it can close the element.
fn script_json(value: &Value) -> String {
    value
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use serde_json::json;

    use super::*;
    use crate::{
        AuthSettings,
        entra::tests::{FakeTokens, app_only_claims, entra_settings, user_claims},
        routes::tests::{Answer, App, app_from},
        tests::{APP_ID, service_with},
        util::sha256,
    };

    pub(super) const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    pub(super) const HOST: &str = "localhost:7171";
    pub(super) const APP_ORIGIN: &str = "http://localhost:5175";

    pub(super) fn challenge() -> Value {
        json!({
            "codeChallenge": b64url(&sha256(VERIFIER.as_bytes())),
            "codeChallengeMethod": "S256",
            "state": "Gm8vX0sQnUfT1k3a2Jd9Zw",
            "returnOrigin": APP_ORIGIN,
        })
    }

    pub(super) fn with(mut body: Value, extra: Value) -> Value {
        for (name, value) in extra.as_object().unwrap() {
            body[name] = value.clone();
        }
        body
    }

    pub(super) async fn portal_app(settings: AuthSettings) -> App {
        let (dir, auth) = service_with(settings).await;
        app_from(dir, auth)
    }

    async fn open_page(app: &App) -> (Answer, String) {
        let page = app
            .get("/.rayfin-local/portal/secureItemEmbed?workspaceId=w")
            .await;
        let html = String::from_utf8(page.raw.to_vec()).unwrap();
        let config = between(
            &html,
            r#"<script type="application/json" id="rl-config">"#,
            "</script>",
        );
        let csrf = serde_json::from_str::<Value>(config).unwrap()["csrf"]
            .as_str()
            .unwrap()
            .to_owned();
        (page, csrf)
    }

    pub(super) fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
        let from = text.find(start).unwrap() + start.len();
        &text[from..from + text[from..].find(end).unwrap()]
    }

    async fn call(
        app: &App,
        endpoint: &str,
        csrf: Option<&str>,
        origin: Option<&str>,
        body: Value,
    ) -> Answer {
        call_on(app, HOST, endpoint, csrf, origin, body).await
    }

    pub(super) async fn call_on(
        app: &App,
        host: &str,
        endpoint: &str,
        csrf: Option<&str>,
        origin: Option<&str>,
        body: Value,
    ) -> Answer {
        let mut request = Request::post(format!("{API_PATH}/{endpoint}"))
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(csrf) = csrf {
            request = request.header(CSRF_HEADER, csrf);
        }
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        app.send(request.body(Body::from(body.to_string())).unwrap())
            .await
    }

    /// A call from the page, as the browser makes it.
    async fn from_page(app: &App, csrf: &str, endpoint: &str, body: Value) -> Answer {
        call(
            app,
            endpoint,
            Some(csrf),
            Some("http://localhost:7171"),
            body,
        )
        .await
    }

    pub(super) async fn redeem(
        app: &App,
        code: &str,
        verifier: &str,
        redirect_uri: &str,
    ) -> Answer {
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

    #[tokio::test]
    async fn the_page_runs_under_a_fresh_nonce_and_strict_headers() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.app_name = Some("Explorer <dev>".into());
        let app = portal_app(settings).await;
        let mut nonces = Vec::new();
        for path in [
            "/.rayfin-local/portal",
            "/.rayfin-local/portal/",
            "/.rayfin-local/portal/secureItemEmbed?workspaceId=w&itemType=AppBackend",
            "/.rayfin-local/portal/groups/w/appbackends/i/brokeredauth",
        ] {
            let page = app.get(path).await;
            assert_eq!(page.status, StatusCode::OK, "{path}");
            let header = |name| page.headers[name].to_str().unwrap();
            assert_eq!(header(header::CONTENT_TYPE), "text/html; charset=utf-8");
            assert_eq!(header(header::X_FRAME_OPTIONS), "DENY");
            assert_eq!(header(header::CACHE_CONTROL), "no-store");
            assert_eq!(header(header::X_CONTENT_TYPE_OPTIONS), "nosniff");
            let csp = header(header::CONTENT_SECURITY_POLICY);
            let nonce = between(csp, "script-src 'nonce-", "'");
            assert!(csp.contains(&format!("style-src 'nonce-{nonce}'")));
            assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
            let html = String::from_utf8(page.raw.to_vec()).unwrap();
            assert!(html.contains(&format!(r#"<script nonce="{nonce}">"#)));
            assert!(html.contains(&format!(r#"<style nonce="{nonce}">"#)));
            assert!(html.contains("<title>Sign in to Explorer &lt;dev&gt;</title>"));
            assert!(!html.contains("{{"));
            assert!(!html.contains("<dev>"));
            nonces.push(nonce.to_owned());
        }
        nonces.dedup();
        assert_eq!(nonces.len(), 4, "every page gets its own nonce");
    }

    #[tokio::test]
    async fn only_the_page_itself_can_call_the_portal() {
        let app = portal_app(AuthSettings::new(APP_ID)).await;
        let (_, csrf) = open_page(&app).await;
        let (forged_body, _) = csrf.rsplit_once('.').unwrap();
        let forged = format!("{forged_body}.{}", b64url(&[0; 32]));
        let refused = [
            (None, Some("http://localhost:7171")),
            (Some(forged.as_str()), Some("http://localhost:7171")),
            (Some(csrf.as_str()), None),
            (Some(csrf.as_str()), Some("http://localhost:5175")),
            (Some(csrf.as_str()), Some("http://evil.example")),
        ];
        for (token, origin) in refused {
            let answer = call(&app, "challenge", token, origin, challenge()).await;
            assert_eq!(answer.status, StatusCode::FORBIDDEN, "{token:?} {origin:?}");
            assert_eq!(answer.body["error"], "invalid_portal_request");
        }
        let ok = from_page(&app, &csrf, "challenge", challenge()).await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
        assert_eq!(ok.body, json!({ "returnOrigin": APP_ORIGIN }));
        assert_eq!(ok.headers[header::CACHE_CONTROL], "no-store");

        app.auth.advance_clock(CSRF_LIFETIME_MS + 1);
        let expired = from_page(&app, &csrf, "challenge", challenge()).await;
        assert_eq!(expired.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn codes_only_go_to_loopback_or_allowed_origins() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.allowed_redirect_uris = vec!["https://App.Contoso.com/auth/callback".into()];
        let app = portal_app(settings).await;
        let (_, csrf) = open_page(&app).await;
        let accepted = [
            ("http://127.0.0.1:5173", "http://127.0.0.1:5173"),
            ("http://[::1]:5173/", "http://[::1]:5173"),
            ("https://app.localhost", "https://app.localhost"),
            ("http://localhost:7171", "http://localhost:7171"),
            ("https://app.contoso.com", "https://app.contoso.com"),
        ];
        for (origin, normalized) in accepted {
            let body = with(challenge(), json!({ "returnOrigin": origin }));
            let answer = from_page(&app, &csrf, "challenge", body).await;
            assert_eq!(answer.status, StatusCode::OK, "{origin}: {}", answer.body);
            assert_eq!(answer.body["returnOrigin"], normalized);
        }
        for origin in [
            "http://192.168.1.20:5173",
            "https://contoso.com",
            "null",
            "file:///C:/app",
        ] {
            let body = with(challenge(), json!({ "returnOrigin": origin }));
            let answer = from_page(&app, &csrf, "challenge", body).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{origin}");
            assert_eq!(answer.body["error"], "invalid_return_origin", "{origin}");
        }
        let malformed = [
            json!({ "codeChallengeMethod": "plain" }),
            json!({ "codeChallenge": "too-short" }),
            json!({ "state": "" }),
            json!({ "state": "s".repeat(513) }),
            json!({ "returnOrigin": null }),
        ];
        for change in malformed {
            let answer =
                from_page(&app, &csrf, "challenge", with(challenge(), change.clone())).await;
            assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{change}");
            assert_eq!(answer.body["error"], "invalid_request", "{change}");
        }
    }

    #[tokio::test]
    async fn signing_in_gives_a_code_the_sdk_redeems_once() {
        let app = portal_app(AuthSettings::new(APP_ID)).await;
        let user = app
            .auth
            .create_user("ada@contoso.com", "correct horse")
            .await
            .unwrap();
        let (_, csrf) = open_page(&app).await;

        let wrong = from_page(
            &app,
            &csrf,
            "signin",
            with(
                challenge(),
                json!({ "username": "ada@contoso.com", "password": "wrong" }),
            ),
        )
        .await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
        assert_eq!(wrong.body["error"], "invalid_credentials");

        let signed_in = from_page(
            &app,
            &csrf,
            "signin",
            with(
                challenge(),
                json!({ "username": "ADA@contoso.com", "password": "correct horse" }),
            ),
        )
        .await;
        assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
        assert_eq!(signed_in.body["state"], challenge()["state"]);
        let code = signed_in.body["handoffCode"].as_str().unwrap();

        let tokens = redeem(&app, code, VERIFIER, APP_ORIGIN).await;
        assert_eq!(tokens.status, StatusCode::OK, "{}", tokens.body);
        assert_eq!(tokens.body["tokenType"], "Bearer");
        let claims = app
            .auth
            .verify_session_token(tokens.body["accessToken"].as_str().unwrap())
            .unwrap();
        assert_eq!(claims.sub, user.id);
        assert_eq!(claims.email.as_deref(), Some("ada@contoso.com"));

        let again = redeem(&app, code, VERIFIER, APP_ORIGIN).await;
        assert_eq!(again.status, StatusCode::UNAUTHORIZED);
        assert_eq!(again.body["error"], "CODE_ALREADY_USED");
    }

    #[tokio::test]
    async fn signing_up_creates_the_account_unless_it_exists_or_sign_up_is_closed() {
        let app = portal_app(AuthSettings::new(APP_ID)).await;
        let (_, csrf) = open_page(&app).await;
        let account = json!({ "username": "grace@contoso.com", "password": "secret1" });
        let created = from_page(&app, &csrf, "signup", with(challenge(), account.clone())).await;
        assert_eq!(created.status, StatusCode::OK, "{}", created.body);
        let tokens = redeem(
            &app,
            created.body["handoffCode"].as_str().unwrap(),
            VERIFIER,
            APP_ORIGIN,
        )
        .await;
        assert_eq!(tokens.status, StatusCode::OK);
        let taken = from_page(&app, &csrf, "signup", with(challenge(), account.clone())).await;
        assert_eq!(taken.status, StatusCode::CONFLICT);
        assert_eq!(taken.body["error"], "user_exists");

        let mut settings = AuthSettings::new(APP_ID);
        settings.signup_enabled = false;
        let closed = portal_app(settings).await;
        let (_, csrf) = open_page(&closed).await;
        let refused = from_page(&closed, &csrf, "signup", with(challenge(), account)).await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert_eq!(refused.body["error"], "signup_disabled");
    }

    #[tokio::test]
    async fn continue_as_signs_in_the_developer() {
        let (dir, auth) = service_with(entra_settings()).await;
        let app = app_from(
            dir,
            auth.with_entra(Arc::new(FakeTokens(Ok(user_claims())))),
        );
        let (page, csrf) = open_page(&app).await;
        assert!(
            String::from_utf8(page.raw.to_vec())
                .unwrap()
                .contains(r#""entra":true"#)
        );

        let who = from_page(&app, &csrf, "whoami", json!({})).await;
        assert_eq!(
            who.body,
            json!({ "available": true, "name": "Ada Lovelace", "username": "ada@contoso.com" })
        );
        let mut codes = Vec::new();
        for _ in 0..2 {
            let answer = from_page(&app, &csrf, "entra", challenge()).await;
            assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
            let tokens = redeem(
                &app,
                answer.body["handoffCode"].as_str().unwrap(),
                VERIFIER,
                APP_ORIGIN,
            )
            .await;
            let claims = app
                .auth
                .verify_session_token(tokens.body["accessToken"].as_str().unwrap())
                .unwrap();
            assert_eq!(claims.email.as_deref(), Some("ada@contoso.com"));
            codes.push(claims.sub);
        }
        assert_eq!(codes[0], codes[1], "the same user both times");

        let rebound = call_on(
            &app,
            "evil.example:7171",
            "entra",
            Some(&csrf),
            Some("http://evil.example:7171"),
            challenge(),
        )
        .await;
        assert_eq!(rebound.status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn continue_as_refuses_an_app_only_azure_sign_in() {
        let (dir, auth) = service_with(entra_settings()).await;
        let app = app_from(
            dir,
            auth.with_entra(Arc::new(FakeTokens(Ok(app_only_claims())))),
        );
        let (_, csrf) = open_page(&app).await;
        let who = from_page(&app, &csrf, "whoami", json!({})).await;
        assert_eq!(who.body["available"], false);
        assert_eq!(
            who.body["message"],
            "your Azure sign-in is not a user account"
        );
        let refused = from_page(&app, &csrf, "entra", challenge()).await;
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(refused.body["error"], "entra_unavailable");
        assert!(
            refused.body["message"]
                .as_str()
                .unwrap()
                .contains("not a user account")
        );
        assert!(app.auth.list_users().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn continue_as_explains_a_missing_azure_sign_in() {
        let failure = rl_tokens::TokenError {
            scope: rl_tokens::Audience::PowerBi.default_scope().into(),
            message: "the Azure CLI is not signed in".into(),
            hint: Some("Run `az login`.".into()),
        };
        let (dir, auth) = service_with(entra_settings()).await;
        let app = app_from(dir, auth.with_entra(Arc::new(FakeTokens(Err(failure)))));
        let (_, csrf) = open_page(&app).await;
        let who = from_page(&app, &csrf, "whoami", json!({})).await;
        assert_eq!(who.body["available"], false);
        assert_eq!(who.body["hint"], "Run `az login`.");
        let refused = from_page(&app, &csrf, "entra", challenge()).await;
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            refused.body["message"]
                .as_str()
                .unwrap()
                .contains("az login")
        );

        let off = portal_app(AuthSettings::new(APP_ID)).await;
        let (page, csrf) = open_page(&off).await;
        assert!(
            String::from_utf8(page.raw.to_vec())
                .unwrap()
                .contains(r#""entra":false"#)
        );
        assert_eq!(
            from_page(&off, &csrf, "whoami", json!({})).await.status,
            StatusCode::NOT_FOUND
        );
    }
}
