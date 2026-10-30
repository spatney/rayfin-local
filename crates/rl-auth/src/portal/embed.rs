//! The local Fabric host: rayfin-local's stand-in for the Fabric page that runs an app in a
//! frame, for apps that sign in with `initEmbeddedAuth` (anywhere else they show "Can't open
//! this app outside Fabric").
//!
//! `GET` [`EMBED_HOST_PATH`](crate::EMBED_HOST_PATH)`?app=<url>` (without `app`, the URL set
//! with [`AuthService::set_embed_app_url`]) serves a page where the developer signs in with the
//! sign-in page's form; the page then frames the app, with `fabricEmbedded=true` added to its
//! query. The SDK 1.36.2 (`initEmbeddedAuth.js` and `embeddedFabricLogin.js` in
//! `@microsoft/rayfin-auth-provider-fabric`, `postMessageBridge.js` in
//! `@microsoft/fabric-embedded-host`) then runs this handshake with the page:
//!
//! 1. app → parent, target `*`: `{channel: 'fabric-auth', version: 1, kind:
//!    'externalEmbed.ready', requestId}`. Fabric does not answer it, and neither does the
//!    page: after 300 ms the app takes Fabric's path.
//! 2. With no `_fu` user hint in its URL (the page adds none), the app signs out, then posts
//!    (target `*`) `{channel: 'fabric-auth', version: 1, kind: 'auth.requestHandoff',
//!    requestId, payload: {callbackUrl, codeChallenge, codeChallengeMethod: 'S256', state}}`
//!    and waits 30 seconds for its parent's answer.
//! 3. The page gets a handoff code for the developer (`POST …/api/embed/handoff`) and answers,
//!    to the app's origin only: `{channel: 'fabric-auth', version: 1, kind: 'response',
//!    requestId, success: true, result: {handoffCode, state}}`, or `success: false, error:
//!    {code, message}`.
//! 4. The app redeems the code at `POST /api/auth/v1/token` with `redirectUri: callbackUrl`.
//!
//! Every load of the app asks again, and the page answers at once for its current user: "Switch
//! user" signs someone else in, then reloads the app. Other requests with a `requestId` get
//! `NOT_SUPPORTED` at once, so the app does not wait for its timeout.
//!
//! The JSON endpoints take the sign-in page's checks (its CSRF token and `Origin`; "Continue as"
//! needs a loopback `Host`):
//!
//! - `POST …/api/embed/signin` and `…/embed/signup` (`{username, password}`), `…/embed/entra`
//!   (`{}`): a host session, `{hostToken, user: {id, username, email, name}}`. The server keeps
//!   the token's hash only, in memory, for 12 hours; the page keeps the token for its tab.
//! - `POST …/api/embed/handoff` (`{hostToken, returnOrigin, codeChallenge, codeChallengeMethod,
//!   state}`): `{handoffCode, state}`, checked like the sign-in page's challenges; 401
//!   `invalid_host_session` when the session expired.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use parking_lot::{Mutex, RwLock};
use serde_json::{Value, json};
use url::Url;

use super::{
    API_PATH, AppState, CARD, SCRIPT, STYLE, cannot_sign_in, error, escape_html, fill, html_page,
    page_csp, request_host, script_json,
};
use crate::{
    AuthError, AuthService,
    routes::{ApiError, Fields, respond},
    users::{User, find_user},
    util::{b64url, random_bytes, sha256, web_origin},
};

/// How long a sign-in on the host page lasts; the page itself too.
const HOST_SESSION_LIFETIME_MS: i64 = 12 * 60 * 60 * 1000;
/// The most host sessions kept: past it, a sign-in ends the oldest.
const MAX_HOST_SESSIONS: usize = 1000;
/// What puts the SDK in embedded mode (`isEmbeddedMode` in `@microsoft/fabric-embedded-host`).
const EMBEDDED_PARAM: &str = "fabricEmbedded";

const PAGE: &str = include_str!("embed.html");
const HOST_STYLE: &str = include_str!("embed.css");
const NOTICE: &str = include_str!("notice.html");

