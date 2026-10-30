//! The control API for hosts (docs/embedding.md): `{apiUrl}/.rayfin-local/control/*`, served when
//! `RAYFIN_LOCAL_CONTROL_TOKEN` is set. It answers only loopback peers that send that token as a
//! bearer token and no `Origin` (browsers send one, hosts don't); `rl-server` never lets CORS
//! allow it, and it needs no publishable key.
//!
//! Errors are `{"error": {"code", "message", "hint"}}`; `code` is stable.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::QueryRejection;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rl_auth::{AuthError, AuthService, SignInAs, User};
use rl_db::Db;
use rl_graphql::DataApi;
use rl_graphql::admin::{AdminError, MAX_ROWS, RowsQuery};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::events::{self, LiveState};

/// The rows `GET /entities/{name}/rows` reads without `limit`.
const DEFAULT_ROWS: u32 = 50;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What `POST /reload` asks of live reload.
pub trait ReloadTrigger: Send + Sync + 'static {
    /// Reads `rayfin.yml`, the entities and the secret values again and applies them now;
    /// `force_schema` applies a schema change that loses data. `None` when this run can't reload
    /// on request.
    fn reload(&self, force_schema: bool) -> BoxFuture<'_, Option<Reloaded>>;
}

/// What a reload did: its `schema` event and its `config-changed` event (`None` when the
/// configuration didn't change), as their lines have them (`Event::to_json`).
pub struct Reloaded {
    pub schema: Value,
    pub config: Option<Value>,
}

/// For runs whose live reload can't be asked to reload: `POST /reload` answers 501.
pub struct NoReload;

impl ReloadTrigger for NoReload {
    fn reload(&self, _force_schema: bool) -> BoxFuture<'_, Option<Reloaded>> {
        Box::pin(async { None })
    }
}

/// What the control API works on.
pub struct Control {
    /// The host's secret (`RAYFIN_LOCAL_CONTROL_TOKEN`).
    pub token: String,
    pub auth: Arc<AuthService>,
    pub db: Db,
    /// The data API (`None` when `services.data` is off): the served model and its rows.
    pub data: Option<Arc<DataApi>>,
    /// Where the stored files are (`.rayfin-local/storage`).
    pub storage_dir: PathBuf,
    pub reload: Arc<dyn ReloadTrigger>,
    /// The last events, for `GET /status`.
    pub live: LiveState,
}

type AppState = State<Arc<Control>>;

impl Control {
    /// The routes, with their checks, under `/.rayfin-local/control`.
    pub fn router(self) -> Router {
        let control = Arc::new(self);
        Router::new()
            .route("/.rayfin-local/control/status", get(status))
            .route(
                "/.rayfin-local/control/identity",
                get(identity).put(set_identity),
            )
            .route("/.rayfin-local/control/users", get(users).post(add_user))
            .route("/.rayfin-local/control/users/{user}", delete(delete_user))
            .route("/.rayfin-local/control/token", post(token))
            .route("/.rayfin-local/control/entities", get(entities))
            .route(
                "/.rayfin-local/control/entities/{name}/rows",
                get(rows).delete(clear_rows),
            )
            .route("/.rayfin-local/control/data/reset", post(reset_data))
            .route("/.rayfin-local/control/reload", post(reload))
            .route_layer(middleware::from_fn_with_state(Arc::clone(&control), guard))
            .layer(middleware::map_response(no_store))
            .with_state(control)
    }
}

/// An error answer: `{"error": {"code", "message", "hint"}}`.
#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    hint: Option<String>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            hint: None,
        }
    }

    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid-request", message)
    }

    fn user_not_found(user: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "user-not-found",
            format!("There is no user {user}."),
        )
        .hint("Add the user with POST /users, or send \"create\": true.")
    }

    /// A failure of rayfin-local's own, logged; the host can only retry.
    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!("a control API request failed: {error}");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            format!("rayfin-local failed: {error}."),
        )
        .hint("See rayfin-local's log, then retry.")
    }
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::InvalidInput(message) => Self::invalid(message),
            error => Self::internal(error),
        }
    }
}

