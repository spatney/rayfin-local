//! Request checks that run before the feature routers: the publishable key and the Host header.

use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{OriginalUri, Request, State};
use axum::http::uri::Authority;
use axum::http::{Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use crate::routes::{json_error, requires_publishable_key};

const PUBLISHABLE_KEY: &str = "x-publishable-key";

/// The path as the client sent it, even inside a nested router.
fn request_path(req: &Request) -> &str {
    match req.extensions().get::<OriginalUri>() {
        Some(OriginalUri(uri)) => uri.path(),
        None => req.uri().path(),
    }
}

/// The app's publishable key and the exact paths that need none.
pub(crate) struct KeyCheck {
    key: Box<str>,
    exempt: HashSet<String>,
}

impl KeyCheck {
    pub(crate) fn new(key: &str, exempt: &[String]) -> Self {
        Self {
            key: key.into(),
            exempt: exempt.iter().cloned().collect(),
        }
    }
}

/// Rejects SDK API calls without this app's publishable key: 403, never 401, because a 401 makes
/// the SDK refresh the session and replay, then sign the user out (compat §2.2, §2.5).
pub(crate) async fn require_publishable_key(
    State(check): State<Arc<KeyCheck>>,
    req: Request,
    next: Next,
) -> Response {
    let path = request_path(&req);
    if req.method() == Method::OPTIONS
        || !requires_publishable_key(path)
        || check.exempt.contains(path)
    {
        return next.run(req).await;
    }
    let message = match req.headers().get(PUBLISHABLE_KEY) {
        Some(value) if value.as_bytes() == check.key.as_bytes() => return next.run(req).await,
        Some(_) => {
            "The X-Publishable-Key header does not match this local backend's publishable key. \
             Start the app with `rayfin-local serve` (it sets VITE_RAYFIN_PUBLISHABLE_KEY for \
             the frontend) and reload the page."
        }
        None => {
            "The X-Publishable-Key header is missing. The Rayfin SDK sends it on every call; \
             other clients must send the publishable key from rayfin.config.json."
        }
    };
    json_error(StatusCode::FORBIDDEN, "invalid_publishable_key", message)
}

/// The Host names the server answers to (DNS-rebinding protection). Loopback names are always
/// allowed: a rebinding attack needs a name the attacker controls.
pub(crate) struct HostAllowlist {
    hosts: HashSet<String>,
    api_origin: String,
}

impl HostAllowlist {
    pub(crate) fn new(hosts: &[String], api_origin: &str) -> Self {
        Self {
            hosts: hosts
                .iter()
                .filter_map(|host| normalize_host(host))
                .collect(),
            api_origin: api_origin.trim_end_matches('/').to_owned(),
        }
    }

    fn allows(&self, host: &str) -> bool {
        is_loopback_host(host) || self.hosts.contains(host)
    }
}

pub(crate) async fn check_host(
    State(allowlist): State<Arc<HostAllowlist>>,
    req: Request,
    next: Next,
) -> Response {
    let host = match req.headers().get(header::HOST) {
        Some(value) => value.to_str().ok().and_then(normalize_host),
        None => req
            .uri()
            .authority()
            .and_then(|a| normalize_host(a.as_str())),
    };
    match host {
        Some(host) if allowlist.allows(&host) => next.run(req).await,
        host => {
            let shown = host.map_or_else(
                || "a missing or invalid Host header".to_owned(),
                |host| format!("host '{}'", host.chars().take(100).collect::<String>()),
            );
            let message = format!(
                "rayfin-local only answers requests addressed to localhost or its allowed hosts, \
                 not {shown} (DNS-rebinding protection). Open {} instead.",
                allowlist.api_origin
            );
            json_error(StatusCode::FORBIDDEN, "host_not_allowed", &message)
        }
    }
}

/// The host part of a Host header or host name, without port or IPv6 brackets, lowercased, with
/// IP addresses in canonical form.
fn normalize_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    // A Host header never carries user info.
    if raw.contains('@') {
        return None;
    }
    let authority: Authority = raw.parse().ok()?;
    let host = authority.host();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(ip.to_string());
    }
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// `localhost`, `*.localhost` (browsers resolve both to loopback themselves), `127.0.0.0/8` and
/// `::1`. Expects a host from [`normalize_host`].
fn is_loopback_host(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_headers_are_normalized_before_matching() {
        assert_eq!(
            normalize_host("LocalHost:7171").as_deref(),
            Some("localhost")
        );
        assert_eq!(normalize_host("[::1]:7171").as_deref(), Some("::1"));
        assert_eq!(normalize_host("[0:0:0:0:0:0:0:1]").as_deref(), Some("::1"));
        assert_eq!(normalize_host("::1").as_deref(), Some("::1"));
        assert_eq!(
            normalize_host("myapp.test.:80").as_deref(),
            Some("myapp.test")
        );
        assert_eq!(normalize_host("user@evil.example"), None);
        assert_eq!(normalize_host(""), None);
    }

    #[test]
    fn only_loopback_names_are_implicitly_allowed() {
        for host in [
            "localhost",
            "app.localhost",
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_loopback_host(&normalize_host(host).unwrap()), "{host}");
        }
        for host in [
            "localhost.evil.example",
            "evil.example",
            "10.0.0.1",
            "0.0.0.0",
            "::",
        ] {
            assert!(!is_loopback_host(&normalize_host(host).unwrap()), "{host}");
        }
    }
}
