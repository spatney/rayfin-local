//! CORS for apps that call the backend directly from another origin (`VITE_RAYFIN_API_URL` as the
//! SDK's `baseUrl`), such as the Vite dev server on another port.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::request::Parts;
use axum::http::{HeaderName, Method, header};
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};
use url::{Host, Url};

use crate::guards::{self, HostAllowlist};
use crate::routes;

/// Response headers the SDK and apps read: correlation and request ids (storage, connectors),
/// the functions status, `Retry-After` on 429/503, and the download metadata (compat §9.2.4).
fn exposed_headers() -> [HeaderName; 7] {
    [
        HeaderName::from_static("x-correlation-id"),
        HeaderName::from_static("requestid"),
        HeaderName::from_static("x-fabric-udf-status"),
        header::RETRY_AFTER,
        header::CONTENT_DISPOSITION,
        HeaderName::from_static("x-ms-request-id"),
        header::ETAG,
    ]
}

/// Allows loopback origins on any port, `extra_origins` (the app's `allowedRedirectUris`) and,
/// with the Host check on (`hosts`), origins on the host a request is addressed to when that host
/// passes the check: with `--host 192.168.1.20`, the dev server at `http://192.168.1.20:5173`
/// calls the backend at `http://192.168.1.20:7171`. Request headers are mirrored, so every SDK
/// header (`X-Publishable-Key`, `Authorization`, `x-ms-workload-resource-moniker`,
/// `X-Correlation-ID`, ...) passes a preflight. No credentials: the SDK sends bearer tokens,
/// never cookies. Routes for programs rather than browsers (`routes::is_private`) allow no
/// origin.
pub(crate) fn layer(extra_origins: &[String], hosts: Option<Arc<HostAllowlist>>) -> CorsLayer {
    let extra: HashSet<String> = extra_origins
        .iter()
        .filter_map(|origin| normalize_origin(origin))
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, parts| {
            !routes::is_private(parts.uri.path())
                && origin.to_str().is_ok_and(|origin| {
                    is_loopback_origin(origin)
                        || extra.contains(origin)
                        || hosts
                            .as_deref()
                            .is_some_and(|hosts| is_allowed_host_origin(origin, parts, hosts))
                })
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers(exposed_headers())
        .max_age(Duration::from_secs(600))
}

/// The serialized origin of a URL (`https://App.example/cb` → `https://app.example`), as browsers
/// send it in `Origin`.
fn normalize_origin(value: &str) -> Option<String> {
    let origin = Url::parse(value.trim()).ok()?.origin();
    origin.is_tuple().then(|| origin.ascii_serialization())
}

/// `origin` parsed, when it is an `http(s)` origin as browsers serialize it.
fn parse_origin(origin: &str) -> Option<Url> {
    let url = Url::parse(origin).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.origin().ascii_serialization() == origin)
        .then_some(url)
}

/// `http(s)://` + `localhost`, `*.localhost`, `127.0.0.0/8` or `[::1]`, on any port.
fn is_loopback_origin(origin: &str) -> bool {
    parse_origin(origin).is_some_and(|url| match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost" || domain.ends_with(".localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    })
}

/// Whether `origin` is on the host the request is addressed to, on any port (another server on
/// the same machine), and that host passes the Host check.
fn is_allowed_host_origin(origin: &str, request: &Parts, hosts: &HostAllowlist) -> bool {
    let Some(host) = guards::request_host(&request.headers, &request.uri) else {
        return false;
    };
    hosts.allows(&host)
        && parse_origin(origin)
            .and_then(|url| url.host_str().and_then(guards::normalize_host))
            .is_some_and(|origin_host| origin_host == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_origins_on_any_port() {
        for origin in [
            "http://localhost:5175",
            "https://localhost",
            "http://127.0.0.1:3000",
            "http://[::1]:8080",
            "http://app.localhost:5173",
        ] {
            assert!(is_loopback_origin(origin), "{origin}");
        }
        for origin in [
            "null",
            "http://localhost.evil.example",
            "http://192.168.1.10:5173",
            "file:///C:/app/index.html",
            "ws://localhost:5173",
            "http://localhost:5175/path",
        ] {
            assert!(!is_loopback_origin(origin), "{origin}");
        }
    }

    #[test]
    fn configured_origins_are_compared_as_browsers_serialize_them() {
        assert_eq!(
            normalize_origin("https://App.Contoso.com:443/auth/callback").as_deref(),
            Some("https://app.contoso.com")
        );
        assert_eq!(
            normalize_origin("http://localhost:5174/").as_deref(),
            Some("http://localhost:5174")
        );
        assert_eq!(normalize_origin("not a url"), None);
        assert_eq!(normalize_origin("file:///tmp/x"), None);
    }
}