impl From<AdminError> for ApiError {
    fn from(error: AdminError) -> Self {
        match &error {
            AdminError::UnknownEntity(_) => Self::new(
                StatusCode::NOT_FOUND,
                "entity-not-found",
                capitalized(&error),
            )
            .hint("GET /entities lists the app's entities."),
            AdminError::UnknownField { .. } => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid-order-by",
                capitalized(&error),
            ),
            AdminError::Referenced { .. } => {
                Self::new(StatusCode::CONFLICT, "rows-referenced", capitalized(&error)).hint(
                    "Clear the rows that refer to them first, or delete all the local data with \
                     POST /data/reset.",
                )
            }
            AdminError::Database(_) => Self::internal(&error),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "error": { "code": self.code, "message": self.message, "hint": self.hint },
        });
        let mut response = (self.status, Json(body)).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

/// `error`'s message as a sentence.
fn capitalized(error: &impl std::fmt::Display) -> String {
    let text = error.to_string();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => text,
    }
}

/// Only the host: a loopback peer, no `Origin`, and the token.
async fn guard(State(control): AppState, request: Request, next: Next) -> Response {
    let loopback = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ConnectInfo(peer)| is_loopback(peer.ip()));
    if !loopback {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "The control API only answers programs on this machine.",
        )
        .into_response();
    }
    if request.headers().contains_key(header::ORIGIN) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "The control API doesn't answer requests from web pages (with an Origin header).",
        )
        .into_response();
    }
    let presented = rl_model::bearer_token(
        request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok()),
    );
    if !presented.is_some_and(|token| same_secret(token, &control.token)) {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "The control API needs the token in RAYFIN_LOCAL_CONTROL_TOKEN as a bearer token \
             (the Authorization header).",
        )
        .into_response();
    }
    next.run(request).await
}

async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback(),
        IpAddr::V6(ip) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Compares the secrets' SHA-256 digests without an early exit, so neither how much of a guess
/// matched nor its length shows in the timing.
fn same_secret(presented: &str, expected: &str) -> bool {
    let digest = |text: &str| ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    let (presented, expected) = (digest(presented), digest(expected));
    let difference = presented
        .as_ref()
        .iter()
        .zip(expected.as_ref())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b));
    std::hint::black_box(difference) == 0
}

/// A JSON body; an empty one is `T::default()`.
fn body<T: DeserializeOwned + Default>(bytes: &[u8]) -> Result<T, ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes).map_err(|error| {
        ApiError::invalid(format!(
            "The body is not the JSON this route takes: {error}."
        ))
    })
}

/// A user as hosts see it.
fn user_json(user: &User) -> Value {
    let mut sign_in = Vec::new();
    if user.has_password {
        sign_in.push("password");
    }
    if user.entra_oid.is_some() {
        sign_in.push("entra");
    }
    json!({
        "id": user.id,
        "username": user.username,
        "email": user.email,
        "createdAt": events::timestamp(user.created_at),
        "signIn": sign_in,
        "developer": user.is_developer(),
    })
}

async fn status(State(control): AppState) -> Result<Json<Value>, ApiError> {
    let Some(mut status) = control.live.status() else {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "starting",
            "rayfin-local is still starting.",
        )
        .hint("Ask again after the `ready` event."));
    };
    // The identity as it is now: the host may have changed it since `ready`.
    status["signInAs"] = serde_json::to_value(events::SignInAs::of(&control.auth.sign_in_as()))
        .unwrap_or(Value::Null);
    Ok(Json(status))
}

async fn identity(State(control): AppState) -> Result<Json<Value>, ApiError> {
    Ok(Json(identity_json(&control.auth).await?))
}

