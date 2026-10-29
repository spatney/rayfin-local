//! HTTP composition for the local backend: `/rayfin.config.json`, CORS, the publishable-key and
//! Host checks, static hosting for `--static`, and merging the feature routers.
//!
//! [`build_router`] merges the feature crates' routers (state already applied) and adds:
//!
//! - `GET /rayfin.config.json`: `{apiUrl, publishableKey, workspaceId, itemId, portalUrl,
//!   tenantId}` with `Cache-Control: no-store` (compat §1). `apiUrl` is
//!   [`ServerConfig::api_origin`], the backend's own absolute origin. The SPA reads this file only
//!   in `--static` mode, where it shares that origin. In dev mode the Vite plugin serves its own
//!   copy (with the dev server's origin), and without the plugin Vite answers its index.html,
//!   which the SDK treats as "no file", so it uses the build-time `VITE_RAYFIN_API_URL`.
//! - `GET /.rayfin-local/health` ([`HEALTH_PATH`]): `{"ok":true}`.
//! - The publishable key: `/api/*`, `/graphql`, `/functions/*`, `/connector-invoke/*` and
//!   `/connectors/*` need `X-Publishable-Key` equal to the app's key. A missing or wrong key is
//!   `403 {"error":"invalid_publishable_key","error_description","message"}`, never 401, which
//!   would make the SDK refresh and then sign the user out (compat §2.2, §2.5, §9.1, §9.3). The
//!   JWKS (`/.well-known/*`, `/api/auth/v1/.well-known/*`, compat §3.6), `/rayfin.config.json`,
//!   `/.rayfin-local/*` (portal, signed storage URLs, health), static files and `OPTIONS` are exempt.
//! - CORS for loopback origins on any port, [`ServerConfig::allowed_origins`] and, with the Host
//!   check on, origins on the host a request is addressed to (another port of the same machine):
//!   the usual methods, mirrored request headers, the SDK's response headers exposed, no
//!   credentials, 10-minute preflight cache.
//! - The Host check, when [`ServerConfig::allowed_hosts`] is set (DNS-rebinding protection).
//! - Static hosting with the SPA fallback, when [`ServerConfig::static_site`] is set.
//! - A JSON 404 (`{"error":"not_found",...}`) for everything else.
//! - Request logs at debug level: method, path (no query), status and latency; never headers or
//!   bodies.
//!
//! [`bind`] and [`serve`] run the router. "compat" is micro-fabric's `docs/compatibility.md`.

mod cors;
mod guards;
mod listen;
mod routes;
mod static_site;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::Request;
use axum::http::header;
use axum::middleware::from_fn_with_state;
use axum::response::Response;
use axum::routing::get;
use serde::Serialize;
use tower_http::trace::TraceLayer;
use tracing::Span;

pub use guards::{ANONYMOUS_ACCESS_DISABLED, reject_anonymous};
pub use listen::{bind, is_port_free, serve};

/// The readiness endpoint; answers `{"ok":true}`.
pub const HEALTH_PATH: &str = "/.rayfin-local/health";

/// What the server needs to know about the app; filled by the `serve` command.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// The origin the backend is reached at, such as `http://localhost:7171`. It is the `apiUrl`
    /// of `/rayfin.config.json`.
    pub api_origin: String,
    pub publishable_key: String,
    pub workspace_id: String,
    pub item_id: String,
    pub tenant_id: Option<String>,
    /// The sign-in portal, such as `http://localhost:7171/.rayfin-local/portal`.
    pub portal_url: String,
    /// Origins allowed by CORS besides loopback ones, which are always allowed (the app's
    /// `allowedRedirectUris`; URLs are reduced to their origin).
    pub allowed_origins: Vec<String>,
    /// Host names (ports ignored) requests must be addressed to; loopback names are always
    /// allowed. CORS then also allows origins on the host a request is addressed to, on any port.
    /// `None` turns the check off.
    pub allowed_hosts: Option<Vec<String>>,
    /// The built app to serve (`--static`).
    pub static_site: Option<StaticSite>,
    /// Exact paths under the keyed roots that need no publishable key, such as the auth routes
    /// the real service serves without one (`rl_auth::PUBLISHABLE_KEY_EXEMPT_PATHS`).
    pub publishable_key_exempt: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct StaticSite {
    /// The build output folder (`staticHosting.folder`).
    pub root: PathBuf,
    /// The SPA entry point relative to `root` (`staticHosting.indexDocument`, usually `index.html`).
    pub index_document: String,
}