/// The host's state: lost when rayfin-local stops.
#[derive(Default)]
pub(super) struct Host {
    /// The app to frame when the page's URL names none.
    app_url: RwLock<Option<String>>,
    /// Signed-in developers, by the SHA-256 of their host token.
    sessions: Mutex<HashMap<[u8; 32], HostSession>>,
}

struct HostSession {
    user_id: String,
    expires_at: i64,
}

/// The app the page frames.
struct FramedApp {
    origin: String,
    /// Its URL in embedded mode.
    src: String,
}

pub(super) async fn host_page(State(auth): AppState, headers: HeaderMap, uri: Uri) -> Response {
    auth.host_page(&headers, &uri)
        .unwrap_or_else(|error| ApiError::internal(&error).into_response())
}

pub(super) async fn sign_in(
    State(auth): AppState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    respond(auth.host_sign_in(&headers, &uri, &body).await)
}

pub(super) async fn sign_up(
    State(auth): AppState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    respond(auth.host_sign_up(&headers, &uri, &body).await)
}

pub(super) async fn entra(State(auth): AppState, headers: HeaderMap, uri: Uri) -> Response {
    respond(auth.host_entra(&headers, &uri).await)
}

pub(super) async fn handoff(
    State(auth): AppState,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    respond(auth.host_handoff(&headers, &uri, &body).await)
}

fn sign_in_again() -> ApiError {
    error(
        StatusCode::UNAUTHORIZED,
        "invalid_host_session",
        "Sign in again: your sign-in on the Fabric host page has ended (it lasts 12 hours, and \
         rayfin-local forgets it when it restarts).",
    )
}

impl AuthService {
    /// The app the local Fabric host ([`EMBED_HOST_PATH`](crate::EMBED_HOST_PATH)) frames when
    /// its URL has no `?app=`, such as the app's dev server (`http://localhost:5173/`); `None`
    /// (or blank) makes the page ask for one. The page checks it like a `returnOrigin`: on this
    /// machine, in `allowedRedirectUris`, or the portal's own origin.
    pub fn set_embed_app_url(&self, url: Option<String>) {
        *self.portal.host.app_url.write() = url
            .map(|url| url.trim().to_owned())
            .filter(|url| !url.is_empty());
    }

    /// The host page, under the sign-in page's CSP plus `frame-src` for the app; or a page that
    /// explains why it cannot frame the app.
    fn host_page(&self, headers: &HeaderMap, uri: &Uri) -> Result<Response, AuthError> {
        let settings = self.settings();
        if !settings.enabled {
            return notice(
                StatusCode::NOT_FOUND,
                "Sign-in is turned off",
                "This app has authentication turned off (services.auth.enabled is false in \
                 rayfin/rayfin.yml), so the local Fabric host cannot sign it in.",
            );
        }
        let requested = uri
            .query()
            .and_then(|query| {
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(name, _)| name == "app")
                    .map(|(_, value)| value.trim().to_owned())
            })
            .filter(|value| !value.is_empty());
        let Some(app_url) = requested.or_else(|| self.portal.host.app_url.read().clone()) else {
            return notice(
                StatusCode::BAD_REQUEST,
                "Which app?",
                "The local Fabric host runs your app in a frame and signs it in as Fabric does. \
                 Open this page from the rayfin-local banner, or add the app's URL: \
                 ?app=http://localhost:5173/",
            );
        };
        let app = match self.framed_app(&app_url, request_host(headers, uri)) {
            Ok(app) => app,
            Err(problem) => {
                return notice(
                    StatusCode::BAD_REQUEST,
                    "This app cannot be opened here",
                    &problem,
                );
            }
        };