/// `{"kind", "user"}`.
async fn identity_json(auth: &AuthService) -> Result<Value, ApiError> {
    let kind = match auth.sign_in_as() {
        SignInAs::Developer => "developer",
        SignInAs::User(_) => "user",
        SignInAs::None => "none",
    };
    let user = auth.identity_user().await?;
    Ok(json!({ "kind": kind, "user": user.as_ref().map(user_json) }))
}

#[derive(Default, Deserialize)]
struct IdentityRequest {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    create: bool,
}

async fn set_identity(State(control): AppState, bytes: Bytes) -> Result<Json<Value>, ApiError> {
    let request: IdentityRequest = body(&bytes)?;
    let identity = match request.kind.as_str() {
        "developer" => SignInAs::Developer,
        "none" => SignInAs::None,
        "user" => {
            let name = named(request.user.as_deref()).ok_or_else(|| {
                ApiError::invalid("`user` names the user to sign in as, by username or id.")
            })?;
            let user = find_user(&control.auth, name, request.create).await?;
            SignInAs::User(user.username)
        }
        other => {
            return Err(ApiError::invalid(format!(
                "`kind` is `developer`, `user` or `none`, not `{other}`."
            )));
        }
    };
    let ended = control.auth.switch_sign_in_as(identity).await?;
    tracing::info!("Changed who the app signs in as automatically; signed out {ended} sessions");
    Ok(Json(identity_json(&control.auth).await?))
}

/// A non-blank name, trimmed.
fn named(name: Option<&str>) -> Option<&str> {
    name.map(str::trim).filter(|name| !name.is_empty())
}

/// The user with this username or id; with `create`, a new one without a password when missing.
async fn find_user(auth: &AuthService, name: &str, create: bool) -> Result<User, ApiError> {
    let found = if create {
        auth.find_or_create_user(name).await.map(Some)?
    } else {
        auth.find_user(name).await?
    };
    found.ok_or_else(|| ApiError::user_not_found(name))
}

async fn users(State(control): AppState) -> Result<Json<Value>, ApiError> {
    let users = control.auth.list_users().await?;
    let users: Vec<Value> = users.iter().map(user_json).collect();
    Ok(Json(json!({ "users": users })))
}

#[derive(Default, Deserialize)]
struct NewUser {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

async fn add_user(State(control): AppState, bytes: Bytes) -> Result<Response, ApiError> {
    let request: NewUser = body(&bytes)?;
    let username = named(request.username.as_deref())
        .ok_or_else(|| ApiError::invalid("`username` is required."))?;
    let created = match &request.password {
        Some(password) => control.auth.create_user(username, password).await,
        None => control.auth.create_user_without_password(username).await,
    };
    match created {
        Ok(user) => Ok((StatusCode::CREATED, Json(user_json(&user))).into_response()),
        Err(AuthError::UsernameTaken) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "user-exists",
            format!("A user {username} already exists."),
        )
        .hint("Pick another username, or delete that user first.")),
        Err(error) => Err(error.into()),
    }
}

async fn delete_user(
    State(control): AppState,
    Path(user): Path<String>,
) -> Result<StatusCode, ApiError> {
    match control.auth.delete_user(&user).await {
        Ok(_) => Ok(StatusCode::NO_CONTENT),
        Err(AuthError::UserNotFound) => Err(ApiError::user_not_found(&user)),
        Err(error) => Err(error.into()),
    }
}

#[derive(Default, Deserialize)]
struct TokenRequest {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    create: bool,
}

async fn token(State(control): AppState, bytes: Bytes) -> Result<Json<Value>, ApiError> {
    let request: TokenRequest = body(&bytes)?;
    let user = match named(request.user.as_deref()) {
        None => control.auth.developer_account().await?,
        Some(name) => find_user(&control.auth, name, request.create).await?,
    };
    let tokens = control.auth.issue_access_token(&user).await?;
    Ok(Json(json!({
        "accessToken": tokens.access_token,
        "tokenType": tokens.token_type,
        "expiresIn": tokens.expires_in,
        "user": user_json(&user),
    })))
}