/// The runtime config file the SDK's `resolveRayfinConfig()` reads (compat §1). Empty values are
/// left out, as the SDK treats them as absent anyway.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeConfig<'a> {
    api_url: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    publishable_key: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    portal_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tenant_id: Option<&'a str>,
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

/// Signed storage URLs carry their capability token in the path; logs keep only the route.
const SIGNED_URL_ROOT: &str = "/.rayfin-local/storage/";

fn loggable_path(path: &str) -> std::borrow::Cow<'_, str> {
    match path.strip_prefix(SIGNED_URL_ROOT) {
        Some(rest) => {
            let kind = rest.split('/').next().unwrap_or_default();
            format!("{SIGNED_URL_ROOT}{kind}/<redacted>").into()
        }
        None => path.into(),
    }
}

fn runtime_config_json(config: &ServerConfig) -> Bytes {
    let runtime_config = RuntimeConfig {
        api_url: config.api_origin.trim().trim_end_matches('/'),
        publishable_key: non_empty(&config.publishable_key),
        workspace_id: non_empty(&config.workspace_id),
        item_id: non_empty(&config.item_id),
        portal_url: non_empty(&config.portal_url),
        tenant_id: config.tenant_id.as_deref().and_then(non_empty),
    };
    Bytes::from(serde_json::to_vec(&runtime_config).expect("the runtime config serializes"))
}

fn json_body(body: Bytes) -> impl axum::response::IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
}

/// Merges the feature routers and adds the shared routes, checks and fallback (see the crate docs).
///
/// Feature routers must not set a fallback or define `/rayfin.config.json` or [`HEALTH_PATH`]:
/// axum panics on merging those.
pub fn build_router(config: ServerConfig, features: Vec<Router>) -> Router {
    let runtime_config = runtime_config_json(&config);
    let mut router = Router::new()
        .route(
            "/rayfin.config.json",
            get(move || async move { json_body(runtime_config) }),
        )
        .route(
            HEALTH_PATH,
            get(|| async { json_body(Bytes::from_static(br#"{"ok":true}"#)) }),
        );
    for feature in features {
        router = router.merge(feature);
    }

    router = match config.static_site {
        Some(site) => {
            let files = Arc::new(static_site::StaticFiles::new(site));
            router.fallback(move |req: Request| async move { files.respond(req).await })
        }
        None => router.fallback(|req: Request| async move {
            routes::not_found(req.method(), req.uri().path())
        }),
    };

    // The last layer added runs first: logging, CORS (so rejections carry CORS headers too and the
    // SDK can read them), the Host check, then the publishable key.
    let key = Arc::new(guards::KeyCheck::new(
        &config.publishable_key,
        &config.publishable_key_exempt,
    ));
    router = router.layer(from_fn_with_state(key, guards::require_publishable_key));
    let allowlist = config
        .allowed_hosts
        .as_ref()
        .map(|hosts| Arc::new(guards::HostAllowlist::new(hosts, &config.api_origin)));
    if let Some(allowlist) = &allowlist {
        router = router.layer(from_fn_with_state(
            Arc::clone(allowlist),
            guards::check_host,
        ));
    }
    router.layer(cors::layer(&config.allowed_origins, allowlist)).layer(
        TraceLayer::new_for_http()
            .make_span_with(|req: &Request| {
                tracing::debug_span!("http", method = %req.method(), path = %loggable_path(req.uri().path()))
            })
            .on_request(())
            .on_response(|res: &Response<Body>, latency: Duration, _: &Span| {
                tracing::debug!(
                    status = res.status().as_u16(),
                    latency_ms = u64::try_from(latency.as_millis()).unwrap_or(u64::MAX),
                    "request"
                );
            })
            .on_body_chunk(())
            .on_eos(())
            .on_failure(()),
    )
}

#[cfg(test)]
mod tests {
    use super::loggable_path;

    #[test]
    fn signed_storage_urls_are_redacted_in_logs() {
        assert_eq!(
            loggable_path("/.rayfin-local/storage/u/abc.123.mac"),
            "/.rayfin-local/storage/u/<redacted>"
        );
        assert_eq!(loggable_path("/graphql"), "/graphql");
    }
}
