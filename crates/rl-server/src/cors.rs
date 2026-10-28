//! CORS for apps that call the backend directly from another origin (`VITE_RAYFIN_API_URL` as the
//! SDK's `baseUrl`), such as the Vite dev server on another port.

use std::collections::HashSet;
use std::time::Duration;

use axum::http::{HeaderName, Method, header};
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};
use url::{Host, Url};

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

/// Allows loopback origins on any port plus `extra_origins` (the app's `allowedRedirectUris`).
/// Request headers are mirrored, so every SDK header (`X-Publishable-Key`, `Authorization`,
/// `x-ms-workload-resource-moniker`, `X-Correlation-ID`, ...) passes a preflight. No credentials:
/// the SDK sends bearer tokens, never cookies.
pub(crate) fn layer(extra_origins: &[String]) -> CorsLayer {
    let extra: HashSet<String> = extra_origins
        .iter()
        .filter_map(|origin| normalize_origin(origin))
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            origin
                .to_str()
                .is_ok_and(|origin| is_loopback_origin(origin) || extra.contains(origin))
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

/// `http(s)://` + `localhost`, `*.localhost`, `127.0.0.0/8` or `[::1]`, on any port.
fn is_loopback_origin(origin: &str) -> bool {
    let Ok(url) = Url::parse(origin) else {
        return false;
    };
    let loopback = match url.host() {
        Some(Host::Domain(domain)) => domain == "localhost" || domain.ends_with(".localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    loopback
        && matches!(url.scheme(), "http" | "https")
        && url.origin().ascii_serialization() == origin
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