/// The data API, or why there are no entities.
fn data(control: &Control, entity: &str) -> Result<Arc<DataApi>, ApiError> {
    control.data.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "entity-not-found",
            format!("There is no entity {entity}: the app's data service is off."),
        )
        .hint("services.data.enabled is false in rayfin/rayfin.yml.")
    })
}

async fn entities(State(control): AppState) -> Result<Json<Value>, ApiError> {
    let counts = match &control.data {
        Some(data) => data.row_counts().await?,
        None => Vec::new(),
    };
    let entities: Vec<Value> = counts
        .into_iter()
        .map(|(name, rows)| json!({ "name": name, "rows": rows }))
        .collect();
    Ok(Json(json!({ "entities": entities })))
}

async fn rows(
    State(control): AppState,
    Path(entity): Path<String>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid("The query string is malformed."))?;
    let rows_query = rows_query(&query)?;
    let rows = data(&control, &entity)?.rows(&entity, &rows_query).await?;
    serde_json::to_value(rows)
        .map(Json)
        .map_err(ApiError::internal)
}

/// `offset`, `limit` (1 to [`MAX_ROWS`]), `orderBy` and `desc`.
fn rows_query(query: &HashMap<String, String>) -> Result<RowsQuery, ApiError> {
    let text = |name: &str| {
        query
            .get(name)
            .map(|value| value.trim())
            .filter(|v| !v.is_empty())
    };
    let offset = match text("offset") {
        None => 0,
        Some(offset) => offset.parse().map_err(|_| {
            ApiError::invalid(format!(
                "`offset` is the number of rows to skip, not `{offset}`."
            ))
        })?,
    };
    let limit = match text("limit") {
        None => DEFAULT_ROWS,
        Some(limit) => limit
            .parse()
            .ok()
            .filter(|limit| (1..=MAX_ROWS).contains(limit))
            .ok_or_else(|| {
                ApiError::invalid(format!(
                    "`limit` is a number of rows from 1 to {MAX_ROWS}, not `{limit}`."
                ))
            })?,
    };
    let desc = match text("desc").map(str::to_ascii_lowercase).as_deref() {
        None | Some("false" | "0") => false,
        Some("true" | "1") => true,
        Some(other) => {
            return Err(ApiError::invalid(format!(
                "`desc` is `true` or `false`, not `{other}`."
            )));
        }
    };
    Ok(RowsQuery {
        offset,
        limit,
        order_by: text("orderBy").map(str::to_owned),
        desc,
    })
}

async fn clear_rows(
    State(control): AppState,
    Path(entity): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let deleted = data(&control, &entity)?.clear_rows(&entity).await?;
    Ok(Json(json!({ "deleted": deleted })))
}

async fn reset_data(State(control): AppState) -> Result<Json<Value>, ApiError> {
    if let Some(data) = &control.data {
        data.clear_all_rows().await?;
    }
    control.auth.delete_all_users().await?;
    rl_storage::clear(&control.db, &control.storage_dir)
        .await
        .map_err(ApiError::internal)?;
    tracing::info!("Deleted the local data: the entities' rows, the users and the stored files");
    Ok(Json(json!({ "ok": true })))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReloadRequest {
    #[serde(default)]
    force_schema: bool,
}

async fn reload(State(control): AppState, bytes: Bytes) -> Result<Json<Value>, ApiError> {
    let request: ReloadRequest = body(&bytes)?;
    match control.reload.reload(request.force_schema).await {
        Some(reloaded) => Ok(Json(
            json!({ "schema": reloaded.schema, "config": reloaded.config }),
        )),
        None => Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "not-available",
            "This rayfin-local can't reload on request.",
        )
        .hint(
            "It reloads the entities by itself when their files change; restart `serve` for \
             rayfin.yml and secret changes (with --force-schema to apply a schema change that \
             loses data).",
        )),
    }
}

#[cfg(test)]
mod tests;