        let nonce = b64url(&random_bytes::<16>()?);
        let csrf = self
            .portal
            .csrf_token(self.now_ms(), HOST_SESSION_LIFETIME_MS)?;
        let app_name = settings.app_name.as_deref();
        let label = app_name.unwrap_or(&app.origin);
        let heading =
            app_name.map_or_else(|| "Sign in".to_owned(), |name| format!("Sign in to {name}"));
        let config = json!({
            "csrf": csrf,
            "api": API_PATH,
            "appName": app_name,
            "authEnabled": true,
            "signUp": settings.signup_enabled,
            "entra": self.entra_portal_enabled(),
            "embed": { "src": app.src, "origin": app.origin },
        });
        let html = fill(
            PAGE,
            &[
                ("nonce", &nonce),
                (
                    "title",
                    &escape_html(&format!("{label} · Local Fabric host")),
                ),
                ("config", &script_json(&config)),
                ("style", &format!("{STYLE}{HOST_STYLE}")),
                ("app", &escape_html(label)),
                ("card", &fill(CARD, &[("heading", &escape_html(&heading))])),
                ("script", SCRIPT),
            ],
        );
        let csp = page_csp(&nonce, Some(&frame_source(&app.origin)));
        html_page(StatusCode::OK, &csp, html)
    }

    /// The app at `value`, if its origin may be signed in.
    fn framed_app(&self, value: &str, host: Option<&str>) -> Result<FramedApp, String> {
        let (Some(origin), Ok(url)) = (web_origin(value), Url::parse(value)) else {
            return Err(format!(
                "The app's URL ({value}) is not an http(s) URL such as http://localhost:5173/."
            ));
        };
        if !self.may_sign_in(&origin, host) {
            return Err(cannot_sign_in(&origin));
        }
        Ok(FramedApp {
            src: embedded_url(url),
            origin,
        })
    }

    /// The checks of the host's JSON endpoints: the sign-in page's, with the host's message.
    fn host_request(&self, headers: &HeaderMap, uri: &Uri) -> Result<String, ApiError> {
        self.require_enabled()?;
        self.page_request(headers, uri).ok_or_else(|| {
            error(
                StatusCode::FORBIDDEN,
                "invalid_portal_request",
                "This request did not come from an open rayfin-local Fabric host page, or the \
                 page has expired (rayfin-local restarted, or the page is 12 hours old). Reload \
                 the page.",
            )
        })
    }

    async fn host_sign_in(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        self.host_request(headers, uri)?;
        let user = self.password_user(&Fields::json(body)?).await?;
        self.start_host_session(&user)
    }

    async fn host_sign_up(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        self.host_request(headers, uri)?;
        self.require_sign_up()?;
        let user = self.new_user(&Fields::json(body)?).await?;
        self.start_host_session(&user)
    }

    async fn host_entra(&self, headers: &HeaderMap, uri: &Uri) -> Result<Value, ApiError> {
        let host = self.host_request(headers, uri)?;
        self.require_entra_portal(&host)?;
        let user = self.developer_user().await?;
        self.start_host_session(&user)
    }

    /// Signs `user` in on the host page: `{hostToken, user}`.
    fn start_host_session(&self, user: &User) -> Result<Value, ApiError> {
        let token = b64url(&random_bytes::<32>().map_err(|failure| ApiError::internal(&failure))?);
        let now = self.now_ms();
        let mut sessions = self.portal.host.sessions.lock();
        sessions.retain(|_, session| session.expires_at > now);
        if sessions.len() >= MAX_HOST_SESSIONS {
            let oldest = sessions
                .iter()
                .min_by_key(|(_, session)| session.expires_at)
                .map(|(key, _)| *key);
            if let Some(oldest) = oldest {
                sessions.remove(&oldest);
            }
        }
        sessions.insert(
            sha256(token.as_bytes()),
            HostSession {
                user_id: user.id.clone(),
                expires_at: now + HOST_SESSION_LIFETIME_MS,
            },
        );
        drop(sessions);
        Ok(json!({
            "hostToken": token,
            "user": {
                "id": user.id,
                "username": user.username,
                "email": user.email,
                "name": user.display_name,
            },
        }))
    }

    /// A handoff code for the app, for the host session's user.
    async fn host_handoff(
        &self,
        headers: &HeaderMap,
        uri: &Uri,
        body: &[u8],
    ) -> Result<Value, ApiError> {
        let host = self.host_request(headers, uri)?;
        let fields = Fields::json(body)?;
        let user_id = self.host_session_user(fields.text("hostToken")?)?;
        let request = self.handoff_request(&fields, &host)?;
        let user = self
            .user_by_id(user_id.clone())
            .await
            .map_err(|failure| ApiError::internal(&failure))?;
        let Some(user) = user else {
            // Deleted since (`rayfin-local users delete`).
            self.portal
                .host
                .sessions
                .lock()
                .retain(|_, session| session.user_id != user_id);
            return Err(sign_in_again());
        };
        self.handoff(&user, &request).await
    }

    async fn user_by_id(&self, id: String) -> Result<Option<User>, AuthError> {
        self.db
            .call(move |conn| find_user(conn, "id = ?1", &id))
            .await?
    }

    /// The user of the live host session `token` starts.
    fn host_session_user(&self, token: Option<&str>) -> Result<String, ApiError> {
        let key = sha256(token.unwrap_or_default().as_bytes());
        let now = self.now_ms();
        let mut sessions = self.portal.host.sessions.lock();
        match sessions.get(&key) {
            Some(session) if session.expires_at > now => Ok(session.user_id.clone()),
            Some(_) => {
                sessions.remove(&key);
                Err(sign_in_again())
            }
            None => Err(sign_in_again()),
        }
    }
}

/// `url` in embedded mode: `fabricEmbedded=true` in its query (replacing any value it had), its
/// other parameters and its fragment as they were.
fn embedded_url(mut url: Url) -> String {
    let flag = format!("{EMBEDDED_PARAM}=true");
    let query = url
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty() && pair.split('=').next() != Some(EMBEDDED_PARAM))
        .chain([flag.as_str()])
        .collect::<Vec<_>>()
        .join("&");
    url.set_query(Some(&query));
    url.into()
}

/// `origin` as a CSP source. CSP cannot name IPv6 addresses: for those, any origin of the
/// scheme.
fn frame_source(origin: &str) -> String {
    match origin.split_once("://[") {
        Some((scheme, _)) => format!("{scheme}:"),
        None => origin.to_owned(),
    }
}

/// A page that explains why the host cannot run; it has no script.
fn notice(status: StatusCode, title: &str, text: &str) -> Result<Response, AuthError> {
    let nonce = b64url(&random_bytes::<16>()?);
    let html = fill(
        NOTICE,
        &[
            ("nonce", &nonce),
            ("title", &escape_html(title)),
            ("style", STYLE),
            ("text", &escape_html(text)),
        ],
    );
    html_page(status, &page_csp(&nonce, None), html)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Request, header},
    };
    use serde_json::json;

    use super::*;
    use crate::{
        AuthSettings, EMBED_HOST_PATH,
        entra::tests::{FakeTokens, entra_settings, user_claims},
        portal::tests::{
            APP_ORIGIN, HOST, VERIFIER, between, call_on, challenge, portal_app, redeem, with,
        },
        routes::tests::{Answer, App, app_from},
        tests::{APP_ID, service_with},
        util::b64url_decode,
    };

    const PAGE_ORIGIN: &str = "http://localhost:7171";
    const USER: &str = "ada@contoso.com";
    const PASSWORD: &str = "correct horse";

    fn config(page: &Answer) -> Value {
        let html = std::str::from_utf8(&page.raw).unwrap();
        let config = between(
            html,
            r#"<script type="application/json" id="rl-config">"#,
            "</script>",
        );
        serde_json::from_str(config).unwrap()
    }

    /// Opens the host page for the app at [`APP_ORIGIN`]; returns its CSRF token.
    async fn open_host(app: &App) -> String {
        let page = app
            .get(&format!("{EMBED_HOST_PATH}?app={APP_ORIGIN}"))
            .await;
        assert_eq!(page.status, StatusCode::OK);
        config(&page)["csrf"].as_str().unwrap().to_owned()
    }

    /// A call from the host page, as the browser makes it.
    async fn from_host(app: &App, csrf: &str, endpoint: &str, body: Value) -> Answer {
        let endpoint = format!("embed/{endpoint}");
        call_on(app, HOST, &endpoint, Some(csrf), Some(PAGE_ORIGIN), body).await
    }

    async fn sign_in(app: &App, csrf: &str) -> String {
        let account = json!({ "username": USER, "password": PASSWORD });
        let answer = from_host(app, csrf, "signin", account).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        answer.body["hostToken"].as_str().unwrap().to_owned()
    }

    /// The app's `auth.requestHandoff`, as the page forwards it.
    fn handoff_request(host_token: &str) -> Value {
        with(challenge(), json!({ "hostToken": host_token }))
    }

    fn subject(app: &App, tokens: &Answer) -> String {
        let access_token = tokens.body["accessToken"].as_str().unwrap();
        app.auth.verify_session_token(access_token).unwrap().sub
    }

    #[tokio::test]
    async fn the_host_page_frames_the_app_in_embedded_mode() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.app_name = Some("Explorer <dev>".into());
        let app = portal_app(settings).await;
        app.auth.set_embed_app_url(Some(
            " http://localhost:5173/explorer?tab=a%20b&fabricEmbedded=false#top ".into(),
        ));
        let cases = [
            (
                EMBED_HOST_PATH.to_owned(),
                "http://localhost:5173",
                "http://localhost:5173/explorer?tab=a%20b&fabricEmbedded=true#top",
                "frame-src http://localhost:5173;",
            ),
            (
                format!("{EMBED_HOST_PATH}?app=http%3A%2F%2F127.0.0.1%3A4000"),
                "http://127.0.0.1:4000",
                "http://127.0.0.1:4000/?fabricEmbedded=true",
                "frame-src http://127.0.0.1:4000;",
            ),
            // CSP cannot name IPv6 addresses.
            (
                format!("{EMBED_HOST_PATH}?app=http://%5B::1%5D:5173/"),
                "http://[::1]:5173",
                "http://[::1]:5173/?fabricEmbedded=true",
                "frame-src http:;",
            ),
        ];
        for (path, origin, src, frame_src) in cases {
            let page = app.get(&path).await;
            assert_eq!(page.status, StatusCode::OK, "{path}");
            assert_eq!(page.headers[header::X_FRAME_OPTIONS], "DENY");
            assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
            let csp = page.headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap();
            assert!(csp.contains(frame_src), "{csp}");
            assert!(csp.contains("frame-ancestors 'none'") && csp.contains("default-src 'none'"));
            let nonce = between(csp, "script-src 'nonce-", "'");
            let html = std::str::from_utf8(&page.raw).unwrap();
            assert!(html.contains(&format!(r#"<script nonce="{nonce}">"#)));
            assert!(html.contains("<title>Explorer &lt;dev&gt; · Local Fabric host</title>"));
            assert!(!html.contains("{{") && !html.contains("<dev>"));
            assert_eq!(
                config(&page)["embed"],
                json!({ "src": src, "origin": origin })
            );
        }
    }

    #[tokio::test]
    async fn the_host_only_frames_apps_it_can_sign_in() {
        let mut settings = AuthSettings::new(APP_ID);
        settings.allowed_redirect_uris = vec!["https://app.contoso.com/auth/callback".into()];
        let app = portal_app(settings).await;
        let refused = [
            (EMBED_HOST_PATH.to_owned(), "?app=http://localhost:5173/"),
            (
                format!("{EMBED_HOST_PATH}?app=https://contoso.com/"),
                "https://contoso.com cannot receive sign-ins",
            ),
            (
                format!("{EMBED_HOST_PATH}?app=javascript:alert(1)"),
                "is not an http(s) URL",
            ),
        ];
        for (path, explanation) in refused {
            let page = app.get(&path).await;
            assert_eq!(page.status, StatusCode::BAD_REQUEST, "{path}");
            let html = std::str::from_utf8(&page.raw).unwrap();
            assert!(html.contains(explanation), "{path}: {html}");
            assert!(!html.contains("<script"), "{path}");
            let csp = page.headers[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap();
            assert!(!csp.contains("frame-src"), "{path}");
        }
        // Listed origins, and the portal's own (the `--static` app).
        for (host, app_url, status) in [
            (HOST, "https://app.contoso.com/", StatusCode::OK),
            (
                "devbox.example:7171",
                "http://devbox.example:7171/app/",
                StatusCode::OK,
            ),
            (
                HOST,
                "http://devbox.example:7171/app/",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let request = Request::get(format!("{EMBED_HOST_PATH}?app={app_url}"))
                .header(header::HOST, host)
                .body(Body::empty())
                .unwrap();
            assert_eq!(app.send(request).await.status, status, "{host} {app_url}");
        }

        let mut settings = AuthSettings::new(APP_ID);
        settings.enabled = false;
        let off = portal_app(settings).await;
        let page = off
            .get(&format!("{EMBED_HOST_PATH}?app={APP_ORIGIN}"))
            .await;
        assert_eq!(page.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn signing_in_on_the_host_gives_the_app_codes_it_redeems() {
        let app = portal_app(AuthSettings::new(APP_ID)).await;
        let ada = app.auth.create_user(USER, PASSWORD).await.unwrap();
        let csrf = open_host(&app).await;

        let wrong = json!({ "username": USER, "password": "wrong" });
        let wrong = from_host(&app, &csrf, "signin", wrong).await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
        assert_eq!(wrong.body["error"], "invalid_credentials");

        let account = json!({ "username": "ADA@contoso.com", "password": PASSWORD });
        let signed_in = from_host(&app, &csrf, "signin", account).await;
        assert_eq!(signed_in.status, StatusCode::OK, "{}", signed_in.body);
        assert_eq!(
            signed_in.body["user"],
            json!({ "id": ada.id, "username": USER, "email": USER, "name": null })
        );
        let host_token = signed_in.body["hostToken"].as_str().unwrap();
        assert_eq!(b64url_decode(host_token).unwrap().len(), 32);

        // Every load of the app asks for a code, bound to its verifier and origin.
        for _ in 0..2 {
            let code = from_host(&app, &csrf, "handoff", handoff_request(host_token)).await;
            assert_eq!(code.status, StatusCode::OK, "{}", code.body);
            assert_eq!(code.body["state"], challenge()["state"]);
            let code = code.body["handoffCode"].as_str().unwrap();
            let other_verifier = redeem(&app, code, &"x".repeat(43), APP_ORIGIN).await;
            assert_eq!(other_verifier.body["error"], "INVALID_CODE_VERIFIER");
            let other_origin = redeem(&app, code, VERIFIER, "http://localhost:5176").await;
            assert_eq!(other_origin.body["error"], "REDIRECT_URI_MISMATCH");
            let tokens = redeem(&app, code, VERIFIER, APP_ORIGIN).await;
            assert_eq!(tokens.status, StatusCode::OK, "{}", tokens.body);
            assert_eq!(subject(&app, &tokens), ada.id);
        }

        // "Switch user": another sign-in, here a new account.
        let account = json!({ "username": "grace@contoso.com", "password": "secret1" });
        let created = from_host(&app, &csrf, "signup", account.clone()).await;
        assert_eq!(created.status, StatusCode::OK, "{}", created.body);
        let grace = created.body["hostToken"].as_str().unwrap();
        let code = from_host(&app, &csrf, "handoff", handoff_request(grace)).await;
        let code = code.body["handoffCode"].as_str().unwrap();
        let tokens = redeem(&app, code, VERIFIER, APP_ORIGIN).await;
        assert_eq!(
            subject(&app, &tokens),
            created.body["user"]["id"].as_str().unwrap()
        );
        let taken = from_host(&app, &csrf, "signup", account).await;
        assert_eq!(taken.status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn host_handoffs_need_the_page_and_a_live_session() {
        let app = portal_app(AuthSettings::new(APP_ID)).await;
        let ada = app.auth.create_user(USER, PASSWORD).await.unwrap();
        let csrf = open_host(&app).await;
        let host_token = sign_in(&app, &csrf).await;
        let request = handoff_request(&host_token);

        for endpoint in ["embed/handoff", "embed/signin"] {
            for (token, origin) in [
                (None, Some(PAGE_ORIGIN)),
                (Some(csrf.as_str()), None),
                (Some(csrf.as_str()), Some(APP_ORIGIN)),
                (Some(csrf.as_str()), Some("http://evil.example")),
            ] {
                let answer = call_on(&app, HOST, endpoint, token, origin, request.clone()).await;
                assert_eq!(
                    answer.status,
                    StatusCode::FORBIDDEN,
                    "{endpoint} {origin:?}"
                );
                assert_eq!(answer.body["error"], "invalid_portal_request");
                let message = answer.body["message"].as_str().unwrap();
                assert!(message.contains("Fabric host page"), "{message}");
            }
        }
        for unknown in [json!(null), json!(""), json!(format!("{host_token}x"))] {
            let body = with(request.clone(), json!({ "hostToken": unknown }));
            let answer = from_host(&app, &csrf, "handoff", body).await;
            assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{unknown}");
            assert_eq!(answer.body["error"], "invalid_host_session");
        }
        // The app's request is checked like the sign-in page's challenges.
        let elsewhere = with(
            request.clone(),
            json!({ "returnOrigin": "https://contoso.com" }),
        );
        let elsewhere = from_host(&app, &csrf, "handoff", elsewhere).await;
        assert_eq!(elsewhere.body["error"], "invalid_return_origin");
        let plain = with(request.clone(), json!({ "codeChallengeMethod": "plain" }));
        assert_eq!(
            from_host(&app, &csrf, "handoff", plain).await.status,
            StatusCode::BAD_REQUEST
        );

        // The page outlasts the sign-in page's hour; sign-ins and pages end after 12 hours.
        app.auth.advance_clock(2 * 60 * 60 * 1000);
        let later = from_host(&app, &csrf, "handoff", request.clone()).await;
        assert_eq!(later.status, StatusCode::OK, "{}", later.body);
        app.auth.advance_clock(HOST_SESSION_LIFETIME_MS);
        let old_page = from_host(&app, &csrf, "handoff", request.clone()).await;
        assert_eq!(old_page.status, StatusCode::FORBIDDEN);
        let csrf = open_host(&app).await;
        let expired = from_host(&app, &csrf, "handoff", request).await;
        assert_eq!(expired.status, StatusCode::UNAUTHORIZED);
        assert_eq!(expired.body["error"], "invalid_host_session");

        // Deleting the user ends their sessions.
        let host_token = sign_in(&app, &csrf).await;
        app.auth.delete_user(&ada.id).await.unwrap();
        let deleted = from_host(&app, &csrf, "handoff", handoff_request(&host_token)).await;
        assert_eq!(deleted.status, StatusCode::UNAUTHORIZED);
        assert!(app.auth.portal.host.sessions.lock().is_empty());
    }

    #[tokio::test]
    async fn continue_as_on_the_host_needs_localhost() {
        let (dir, auth) = service_with(entra_settings()).await;
        let app = app_from(
            dir,
            auth.with_entra(Arc::new(FakeTokens(Ok(user_claims())))),
        );
        let csrf = open_host(&app).await;
        let answer = from_host(&app, &csrf, "entra", json!({})).await;
        assert_eq!(answer.status, StatusCode::OK, "{}", answer.body);
        assert_eq!(answer.body["user"]["name"], "Ada Lovelace");
        let code = from_host(
            &app,
            &csrf,
            "handoff",
            handoff_request(answer.body["hostToken"].as_str().unwrap()),
        )
        .await;
        let code = code.body["handoffCode"].as_str().unwrap();
        let tokens = redeem(&app, code, VERIFIER, APP_ORIGIN).await;
        assert_eq!(
            subject(&app, &tokens),
            answer.body["user"]["id"].as_str().unwrap()
        );

        let rebound = call_on(
            &app,
            "evil.example:7171",
            "embed/entra",
            Some(&csrf),
            Some("http://evil.example:7171"),
            json!({}),
        )
        .await;
        assert_eq!(rebound.status, StatusCode::FORBIDDEN);
    }
}
